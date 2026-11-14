//! The codex event translator: one `codex exec --json` event becomes the runtime's events.
//!
//! The translator holds the run's state: the text each item has emitted so far, so a cumulative item yields only its
//! new part.

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_core::str_methods;
use charliebot_core::text_repr::{DEFAULT_JSON, json_dumps, value_str};
use charliebot_core::web_search::TOOL_NAME;
use charliebot_registry::backend::Translate;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use super::pricing;
use super::text::{count_or_zero, empty_object, is_truthy};
use super::usage_fields;
use charliebot_agents::event_builders::{
  make_error_event, make_result_event, make_system_message_event, make_text_event, make_tool_result_event,
  make_tool_use_event,
};

/// The event type the translation emits for each file a `file_change` item adds or updates. The wire value is
/// persisted in events.jsonl.
pub const FILE_WRITE: &str = "file_write";

/// Codex item types that translate to one fixed tool event: (item type, (tool name, payload field carrying the input,
/// whether the completed output is turned into text)). The output is `aggregated_output` first (codex-cli 0.157.0
/// writes a command's output there), and `output` when that key is absent or null.
const TOOL_ITEM_SPECS: &[(&str, (&str, &str, bool))] =
  &[("command_execution", ("Bash", "command", false)), ("web_search", (TOOL_NAME, "query", true))];

/// Translates one codex run's events. A fresh translator starts a run.
pub struct CodexTranslator {
  model: String,
  /// The thread id `thread.started` named. Each result of the run names it as its native session.
  thread_id: Option<String>,
  last_agent_text: HashMap<ItemKey, String>,
  last_reasoning_text: HashMap<ItemKey, String>,
  last_todo_text: HashMap<ItemKey, String>,
}

/// The keys an item's `id` field can take: null, a boolean (which shares the key of 0 or 1), a number, or a string.
/// An array or an object fails the item's cache access.
#[derive(Eq, Hash, PartialEq)]
enum ItemKey {
  Null,
  Integer(i128),
  Float(u64),
  String(String),
}

impl CodexTranslator {
  pub fn new(model: &str) -> Self {
    Self {
      model: model.to_string(),
      thread_id: None,
      last_agent_text: HashMap::new(),
      last_reasoning_text: HashMap::new(),
      last_todo_text: HashMap::new(),
    }
  }

  /// Translates one event. An event whose shape the translator rejects fails here.
  pub fn translate_event(&mut self, event: &Value) -> anyhow::Result<Vec<Value>> {
    let Some(event) = event.as_object() else {
      bail!("codex event is not a JSON object: {event}");
    };
    let event_type = text_of(event, "type").unwrap_or("");
    match event_type {
      "thread.started" => {
        let thread_id = event.get("thread_id").cloned().unwrap_or_else(|| json!(""));
        self.thread_id = thread_id.as_str().filter(|id| !id.is_empty()).map(str::to_string);
        Ok(vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": thread_id})])
      }
      "turn.started" => Ok(Vec::new()),
      "turn.completed" => self.turn_completed(event),
      "turn.failed" | "error" => Ok(vec![make_error_event(&failure_message(event_type, event))]),
      "item.started" | "item.updated" | "item.completed" => self.translate_item(event),
      _ => {
        tracing::debug!(target: "charliebot_agents", r#type = event_type, "codex_event_unhandled");
        Ok(Vec::new())
      }
    }
  }

  fn turn_completed(&self, event: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    let usage = match event.get("usage") {
      None => empty_object(),
      Some(Value::Object(usage)) => usage,
      Some(other) => bail!("codex turn.completed usage must be a JSON object, got {other}"),
    };
    let Some(thread_id) = self.thread_id.as_deref() else {
      bail!("codex turn.completed arrived before thread.started named its thread");
    };
    let cost = pricing::calculate_codex_usage_cost_usd(&self.model, usage)?;
    let mut result = make_result_event(
      count_or_zero(usage, usage_fields::INPUT_TOKENS)?,
      count_or_zero(usage, usage_fields::OUTPUT_TOKENS)?,
      count_or_zero(usage, usage_fields::CACHED_INPUT_TOKENS)?,
      0,
      json!(cost),
      None,
    );
    // The usage of turn.completed accumulates over the thread's runs, so the usage fold reads the thread's running
    // total from the thread id.
    result[event_types::NATIVE_SESSION_ID] = json!(thread_id);
    Ok(vec![result])
  }

  fn translate_item(&mut self, event: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    let item = item_of(event)?;
    // Each handler checks its own item type, so the handlers run in order and their results concatenate.
    let mut results = self.agent_message(item)?;
    results.extend(self.reasoning(item)?);
    results.extend(self.tool_item(event, item));
    results.extend(file_change(event, item)?);
    results.extend(mcp_tool_call(event, item));
    results.extend(self.todo_list(item)?);
    results.extend(error_item(event, item));
    Ok(results)
  }

  fn agent_message(&mut self, item: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    if text_of(item, "type") != Some("agent_message") {
      return Ok(Vec::new());
    }
    let item_id = item_key(item)?;
    // Newer codex emits `text`; the older schema carried `content[].text`.
    let full_text = match item.get("text") {
      Some(value) if is_truthy(value) => {
        value.as_str().context("codex agent_message text must be a string")?.to_string()
      }
      _ => content_text(item)?,
    };
    if full_text.is_empty() {
      return Ok(Vec::new());
    }
    let previous_len = self.last_agent_text.get(&item_id).map_or(0, |previous| str_methods::code_point_len(previous));
    let delta = str_methods::slice_from(&full_text, previous_len);
    self.last_agent_text.insert(item_id, full_text);
    if delta.is_empty() {
      return Ok(Vec::new());
    }
    Ok(vec![make_text_event(&delta)])
  }

  fn reasoning(&mut self, item: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    if text_of(item, "type") != Some("reasoning") {
      return Ok(Vec::new());
    }
    // Newer codex emits `text`; the older schema carried the `summary_text` parts of `summary`.
    let mut text = match item.get("text") {
      Some(value) if is_truthy(value) => value.as_str().context("codex reasoning text must be a string")?.to_string(),
      _ => String::new(),
    };
    if text.is_empty()
      && let Some(summary) = item.get("summary").and_then(Value::as_array)
    {
      for part in summary.iter().filter_map(Value::as_object) {
        if text_of(part, "type") == Some("summary_text") {
          text.push_str(text_of(part, "text").unwrap_or(""));
        }
      }
    }
    if text.is_empty() {
      return Ok(Vec::new());
    }
    let item_id = item_key(item)?;
    let previous_len =
      self.last_reasoning_text.get(&item_id).map_or(0, |previous| str_methods::code_point_len(previous));
    let delta = str_methods::slice_from(&text, previous_len);
    self.last_reasoning_text.insert(item_id, text);
    if delta.is_empty() {
      return Ok(Vec::new());
    }
    Ok(vec![json!({"type": event_types::THINKING, "content": delta})])
  }

  fn tool_item(&self, event: &Map<String, Value>, item: &Map<String, Value>) -> Vec<Value> {
    let Some((_, (tool, payload_field, stringify_output))) =
      TOOL_ITEM_SPECS.iter().find(|(kind, _)| text_of(item, "type") == Some(*kind))
    else {
      return Vec::new();
    };
    match text_of(event, "type") {
      Some("item.started") => {
        let mut input = Map::new();
        input.insert((*payload_field).to_string(), item.get(*payload_field).cloned().unwrap_or_else(|| json!("")));
        vec![make_tool_use_event(tool, Value::Object(input))]
      }
      Some("item.completed") => {
        let output = match item.get("aggregated_output") {
          Some(value) if !value.is_null() => value.clone(),
          _ => item.get("output").cloned().unwrap_or_else(|| json!("")),
        };
        if *stringify_output {
          vec![make_tool_result_event(tool, &value_str(&output))]
        } else {
          vec![json!({"type": event_types::TOOL_RESULT, event_types::TOOL_NAME: tool, "content": output})]
        }
      }
      _ => Vec::new(),
    }
  }

  fn todo_list(&mut self, item: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    if text_of(item, "type") != Some("todo_list") {
      return Ok(Vec::new());
    }
    let todos: &[Value] = match item.get("items") {
      None => &[],
      Some(Value::Array(todos)) => todos,
      Some(_) => return Ok(Vec::new()),
    };
    let mut lines = Vec::new();
    for todo in todos.iter().filter_map(Value::as_object) {
      let label = todo_label(todo);
      if !label.is_empty() {
        lines.push(format!("- {} {label}", todo_marker(todo)));
      }
    }
    let item_id_is_truthy = item.get("id").is_some_and(is_truthy);
    if lines.is_empty() {
      if item_id_is_truthy {
        self.last_todo_text.remove(&item_key(item)?);
      }
      return Ok(Vec::new());
    }
    let text = lines.join("\n");
    if item_id_is_truthy {
      let item_id = item_key(item)?;
      if self.last_todo_text.get(&item_id) == Some(&text) {
        return Ok(Vec::new());
      }
      self.last_todo_text.insert(item_id, text.clone());
    }
    Ok(vec![make_text_event(&text)])
  }
}

impl Translate for CodexTranslator {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    self.translate_event(&raw)
  }
}

fn text_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
  map.get(key).and_then(Value::as_str)
}

/// The item's key in the delta caches. An item without an id shares the empty-string key.
fn item_key(item: &Map<String, Value>) -> anyhow::Result<ItemKey> {
  match item.get("id") {
    None => Ok(ItemKey::String(String::new())),
    Some(Value::Null) => Ok(ItemKey::Null),
    Some(Value::Bool(value)) => Ok(ItemKey::Integer(i128::from(u8::from(*value)))),
    Some(Value::Number(number)) => {
      if let Some(value) = number.as_i64() {
        return Ok(ItemKey::Integer(i128::from(value)));
      }
      if let Some(value) = number.as_u64() {
        return Ok(ItemKey::Integer(i128::from(value)));
      }
      let value = number.as_f64().context("codex item id number is not finite")?;
      if value.fract() == 0.0 && value >= i128::MIN as f64 && value < i128::MAX as f64 {
        Ok(ItemKey::Integer(value as i128))
      } else {
        let normalized = if value == 0.0 { 0.0 } else { value };
        Ok(ItemKey::Float(normalized.to_bits()))
      }
    }
    Some(Value::String(value)) => Ok(ItemKey::String(value.clone())),
    Some(value @ (Value::Array(_) | Value::Object(_))) => {
      bail!("codex item id is not a hashable Python dictionary key: {value}")
    }
  }
}

fn item_of(event: &Map<String, Value>) -> anyhow::Result<&Map<String, Value>> {
  match event.get("item") {
    None => Ok(empty_object()),
    Some(Value::Object(item)) => Ok(item),
    Some(other) => bail!("codex item must be a JSON object, got {other}"),
  }
}

/// The text of the older `content[]` schema: the `text` of each text part, in order.
fn content_text(item: &Map<String, Value>) -> anyhow::Result<String> {
  let Some(parts) = item.get("content").and_then(Value::as_array) else {
    return Ok(String::new());
  };
  let mut text = String::new();
  for part in parts.iter().filter_map(Value::as_object) {
    if text_of(part, "type") != Some("text") {
      continue;
    }
    match part.get("text") {
      None => {}
      Some(Value::String(piece)) => text.push_str(piece),
      Some(other) => bail!("codex content text must be a string, got {other}"),
    }
  }
  Ok(text)
}

fn file_change(event: &Map<String, Value>, item: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
  if text_of(event, "type") != Some("item.completed") || text_of(item, "type") != Some("file_change") {
    return Ok(Vec::new());
  }
  let changes = item.get("changes").and_then(Value::as_array).context("codex file_change item has no changes list")?;
  let mut writes = Vec::new();
  for change in changes {
    let change = change.as_object().context("codex file_change change must be a JSON object")?;
    let kind = change.get("kind").context("codex file_change change has no kind")?;
    if !matches!(kind.as_str(), Some("add" | "update")) {
      continue;
    }
    let path = change.get("path").context("codex file_change change has no path")?;
    writes.push(json!({"type": FILE_WRITE, "path": path}));
  }
  Ok(writes)
}

fn mcp_tool_call(event: &Map<String, Value>, item: &Map<String, Value>) -> Vec<Value> {
  if text_of(item, "type") != Some("mcp_tool_call") {
    return Vec::new();
  }
  let server = item.get("server_label").cloned().unwrap_or_else(|| json!(""));
  let tool = item.get("name").cloned().unwrap_or_else(|| json!(""));
  let tool_name = if is_truthy(&server) {
    format!("mcp:{}/{}", value_str(&server), value_str(&tool))
  } else {
    format!("mcp:{}", value_str(&tool))
  };
  match text_of(event, "type") {
    Some("item.started") => {
      let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
      let arguments = match arguments {
        Value::String(raw) => match serde_json::from_str::<Value>(&raw) {
          Ok(parsed) => parsed,
          Err(_) => {
            tracing::warn!(target: "charliebot_agents", raw_arguments = %raw, "codex_malformed_tool_args");
            json!({"raw": raw})
          }
        },
        other => other,
      };
      vec![make_tool_use_event(&tool_name, arguments)]
    }
    Some("item.completed") => {
      let output = match item.get("result") {
        Some(result) => result.clone(),
        None => item.get("error").cloned().unwrap_or_else(|| json!("")),
      };
      vec![make_tool_result_event(&tool_name, &value_str(&output))]
    }
    _ => Vec::new(),
  }
}

/// The first non-empty label of a todo, across the old and current schemas.
fn todo_label(todo: &Map<String, Value>) -> String {
  for key in ["text", "label", "content", "step"] {
    if let Some(value) = text_of(todo, key) {
      let stripped = str_methods::strip(value);
      if !stripped.is_empty() {
        return stripped.to_string();
      }
    }
  }
  String::new()
}

fn todo_marker(todo: &Map<String, Value>) -> &'static str {
  match todo.get("status") {
    Some(Value::String(status)) => match status.as_str() {
      "completed" => "[x]",
      "in_progress" => "[~]",
      _ => "[ ]",
    },
    _ if todo.get("completed") == Some(&Value::Bool(true)) => "[x]",
    _ => "[ ]",
  }
}

/// An item-level error is a non-fatal notice the turn continues past. It becomes a `system` event and stays out of
/// the run's failure-cause selection, which reads only `error` events. A top-level `error` or `turn.failed` is a turn
/// failure and keeps its `error` event.
fn error_item(event: &Map<String, Value>, item: &Map<String, Value>) -> Vec<Value> {
  if text_of(item, "type") != Some(event_types::ERROR) {
    return Vec::new();
  }
  let message = match item.get("message") {
    Some(message) if is_truthy(message) => value_str(message),
    _ => format!("Codex item error with no message. Full event: {}", full_event(event)),
  };
  vec![make_system_message_event(&message)]
}

/// The message of a `turn.failed` or top-level `error` event: the error's message, else the event's own, else the
/// event.
fn failure_message(event_type: &str, event: &Map<String, Value>) -> String {
  let error = event.get("error").cloned().unwrap_or_else(|| json!({}));
  let mut message = match &error {
    Value::Object(error) => error.get("message").cloned().unwrap_or(Value::Null),
    other => Value::String(value_str(other)),
  };
  if !is_truthy(&message) {
    message = event.get("message").cloned().unwrap_or(Value::Null);
  }
  if is_truthy(&message) {
    return value_str(&message);
  }
  format!("Codex {event_type} with no message. Full event: {}", full_event(event))
}

/// The event as JSON text, with spaces after separators and non-ASCII escaped: the text the logs and messages carry.
fn full_event(event: &Map<String, Value>) -> String {
  json_dumps(&Value::Object(event.clone()), &DEFAULT_JSON)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cumulative_agent_text_emits_only_the_new_part() {
    let mut translator = CodexTranslator::new("gpt-5.5");
    let first = translator
      .translate_event(&json!({"type": "item.updated", "item": {"id": "m1", "type": "agent_message", "text": "héllo"}}))
      .unwrap();
    let second = translator
      .translate_event(
        &json!({"type": "item.completed", "item": {"id": "m1", "type": "agent_message", "text": "héllo wörld"}}),
      )
      .unwrap();
    assert_eq!(first, [make_text_event("héllo")]);
    assert_eq!(second, [make_text_event(" wörld")]);
  }

  #[test]
  fn non_string_item_ids_keep_distinct_dictionary_keys() {
    let mut translator = CodexTranslator::new("gpt-5.5");
    let integer =
      |text: &str| json!({"type": "item.updated", "item": {"id": 7, "type": "agent_message", "text": text}});
    let string =
      |text: &str| json!({"type": "item.updated", "item": {"id": "7", "type": "agent_message", "text": text}});

    assert_eq!(translator.translate_event(&integer("integer")).unwrap(), [make_text_event("integer")]);
    assert_eq!(translator.translate_event(&string("string")).unwrap(), [make_text_event("string")]);
    assert_eq!(translator.translate_event(&integer("integer updated")).unwrap(), [make_text_event(" updated")]);
    assert_eq!(translator.translate_event(&string("string updated")).unwrap(), [make_text_event(" updated")]);
    assert!(
      translator
        .translate_event(&json!({"type": "item.updated", "item": {"id": [], "type": "agent_message", "text": "x"}}))
        .is_err()
    );
  }

  #[test]
  fn todo_list_repeats_are_suppressed_until_the_list_changes() {
    let mut translator = CodexTranslator::new("gpt-5.5");
    let todo = |status: &str| {
      json!({"type": "item.updated", "item": {"id": "t1", "type": "todo_list", "items": [
        {"text": " write tests ", "status": status}, {"label": "ship", "completed": true}
      ]}})
    };
    let first = translator.translate_event(&todo("in_progress")).unwrap();
    assert_eq!(first, [make_text_event("- [~] write tests\n- [x] ship")]);
    assert!(translator.translate_event(&todo("in_progress")).unwrap().is_empty());
    assert_eq!(
      translator.translate_event(&todo("completed")).unwrap(),
      [make_text_event("- [x] write tests\n- [x] ship")]
    );
  }

  #[test]
  fn failures_fall_back_from_error_message_to_event_message_to_the_event() {
    let mut translator = CodexTranslator::new("gpt-5.5");
    let nested = translator.translate_event(&json!({"type": "turn.failed", "error": {"message": "quota"}})).unwrap();
    assert_eq!(nested, [make_error_event("quota")]);
    let top = translator.translate_event(&json!({"type": "error", "message": "boom"})).unwrap();
    assert_eq!(top, [make_error_event("boom")]);
    let bare = translator.translate_event(&json!({"type": "error"})).unwrap();
    assert_eq!(bare[0]["message"], "Codex error with no message. Full event: {\"type\": \"error\"}");
  }

  /// The probe line of one real codex-cli 0.164.0 run under `--dangerously-bypass-hook-trust`: the notice precedes
  /// `turn.started` and the turn continues.
  #[test]
  fn an_item_level_error_becomes_one_system_notice_and_no_error_event() {
    let message =
      "`--dangerously-bypass-hook-trust` is enabled. Enabled hooks may run without review for this invocation.";
    let event = json!({"type": "item.completed", "item": {"id": "item_0", "type": "error", "message": message}});
    let mut translator = CodexTranslator::new("gpt-5.5");
    assert_eq!(translator.translate_event(&event).unwrap(), [make_system_message_event(message)]);
  }

  #[test]
  fn command_output_prefers_aggregated_output_and_keeps_its_shape() {
    let mut translator = CodexTranslator::new("gpt-5.5");
    let completed = translator
      .translate_event(&json!({"type": "item.completed", "item": {"type": "command_execution",
        "aggregated_output": null, "output": ["raw"]}}))
      .unwrap();
    assert_eq!(completed, [json!({"type": "tool_result", "tool_name": "Bash", "content": ["raw"]})]);
  }

  #[test]
  fn trait_translation_returns_every_shape_error_without_panicking() {
    let invalid_events = [
      json!(null),
      json!({"type": "turn.completed", "usage": []}),
      json!({"type": "turn.completed", "usage": {"input_tokens": "many"}}),
      json!({"type": "item.updated", "item": []}),
      json!({"type": "item.updated", "item": {"type": "agent_message", "text": 5}}),
      json!({"type": "item.updated", "item": {"type": "agent_message", "id": [], "text": "hello"}}),
      json!({"type": "item.updated", "item": {"type": "agent_message", "content": [{"type": "text", "text": 5}]}}),
      json!({"type": "item.updated", "item": {"type": "reasoning", "text": true}}),
      json!({"type": "item.updated", "item": {"type": "reasoning", "id": {}, "text": "thinking"}}),
      json!({"type": "item.completed", "item": {"type": "file_change"}}),
      json!({"type": "item.completed", "item": {"type": "file_change", "changes": [null]}}),
      json!({"type": "item.completed", "item": {"type": "file_change", "changes": [{"kind": "add"}]}}),
      json!({"type": "item.updated", "item": {"type": "todo_list", "id": [1], "items": [{"text": "task"}]}}),
    ];
    for event in invalid_events {
      let mut translator = CodexTranslator::new("gpt-5.5");
      assert!(translator.translate(event).is_err());
    }
  }
}
