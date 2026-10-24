use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::event_types::{self, EventFields, is_truthy};
use serde_json::{Map, Value, json};

use crate::hooks::EventRenderer;
use crate::message_events;
use crate::{StoreError, StoreResult};

/// Maximum string length sent for one tool preview field.
pub const TOOL_PREVIEW_CHARS: usize = 500;

const TOOL_INPUT_DEAD_FIELD_CHARS: usize = 60;
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
pub struct MessageAggregator {
  event_index_offset: usize,
  processed: usize,
  assistant_buf: String,
  thinking_buf: String,
  last_assistant_timestamp: Option<Value>,
  last_event_index: usize,
  last_event_id: Option<String>,
  tools_buf: Vec<Value>,
  emit_stream_deltas: bool,
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
      tools_buf: Vec::new(),
      emit_stream_deltas,
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
      tools_buf: self.tools_buf.clone(),
      emit_stream_deltas: self.emit_stream_deltas,
      event_renderers: self.event_renderers.clone(),
    }
  }

  /// Commit a buffered draft at the end of a stable history.
  pub fn flush_pending(&mut self) -> Vec<Value> {
    self.flush_to_message_delta()
  }

  /// Return a copy of the current draft, or no value when the draft is empty.
  pub fn pending_draft_message(&self) -> Option<Value> {
    if self.assistant_buf.is_empty() && self.tools_buf.is_empty() && self.thinking_buf.is_empty() {
      return None;
    }
    let mut message = json!({
      "role": "assistant",
      "content": self.assistant_buf.as_str(),
      "event_index": self.last_event_index,
      "id": self.last_event_id.clone().unwrap_or_else(|| format!("legacy:{}", self.last_event_index)),
      "timestamp": self.last_assistant_timestamp.clone().unwrap_or(Value::Null),
    });
    if !self.tools_buf.is_empty() {
      message["tools"] = Value::Array(self.tools_buf.clone());
    }
    if !self.thinking_buf.is_empty() {
      message["thinking"] = json!(self.thinking_buf.as_str());
    }
    Some(message)
  }

  /// Set whether future feeds emit stream snapshots.
  pub fn set_emit_stream_deltas(&mut self, emit: bool) {
    self.emit_stream_deltas = emit;
  }

  /// Return whether stream snapshots are enabled.
  pub fn emits_stream_deltas(&self) -> bool {
    self.emit_stream_deltas
  }

  fn feed_at<E: EventFields>(&mut self, event: &E, index: usize) -> StoreResult<Vec<Value>> {
    if !event.is_object() {
      return Err(StoreError::new("chat event must be a JSON object"));
    }
    let event_type = event.field("type").and_then(EventFields::as_str_field);
    let event_id = event
      .field("id")
      .filter(|value| is_truthy(*value))
      .map(python_string)
      .unwrap_or_else(|| format!("legacy:{index}"));
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
    if event.field("message").is_some() && event.field("content").is_none() {
      let content = event.field("message").and_then(|message| message.field("content"));
      for block in message_blocks(content, "user message content", false)? {
        if block.field("type").and_then(EventFields::as_str_field) == Some("tool_result") && !self.tools_buf.is_empty()
        {
          self.attach_tool_output(extract_tool_result_text(block)?, block.field("is_error").is_some_and(is_truthy));
        }
      }
      return Ok(self.stream_delta());
    }

    let mut deltas = self.flush_to_message_delta();
    let normalized = message_events::normalize_user_message_event(event)?;
    let mut message = json!({
      "role": "user",
      "content": normalized.content,
      "uploaded_files": normalized.uploaded_files,
      "input_mode": event.field("input_mode").map(EventFields::to_owned_value).unwrap_or(Value::Null),
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
          deltas.push(json!({"type": "message", "message": plan_message}));
        }
      }
    }

    for block in &blocks {
      if block.field("type").and_then(EventFields::as_str_field) == Some("tool_use")
        && block.field("name").and_then(EventFields::as_str_field) != Some("ExitPlanMode")
      {
        self.buffer_tool(
          block.field("name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
          block.field("input").map(EventFields::to_owned_value).unwrap_or_else(|| json!({})),
        );
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
    if !self.assistant_buf.is_empty() || !self.tools_buf.is_empty() || !self.thinking_buf.is_empty() {
      self.last_event_id = Some(event_id.to_string());
    }
    deltas.extend(self.stream_delta());
    Ok(deltas)
  }

  fn feed_tool_use<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    self.buffer_tool(
      event.field("name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      event.field("input").map(EventFields::to_owned_value).unwrap_or_else(|| json!({})),
    );
    self.last_event_index = index;
    self.last_event_id = Some(event_id.to_string());
    if self.last_assistant_timestamp.is_none() {
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    Ok(self.stream_delta())
  }

  fn feed_tool_result<E: EventFields>(&mut self, event: &E, index: usize) -> StoreResult<Vec<Value>> {
    if self.tools_buf.is_empty() {
      return Ok(Vec::new());
    }
    self.attach_tool_output(
      event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      event.field("is_error").is_some_and(is_truthy),
    );
    self.last_event_index = index;
    Ok(self.stream_delta())
  }

  fn feed_thinking<E: EventFields>(&mut self, event: &E, index: usize, event_id: &str) -> StoreResult<Vec<Value>> {
    self.last_event_index = index;
    let content = event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!(""));
    self.thinking_buf.push_str(&python_string(&content));
    if self.last_assistant_timestamp.is_none() {
      self.last_assistant_timestamp = event.field("timestamp").map(EventFields::to_owned_value);
    }
    if !self.assistant_buf.is_empty() || !self.tools_buf.is_empty() || !self.thinking_buf.is_empty() {
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
      let Some(mut message) = render_system(event) else { return Ok(Vec::new()) };
      let mut deltas = self.flush_to_message_delta();
      add_message_metadata(&mut message, event, index, event_id)?;
      deltas.push(json!({"type": "message", "message": message}));
      return Ok(deltas);
    }
    if !builtin_event_types().contains(&event_type) && !self.event_renderers.contains_key(event_type) {
      return Ok(Vec::new());
    }
    let mut deltas = self.flush_to_message_delta();
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
    let Some(message) = self.pending_draft_message() else { return Vec::new() };
    self.assistant_buf.clear();
    self.thinking_buf.clear();
    self.last_assistant_timestamp = None;
    self.last_event_id = None;
    self.tools_buf.clear();
    vec![json!({"type": "message", "message": message})]
  }

  fn buffer_tool(&mut self, name: Value, input: Value) {
    self.tools_buf.push(tool_preview(json!({
      "name": name,
      "input": input,
      "output": "",
      "is_error": false,
    })));
  }

  fn attach_tool_output(&mut self, output: Value, is_error: bool) {
    let last = self.tools_buf.last().expect("tool output has a buffered tool");
    let mut row = last.as_object().expect("tool rows are objects").clone();
    row.insert("output".to_string(), output);
    row.insert("is_error".to_string(), json!(is_error));
    *self.tools_buf.last_mut().expect("tool output has a buffered tool") = tool_preview(Value::Object(row));
  }

  fn stream_delta(&self) -> Vec<Value> {
    if !self.emit_stream_deltas {
      return Vec::new();
    }
    let Some(mut message) = self.pending_draft_message() else { return Vec::new() };
    if let Some(tools) = message.get("tools").and_then(Value::as_array).cloned() {
      message["tools"] = Value::Array(tools.into_iter().map(tool_preview).collect());
    }
    vec![json!({"type": "stream", "message": message})]
  }
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
      "content": format!("Error: {}", event.field("content").map_or_else(String::new, python_string)),
    }),
    event_types::ERROR => {
      let content = event
        .field("content")
        .filter(|value| is_truthy(*value))
        .or_else(|| event.field("message").filter(|value| is_truthy(*value)))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!("Unknown error"));
      json!({"role": "system", "content": format!("Error: {}", python_string(&content))})
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
      // Python's `or` yields the fallback's own value, so both fields absent give null.
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
        "content": format!("{icon} {}: {}", py_string_default(event, "task", ""), py_string_default(event, "message", "")),
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
      let content = error.map_or(head.clone(), |error| format!("{head} — {}", python_string(error)));
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
    event_types::AGENT_MESSAGE => json!({
      "role": event_types::AGENT_MESSAGE,
      "content": event.field("content").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "from_session": event.field("from_session").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
      "from_session_name": event.field("from_session_name").map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
    }),
    event_types::BACKEND_SWITCHED => json!({
      "role": "system",
      "content": format!("Backend switched: {} → {}", py_string_default(event, "from", ""), py_string_default(event, "to", "")),
    }),
    event_types::QUOTA_FAILOVER => {
      let from_backend = py_string_default(event, "from", "");
      let recovers_at = event.field("recovers_at").filter(|value| is_truthy(*value));
      let recovery =
        recovers_at.map_or_else(String::new, |value| format!("; {from_backend} recovers at {}", python_string(value)));
      json!({
        "role": "system",
        "content": format!("Quota exhausted on {from_backend}: switched to {}{recovery}", py_string_default(event, "to", "")),
      })
    }
    event_types::BACKEND_OVERLAY_INACTIVE | event_types::BACKEND_OVERLAY_UNDECLARED => {
      let backend = py_string_default(event, "backend", "");
      if event.field("reason").and_then(EventFields::as_str_field) == Some(event_types::OVERLAY_REASON_UNREADABLE) {
        json!({
          "role": "system",
          "content": format!(
            "Backend {backend}: prompt_overlay '{}' could not be loaded ({}) — running without a fence",
            py_string_default(event, "overlay", ""),
            py_string_default(event, "error", ""),
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
        event.field("configured_model").filter(|value| is_truthy(*value)).map(python_string).unwrap_or_default();
      let served = python_join_strings(event.field("served_models"), "served_models")?;
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
      let text = python_string(&content);
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
        "base_branch": event.field("base_branch").filter(|value| is_truthy(*value)).map(EventFields::to_owned_value).unwrap_or_else(|| json!("")),
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
      let content = format!("Task {}: {}", python_string(&outcome), python_string(&summary)).trim_end().to_string();
      json!({"role": "system", "content": content})
    }
    event_types::TASK_REOPENED => {
      let reason = event
        .field("reason")
        .filter(|value| is_truthy(*value))
        .map(EventFields::to_owned_value)
        .unwrap_or_else(|| json!(""));
      json!({"role": "system", "content": format!("Task reopened: {}", python_string(&reason)).trim_end()})
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
  let kind =
    event.field("kind").filter(|value| is_truthy(*value)).map(python_string).unwrap_or_else(|| "run".to_string());
  let backend = event
    .field("backend_label")
    .filter(|value| is_truthy(*value))
    .or_else(|| event.field("backend").filter(|value| is_truthy(*value)))
    .map(python_string)
    .unwrap_or_default();
  let state = event.field("state").filter(|value| is_truthy(*value)).map(python_string).unwrap_or_default();
  let state_word = if state == "withheld" && event.field("withheld").is_some_and(is_truthy) {
    format!("withheld · {}", python_string(event.field("withheld").expect("checked field")))
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
    let count = python_int(tool_calls).ok_or_else(|| StoreError::new("run header tool_calls must be an integer"))?;
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
    "launch_prompt_ref": required_field(event, "launch_prompt_ref")?.to_owned_value(),
  }))
}

fn render_task_input_acknowledged<E: EventFields>(event: &E) -> StoreResult<Value> {
  let input_ids = python_join_strings(event.field("input_ids"), "input_ids")?;
  let mut content = format!("Task input(s) acknowledged as handled: {input_ids}");
  if event.field("note").is_some_and(is_truthy) {
    content.push_str(" — ");
    content.push_str(&python_string(event.field("note").expect("checked field")));
  }
  Ok(json!({"role": "system", "content": content.trim_end()}))
}

/// The content blocks Python iterates for a message field.
///
/// Python iterates a string's characters or an object's keys when `content` is not a list, and
/// none of those values is a block object, so those shapes contribute no blocks. A scalar that
/// Python cannot iterate still fails, while an assistant field treats a falsy value as empty
/// because Python's `content or []` replaces it.
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
    Some(raw) => Ok(json!(python_string(raw))),
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

fn tool_preview(tool: Value) -> Value {
  let mut tool = tool.as_object().expect("tool rows are objects").clone();
  let output_trims =
    tool.get("output").and_then(Value::as_str).is_some_and(|value| value.chars().count() > TOOL_PREVIEW_CHARS);
  let read_fields = tool
    .get("input")
    .and_then(Value::as_object)
    .map(|input| input_read_fields(tool.get("name"), input))
    .unwrap_or_default();
  let input_trims = tool.get("input").and_then(Value::as_object).is_some_and(|input| {
    input.iter().any(|(key, value)| {
      value.as_str().is_some_and(|text| {
        text.chars().count() > if read_fields.contains(key) { TOOL_PREVIEW_CHARS } else { TOOL_INPUT_DEAD_FIELD_CHARS }
      })
    })
  });
  if !output_trims && !input_trims {
    return Value::Object(tool);
  }
  if output_trims {
    let output = tool["output"].as_str().expect("trimmed output is a string");
    tool.insert("output".to_string(), json!(take_chars(output, TOOL_PREVIEW_CHARS)));
    tool.insert("output_truncated".to_string(), json!(true));
  }
  if input_trims {
    let mut input = tool["input"].as_object().expect("trimmed input is an object").clone();
    for (key, value) in &mut input {
      if let Some(text) = value.as_str() {
        let limit = if read_fields.contains(key) { TOOL_PREVIEW_CHARS } else { TOOL_INPUT_DEAD_FIELD_CHARS };
        if text.chars().count() > limit {
          *value = json!(take_chars(text, limit));
        }
      }
    }
    tool.insert("input".to_string(), Value::Object(input));
    tool.insert("input_truncated".to_string(), json!(true));
  }
  Value::Object(tool)
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
          .map(|value| python_string(&value.to_owned_value()))
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

fn input_read_fields(name: Option<&Value>, input: &Map<String, Value>) -> Vec<String> {
  match name.and_then(Value::as_str) {
    Some("Bash" | "bash") => vec!["command".to_string()],
    Some("Read" | "Edit" | "Write") => vec!["file_path".to_string()],
    Some("Glob") => vec!["pattern".to_string()],
    Some("Grep") => vec!["pattern".to_string(), "path".to_string()],
    _ => input.keys().next().cloned().into_iter().collect(),
  }
}

fn take_chars(value: &str, count: usize) -> String {
  value.chars().take(count).collect()
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

fn py_string_default<E: EventFields>(event: &E, key: &str, default: &str) -> String {
  event.field(key).map_or_else(|| default.to_string(), python_string)
}

fn required_field<'a, E: EventFields>(event: &'a E, key: &str) -> StoreResult<&'a E> {
  event.field(key).ok_or_else(|| StoreError::new(format!("event renderer requires field {key}")))
}

/// Python's `", ".join(value or [])`: an array joins its strings, while a string or object
/// contributes its characters or keys, matching how Python iterates those values.
fn python_join_strings<E: EventFields>(value: Option<&E>, field: &str) -> StoreResult<String> {
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

/// Python's `int(value)` for the JSON shapes a tool-call count can carry.
fn python_int<E: EventFields>(value: &E) -> Option<String> {
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
  value.as_str_field().and_then(parse_python_int_string)
}

fn parse_python_int_string(value: &str) -> Option<String> {
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

/// Python's `str(value)`. A string is read in place; other shapes take the owned form that `python_repr` formats.
fn python_string<E: EventFields>(value: &E) -> String {
  match value.as_str_field() {
    Some(text) => text.to_string(),
    None => python_string_of_value(&value.to_owned_value()),
  }
}

fn python_string_of_value(value: &Value) -> String {
  match value {
    Value::Null => "None".to_string(),
    Value::Bool(true) => "True".to_string(),
    Value::Bool(false) => "False".to_string(),
    Value::String(value) => value.clone(),
    Value::Number(value) => value.to_string(),
    Value::Array(_) | Value::Object(_) => python_repr(value),
  }
}

fn python_repr(value: &Value) -> String {
  match value {
    Value::String(value) => python_repr_string(value),
    Value::Array(values) => format!("[{}]", values.iter().map(python_repr).collect::<Vec<_>>().join(", ")),
    Value::Object(values) => format!(
      "{{{}}}",
      values
        .iter()
        .map(|(key, value)| format!("{}: {}", python_repr_string(key), python_repr(value)))
        .collect::<Vec<_>>()
        .join(", ")
    ),
    _ => python_string_of_value(value),
  }
}

fn python_repr_string(value: &str) -> String {
  let quote = if !value.contains('\'') || value.contains('"') { '\'' } else { '"' };
  let mut repr = String::with_capacity(value.len() + 2);
  repr.push(quote);
  for character in value.chars() {
    match character {
      '\\' => repr.push_str("\\\\"),
      '\n' => repr.push_str("\\n"),
      '\r' => repr.push_str("\\r"),
      '\t' => repr.push_str("\\t"),
      '\x08' => repr.push_str("\\x08"),
      '\x0c' => repr.push_str("\\x0c"),
      value if value == quote => {
        repr.push('\\');
        repr.push(value);
      }
      value if value.is_control() && (value as u32) <= 0xff => repr.push_str(&format!("\\x{:02x}", value as u32)),
      value if value.is_control() && (value as u32) <= 0xffff => repr.push_str(&format!("\\u{:04x}", value as u32)),
      value if value.is_control() => repr.push_str(&format!("\\U{:08x}", value as u32)),
      value => repr.push(value),
    }
  }
  repr.push(quote);
  repr
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::store::test_support::{assistant_text_event, assistant_text_tool_use_event};

  fn parity_renderers() -> HashMap<String, Arc<dyn EventRenderer>> {
    let slack: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      let content = event.get("content").cloned().unwrap_or_else(|| json!(""));
      Ok(Some(json!({"role": "system", "content": format!("Posted to Slack: {}", python_string(&content))})))
    });
    let discord: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
      let content = event.get("content").cloned().unwrap_or_else(|| json!(""));
      Ok(Some(json!({"role": "system", "content": format!("Posted to Discord: {}", python_string(&content))})))
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
      let value = |key: &str| python_string(event.get(key).unwrap_or(&json!("")));
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

  fn assert_parity_frames(actual: Vec<Value>, expected: &[Value], case: &str) {
    if actual == expected {
      return;
    }
    let first_difference = actual
      .iter()
      .zip(expected)
      .position(|(actual, expected)| actual != expected)
      .unwrap_or(actual.len().min(expected.len()));
    panic!(
      "Python aggregator frames differ for {case} at frame {first_difference}; actual: {:?}; expected: {:?}; lengths: {}/{}",
      actual.get(first_difference),
      expected.get(first_difference),
      actual.len(),
      expected.len()
    );
  }

  #[test]
  fn parity_fixture_matches_python_frames_for_chat_logs_and_synthetic_sequences() {
    // Python: src/runtime/message_aggregator.py:539-559 includes the nullable route on every delegation card.
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
    assert_eq!(stream[0]["message"]["id"], "legacy:1");
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
    assert_eq!(deltas[0]["message"]["tools"][0]["output"], "read");
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
    // Python: src/runtime/message_aggregator.py:539-559 carries the event route onto the delegation card.
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
    // Python: src/runtime/message_aggregator.py:367-375 renders the same recovery line.
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
