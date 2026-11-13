use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::event_types::{self, EventFields, is_truthy};
use charliebot_core::text_repr::value_str;
use serde_json::{Value, json};

use crate::hooks::EventRenderer;
use crate::message_events;
use crate::metadata::BASE_BRANCH_FIELD;
use crate::streaming::{APPEND, LENGTH, THINKING_APPEND, THINKING_LENGTH};
use crate::tool_rows::{SubagentStatus, ToolRow, ToolRows, Usage, tool_preview};
use crate::{StoreError, StoreResult};

pub use crate::tool_rows::TOOL_PREVIEW_CHARS;

const BUILTIN_EVENT_TYPES: &[&str] = &[
  event_types::MASTER_DONE,
  event_types::ASSISTANT_ERROR,
  event_types::ERROR,
  event_types::TASK_DELEGATED,
  event_types::WORKER_SUMMARY,
  event_types::HANDLER_RESULT,
  event_types::CONTEXT_COMPACTED,
  event_types::CONTEXT_COMPACT_FAILED,
  event_types::RESUME_CONTEXT_DROPPED,
  event_types::SYSTEM,
  event_types::CLONE_START,
  event_types::SCHEDULED_TRIGGER,
  event_types::AGENT_MESSAGE,
  event_types::BACKEND_SWITCHED,
  event_types::QUOTA_FAILOVER,
  event_types::BACKEND_OVERLAY_INACTIVE,
  event_types::BACKEND_OVERLAY_UNDECLARED,
  event_types::MODEL_FALLBACK_NOTICE,
  event_types::CHILD_REPORT,
  event_types::RUN_HEADER,
  event_types::RUN_END,
  event_types::RUN_ERROR,
  event_types::RUN_DELIVERY,
  event_types::TASK_CLOSED,
  event_types::TASK_REOPENED,
  event_types::TASK_INPUT_ACKNOWLEDGED,
];

/// Event types rendered by the built-in message table.
pub fn builtin_event_types() -> &'static [&'static str] {
  BUILTIN_EVENT_TYPES
}

/// Convert raw chat events into committed message and in-progress stream frames.
///
/// A `message_update` frame carries a committed message again, with the same id. A subagent event that arrives
/// after the commit changes that message's rows, so its consumers replace the message rather than append it.
pub struct MessageAggregator {
  event_index_offset: usize,
  processed: usize,
  assistant_buf: String,
  thinking_buf: String,
  last_assistant_timestamp: Option<Value>,
  last_event_index: usize,
  last_event_id: Option<String>,
  tool_rows: ToolRows,
  emit_stream_deltas: bool,
  omit_streamed_content: bool,
  /// The global index of the event being fed, which a stream frame carries so a page can place it in the replay.
  current_index: usize,
  /// What earlier stream frames carried: the assistant text's byte length, and the thinking text itself, which
  /// an assistant event may replace rather than extend. The UTF-16 lengths are the draft's lengths at that point.
  streamed_text_bytes: usize,
  streamed_text_utf16: usize,
  streamed_thinking: String,
  streamed_thinking_utf16: usize,
  /// The draft tool rows of the previous stream frame, so a frame repeats them only when they changed.
  streamed_tools: Option<Value>,
  event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
}

impl MessageAggregator {
  /// Build an aggregator with a global index offset and stream-delta policy.
  pub fn new(event_index_offset: usize, emit_stream_deltas: bool) -> Self {
    Self::with_renderers(event_index_offset, emit_stream_deltas, HashMap::new())
  }

  /// Build an aggregator with feature renderers supplied by the store.
  pub fn with_renderers(
    event_index_offset: usize,
    emit_stream_deltas: bool,
    event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
  ) -> Self {
    Self {
      event_index_offset,
      processed: 0,
      assistant_buf: String::new(),
      thinking_buf: String::new(),
      last_assistant_timestamp: None,
      last_event_index: 0,
      last_event_id: None,
      tool_rows: ToolRows::default(),
      emit_stream_deltas,
      omit_streamed_content: false,
      current_index: 0,
      streamed_text_bytes: 0,
      streamed_text_utf16: 0,
      streamed_thinking: String::new(),
      streamed_thinking_utf16: 0,
      streamed_tools: None,
      event_renderers,
    }
  }

  /// Process one persisted event and return its message or stream frames.
  pub fn feed<E: EventFields>(&mut self, event: &E) -> StoreResult<Vec<Value>> {
    let index = self.event_index_offset + self.processed;
    self.processed += 1;
    self.feed_at(event, index)
  }

  /// Process borrowed events whose source indexes are already known.
  pub fn feed_indexed<E: EventFields>(&mut self, events: &[(usize, &E)]) -> StoreResult<Vec<Value>> {
    let mut deltas = Vec::new();
    for &(index, event) in events {
      self.processed += 1;
      deltas.extend(self.feed_at(event, self.event_index_offset + index)?);
    }
    Ok(deltas)
  }

  /// Process an event at the absolute event index the store stamped on it, which is its position in the
  /// session's event log. The fed-event counter skips events that are appended without being fed (control
  /// facts), so a frame that used it could carry an index below the event's place in the log.
  pub fn feed_at_absolute<E: EventFields>(&mut self, event: &E, index: usize) -> StoreResult<Vec<Value>> {
    self.processed += 1;
    self.feed_at(event, index)
  }

  /// Return an independent copy that continues at the same event index.
  pub fn clone_state(&self) -> Self {
    Self {
      event_index_offset: self.event_index_offset,
      processed: self.processed,
      assistant_buf: self.assistant_buf.clone(),
      thinking_buf: self.thinking_buf.clone(),
      last_assistant_timestamp: self.last_assistant_timestamp.clone(),
      last_event_index: self.last_event_index,
      last_event_id: self.last_event_id.clone(),
      tool_rows: self.tool_rows.clone(),
      emit_stream_deltas: self.emit_stream_deltas,
      omit_streamed_content: self.omit_streamed_content,
      current_index: self.current_index,
      streamed_text_bytes: self.streamed_text_bytes,
      streamed_text_utf16: self.streamed_text_utf16,
      streamed_thinking: self.streamed_thinking.clone(),
      streamed_thinking_utf16: self.streamed_thinking_utf16,
      streamed_tools: self.streamed_tools.clone(),
      event_renderers: self.event_renderers.clone(),
    }
  }

  /// Commit a buffered draft at the end of a stable history.
  pub fn flush_pending(&mut self) -> Vec<Value> {
    self.flush_to_message_delta()
  }

  /// Return a copy of the current draft, or no value when the draft is empty.
  pub fn pending_draft_message(&self) -> Option<Value> {
    if self.assistant_buf.is_empty() && self.tool_rows.is_draft_empty() && self.thinking_buf.is_empty() {
      return None;
    }
    let mut message = json!({
      "role": "assistant",
      "content": self.assistant_buf.as_str(),
      "event_index": self.last_event_index,
      "id": self.last_event_id.clone().unwrap_or_else(|| format!("legacy:{}", self.last_event_index)),
      "timestamp": self.last_assistant_timestamp.clone().unwrap_or(Value::Null),
    });
    if let Some(tools) = self.tool_rows.draft_json() {
      message["tools"] = tools;
    }
    if !self.thinking_buf.is_empty() {
      message["thinking"] = json!(self.thinking_buf.as_str());
    }
    Some(message)
  }

  /// Set whether future feeds emit stream frames.
  pub fn set_emit_stream_deltas(&mut self, emit: bool) {
    self.emit_stream_deltas = emit;
  }

  /// Return whether stream frames are enabled.
  pub fn emits_stream_deltas(&self) -> bool {
    self.emit_stream_deltas
  }

  /// Set whether a committed draft leaves out the text already sent in stream frames. The live feed sets this,
  /// so a page that holds the draft is not sent it twice; a replay keeps the full text for a page that missed it.
  pub fn set_omit_streamed_content(&mut self, omit: bool) {
    self.omit_streamed_content = omit;
  }

  fn feed_at<E: EventFields>(&mut self, event: &E, index: usize) -> StoreResult<Vec<Value>> {
    self.current_index = index;
    if !event.is_object() {
      return Err(StoreError::new("chat event must be a JSON object"));
    }
    let event_type = event.field("type").and_then(EventFields::as_str_field);
    let event_id =
      event.field("id").filter(|value| is_truthy(*value)).map(event_str).unwrap_or_else(|| format!("legacy:{index}"));
    match event_type {
      Some(event_types::USER) => self.feed_user(event, index, &event_id),
      Some(event_types::ASSISTANT) => self.feed_assistant(event, index, &event_id),
      Some(event_types::TOOL_USE) => self.feed_tool_use(event, index, &event_id),
      Some(event_types::TOOL_RESULT) => self.feed_tool_result(event, index),
      Some(event_types::THINKING) => self.feed_thinking(event, index, &event_id),
      _ => self.feed_rendered(event, event_type, index, &event_id),
    }
  }

  fn feed_user<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    if let Some(parent) = parent_tool_use_id(event)? {
      return self.feed_subagent_user(event, parent);
    }
    if event.field("message").is_some() && event.field("content").is_none() {
      let content = event.field("message").and_then(|message| message.field("content"));
      let mut deltas = Vec::new();
      for block in message_blocks(content, "user message content", false)? {
        if block.field("type").and_then(EventFields::as_str_field) != Some("tool_result") {
          continue;
        }
        let tool_use_id = block.field(event_types::TOOL_USE_ID).and_then(EventFields::as_str_field);
        if !self.result_has_target(tool_use_id) {
          continue;
        }
        let output = extract_tool_result_text(block)?;
        let is_error = block.field("is_error").is_some_and(is_truthy);
        deltas.extend(self.attach_result(tool_use_id, output, is_error));
      }
      deltas.extend(self.stream_delta());
      return Ok(deltas);
    }

    let mut deltas = self.flush_to_message_delta();
    let normalized = message_events::normalize_user_message_event(event)?;
    let mut message = json!({
      "role": "user",
      "content": normalized.content,
      (event_types::UPLOADED_FILES): normalized.uploaded_files,
      (event_types::INPUT_MODE): event
        .field(event_types::INPUT_MODE)
        .map(EventFields::to_owned_value)
        .unwrap_or(Value::Null),
      "event_index": index,
      "id": event_id,
      "timestamp": event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null),
    });
    if let Some(request_id) = event.field("request_id") {
      message["request_id"] = request_id.to_owned_value();
    }
    deltas.push(json!({"type": "message", "message": message}));
    Ok(deltas)
  }

  fn feed_assistant<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    if let Some(parent) = parent_tool_use_id(event)? {
      return self.feed_subagent_assistant(event, parent);
    }
    self.last_event_index = index;
    if self.assistant_buf.is_empty() {
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    let message = event.field("message").filter(|value| is_truthy(*value));
    if message.is_some_and(|value| !value.is_object()) {
      return Err(StoreError::new("assistant event message must be an object"));
    }
    let content = message.and_then(|message| message.field("content"));
    let blocks = message_blocks(content, "assistant message content", true)?;
    let mut deltas = Vec::new();

    for block in &blocks {
      if block.field("type").and_then(EventFields::as_str_field) == Some("tool_use")
        && block.field("name").and_then(EventFields::as_str_field) == Some("ExitPlanMode")
      {
        let plan = block
          .field("input")
          .and_then(|input| input.field("plan"))
          .map(EventFields::to_owned_value)
          .unwrap_or(Value::Null);
        if is_truthy(&plan) {
          deltas.extend(self.flush_to_message_delta());
          let mut plan_message = json!({
            "role": "plan",
            "content": plan,
            "event_index": index,
            "id": event_id,
            "timestamp": event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null),
          });
          add_message_metadata(&mut plan_message, event, index, event_id)?;
          deltas.push(json!({"type": "message", "message": plan_message}));
        } else if !self.assistant_buf.is_empty() {
          let plan_message = json!({
            "role": "plan",
            "content": self.assistant_buf,
            "event_index": index,
            "id": event_id,
            "timestamp": self.last_assistant_timestamp.clone().unwrap_or(Value::Null),
          });
          self.assistant_buf.clear();
          self.last_assistant_timestamp = None;
          self.last_event_id = None;
          self.reset_stream_positions();
          deltas.push(json!({"type": "message", "message": plan_message}));
        }
      }
    }

    for block in &blocks {
      if block.field("type").and_then(EventFields::as_str_field) == Some("tool_use")
        && block.field("name").and_then(EventFields::as_str_field) != Some("ExitPlanMode")
      {
        self.tool_rows.push(tool_call_row(*block));
      }
    }
    let thinking = extract_message_field(&blocks, "thinking")?;
    if !thinking.is_empty() {
      self.thinking_buf = thinking;
    }
    let text = extract_message_field(&blocks, "text")?;
    if !text.is_empty() && !self.assistant_buf.is_empty() {
      deltas.extend(self.flush_to_message_delta());
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    self.assistant_buf.push_str(&text);
    if !self.assistant_buf.is_empty() || !self.tool_rows.is_draft_empty() || !self.thinking_buf.is_empty() {
      self.last_event_id = Some(event_id.to_string());
    }
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  fn feed_tool_use<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    self.tool_rows.push(ToolRow::new(
      event.field(event_types::TOOL_USE_ID).and_then(EventFields::as_str_field).map(str::to_string),
      event.field("name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      event.field("input").map(EventFields::to_owned_value).unwrap_or_else(|| json!({})),
    ));
    self.last_event_index = index;
    self.last_event_id = Some(event_id.to_string());
    if self.last_assistant_timestamp.is_none() {
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    Ok(self.stream_delta())
  }

  fn feed_tool_result<E: EventFields>(&mut self, event: &E, index: usize) -> StoreResult<Vec<Value>> {
    let tool_use_id = event.field(event_types::TOOL_USE_ID).and_then(EventFields::as_str_field);
    if !self.result_has_target(tool_use_id) {
      return Ok(Vec::new());
    }
    let output = event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!(""));
    let is_error = event.field("is_error").is_some_and(is_truthy);
    let mut deltas = self.attach_result(tool_use_id, output, is_error);
    self.last_event_index = index;
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  fn feed_thinking<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    self.last_event_index = index;
    let content = event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!(""));
    self.thinking_buf.push_str(&event_str(&content));
    if self.last_assistant_timestamp.is_none() {
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    if !self.assistant_buf.is_empty() || !self.tool_rows.is_draft_empty() || !self.thinking_buf.is_empty() {
      self.last_event_id = Some(event_id.to_string());
    }
    Ok(self.stream_delta())
  }

  fn feed_rendered<E: EventFields>(
    &mut self,
    event: &E,
    event_type: Option<&str>,
    index: usize,
    event_id: &str,
  ) -> StoreResult<Vec<Value>> {
    let Some(event_type) = event_type else { return Ok(Vec::new()) };
    if event_type == event_types::SYSTEM {
      if let Some(subtype) = event.field("subtype").and_then(EventFields::as_str_field)
        && matches!(subtype, event_types::TASK_STARTED | event_types::TASK_PROGRESS | event_types::TASK_NOTIFICATION)
      {
        return self.feed_subagent_event(event, subtype);
      }
      let Some(mut message) = render_system(event) else { return Ok(Vec::new()) };
      let mut deltas = self.flush_to_message_delta();
      add_message_metadata(&mut message, event, index, event_id)?;
      deltas.push(json!({"type": "message", "message": message}));
      return Ok(deltas);
    }
    if !builtin_event_types().contains(&event_type) && !self.event_renderers.contains_key(event_type) {
      return Ok(Vec::new());
    }
    // A run that ends leaves no subagent running: its cards show stopped, committed drafts included.
    let ends_run = matches!(event_type, event_types::MASTER_DONE | event_types::RUN_END | event_types::RUN_ERROR);
    let mut deltas = if ends_run { self.tool_rows.end_run() } else { Vec::new() };
    if ends_run {
      // The stop lands in the open draft's rows right before the commit closes it, and the live commit leaves out
      // the rows a page holds, so the suffix frame is what tells that page its rows stopped.
      deltas.extend(self.stream_delta());
    }
    deltas.extend(self.flush_to_message_delta());
    if ends_run {
      self.tool_rows.forget_committed();
    }
    let rendered = if builtin_event_types().contains(&event_type) {
      render_builtin(event_type, event)?
    } else {
      self.event_renderers[event_type].render(&event.to_owned_value())?
    };
    let Some(mut message) = rendered else { return Ok(deltas) };
    add_message_metadata(&mut message, event, index, event_id)?;
    deltas.push(json!({"type": "message", "message": message}));
    Ok(deltas)
  }

  fn flush_to_message_delta(&mut self) -> Vec<Value> {
    self.reset_stream_positions();
    let Some(message) = self.pending_draft_message() else { return Vec::new() };
    self.tool_rows.commit_draft(&message);
    self.assistant_buf.clear();
    self.thinking_buf.clear();
    self.last_assistant_timestamp = None;
    self.last_event_id = None;
    vec![json!({"type": "message", "message": self.committed(message)})]
  }

  /// The committed message as the live feed sends it: a page that received the stream frames holds the draft
  /// text, thinking and tool rows, so the frame carries only what the page needs to commit them.
  fn committed(&self, mut message: Value) -> Value {
    if !self.omit_streamed_content || !self.emit_stream_deltas {
      return message;
    }
    let Some(object) = message.as_object_mut() else { return message };
    for streamed in ["content", "thinking", "tools"] {
      object.remove(streamed);
    }
    object.insert(event_types::MESSAGE_CONTENT_STREAMED.to_string(), json!(true));
    message
  }

  fn reset_stream_positions(&mut self) {
    self.streamed_text_bytes = 0;
    self.streamed_text_utf16 = 0;
    self.streamed_thinking.clear();
    self.streamed_thinking_utf16 = 0;
    self.streamed_tools = None;
  }

  /// Whether a tool result has a row to attach to: the row that its tool_use id names, when that row has a
  /// subagent, or else the newest plain row of the open draft, as results have always attached.
  fn result_has_target(&self, tool_use_id: Option<&str>) -> bool {
    tool_use_id.is_some_and(|id| self.tool_rows.holds_subagent(id)) || self.tool_rows.has_plain_row()
  }

  /// Attach a result that `result_has_target` accepted. Returns the update frames of a committed draft it changed.
  fn attach_result(&mut self, tool_use_id: Option<&str>, output: Value, is_error: bool) -> Vec<Value> {
    match tool_use_id {
      Some(id) if self.tool_rows.holds_subagent(id) => {
        self.tool_rows.update_row(id, |row| row.attach_output(output, is_error))
      }
      _ => {
        self.tool_rows.attach_plain(output, is_error);
        Vec::new()
      }
    }
  }

  /// A subagent's own assistant event. Its tool calls become rows under the subagent. Its text and thinking stay
  /// out of the draft, which belongs to the turn.
  fn feed_subagent_assistant<E: EventFields>(&mut self, event: &E, parent: &str) -> StoreResult<Vec<Value>> {
    let content = event.field("message").and_then(|message| message.field("content"));
    let rows: Vec<ToolRow> = message_blocks(content, "assistant message content", true)?
      .into_iter()
      .filter(|block| block.field("type").and_then(EventFields::as_str_field) == Some("tool_use"))
      .map(tool_call_row)
      .collect();
    let mut deltas = self.tool_rows.update_subagent(parent, |subagent| subagent.add_tools(rows));
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  /// A subagent's own user event. It carries the subagent's prompt, when `task_started` gave none, and the
  /// results of the subagent's tools, which attach to their rows by id.
  fn feed_subagent_user<E: EventFields>(&mut self, event: &E, parent: &str) -> StoreResult<Vec<Value>> {
    let content = event.field("message").and_then(|message| message.field("content"));
    let blocks = message_blocks(content, "user message content", false)?;
    let prompt = match content.and_then(EventFields::as_str_field) {
      Some(text) => text.to_string(),
      None => extract_message_field(&blocks, "text")?,
    };
    let mut deltas = Vec::new();
    if !prompt.is_empty() {
      deltas.extend(self.tool_rows.update_row(parent, |row| {
        if let Some(subagent) = row.subagent_mut() {
          subagent.set_prompt_if_missing(&prompt);
        }
      }));
    }
    for block in blocks {
      if block.field("type").and_then(EventFields::as_str_field) != Some("tool_result") {
        continue;
      }
      let tool_use_id = required_str(block, event_types::TOOL_USE_ID)?;
      let output = extract_tool_result_text(block)?;
      let is_error = block.field("is_error").is_some_and(is_truthy);
      deltas.extend(self.tool_rows.update_row(tool_use_id, |row| row.attach_output(output, is_error)));
    }
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  /// A `task_started`, `task_progress` or `task_notification`: the row that the subagent hangs under takes the
  /// subagent's fields. The event names that row by its tool_use id.
  fn feed_subagent_event<E: EventFields>(&mut self, event: &E, subtype: &str) -> StoreResult<Vec<Value>> {
    let tool_use_id = required_str(event, event_types::TOOL_USE_ID)?;
    let usage = subagent_usage(event);
    let mut deltas = match subtype {
      event_types::TASK_STARTED => {
        let kind = string_default(event, "subagent_type", "");
        let description = string_default(event, "description", "");
        let prompt = string_default(event, "prompt", "");
        self.tool_rows.update_subagent(tool_use_id, |subagent| subagent.start(&kind, &description, &prompt))
      }
      event_types::TASK_PROGRESS => {
        let description = event.field("description").and_then(EventFields::as_str_field);
        self.tool_rows.update_subagent(tool_use_id, |subagent| subagent.progress(description, usage))
      }
      event_types::TASK_NOTIFICATION => {
        let status = SubagentStatus::from_notification(&string_default(event, "status", ""))?;
        self.tool_rows.update_subagent(tool_use_id, |subagent| subagent.finish(status, usage))
      }
      other => return Err(StoreError::new(format!("{other} is not a subagent event"))),
    };
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  /// The current draft as a stream frame: only the text, thinking and tool rows the page does not hold yet. The
  /// tool rows go out as a suffix: `tools_from` is the index of the frame's first row in the draft's rows, from 0.
  /// `length` and `thinking_length` are the draft's lengths after this frame, in UTF-16 code units, so the page
  /// can tell a replayed append from new text.
  fn stream_delta(&mut self) -> Vec<Value> {
    if !self.emit_stream_deltas {
      return Vec::new();
    }
    if self.assistant_buf.is_empty() && self.tool_rows.is_draft_empty() && self.thinking_buf.is_empty() {
      self.reset_stream_positions();
      return Vec::new();
    }
    let append = self.assistant_buf[self.streamed_text_bytes..].to_string();
    // An assistant event may replace the thinking instead of extending it; a replacement resends it whole.
    let thinking_extends = self.thinking_buf.starts_with(self.streamed_thinking.as_str());
    let thinking_append = if thinking_extends {
      self.thinking_buf[self.streamed_thinking.len()..].to_string()
    } else {
      self.thinking_buf.clone()
    };
    let tools = self.tool_rows.draft_json();
    // The frame carries the rows from the first one that differs from the rows the earlier frames streamed, so a
    // draft's row bytes grow with its row count instead of with its row count squared.
    let tools_from = first_changed_row(self.streamed_tools.as_ref(), tools.as_ref());
    if append.is_empty() && thinking_append.is_empty() && tools_from.is_none() {
      return Vec::new();
    }
    let mut frame = json!({"type": "stream", "event_index": self.current_index});
    if !append.is_empty() {
      self.streamed_text_bytes = self.assistant_buf.len();
      self.streamed_text_utf16 += append.encode_utf16().count();
      frame[APPEND] = json!(append);
      frame[LENGTH] = json!(self.streamed_text_utf16);
    }
    if !thinking_append.is_empty() {
      self.streamed_thinking_utf16 = if thinking_extends {
        self.streamed_thinking_utf16 + thinking_append.encode_utf16().count()
      } else {
        self.thinking_buf.encode_utf16().count()
      };
      self.streamed_thinking.clone_from(&self.thinking_buf);
      frame[THINKING_APPEND] = json!(thinking_append);
      frame[THINKING_LENGTH] = json!(self.streamed_thinking_utf16);
    }
    if let Some(from) = tools_from {
      self.streamed_tools = tools.clone();
      if let Some(tools) = tools {
        let rows = tools.as_array().expect("draft_json renders an array");
        frame["tools_from"] = json!(from);
        frame["tools"] = json!(rows[from..]);
      }
    }
    vec![frame]
  }
}

/// The index of the first row of `current` that the rows streamed so far do not hold, when the rows changed. Rows
/// never streamed start the suffix at 0; a draft whose rows equal the streamed ones changed none; rows only ever
/// grow or change in place between commits, so a suffix that would shrink has no case.
fn first_changed_row(streamed: Option<&Value>, current: Option<&Value>) -> Option<usize> {
  let Some(current_rows) = current.and_then(Value::as_array) else { return streamed.is_some().then_some(0) };
  let Some(streamed_rows) = streamed.and_then(Value::as_array) else { return Some(0) };
  let shared = streamed_rows.len().min(current_rows.len());
  match (0..shared).find(|&index| streamed_rows[index] != current_rows[index]) {
    Some(index) => Some(index),
    None if current_rows.len() > streamed_rows.len() => Some(streamed_rows.len()),
    None => None,
  }
}

/// The tool row of one tool_use block of an assistant message.
fn tool_call_row<E: EventFields>(block: &E) -> ToolRow {
  ToolRow::new(
    block.field("id").and_then(EventFields::as_str_field).map(str::to_string),
    block.field("name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    block.field("input").map(EventFields::to_owned_value).unwrap_or_else(|| json!({})),
  )
}

/// The tool_use id that a subagent event names as its parent. An event without one belongs to the turn.
fn parent_tool_use_id<E: EventFields>(event: &E) -> StoreResult<Option<&str>> {
  match event.field(event_types::PARENT_TOOL_USE_ID).filter(|value| is_truthy(*value)) {
    None => Ok(None),
    Some(value) => value.as_str_field().map(Some).ok_or_else(|| StoreError::new("parent_tool_use_id must be a string")),
  }
}

fn required_str<'a, E: EventFields>(event: &'a E, key: &str) -> StoreResult<&'a str> {
  event.field(key).and_then(EventFields::as_str_field).ok_or_else(|| StoreError::new(format!("{key} must be a string")))
}

fn subagent_usage<E: EventFields>(event: &E) -> Usage {
  let count = |key: &str| event.field("usage").and_then(|usage| usage.field(key)).and_then(EventFields::as_u64_field);
  Usage { tool_uses: count("tool_uses"), duration_ms: count("duration_ms"), total_tokens: count("total_tokens") }
}

fn render_system<E: EventFields>(event: &E) -> Option<Value> {
  if event.field("subtype").and_then(EventFields::as_str_field) != Some(event_types::COMMAND_PROGRESS) {
    return None;
  }
  Some(
    json!({"role": "system", "content": event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!(""))}),
  )
}

fn render_builtin<E: EventFields>(event_type: &str, event: &E) -> StoreResult<Option<Value>> {
  let rendered = match event_type {
    event_types::MASTER_DONE => json!({
      "role": "separator",
      event_types::THINKING_SECONDS: event.field(event_types::THINKING_SECONDS).map(EventFields::to_owned_value).unwrap_or(Value::Null),
    }),
    event_types::ASSISTANT_ERROR => json!({
      "role": "system",
      "content": format!("Error: {}", event.field("content").map_or_else(String::new, event_str)),
    }),
    event_types::ERROR => {
      let content = event
        .field("content")
        .filter(|value| is_truthy(*value))
        .or_else(|| event.field("message").filter(|value| is_truthy(*value)))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!("Unknown error"));
      json!({"role": "system", "content": format!("Error: {}", event_str(&content))})
    }
    event_types::TASK_DELEGATED => json!({
      "role": event_types::TASK_DELEGATED,
      "content": "Task delegated",
      "thread_id": event.field("thread_id").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "child_session_id": event.field("child_session_id").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      event_types::DELEGATE_INVOCATION: event.field(event_types::DELEGATE_INVOCATION).map(EventFields::to_owned_value).unwrap_or(Value::Null),
      "backend": preferred_value(event, "backend", "resolved_backend"),
      "model": preferred_value(event, "model", "resolved_model"),
      "route": event.field("route").map(EventFields::to_owned_value).unwrap_or(Value::Null),
      // A falsy timestamp falls back to created_at, so both fields absent give null.
      "timestamp": event.field("timestamp").filter(|value| is_truthy(*value)).or_else(|| event.field("created_at")).map(EventFields::to_owned_value).unwrap_or(Value::Null),
    }),
    event_types::WORKER_SUMMARY => {
      let mut message = json!({
        "role": event_types::WORKER_SUMMARY,
        "content": event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "thread_id": event.field("thread_id").map(EventFields::to_owned_value).unwrap_or(Value::Null),
      });
      if let Some(origin) = event.field("origin_session_id").filter(|value| !value.is_null()) {
        message["origin_session_id"] = origin.to_owned_value();
      }
      message
    }
    event_types::HANDLER_RESULT => {
      let icon = if event.field("status").and_then(EventFields::as_str_field) == Some(event_types::HANDLER_STATUS_OK) {
        "✓"
      } else {
        "✗"
      };
      json!({
        "role": "system",
        "content": format!("{icon} {}: {}", string_default(event, "task", ""), string_default(event, "message", "")),
      })
    }
    event_types::CONTEXT_COMPACTED => render_context_compacted(event)?,
    event_types::CONTEXT_COMPACT_FAILED => {
      let trigger = event.field("trigger").filter(|value| is_truthy(*value)).and_then(EventFields::as_str_field);
      let note = model_note(event);
      let mut head = trigger.map_or_else(|| "Compaction".to_string(), |trigger| format!("Compaction ({trigger})"));
      if !note.is_empty() {
        head.push(' ');
        head.push_str(&note);
      }
      head.push_str(" failed");
      let error = event.field("error").filter(|value| is_truthy(*value));
      let content = error.map_or(head.clone(), |error| format!("{head} — {}", event_str(error)));
      json!({"role": "system", "kind": event_types::CONTEXT_COMPACT_FAILED, "content": content})
    }
    event_types::RESUME_CONTEXT_DROPPED => {
      let reason = event.field("reason").and_then(EventFields::as_str_field);
      let head = match reason {
        Some(event_types::RESUME_REASON_ANCHOR_MISSING) => "Context not resumed: no previous session anchor was found",
        Some(event_types::RESUME_REASON_TRANSCRIPT_MISSING) => {
          "Context not resumed: the previous session transcript is missing"
        }
        _ => "Context not resumed",
      };
      json!({"role": "system", "content": format!("{head} — started a new conversation")})
    }
    event_types::CLONE_START => json!({
      "role": event_types::CLONE_START,
      "content": event.field("parent_session_name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("Unknown session")),
      "parent_session_id": event.field("parent_session_id").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    }),
    event_types::SCHEDULED_TRIGGER => json!({
      "role": event_types::SCHEDULED_TRIGGER,
      "content": event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    }),
    event_types::AGENT_MESSAGE => {
      let mut obj = json!({
        "role": event_types::AGENT_MESSAGE,
        "content": event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        (event_types::FROM_SESSION): event.field(event_types::FROM_SESSION).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        (event_types::FROM_SESSION_NAME): event.field(event_types::FROM_SESSION_NAME).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      });
      for key in event_types::THREAD_PLATFORM_KEYS {
        if let Some(block) = event.field(key) {
          obj[*key] = block.to_owned_value();
        }
      }
      obj
    }
    event_types::BACKEND_SWITCHED => json!({
      "role": "system",
      "content": format!("Backend switched: {} → {}", string_default(event, "from", ""), string_default(event, "to", "")),
    }),
    event_types::QUOTA_FAILOVER => {
      let from_backend = string_default(event, "from", "");
      let to_backend = string_default(event, "to", "");
      let recovers_at = event.field("recovers_at").filter(|value| is_truthy(*value));
      let recovery =
        recovers_at.map_or_else(String::new, |value| format!("; {from_backend} recovers at {}", event_str(value)));
      let reason = event.field(event_types::QUOTA_FAILOVER_REASON).and_then(EventFields::as_str_field);
      let head = match reason {
        Some(event_types::QUOTA_REASON_NEAR_EXHAUSTION) => {
          format!("Near quota exhaustion on {from_backend}: switched to {to_backend}")
        }
        Some(event_types::QUOTA_REASON_SUBSCRIPTION_RESET) => {
          format!("Subscription quota reset: switched {from_backend} → {to_backend}")
        }
        Some(event_types::QUOTA_REASON_ACCESS_REFUSED) => {
          format!("Access refused on {from_backend}: switched to {to_backend}")
        }
        Some(event_types::QUOTA_REASON_TIMEOUT) => {
          format!("Timeout on {from_backend}: switched to {to_backend}")
        }
        Some(event_types::QUOTA_REASON_ITERATION_FAILURE) => {
          format!("Iteration failed on {from_backend}: switched to {to_backend}")
        }
        Some(event_types::QUOTA_REASON_QUOTA_ERROR) | None => {
          format!("Quota exhausted on {from_backend}: switched to {to_backend}")
        }
        Some(reason) => format!("Quota change on {from_backend}: switched to {to_backend} ({reason})"),
      };
      let handoff = event
        .field(event_types::QUOTA_FAILOVER_SUMMARY)
        .and_then(EventFields::as_str_field)
        .filter(|summary| !summary.is_empty() && *summary != event_types::QUOTA_SUMMARY_OK)
        .map_or_else(String::new, |summary| format!("; handoff summary {summary}"));
      json!({"role": "system", "content": format!("{head}{recovery}{handoff}")})
    }
    event_types::BACKEND_OVERLAY_INACTIVE | event_types::BACKEND_OVERLAY_UNDECLARED => {
      let backend = string_default(event, "backend", "");
      if event.field("reason").and_then(EventFields::as_str_field) == Some(event_types::OVERLAY_REASON_UNREADABLE) {
        json!({
          "role": "system",
          "content": format!(
            "Backend {backend}: prompt_overlay '{}' could not be loaded ({}) — running without a fence",
            string_default(event, "overlay", ""),
            string_default(event, "error", ""),
          ),
        })
      } else {
        json!({
          "role": "system",
          "content": format!("Backend {backend} declares no prompt_overlay — running without a fence"),
        })
      }
    }
    event_types::MODEL_FALLBACK_NOTICE => {
      let configured =
        event.field(event_types::CONFIGURED_MODEL).filter(|value| is_truthy(*value)).map(event_str).unwrap_or_default();
      let served = join_strings(event.field(event_types::SERVED_MODELS), event_types::SERVED_MODELS)?;
      json!({"role": "system", "content": format!("Served by {served} (configured {configured})")})
    }
    event_types::CHILD_REPORT => json!({
      "role": event_types::CHILD_REPORT,
      "content": event.field("summary").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "child_session_id": event.field("child_session_id").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "outcome": event.field("outcome").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "result_refs": event.field("result_refs").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!([])),
    }),
    event_types::RUN_HEADER => render_run_header(event)?,
    event_types::RUN_END => json!({
      "role": "separator",
      "kind": event_types::RUN_END,
      event_types::THINKING_SECONDS: event.field("duration_seconds").map(EventFields::to_owned_value).unwrap_or(Value::Null),
    }),
    event_types::RUN_ERROR => {
      let content = event
        .field("content")
        .filter(|value| is_truthy(*value))
        .or_else(|| event.field("message").filter(|value| is_truthy(*value)))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!("Unknown error"));
      let text = event_str(&content);
      json!({
        "role": "system",
        "kind": event_types::RUN_ERROR,
        "content": format!("Error: {text}"),
        "line_count": text.matches('\n').count() + 1,
      })
    }
    event_types::RUN_DELIVERY => {
      let task_state = event
        .field("task_state")
        .filter(|value| is_truthy(*value))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!(""));
      json!({
        "role": event_types::RUN_DELIVERY,
        "content": event.field("summary").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "task_state": task_state,
        "completed": event.field("task_state").and_then(EventFields::as_str_field) == Some("completed"),
        "result_refs": event.field("result_refs").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!([])),
        "raw_log_ref": event.field("raw_log_ref").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "events_ref": event.field("events_ref").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "result_ref": event.field("result_ref").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "repo_path": event.field("repo_path").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        (BASE_BRANCH_FIELD): event.field(BASE_BRANCH_FIELD).filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "branch_name": event.field("branch_name").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
        "run_id": event.field("run_id").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      })
    }
    event_types::TASK_CLOSED => {
      let outcome = event.field("outcome").map(EventFields::to_owned_value).unwrap_or_else(|| json!("completed"));
      let summary = event
        .field("summary")
        .filter(|value| is_truthy(*value))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!(""));
      let content = format!("Task {}: {}", event_str(&outcome), event_str(&summary)).trim_end().to_string();
      json!({"role": "system", "content": content})
    }
    event_types::TASK_REOPENED => {
      let reason = event
        .field("reason")
        .filter(|value| is_truthy(*value))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!(""));
      json!({"role": "system", "content": format!("Task reopened: {}", event_str(&reason)).trim_end()})
    }
    event_types::TASK_INPUT_ACKNOWLEDGED => render_task_input_acknowledged(event)?,
    _ => return Ok(None),
  };
  Ok(Some(rendered))
}

fn render_context_compacted<E: EventFields>(event: &E) -> StoreResult<Value> {
  let trigger = event.field("trigger").map(EventFields::to_owned_value).unwrap_or_else(|| json!("auto"));
  let note = model_note(event);
  let mut qualifiers = Vec::new();
  if is_truthy(&trigger) {
    qualifiers
      .push(trigger.as_str().ok_or_else(|| StoreError::new("compaction trigger must be a string"))?.to_string());
  }
  if !note.is_empty() {
    qualifiers.push(note);
  }
  let mut content = "Context compacted".to_string();
  if !qualifiers.is_empty() {
    content.push_str(" (");
    content.push_str(&qualifiers.join(", "));
    content.push(')');
  }
  let meta = event.field(event_types::COMPACT_METADATA).filter(|value| is_truthy(*value));
  let (pre_tokens, post_tokens) = if let Some(meta) = meta {
    (meta.field(event_types::COMPACT_PRE_TOKENS), meta.field(event_types::COMPACT_POST_TOKENS))
  } else {
    (event.field(event_types::COMPACT_PRE_TOKENS), None)
  };
  if let Some(pre_tokens) = pre_tokens.filter(|value| is_truthy(*value)) {
    if let Some(post_tokens) = post_tokens.filter(|value| is_truthy(*value)) {
      content.push_str(&format!(" — {} → {} tokens", format_k_tokens(pre_tokens)?, format_k_tokens(post_tokens)?,));
    } else {
      content.push_str(&format!(" — was {} tokens", format_k_tokens(pre_tokens)?));
    }
  }
  Ok(json!({"role": "system", "content": content, "kind": event_types::CONTEXT_COMPACTED}))
}

fn render_run_header<E: EventFields>(event: &E) -> StoreResult<Value> {
  let kind = event.field("kind").filter(|value| is_truthy(*value)).map(event_str).unwrap_or_else(|| "run".to_string());
  let backend = event
    .field(event_types::BACKEND_LABEL)
    .filter(|value| is_truthy(*value))
    .or_else(|| event.field("backend").filter(|value| is_truthy(*value)))
    .map(event_str)
    .unwrap_or_default();
  let state = event.field("state").filter(|value| is_truthy(*value)).map(event_str).unwrap_or_default();
  let state_word = if state == "withheld" && event.field("withheld").is_some_and(is_truthy) {
    format!("withheld · {}", event_str(event.field("withheld").expect("checked field")))
  } else if is_truthy(required_field(event, "launch_failed")?) {
    "launch failed".to_string()
  } else {
    state.clone()
  };
  let mut parts = vec![format!("Run {kind}")];
  if !backend.is_empty() {
    parts.push(backend);
  }
  if !state_word.is_empty() {
    parts.push(state_word);
  }
  let tool_calls = event.field("tool_calls").filter(|value| !value.is_null());
  if let Some(tool_calls) = tool_calls {
    let count = event_int(tool_calls).ok_or_else(|| StoreError::new("run header tool_calls must be an integer"))?;
    parts.push(if count == "1" { "1 tool call".to_string() } else { format!("{count} tool calls") });
  }
  Ok(json!({
    "role": "system",
    "kind": event_types::RUN_HEADER,
    "content": parts.join(" · "),
    "run_id": event.field("run_id").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    "state": state,
    "error": event.field("error").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    "tool_calls": event.field("tool_calls").map(EventFields::to_owned_value).unwrap_or(Value::Null),
    "launched": required_field(event, "launched")?.to_owned_value(),
    "launch_failed": required_field(event, "launch_failed")?.to_owned_value(),
    "task_spec_ref": required_field(event, "task_spec_ref")?.to_owned_value(),
    (event_types::LAUNCH_PROMPT_REF): required_field(event, event_types::LAUNCH_PROMPT_REF)?.to_owned_value(),
  }))
}

fn render_task_input_acknowledged<E: EventFields>(event: &E) -> StoreResult<Value> {
  let input_ids = join_strings(event.field("input_ids"), "input_ids")?;
  let mut content = format!("Task input(s) acknowledged as handled: {input_ids}");
  if event.field("note").is_some_and(is_truthy) {
    content.push_str(" — ");
    content.push_str(&event_str(event.field("note").expect("checked field")));
  }
  Ok(json!({"role": "system", "content": content.trim_end()}))
}

/// The content blocks of a message field.
///
/// A string's characters or an object's keys are iterated when `content` is not a list.
/// None of those values is a block object, so those shapes contribute no blocks. A scalar that
/// cannot be iterated still fails, while an assistant field treats a falsy value as empty,
/// because a falsy value is replaced by an empty list.
fn message_blocks<'a, E: EventFields>(
  content: Option<&'a E>,
  field: &str,
  falsy_is_empty: bool,
) -> StoreResult<Vec<&'a E>> {
  let Some(content) = content else {
    return Ok(Vec::new());
  };
  if falsy_is_empty && !is_truthy(content) {
    return Ok(Vec::new());
  }
  if let Some(items) = content.array_items() {
    return Ok(items.iter().collect());
  }
  if content.is_str() || content.is_object() {
    return Ok(Vec::new());
  }
  Err(StoreError::new(format!("{field} must be an array")))
}

fn extract_tool_result_text<E: EventFields>(block: &E) -> StoreResult<Value> {
  let raw = block.field("content");
  if let Some(parts) = raw.and_then(E::array_items) {
    let mut text = Vec::new();
    for part in parts {
      if part.field("type").and_then(EventFields::as_str_field) == Some("text") {
        let value = match part.field("text") {
          Some(value) => value.as_str_field().ok_or_else(|| StoreError::new("tool-result text must be a string"))?,
          None => "",
        };
        text.push(value.to_string());
      }
    }
    return Ok(json!(text.join("\n")));
  }
  match raw {
    None => Ok(json!("")),
    Some(raw) if raw.is_str() => Ok(raw.to_owned_value()),
    Some(raw) => Ok(json!(event_str(raw))),
  }
}

fn extract_message_field<E: EventFields>(blocks: &[&E], field: &str) -> StoreResult<String> {
  let mut text = String::new();
  for block in blocks {
    if block.field("type").and_then(EventFields::as_str_field) == Some(field) {
      let value = match block.field(field) {
        Some(value) => {
          value.as_str_field().ok_or_else(|| StoreError::new(format!("message {field} block must contain a string")))?
        }
        None => "",
      };
      text.push_str(value);
    }
  }
  Ok(text)
}

/// Count the tool rows opened by these run events.
pub fn run_tool_call_count<E: EventFields>(events: &[&E]) -> usize {
  events
    .iter()
    .map(|event| match event.field("type").and_then(EventFields::as_str_field) {
      Some(event_types::ASSISTANT) => run_assistant_tool_uses(*event)
        .iter()
        .filter(|block| block.field("name").and_then(EventFields::as_str_field) != Some("ExitPlanMode"))
        .count(),
      Some(event_types::TOOL_USE) => 1,
      _ => 0,
    })
    .sum()
}

/// Return the newest tool row opened by these run events, with its preview and source timestamp.
pub fn run_last_tool<E: EventFields>(events: &[&E]) -> Option<Value> {
  for event in events.iter().rev() {
    let call = match event.field("type").and_then(EventFields::as_str_field) {
      Some(event_types::ASSISTANT) => run_assistant_tool_uses(*event)
        .into_iter()
        .rev()
        .find(|block| block.field("name").and_then(EventFields::as_str_field) != Some("ExitPlanMode"))
        .map(|block| {
          json!({
            "name": block.field("name").and_then(EventFields::as_str_field).unwrap_or(""),
            "input": block.field("input").map(EventFields::to_owned_value).unwrap_or(Value::Null),
          })
        }),
      Some(event_types::TOOL_USE) => Some(json!({
        "name": event.field("name").and_then(EventFields::as_str_field).unwrap_or(""),
        "input": event.field("input").map(EventFields::to_owned_value).unwrap_or(Value::Null),
      })),
      _ => None,
    };
    if let Some(call) = call {
      let tool = tool_preview(call);
      return Some(json!({
        "name": tool.get("name").cloned().unwrap_or_else(|| json!("")),
        "input": tool.get("input").cloned().unwrap_or(Value::Null),
        "timestamp": event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null),
      }));
    }
  }
  None
}

/// Return the newest non-empty assistant text and its source timestamp.
pub fn run_last_assistant_text<E: EventFields>(events: &[&E]) -> StoreResult<Option<Value>> {
  for event in events.iter().rev() {
    if event.field("type").and_then(EventFields::as_str_field) != Some(event_types::ASSISTANT) {
      continue;
    }
    let Some(blocks) =
      event.field("message").and_then(|message| message.field("content")).and_then(EventFields::array_items)
    else {
      continue;
    };
    let block_refs: Vec<&E> = blocks.iter().collect();
    let text = extract_message_field(&block_refs, "text")?.trim().to_string();
    if !text.is_empty() {
      return Ok(Some(json!({
        "text": text,
        "timestamp": event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null),
      })));
    }
  }
  Ok(None)
}

/// Return the newest non-empty result or assistant text and its source timestamp.
pub fn run_last_text<E: EventFields>(events: &[&E]) -> StoreResult<Option<Value>> {
  for event in events.iter().rev() {
    match event.field("type").and_then(EventFields::as_str_field) {
      Some(event_types::RESULT) => {
        if let Some(text) =
          event.field("result").and_then(EventFields::as_str_field).map(str::trim).filter(|text| !text.is_empty())
        {
          return Ok(Some(json!({
            "text": text,
            "timestamp": event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null),
          })));
        }
      }
      Some(event_types::ASSISTANT) => {
        if let Some(text) = run_last_assistant_text::<E>(&[*event])? {
          return Ok(Some(text));
        }
      }
      _ => {}
    }
  }
  Ok(None)
}

/// Return the selected error text, its first line, its event position and timestamp.
pub fn run_cause<E: EventFields>(events: &[&E]) -> Option<(String, String, usize, String)> {
  for key in ["message", "content"] {
    for (index, event) in events.iter().enumerate().rev() {
      if event.field("type").and_then(EventFields::as_str_field) != Some(event_types::ERROR) {
        continue;
      }
      if let Some(text) =
        event.field(key).and_then(EventFields::as_str_field).map(str::trim).filter(|text| !text.is_empty())
      {
        let first_line = text.split('\n').next().unwrap_or_default().to_string();
        let timestamp = event
          .field("timestamp")
          .filter(|value| is_truthy(*value))
          .map(|value| event_str(&value.to_owned_value()))
          .unwrap_or_default();
        return Some((text.to_string(), first_line, index, timestamp));
      }
    }
  }
  None
}

fn run_assistant_tool_uses<E: EventFields>(event: &E) -> Vec<&E> {
  event
    .field("message")
    .and_then(|message| message.field("content"))
    .and_then(EventFields::array_items)
    .into_iter()
    .flatten()
    .filter(|block| block.field("type").and_then(EventFields::as_str_field) == Some("tool_use"))
    .collect()
}

fn add_message_metadata<E: EventFields>(
  message: &mut Value,
  event: &E,
  index: usize,
  event_id: &str,
) -> StoreResult<()> {
  let message = message.as_object_mut().ok_or_else(|| StoreError::new("event renderer must return a JSON object"))?;
  message
    .entry("timestamp".to_string())
    .or_insert_with(|| event.field("timestamp").map(EventFields::to_owned_value).unwrap_or(Value::Null));
  message.insert("event_index".to_string(), json!(index));
  message.insert("id".to_string(), json!(event_id));
  Ok(())
}

fn format_k_tokens<E: EventFields>(value: &E) -> StoreResult<String> {
  let count = match value.as_bool_field() {
    Some(flag) => f64::from(flag),
    None => value.as_f64_field().ok_or_else(|| StoreError::new("compaction token count must be numeric"))?,
  };
  if count < 100_000.0 {
    Ok(format!("{:.1}k", count / 1000.0))
  } else {
    Ok(format!("{}k", (count / 1000.0).round_ties_even() as i64))
  }
}

fn model_note<E: EventFields>(event: &E) -> String {
  let Some(model) = event.field("model").and_then(EventFields::as_str_field).filter(|model| !model.is_empty()) else {
    return String::new();
  };
  let lower = model.to_lowercase();
  let parts: Vec<&str> = lower.split('-').collect();
  let family = if parts.len() > 1 && parts[0] == "claude" { parts[1] } else { parts[0] };
  let mut chars = family.chars();
  let family = chars.next().map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + chars.as_str());
  format!("by {family}")
}

fn preferred_value<E: EventFields>(event: &E, primary: &str, fallback: &str) -> Value {
  event
    .field(primary)
    .filter(|value| is_truthy(*value))
    .or_else(|| event.field(fallback).filter(|value| is_truthy(*value)))
    .map(EventFields::to_owned_value)
    .unwrap_or_else(|| json!(""))
}

fn string_default<E: EventFields>(event: &E, key: &str, default: &str) -> String {
  event.field(key).map_or_else(|| default.to_string(), event_str)
}

fn required_field<'a, E: EventFields>(event: &'a E, key: &str) -> StoreResult<&'a E> {
  event.field(key).ok_or_else(|| StoreError::new(format!("event renderer requires field {key}")))
}

/// Joins with `, `: an array joins its strings, while a string or object contributes its characters or keys.
fn join_strings<E: EventFields>(value: Option<&E>, field: &str) -> StoreResult<String> {
  let Some(value) = value.filter(|value| is_truthy(*value)) else {
    return Ok(String::new());
  };
  if let Some(text) = value.as_str_field() {
    return Ok(text.chars().map(String::from).collect::<Vec<_>>().join(", "));
  }
  if let Some(items) = value.array_items() {
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
      parts.push(
        item.as_str_field().ok_or_else(|| StoreError::new(format!("{field} entries must be strings")))?.to_string(),
      );
    }
    return Ok(parts.join(", "));
  }
  if value.is_object() {
    // The trait exposes no key iteration, so the object's keys come from an owned copy.
    let Value::Object(map) = value.to_owned_value() else { unreachable!("checked above") };
    return Ok(map.keys().cloned().collect::<Vec<_>>().join(", "));
  }
  Err(StoreError::new(format!("{field} must be an array, string or object")))
}

/// The integer a tool-call count reads as, for each JSON shape a count can carry.
fn event_int<E: EventFields>(value: &E) -> Option<String> {
  if let Some(flag) = value.as_bool_field() {
    return Some(if flag { "1" } else { "0" }.to_string());
  }
  if let Some(integer) = value.as_i64_field() {
    return Some(integer.to_string());
  }
  if let Some(unsigned) = value.as_u64_field() {
    return Some(unsigned.to_string());
  }
  if let Some(float) = value.as_f64_field().filter(|float| float.is_finite()) {
    let integer = float.trunc();
    return Some(if integer == 0.0 { "0".to_string() } else { format!("{integer:.0}") });
  }
  value.as_str_field().and_then(parse_int_string)
}

fn parse_int_string(value: &str) -> Option<String> {
  let value = value.trim();
  let (negative, digits) = if let Some(digits) = value.strip_prefix('-') {
    (true, digits)
  } else if let Some(digits) = value.strip_prefix('+') {
    (false, digits)
  } else {
    (false, value)
  };
  let mut parsed_digits = String::with_capacity(digits.len());
  let mut previous_was_digit = false;
  for character in digits.chars() {
    if character.is_ascii_digit() {
      parsed_digits.push(character);
      previous_was_digit = true;
    } else if character == '_' && previous_was_digit {
      previous_was_digit = false;
    } else {
      return None;
    }
  }
  if !previous_was_digit {
    return None;
  }
  let digits = parsed_digits.trim_start_matches('0');
  if digits.is_empty() {
    return Some("0".to_string());
  }
  Some(if negative { format!("-{digits}") } else { digits.to_string() })
}

/// The text of a field value. A string is read in place; other shapes take the text `text_repr::value_str` renders.
fn event_str<E: EventFields>(value: &E) -> String {
  match value.as_str_field() {
    Some(text) => text.to_string(),
    None => value_str(&value.to_owned_value()),
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::store::test_support::{assistant_text_event, assistant_text_tool_use_event};

  fn parity_renderers() -> HashMap<String, Arc<dyn EventRenderer>> {
    let slack: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      let content = event.get("content").cloned().unwrap_or_else(|| json!(""));
      Ok(Some(json!({"role": "system", "content": format!("Posted to Slack: {}", event_str(&content))})))
    });
    let discord: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      let content = event.get("content").cloned().unwrap_or_else(|| json!(""));
      Ok(Some(json!({"role": "system", "content": format!("Posted to Discord: {}", event_str(&content))})))
    });
    let web_reply: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      Ok(Some(json!({"role": "web_reply", "content": event.get("content").cloned().unwrap_or_else(|| json!(""))})))
    });
    let web_reply_notice: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      Ok(Some(
        json!({"role": "web_reply_notice", "content": event.get("content").cloned().unwrap_or_else(|| json!(""))}),
      ))
    });
    let web_status: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      Ok(Some(json!({
        "role": "web_status",
        "content": event.get("content").cloned().unwrap_or_else(|| json!("")),
        "web_status": event.get("web_status").cloned().unwrap_or(Value::Null),
      })))
    });
    let scheduled_run_skipped: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      let value = |key: &str| event_str(event.get(key).unwrap_or(&json!("")));
      Ok(Some(json!({
        "role": "system",
        "content": format!("Scheduled run of '{}' skipped at {}: {}", value("task"), value("skipped_at"), value("reason")),
      })))
    });
    let claude_account_login_required: Arc<dyn EventRenderer> = Arc::new(|_event: &Value| {
      Ok(Some(json!({
        "role": "system",
        "kind": "claude_account_login_required",
        "content": "One account in the Claude pool needs a new login; see the usage panel.",
      })))
    });
    HashMap::from([
      ("slack_reply".to_string(), slack),
      ("discord_reply".to_string(), discord),
      ("web_reply".to_string(), web_reply),
      ("web_reply_notice".to_string(), web_reply_notice),
      ("web_status".to_string(), web_status),
      ("scheduled_run_skipped".to_string(), scheduled_run_skipped),
      ("claude_account_login_required".to_string(), claude_account_login_required),
    ])
  }

  /// The fixture's stream frames are whole-draft snapshots, which is the wire form of the Python aggregator
  /// they were recorded from. The live feed carries only new text, so the appends are accumulated here and the
  /// draft at each frame must equal that frame's snapshot; every other frame must match exactly.
  fn assert_parity_frames(actual: Vec<Value>, expected: &[Value], case: &str) {
    let expected = without_repeated_snapshots(expected);
    let mut actual = actual.into_iter();
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tools = Value::Null;
    for (index, expected_frame) in expected.iter().enumerate() {
      let Some(frame) = actual.next() else {
        panic!("{case}: the feed ended before frame {index}; expected {expected_frame:?}");
      };
      if expected_frame["type"] != "stream" {
        assert_eq!(&frame, expected_frame, "{case}: frame {index} differs from the fixture");
        content.clear();
        thinking.clear();
        tools = Value::Null;
        continue;
      }
      assert_eq!(frame["type"], "stream", "{case}: frame {index} is not a stream frame: {frame:?}");
      if let Some(append) = frame["append"].as_str() {
        content = extend_streamed(&content, append, frame["length"].as_u64());
      }
      if let Some(append) = frame["thinking_append"].as_str() {
        thinking = extend_streamed(&thinking, append, frame["thinking_length"].as_u64());
      }
      if let Some(frame_tools) = frame.get("tools") {
        // A live frame carries the rows from `tools_from` on; the page keeps the rows before it.
        tools = match frame["tools_from"].as_u64() {
          Some(from) => {
            let mut held = tools.as_array().cloned().unwrap_or_default();
            held.truncate(from as usize);
            held.extend(frame_tools.as_array().cloned().unwrap_or_default());
            Value::Array(held)
          }
          None => frame_tools.clone(),
        };
      }
      let snapshot = &expected_frame["message"];
      assert_eq!(content, snapshot["content"].as_str().unwrap_or(""), "{case}: draft text at frame {index}");
      assert_eq!(
        thinking,
        snapshot.get("thinking").and_then(Value::as_str).unwrap_or(""),
        "{case}: draft thinking at frame {index}"
      );
      if let Some(snapshot_tools) = snapshot.get("tools") {
        assert_eq!(&tools, snapshot_tools, "{case}: draft tools at frame {index}");
      }
    }
    assert!(actual.next().is_none(), "{case}: the feed sent frames the fixture does not have");
  }

  /// Drop the snapshots that repeat their predecessor: the append feed sends nothing when a draft is unchanged.
  fn without_repeated_snapshots(frames: &[Value]) -> Vec<Value> {
    let mut kept = Vec::new();
    let mut previous: Option<Value> = None;
    for frame in frames {
      if frame["type"] != "stream" {
        previous = None;
        kept.push(frame.clone());
        continue;
      }
      let snapshot = &frame["message"];
      let state = json!([snapshot["content"], snapshot.get("thinking"), snapshot.get("tools"),]);
      if previous.as_ref() == Some(&state) {
        continue;
      }
      previous = Some(state);
      kept.push(frame.clone());
    }
    kept
  }

  /// The draft text after one append frame, the way the page accumulates it. The fixture's texts are ASCII, so
  /// a UTF-16 length is its byte length.
  fn extend_streamed(held: &str, append: &str, end: Option<u64>) -> String {
    let Some(end) = end.map(|end| end as usize) else { return format!("{held}{append}") };
    let start = end.saturating_sub(append.len());
    if end <= held.len() {
      return if &held[start..end] == append { held.to_string() } else { append.to_string() };
    }
    if start >= held.len() {
      return format!("{held}{append}");
    }
    if held[start..] != append[..held.len() - start] {
      return append.to_string();
    }
    format!("{held}{}", &append[held.len() - start..])
  }

  #[test]
  fn parity_fixture_matches_frames_for_chat_logs_and_synthetic_sequences() {
    // Every delegation card in the parity fixture includes the nullable route.
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/message_aggregator.json")).unwrap();
    let renderers = parity_renderers();

    for case in fixture["parity_chat_logs"].as_array().unwrap() {
      let session = case["session"].as_str().unwrap();
      let mut aggregator = MessageAggregator::with_renderers(0, true, renderers.clone());
      let mut actual = Vec::new();
      for event in case["events"].as_array().unwrap() {
        actual.extend(aggregator.feed(event).unwrap());
      }
      assert_parity_frames(actual, case["frames"].as_array().unwrap(), &format!("session {session}"));
    }

    for case in fixture["synthetic_sequences"].as_array().unwrap() {
      let name = case["name"].as_str().unwrap();
      let offset = case["event_index_offset"].as_u64().unwrap() as usize;
      let mut aggregator = MessageAggregator::with_renderers(offset, true, renderers.clone());
      let mut actual = Vec::new();
      for event in case["events"].as_array().unwrap() {
        actual.extend(aggregator.feed(event).unwrap());
      }
      assert_parity_frames(actual, case["frames"].as_array().unwrap(), name);
    }
  }

  const PARALLEL_STREAM: &str = include_str!("../tests/fixtures/subagents/s_parallel_guard.stream.ndjson");
  const NESTED_STREAM: &str = include_str!("../tests/fixtures/subagents/s_nest.stream.ndjson");

  /// The chat events of a recorded probe stream. Each line wraps one event in its `e` field.
  fn probe_events(stream: &str) -> Vec<Value> {
    stream.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["e"].clone()).collect()
  }

  /// Every frame of the events, through a fresh aggregator and its final flush.
  fn frames_of(events: &[Value]) -> Vec<Value> {
    let mut aggregator = MessageAggregator::new(0, false);
    let mut frames = Vec::new();
    for event in events {
      frames.extend(aggregator.feed(event).unwrap());
    }
    frames.extend(aggregator.flush_pending());
    frames
  }

  /// The tool rows of the last message frame that carries rows.
  fn last_tool_rows(frames: &[Value]) -> Vec<Value> {
    frames.iter().rev().find_map(|frame| frame["message"]["tools"].as_array().cloned()).expect("a message has rows")
  }

  #[test]
  fn parallel_subagents_hang_under_their_own_agent_rows() {
    let frames = frames_of(&probe_events(PARALLEL_STREAM));
    let rows = last_tool_rows(&frames);
    let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(
      ids,
      [
        "toolu_014YYWsBgdnNRsNLC1kwfYEV",
        "toolu_01M7U7UjEQbEV71KVLgZdpbZ",
        "toolu_01XSr4kdnYmP7P9FX4e9XsQx",
        "toolu_017ugFTQWLEdpGHSEUxgLekf",
      ],
      "the parent's rows are its four Agent calls, and the subagent events add none"
    );
    for row in &rows {
      assert_eq!(row["name"], "Agent");
      assert_eq!(row["subagent"]["type"], "general-purpose");
      assert_eq!(row["subagent"]["status"], "completed");
      assert_eq!(row["subagent"]["tool_uses"], 1);
      assert_eq!(row["subagent"]["tools"].as_array().unwrap().len(), 1, "a subagent keeps its own rows");
      assert!(row["output"].as_str().unwrap().contains("[Subagent hand-back]"), "the Agent row shows its report");
    }
    // task_progress rewrites the description that task_started gave.
    assert_eq!(rows[0]["subagent"]["description"], "Running Sleep then echo A");
    assert_eq!(rows[0]["subagent"]["tools"][0]["output"], "A");
    assert_eq!(rows[1]["subagent"]["tools"][0]["output"], "B");
    assert_eq!(rows[2]["subagent"]["tools"][0]["output"], "C");
    let guarded = &rows[3]["subagent"]["tools"][0];
    assert_eq!(guarded["is_error"], true);
    assert!(guarded["output"].as_str().unwrap().contains("hook error"), "the wait guard's refusal lands in D's row");
    assert_eq!(frames.last().unwrap()["message"]["role"], "assistant");
  }

  #[test]
  fn nested_subagents_hang_under_the_agent_row_inside_their_parent() {
    let frames = frames_of(&probe_events(NESTED_STREAM));
    let rows = last_tool_rows(&frames);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "toolu_01YQMbeeCE4eqbdDJ1yozKY5");
    assert_eq!(rows[0]["subagent"]["description"], "Nested reply test");
    let inner_rows = rows[0]["subagent"]["tools"].as_array().unwrap();
    assert_eq!(inner_rows.len(), 1, "the inner Agent call is the outer subagent's only row");
    let inner = &inner_rows[0];
    assert_eq!(inner["name"], "Agent");
    assert_eq!(inner["id"], "toolu_01MZ54DuCghEG3jG5jJrfcta");
    assert_eq!(inner["subagent"]["description"], "Nested reply test");
    assert_eq!(inner["subagent"]["status"], "completed");
    assert_eq!(inner["subagent"]["tool_uses"], 0);
    assert!(inner["output"].as_str().unwrap().contains("[Subagent hand-back]"));
    assert!(!rows[0]["output"].as_str().unwrap().is_empty(), "the outer Agent row shows its report");
    assert_eq!(frames.last().unwrap()["message"]["content"], "NESTED-OK");
  }

  #[test]
  fn a_run_that_ends_before_task_notification_marks_its_subagents_stopped() {
    let mut events = probe_events(PARALLEL_STREAM);
    let first_notification = events.iter().position(|event| event["subtype"] == "task_notification").unwrap();
    events.truncate(first_notification);
    events.push(json!({"type": "master_done", "id": "md", "timestamp": "2026-11-03T00:00:00Z"}));
    let frames = frames_of(&events);
    let rows = last_tool_rows(&frames);
    assert_eq!(rows.len(), 4);
    for row in &rows {
      assert_eq!(row["subagent"]["status"], "stopped");
    }
  }

  #[test]
  fn a_late_subagent_event_updates_the_committed_draft_by_its_message_id() {
    let events = vec![
      json!({"type": "assistant", "id": "a1", "timestamp": "2026-11-03T02:00:00Z", "message": {"content": [
        {"type": "text", "text": "Starting"},
        {"type": "tool_use", "id": "toolu_late", "name": "Agent",
          "input": {"description": "Check", "prompt": "look", "subagent_type": "general-purpose"}},
      ]}}),
      // A user message while the subagent runs commits the draft that holds the Agent row.
      json!({"type": "user", "id": "u2", "content": "one more thing", "timestamp": "2026-11-03T02:00:01Z"}),
      json!({"type": "system", "subtype": "task_started", "tool_use_id": "toolu_late", "description": "Check",
        "subagent_type": "general-purpose", "prompt": "look"}),
      json!({"type": "assistant", "parent_tool_use_id": "toolu_late", "message": {"content": [
        {"type": "tool_use", "id": "toolu_b", "name": "Bash", "input": {"command": "ls"}},
      ]}}),
      json!({"type": "system", "subtype": "task_progress", "tool_use_id": "toolu_late", "description": "Checking",
        "usage": {"tool_uses": 1, "duration_ms": 900, "total_tokens": 500}}),
      json!({"type": "user", "parent_tool_use_id": "toolu_late", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "toolu_b", "content": "x"},
      ]}}),
      json!({"type": "system", "subtype": "task_notification", "tool_use_id": "toolu_late", "status": "completed",
        "usage": {"tool_uses": 1, "duration_ms": 1200, "total_tokens": 600}}),
      json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "toolu_late", "content": "report"},
      ]}}),
    ];
    let frames = frames_of(&events);
    assert_eq!(frames[0]["message"]["id"], "a1");
    assert!(frames[0]["message"]["tools"][0].get("subagent").is_none(), "the commit predates the subagent");
    let updates: Vec<&Value> = frames.iter().filter(|frame| frame["type"] == "message_update").collect();
    assert_eq!(updates.len(), 6, "one update per subagent event after the commit");
    assert!(updates.iter().all(|update| update["message"]["id"] == "a1"), "every update names the committed message");
    let row = |update: &Value| update["message"]["tools"][0].clone();
    assert_eq!(row(updates[0])["subagent"]["status"], "running");
    assert_eq!(row(updates[0])["id"], "toolu_late");
    assert_eq!(row(updates[1])["subagent"]["tools"][0]["name"], "Bash");
    assert_eq!(row(updates[2])["subagent"]["tool_uses"], 1);
    assert_eq!(row(updates[3])["subagent"]["tools"][0]["output"], "x");
    assert_eq!(row(updates[4])["subagent"]["status"], "completed");
    assert_eq!(row(updates[4])["subagent"]["duration_ms"], 1200);
    assert_eq!(row(updates[5])["output"], "report");
  }

  #[test]
  fn a_run_end_stops_a_subagent_of_a_committed_draft_before_the_separator() {
    let events = vec![
      json!({"type": "assistant", "id": "a1", "message": {"content": [
        {"type": "tool_use", "id": "toolu_late", "name": "Agent", "input": {"prompt": "look"}},
      ]}}),
      json!({"type": "user", "id": "u2", "content": "one more thing"}),
      json!({"type": "system", "subtype": "task_started", "tool_use_id": "toolu_late", "description": "Check",
        "subagent_type": "general-purpose", "prompt": "look"}),
      json!({"type": "master_done", "id": "md"}),
    ];
    let frames = frames_of(&events);
    let stopped = frames
      .iter()
      .position(|frame| {
        frame["type"] == "message_update" && frame["message"]["tools"][0]["subagent"]["status"] == "stopped"
      })
      .expect("the run end updates the committed draft");
    let separator = frames.iter().rposition(|frame| frame["type"] == "message").unwrap();
    assert!(stopped < separator, "the update comes before the run's separator");
    assert_eq!(frames.iter().filter(|frame| frame["type"] == "message_update").count(), 2);
  }

  #[test]
  fn user_input_and_assistant_draft_match_the_rendered_shapes() {
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator
      .feed(&json!({
        "type": "user",
        "content": "hello",
        "timestamp": "2026-04-29T00:00:00Z",
      }))
      .unwrap();
    assert_eq!(
      deltas,
      vec![json!({
        "type": "message",
        "message": {
          "role": "user",
          "content": "hello",
          "uploaded_files": [],
          "input_mode": null,
          "event_index": 0,
          "id": "legacy:0",
          "timestamp": "2026-04-29T00:00:00Z",
        }
      })]
    );
    let stream = aggregator
      .feed(&json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "answer"}]},
        "timestamp": "t1",
      }))
      .unwrap();
    assert_eq!(stream[0]["type"], "stream");
    assert_eq!(stream[0]["append"], "answer");
    assert_eq!(stream[0]["length"], 6);
  }

  #[test]
  fn master_done_commits_the_draft_before_its_separator() {
    let mut aggregator = MessageAggregator::new(10, true);
    aggregator
      .feed(&json!({
        "type": "assistant",
        "id": "a",
        "message": {"content": [{"type": "text", "text": "Hi"}]},
        "timestamp": "t1",
      }))
      .unwrap();
    let deltas = aggregator
      .feed(&json!({
        "type": "master_done",
        "id": "done",
        "thinking_seconds": 3,
        "timestamp": "t2",
      }))
      .unwrap();
    assert_eq!(deltas[0]["message"]["event_index"], 10);
    assert_eq!(deltas[0]["message"]["id"], "a");
    assert_eq!(deltas[1]["message"]["event_index"], 11);
    assert_eq!(deltas[1]["message"]["role"], "separator");
    assert!(aggregator.pending_draft_message().is_none());
  }

  #[test]
  fn tool_result_text_is_attached_and_bounded_before_streaming() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator.feed(&json!({"type": "tool_use", "name": "Bash", "input": {"command": "ls"}})).unwrap();
    aggregator.feed(&json!({"type": "tool_result", "content": "z".repeat(10_000)})).unwrap();
    let draft = aggregator.pending_draft_message().unwrap();
    assert_eq!(draft["tools"][0]["output"].as_str().unwrap().len(), TOOL_PREVIEW_CHARS);
    assert_eq!(draft["tools"][0]["output_truncated"], true);
  }

  #[test]
  fn internal_tool_result_updates_the_draft_without_a_user_message() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator.feed(&json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/a"}})).unwrap();
    let deltas = aggregator
      .feed(&json!({
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": [{"type": "text", "text": "read"}]}]},
      }))
      .unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["type"], "stream");
    assert_eq!(deltas[0]["tools_from"], 0);
    assert_eq!(deltas[0]["tools"][0]["output"], "read");
    assert!(deltas[0].get("append").is_none(), "the draft text did not change");
  }

  /// The stream frames of these events, as the live feed sends them.
  fn live_stream_frames(events: &[Value]) -> Vec<Value> {
    let mut aggregator = MessageAggregator::new(0, true);
    let mut frames = Vec::new();
    for event in events {
      frames.extend(aggregator.feed(event).unwrap());
    }
    frames.into_iter().filter(|frame| frame["type"] == "stream").collect()
  }

  fn tool_use_event(id: &str, command: &str) -> Value {
    json!({"type": "tool_use", "tool_use_id": id, "name": "Bash", "input": {"command": command}, "timestamp": "t"})
  }

  #[test]
  fn tool_row_frames_carry_the_suffix_from_the_first_changed_row() {
    let mut events = vec![tool_use_event("r1", "one"), tool_use_event("r2", "two"), tool_use_event("r3", "three")];
    let frames = live_stream_frames(&events);
    let starts: Vec<u64> = frames.iter().map(|frame| frame["tools_from"].as_u64().unwrap()).collect();
    let widths: Vec<usize> = frames.iter().map(|frame| frame["tools"].as_array().unwrap().len()).collect();
    // Each new row appends one row: the suffix starts at the new row and holds it alone.
    assert_eq!(starts, [0, 1, 2]);
    assert_eq!(widths, [1, 1, 1]);
    assert_eq!(frames[0]["tools"][0]["input"]["command"], "one");
    assert_eq!(frames[2]["tools"][0]["input"]["command"], "three");

    // A result on the last row resends that row only, with its output.
    events.push(json!({"type": "tool_result", "tool_use_id": "r3", "content": "done", "timestamp": "t"}));
    // A change to an earlier row starts the suffix there: the subagent hangs under the second row.
    events.push(json!({
      "type": "system", "subtype": "task_started", "tool_use_id": "r2",
      "subagent_type": "general-purpose", "description": "Check", "prompt": "look",
    }));
    let frames = live_stream_frames(&events);
    let result_frame = &frames[3];
    assert_eq!(result_frame["tools_from"], 2);
    assert_eq!(result_frame["tools"].as_array().unwrap().len(), 1);
    assert_eq!(result_frame["tools"][0]["output"], "done");
    let subagent_frame = &frames[4];
    assert_eq!(subagent_frame["tools_from"], 1);
    let suffix = subagent_frame["tools"].as_array().unwrap();
    assert_eq!(suffix.len(), 2, "the suffix reaches the end of the draft's rows");
    assert_eq!(suffix[0]["id"], "r2");
    assert_eq!(suffix[0]["subagent"]["status"], "running");
    assert_eq!(suffix[1]["input"]["command"], "three");
  }

  #[test]
  fn the_first_tool_frame_after_a_commit_starts_the_suffix_at_zero() {
    let mut events = vec![tool_use_event("r1", "one")];
    let frames = live_stream_frames(&events);
    assert_eq!(frames[0]["tools_from"], 0);
    assert_eq!(frames[0]["tools"].as_array().unwrap().len(), 1);

    // The commit closes the draft, so the next draft's rows stream from zero again.
    events.push(json!({"type": "master_done", "timestamp": "t"}));
    events.push(tool_use_event("r2", "two"));
    let frames = live_stream_frames(&events);
    assert_eq!(frames.last().unwrap()["tools_from"], 0);
    assert_eq!(frames.last().unwrap()["tools"].as_array().unwrap().len(), 1);
  }

  #[test]
  fn a_run_end_that_stops_a_row_streams_the_suffix_from_that_row() {
    let events = vec![
      tool_use_event("r1", "one"),
      json!({
        "type": "system", "subtype": "task_started", "tool_use_id": "r1",
        "subagent_type": "general-purpose", "description": "Check", "prompt": "look",
      }),
      json!({"type": "master_done", "timestamp": "t"}),
    ];
    let mut aggregator = MessageAggregator::new(0, true);
    let mut deltas = Vec::new();
    for event in &events {
      deltas.extend(aggregator.feed(event).unwrap());
    }
    // The stop streams before the commit closes the draft.
    let stop = deltas
      .iter()
      .position(|frame| frame["type"] == "stream" && frame["tools"][0]["subagent"]["status"] == "stopped")
      .expect("the run end streams the stopped row");
    let commit = deltas.iter().position(|frame| frame["type"] == "message").unwrap();
    assert!(stop < commit, "the suffix frame precedes the commit");
    assert_eq!(deltas[stop]["tools_from"], 0);
  }

  #[test]
  fn custom_event_renderer_output_gets_the_raw_event_index() {
    let renderers: HashMap<String, Arc<dyn EventRenderer>> = HashMap::from([(
      "feature_event".to_string(),
      Arc::new(|_event: &Value| Ok(Some(json!({"role": "system", "content": "feature"})))) as Arc<dyn EventRenderer>,
    )]);
    let mut aggregator = MessageAggregator::with_renderers(4, false, renderers);
    let deltas = aggregator.feed(&json!({"type": "feature_event", "timestamp": "t"})).unwrap();
    assert_eq!(
      deltas,
      vec![json!({
        "type": "message",
        "message": {
          "role": "system",
          "content": "feature",
          "timestamp": "t",
          "event_index": 4,
          "id": "legacy:4",
        }
      })]
    );
  }

  #[test]
  fn user_event_with_an_input_mode_renders_a_message_that_carries_it() {
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator
      .feed(&json!({
        "type": "user",
        "content": "hello",
        "input_mode": "voice",
        "timestamp": "2026-04-29T00:00:00Z",
      }))
      .unwrap();
    let modes: Vec<Value> = deltas.iter().map(|delta| delta["message"]["input_mode"].clone()).collect();
    assert_eq!(modes, vec![json!("voice")]);
  }

  #[test]
  fn user_event_delta_carries_request_id() {
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator
      .feed(&json!({
        "type": "user",
        "content": "hello",
        "request_id": "browser-send-1",
      }))
      .unwrap();

    assert_eq!(deltas[0]["message"]["request_id"], "browser-send-1");
  }

  #[test]
  fn assistant_text_then_master_done_commits_message() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator
      .feed(&json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
      .unwrap();
    let deltas = aggregator.feed(&json!({"type": "master_done", "thinking_seconds": 3, "timestamp": "t2"})).unwrap();
    assert_eq!(
      deltas,
      vec![
        json!({
          "type": "message",
          "message": {"role": "assistant", "content": "Hi", "event_index": 0, "id": "legacy:0", "timestamp": "t1"},
        }),
        json!({
          "type": "message",
          "message": {
            "role": "separator",
            "thinking_seconds": 3,
            "event_index": 1,
            "id": "legacy:1",
            "timestamp": "t2",
          },
        }),
      ]
    );
    assert!(aggregator.pending_draft_message().is_none());
  }

  #[test]
  fn still_thinking_master_done_yields_separator_without_seconds() {
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator.feed(&json!({"type": "master_done", "still_thinking": true, "timestamp": "t1"})).unwrap();
    assert_eq!(
      deltas,
      vec![json!({
        "type": "message",
        "message": {
          "role": "separator",
          "thinking_seconds": null,
          "event_index": 0,
          "id": "legacy:0",
          "timestamp": "t1",
        },
      })]
    );
  }

  #[test]
  fn task_delegated_message_exposes_metadata_without_full_description_body() {
    // The event route rides the delegation card.
    let mut aggregator = MessageAggregator::new(0, true);
    let long_description = "## Goal\nDo a long task spec that belongs in Workers.";
    let invocation = json!({
      "task_type": "implement",
      "repo_path": "/tmp/repo",
      "base_branch": "main",
      "task_spec_file": "/tmp/task.md",
      "reviewer_context_file": "/tmp/reviewer.md",
      "keep_worktree": false,
      "backend": "codex-o3",
    });
    let deltas = aggregator
      .feed(&json!({
        "type": "task_delegated",
        "thread_id": "thread-id",
        "description": long_description,
        "timestamp": "2026-07-01T12:00:00Z",
        "backend": "codex-o3",
        "model": "o3",
        "route": {"backends": ["codex-o3"], "reason": "design"},
        "delegate_invocation": invocation,
      }))
      .unwrap();
    assert_eq!(
      deltas,
      vec![json!({
        "type": "message",
        "message": {
          "role": "task_delegated",
          "content": "Task delegated",
          "thread_id": "thread-id",
          "child_session_id": "",
          "delegate_invocation": invocation,
          "backend": "codex-o3",
          "model": "o3",
          "route": {"backends": ["codex-o3"], "reason": "design"},
          "event_index": 0,
          "id": "legacy:0",
          "timestamp": "2026-07-01T12:00:00Z",
        },
      })]
    );
    assert!(!deltas[0]["message"]["content"].as_str().unwrap().contains(long_description));
    assert!(deltas[0]["message"].get("description").is_none());
  }

  #[test]
  fn quota_failover_renders_the_backend_and_recovery_time() {
    // The recovery line names the backend and the time it recovers.
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator
      .feed(&json!({
        "type": "quota_failover",
        "from": "fake",
        "to": "strong",
        "recovers_at": "2030-01-02T03:04+00:00",
      }))
      .unwrap();
    assert_eq!(
      deltas[0]["message"],
      json!({
        "role": "system",
        "content": "Quota exhausted on fake: switched to strong; fake recovers at 2030-01-02T03:04+00:00",
        "event_index": 0,
        "id": "legacy:0",
        "timestamp": null,
      })
    );
  }

  #[test]
  fn a_switch_recorded_for_a_refusal_or_a_timeout_names_its_own_reason() {
    for (reason, head) in [
      (event_types::QUOTA_REASON_ACCESS_REFUSED, "Access refused on fake: switched to strong"),
      (event_types::QUOTA_REASON_TIMEOUT, "Timeout on fake: switched to strong"),
      (event_types::QUOTA_REASON_ITERATION_FAILURE, "Iteration failed on fake: switched to strong"),
    ] {
      let mut aggregator = MessageAggregator::new(0, true);
      let deltas = aggregator
        .feed(&json!({
          "type": "quota_failover",
          "from": "fake",
          "to": "strong",
          "reason": reason,
          "recovers_at": "2030-01-02T03:04+00:00",
        }))
        .unwrap();
      assert_eq!(
        deltas[0]["message"]["content"],
        format!("{head}; fake recovers at 2030-01-02T03:04+00:00"),
        "{reason}"
      );
    }
  }

  #[test]
  fn quota_failover_states_an_unavailable_handoff_summary() {
    let mut aggregator = MessageAggregator::new(0, true);
    let deltas = aggregator
      .feed(&json!({
        "type": "quota_failover",
        "from": "fake",
        "to": "strong",
        "recovers_at": "2030-01-02T03:04+00:00",
        "reason": "quota_error",
        "set": "pool",
        "summary": "unavailable: the run's events log is missing",
      }))
      .unwrap();
    assert_eq!(
      deltas[0]["message"]["content"],
      "Quota exhausted on fake: switched to strong; fake recovers at 2030-01-02T03:04+00:00; handoff summary \
       unavailable: the run's events log is missing"
    );
  }

  #[test]
  fn tool_use_attaches_to_buffer_then_tool_result_updates_output() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator.feed(&assistant_text_tool_use_event("Running", "Bash", json!({"command": "ls"}), "t1")).unwrap();
    aggregator
      .feed(&json!({
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": "file1\nfile2", "is_error": false}]},
      }))
      .unwrap();
    let draft = aggregator.pending_draft_message().unwrap();
    assert_eq!(draft["content"], "Running");
    assert_eq!(
      draft["tools"],
      json!([{
        "name": "Bash",
        "input": {"command": "ls"},
        "output": "file1\nfile2",
        "is_error": false,
      }])
    );
  }

  #[test]
  fn run_tool_counts_match_rows_and_keep_standalone_exit_plan_mode() {
    let long_command = "x".repeat(TOOL_PREVIEW_CHARS + 50);
    let events = vec![
      json!({
        "type": "assistant",
        "message": {"content": [
          {"type": "tool_use", "name": "Bash", "input": {"command": long_command}},
          {"type": "tool_use", "name": "ExitPlanMode", "input": {"plan": "done"}},
        ]},
        "timestamp": "assistant-time",
      }),
      json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/file"}, "timestamp": "tool-time"}),
      json!({
        "type": "tool_use",
        "name": "ExitPlanMode",
        "input": {"plan": "standalone event"},
        "timestamp": "standalone-plan-time",
      }),
      json!({
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "name": "ExitPlanMode", "input": {"plan": "done"}}]},
        "timestamp": "plan-time",
      }),
    ];
    let mut aggregator = MessageAggregator::new(0, false);
    let mut messages = Vec::new();
    for event in &events {
      messages.extend(
        aggregator
          .feed(event)
          .unwrap()
          .into_iter()
          .filter(|delta| delta["type"] == "message")
          .map(|delta| delta["message"].clone()),
      );
    }
    messages.extend(aggregator.flush_pending().into_iter().map(|delta| delta["message"].clone()));
    let rendered_tool_rows =
      messages.iter().map(|message| message.get("tools").and_then(Value::as_array).map_or(0, Vec::len)).sum::<usize>();

    assert_eq!(run_tool_call_count(&events.iter().collect::<Vec<_>>()), rendered_tool_rows);
    assert_eq!(rendered_tool_rows, 3);
    assert_eq!(
      run_last_tool(&events.iter().collect::<Vec<_>>()),
      Some(json!({"name": "ExitPlanMode", "input": {"plan": "standalone event"}, "timestamp": "standalone-plan-time"}))
    );
    let last_bash = run_last_tool(&events[..1].iter().collect::<Vec<_>>()).unwrap();
    assert_eq!(last_bash["name"], "Bash");
    assert_eq!(last_bash["input"]["command"].as_str().unwrap().chars().count(), TOOL_PREVIEW_CHARS);
  }

  #[test]
  fn run_cause_uses_message_before_content_and_falls_back_to_newest_text() {
    let events = [
      json!({"type": "error", "content": "usable older stderr", "timestamp": "older"}),
      json!({"type": "error", "content": "  ", "timestamp": "empty"}),
      json!({"type": "error", "timestamp": "newest-empty"}),
    ];
    assert_eq!(
      run_cause(&events.iter().collect::<Vec<_>>()),
      Some(("usable older stderr".to_string(), "usable older stderr".to_string(), 0, "older".to_string()))
    );

    let events = [
      json!({"type": "error", "message": "older message", "timestamp": "older"}),
      json!({"type": "error", "message": "newest message", "content": "newer content", "timestamp": "newest"}),
    ];
    assert_eq!(run_cause(&events.iter().collect::<Vec<_>>()).unwrap().0, "newest message");
  }

  #[test]
  fn thinking_is_flushed_with_assistant_draft() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator
      .feed(&json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
      .unwrap();
    aggregator.feed(&json!({"type": "thinking", "content": "planning", "timestamp": "t2"})).unwrap();
    let deltas = aggregator.feed(&json!({"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"})).unwrap();
    assert_eq!(deltas[0]["message"]["role"], "assistant");
    assert_eq!(deltas[0]["message"]["content"], "Hi");
    assert_eq!(deltas[0]["message"]["thinking"], "planning");
    assert_eq!(deltas[1]["message"]["role"], "separator");
  }

  #[test]
  fn stream_deltas_stay_bounded_after_a_giant_tool_result() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator.feed(&json!({"type": "tool_use", "name": "Bash", "input": {"cmd": "cat big.log"}})).unwrap();
    aggregator.feed(&json!({"type": "tool_result", "tool_name": "Bash", "content": "z".repeat(10_000_000)})).unwrap();
    let mut serialized = Vec::new();
    for i in 0..20 {
      aggregator.feed(&assistant_text_event(&format!("delta {i}"))).unwrap();
      let draft = aggregator.pending_draft_message().unwrap();
      serialized.push(serde_json::to_string(&draft).unwrap().len());
    }
    assert!(serialized.iter().all(|len| *len < 100_000));
  }

  #[test]
  fn task_delegated_timestamp_falls_back_to_created_at_then_null() {
    let cases = [
      (json!({"type": "task_delegated", "thread_id": "t"}), Value::Null),
      (json!({"type": "task_delegated", "timestamp": "", "created_at": ""}), json!("")),
      (json!({"type": "task_delegated", "timestamp": "", "created_at": "c"}), json!("c")),
      (json!({"type": "task_delegated", "timestamp": "t", "created_at": "c"}), json!("t")),
    ];
    for (event, expected) in cases {
      let deltas = MessageAggregator::new(0, false).feed(&event).unwrap();
      assert_eq!(deltas[0]["message"]["timestamp"], expected, "event {event}");
    }
  }

  #[test]
  fn a_system_event_without_a_message_leaves_the_draft_open() {
    let mut aggregator = MessageAggregator::new(0, true);
    aggregator.feed(&assistant_text_event("draft")).unwrap();
    let deltas = aggregator.feed(&json!({"type": "system", "subtype": "status"})).unwrap();
    assert!(deltas.is_empty());
    assert_eq!(aggregator.pending_draft_message().unwrap()["content"], "draft");
  }

  #[test]
  fn a_feature_renderer_that_renders_nothing_still_commits_the_draft() {
    let renderers: HashMap<String, Arc<dyn EventRenderer>> =
      HashMap::from([("quiet_event".to_string(), Arc::new(|_: &Value| Ok(None)) as Arc<dyn EventRenderer>)]);
    let mut aggregator = MessageAggregator::with_renderers(0, true, renderers);
    aggregator.feed(&assistant_text_event("draft")).unwrap();
    let deltas = aggregator.feed(&json!({"type": "quiet_event"})).unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["type"], "message");
    assert_eq!(deltas[0]["message"]["content"], "draft");
    assert!(aggregator.pending_draft_message().is_none());
  }

  #[test]
  fn task_reopened_without_a_reason_drops_the_trailing_space() {
    let deltas = MessageAggregator::new(0, false).feed(&json!({"type": "task_reopened"})).unwrap();
    assert_eq!(deltas[0]["message"]["content"], "Task reopened:");
  }

  #[test]
  fn compaction_model_note_names_the_family_as_model_family_does() {
    for (model, note) in
      [("claude", "by Claude"), ("claude-opus-5", "by Opus"), ("Sonnet-X", "by Sonnet"), ("CLAUDE-haiku-5", "by Haiku")]
    {
      assert_eq!(model_note(&json!({"model": model})), note, "model {model}");
    }
  }
}
