//! The tool rows of the Everything view are the rows of the open draft and the subagents that a row starts. They also
//! include the committed drafts that a late subagent event still updates.
//!
//! Subagent events name their row by tool_use id, so every row keeps its id in memory. A frame shows the id, and
//! the `subagent` object, only on a row that a subagent hangs under. A row without a subagent renders exactly as
//! it did before subagents were shown.

use charliebot_core::event_types::{EDIT_TOOL, GLOB_TOOL, GREP_TOOL, READ_TOOL, TOOL_INPUT_FILE_PATH, WRITE_TOOL};
use charliebot_core::str_methods::take_chars;
use charliebot_core::subagent::SUBAGENT_TOOL_NAMES;
use serde_json::{Value, json};

use crate::{StoreError, StoreResult};

/// Maximum string length sent for one tool preview field, and for a subagent prompt.
pub const TOOL_PREVIEW_CHARS: usize = 500;
const TOOL_INPUT_DEAD_FIELD_CHARS: usize = 60;

/// The status a subagent shows in its card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
  Running,
  Completed,
  Failed,
  Stopped,
}

impl SubagentStatus {
  /// Read the status of a `task_notification`. Any other value is an error, because the card would show nothing.
  pub fn from_notification(status: &str) -> StoreResult<Self> {
    match status {
      "completed" => Ok(Self::Completed),
      "failed" => Ok(Self::Failed),
      "stopped" => Ok(Self::Stopped),
      other => Err(StoreError::new(format!("task_notification status {other:?} is not a subagent status"))),
    }
  }

  fn as_str(self) -> &'static str {
    match self {
      Self::Running => "running",
      Self::Completed => "completed",
      Self::Failed => "failed",
      Self::Stopped => "stopped",
    }
  }
}

/// The usage counts a subagent event reports. A missing count stays unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
  pub tool_uses: Option<u64>,
  pub duration_ms: Option<u64>,
  pub total_tokens: Option<u64>,
}

/// One tool call: the preview fields of the call, plus the subagent it started, if any.
#[derive(Clone)]
pub struct ToolRow {
  id: Option<String>,
  tool: Value,
  subagent: Option<Subagent>,
}

/// A subagent as its stream events report it. Its rows are the tool calls the subagent made.
#[derive(Clone)]
pub struct Subagent {
  kind: String,
  description: String,
  prompt: String,
  status: SubagentStatus,
  usage: Usage,
  tools: Vec<ToolRow>,
}

impl Subagent {
  fn new() -> Self {
    Self {
      kind: String::new(),
      description: String::new(),
      prompt: String::new(),
      status: SubagentStatus::Running,
      usage: Usage::default(),
      tools: Vec::new(),
    }
  }

  /// Record the `task_started` fields of the subagent.
  pub fn start(&mut self, kind: &str, description: &str, prompt: &str) {
    self.kind = kind.to_string();
    self.description = description.to_string();
    self.prompt = take_chars(prompt, TOOL_PREVIEW_CHARS);
  }

  /// Record a `task_progress`: a new description when it carries one, and the usage it reports.
  pub fn progress(&mut self, description: Option<&str>, usage: Usage) {
    if let Some(description) = description {
      self.description = description.to_string();
    }
    self.record_usage(usage);
  }

  /// Record a `task_notification`: the final status and the final usage.
  pub fn finish(&mut self, status: SubagentStatus, usage: Usage) {
    self.status = status;
    self.record_usage(usage);
  }

  /// Keep the prompt that a subagent's own user event carries, when `task_started` did not supply one.
  pub fn set_prompt_if_missing(&mut self, prompt: &str) {
    if self.prompt.is_empty() {
      self.prompt = take_chars(prompt, TOOL_PREVIEW_CHARS);
    }
  }

  pub fn add_tools(&mut self, rows: Vec<ToolRow>) {
    self.tools.extend(rows);
  }

  fn record_usage(&mut self, usage: Usage) {
    self.usage.tool_uses = usage.tool_uses.or(self.usage.tool_uses);
    self.usage.duration_ms = usage.duration_ms.or(self.usage.duration_ms);
    self.usage.total_tokens = usage.total_tokens.or(self.usage.total_tokens);
  }

  /// Mark a subagent that is still running as stopped. Returns whether anything changed.
  fn stop_running(&mut self) -> bool {
    let mut changed = false;
    if self.status == SubagentStatus::Running {
      self.status = SubagentStatus::Stopped;
      changed = true;
    }
    for row in &mut self.tools {
      changed |= row.stop_running();
    }
    changed
  }

  fn render(&self) -> Value {
    json!({
      "type": self.kind,
      "description": self.description,
      "prompt": self.prompt,
      "status": self.status.as_str(),
      "tool_uses": self.usage.tool_uses,
      "duration_ms": self.usage.duration_ms,
      "total_tokens": self.usage.total_tokens,
      "tools": self.tools.iter().map(ToolRow::render).collect::<Vec<_>>(),
    })
  }
}

impl ToolRow {
  /// Build a row from a tool call with no result yet.
  pub fn new(id: Option<String>, name: Value, input: Value) -> Self {
    let tool = tool_preview(json!({"name": name, "input": input, "output": "", "is_error": false}));
    Self { id, tool, subagent: None }
  }

  /// Attach a result to the row and preview it again.
  pub fn attach_output(&mut self, output: Value, is_error: bool) {
    let mut tool = self.tool.as_object().expect("tool rows are objects").clone();
    tool.insert("output".to_string(), output);
    tool.insert("is_error".to_string(), json!(is_error));
    self.tool = tool_preview(Value::Object(tool));
  }

  pub fn subagent_mut(&mut self) -> Option<&mut Subagent> {
    self.subagent.as_mut()
  }

  fn is_subagent_tool(&self) -> bool {
    self.tool.get("name").and_then(Value::as_str).is_some_and(|name| SUBAGENT_TOOL_NAMES.contains(&name))
  }

  fn stop_running(&mut self) -> bool {
    self.subagent.as_mut().is_some_and(Subagent::stop_running)
  }

  fn render(&self) -> Value {
    let mut tool = self.tool.clone();
    if let Some(subagent) = &self.subagent {
      let object = tool.as_object_mut().expect("tool rows are objects");
      // A subagent is attached only to a row found by its tool_use id, so the row has an id.
      let id = self.id.as_deref().expect("a subagent hangs under a row that has a tool_use id");
      object.insert("id".to_string(), json!(id));
      object.insert("subagent".to_string(), subagent.render());
    }
    tool
  }
}

/// A committed draft that holds a subagent tool row. Later subagent events update it by id.
#[derive(Clone)]
struct CommittedDraft {
  message: Value,
  rows: Vec<ToolRow>,
}

impl CommittedDraft {
  fn stop_running(&mut self) -> bool {
    let mut changed = false;
    for row in &mut self.rows {
      changed |= row.stop_running();
    }
    changed
  }

  /// The committed message with its rows as they are now, under the `message_update` frame type.
  fn update_frame(&self) -> Value {
    let mut message = self.message.clone();
    message["tools"] = Value::Array(self.rows.iter().map(ToolRow::render).collect());
    json!({"type": "message_update", "message": message})
  }
}

/// The tool rows that the aggregator holds: the open draft and the committed drafts that subagents can still update.
#[derive(Clone, Default)]
pub struct ToolRows {
  draft: Vec<ToolRow>,
  committed: Vec<CommittedDraft>,
}

impl ToolRows {
  pub fn is_draft_empty(&self) -> bool {
    self.draft.is_empty()
  }

  /// Whether the open draft has a row that a result without a subagent target attaches to.
  pub fn has_plain_row(&self) -> bool {
    self.draft.iter().any(|row| row.subagent.is_none())
  }

  /// Whether a row with this tool_use id has a subagent under it, in the open draft or a committed one.
  pub fn holds_subagent(&self, id: &str) -> bool {
    find_committed_or_draft(&self.draft, &self.committed, id).is_some_and(|row| row.subagent.is_some())
  }

  pub fn push(&mut self, row: ToolRow) {
    self.draft.push(row);
  }

  /// Attach a result to the newest row of the open draft that has no subagent. The caller checks that one exists.
  pub fn attach_plain(&mut self, output: Value, is_error: bool) {
    self
      .draft
      .iter_mut()
      .rev()
      .find(|row| row.subagent.is_none())
      .expect("the caller checked that the draft has a plain row")
      .attach_output(output, is_error);
  }

  /// Change the row with this tool_use id. Returns the update frames of a committed draft that changed; the open
  /// draft changes in place and is rendered by the caller's next frame.
  pub fn update_row<F: FnOnce(&mut ToolRow)>(&mut self, id: &str, change: F) -> Vec<Value> {
    if let Some(row) = find_mut(&mut self.draft, id) {
      change(row);
      return Vec::new();
    }
    for draft in &mut self.committed {
      if let Some(row) = find_mut(&mut draft.rows, id) {
        change(row);
        return vec![draft.update_frame()];
      }
    }
    Vec::new()
  }

  /// Change the subagent of the row with this tool_use id, starting one if the row has none yet.
  pub fn update_subagent<F: FnOnce(&mut Subagent)>(&mut self, id: &str, change: F) -> Vec<Value> {
    self.update_row(id, |row| change(row.subagent.get_or_insert_with(Subagent::new)))
  }

  /// Stop the subagents that are still running at the end of a run, in the open draft and in committed drafts.
  /// Returns the update frames of the committed drafts that changed.
  pub fn end_run(&mut self) -> Vec<Value> {
    for row in &mut self.draft {
      row.stop_running();
    }
    let mut frames = Vec::new();
    for draft in &mut self.committed {
      if draft.stop_running() {
        frames.push(draft.update_frame());
      }
    }
    frames
  }

  /// Stop keeping committed drafts. Called once a run has ended, so no later event can name their rows.
  pub fn forget_committed(&mut self) {
    self.committed.clear();
  }

  /// Move the open draft's rows out at its commit. The draft keeps them only when a subagent tool is among them.
  pub fn commit_draft(&mut self, message: &Value) {
    let rows = std::mem::take(&mut self.draft);
    if rows.iter().any(ToolRow::is_subagent_tool) {
      let mut message = message.clone();
      message.as_object_mut().expect("a draft message is an object").remove("tools");
      self.committed.push(CommittedDraft { message, rows });
    }
  }

  /// The rendered rows of the open draft, or nothing when it has none.
  pub fn draft_json(&self) -> Option<Value> {
    (!self.draft.is_empty()).then(|| Value::Array(self.draft.iter().map(ToolRow::render).collect()))
  }
}

fn find_mut<'a>(rows: &'a mut [ToolRow], id: &str) -> Option<&'a mut ToolRow> {
  for row in rows.iter_mut() {
    if row.id.as_deref() == Some(id) {
      return Some(row);
    }
    if let Some(subagent) = row.subagent.as_mut()
      && let Some(found) = find_mut(&mut subagent.tools, id)
    {
      return Some(found);
    }
  }
  None
}

fn find<'a>(rows: &'a [ToolRow], id: &str) -> Option<&'a ToolRow> {
  for row in rows {
    if row.id.as_deref() == Some(id) {
      return Some(row);
    }
    if let Some(subagent) = &row.subagent
      && let Some(found) = find(&subagent.tools, id)
    {
      return Some(found);
    }
  }
  None
}

fn find_committed_or_draft<'a>(draft: &'a [ToolRow], committed: &'a [CommittedDraft], id: &str) -> Option<&'a ToolRow> {
  find(draft, id).or_else(|| committed.iter().find_map(|draft| find(&draft.rows, id)))
}

/// Preview a tool row's fields: long output and long input are cut to a preview and marked as truncated.
pub fn tool_preview(tool: Value) -> Value {
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

fn input_read_fields(name: Option<&Value>, input: &serde_json::Map<String, Value>) -> Vec<String> {
  match name.and_then(Value::as_str) {
    Some("Bash" | "bash") => vec!["command".to_string()],
    Some(READ_TOOL | EDIT_TOOL | WRITE_TOOL) => vec![TOOL_INPUT_FILE_PATH.to_string()],
    Some(GLOB_TOOL) => vec!["pattern".to_string()],
    Some(GREP_TOOL) => vec!["pattern".to_string(), "path".to_string()],
    _ => input.keys().next().cloned().into_iter().collect(),
  }
}
