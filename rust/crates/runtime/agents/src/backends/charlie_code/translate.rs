//! The charlie-code NDJSON translator (`CharlieCodeBackend.translate_event`). Each event maps to its CC event. The
//! translator keeps each command's text by id, so a progress ping can name the command it reports on.

use std::collections::HashMap;

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_core::pyfmt::{format_g, python_str};
use charliebot_core::pystr;
use charliebot_registry::backend::Translate;
use serde_json::{Map, Value, json};

use crate::cli_base::{int_field, required_str, str_field, truthy};
use crate::event_builders::{
  make_compact_boundary_event, make_context_reading_event, make_error_event, make_result_event, make_text_event,
  make_tool_result_event, make_tool_use_event,
};

/// The most characters of a command that a progress note quotes.
const COMMAND_NOTE_CHARS: usize = 80;

/// One charlie-code stream's translator.
#[derive(Default)]
pub struct CharlieCodeTranslator {
  commands_by_id: HashMap<String, String>,
}

impl CharlieCodeTranslator {
  pub fn translate_event(&mut self, event: &Value) -> anyhow::Result<Vec<Value>> {
    let Some(event) = event.as_object() else {
      bail!("charlie-code event is not a JSON object: {event}");
    };
    match event.get("type").and_then(Value::as_str) {
      Some("session") => {
        Ok(vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": required_str(event, "session_id")?})])
      }
      Some("thought") => Ok(vec![make_text_event(required_str(event, "text")?)]),
      Some("command") => self.command(event),
      Some("command_progress") => self.command_progress(event),
      Some("observation") => {
        let mut translated = make_tool_result_event("Bash", str_field(event, "output", "")?);
        translated["tool_use_id"] = json!(required_str(event, "id")?);
        Ok(vec![translated])
      }
      Some("result") => {
        let empty = Map::new();
        let usage = match event.get("usage") {
          None => &empty,
          Some(Value::Object(usage)) => usage,
          Some(other) => bail!("charlie-code result usage must be a JSON object, got {other}"),
        };
        Ok(vec![make_result_event(
          int_field(usage, "input_tokens")?,
          int_field(usage, "output_tokens")?,
          0,
          0,
          Value::Null,
          None,
        )])
      }
      Some("error") => Ok(vec![make_error_event(str_field(event, "message", "")?)]),
      Some("compact") => {
        Ok(vec![make_compact_boundary_event(required_str(event, "trigger")?, nullable_int(event, "pre_tokens")?)])
      }
      Some("context") => Ok(vec![context_reading(event)?]),
      other => {
        tracing::debug!(target: "charliebot_agents", event_type = ?other, "charlie_code_unknown_event");
        Ok(Vec::new())
      }
    }
  }

  fn command(&mut self, event: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    let id = required_str(event, "id")?;
    let command = required_str(event, "command")?;
    self.commands_by_id.insert(id.to_string(), command.to_string());
    let mut translated = make_tool_use_event("Bash", json!({"command": command}));
    translated["id"] = json!(id);
    Ok(vec![translated])
  }

  fn command_progress(&self, event: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    let id = required_str(event, "id")?;
    let killed = event.get("killed").context("charlie-code command_progress event lacks killed")?;
    let state = if truthy(killed) { "terminated" } else { "still running" };
    let raw = self.commands_by_id.get(id);
    let suffix = match raw {
      Some(raw) => format!(": {}", command_note(raw)),
      None => {
        // A progress ping replayed after a server restart finds no registered command. Its note names no command.
        tracing::debug!(target: "charliebot_agents", id, "charlie_code_progress_unknown_command");
        String::new()
      }
    };
    let elapsed = required_number(event, "elapsed_seconds")?;
    let pid = event.get("pid").context("charlie-code command_progress event lacks pid")?;
    Ok(vec![json!({
      "type": event_types::SYSTEM,
      "subtype": event_types::COMMAND_PROGRESS,
      "content": format!(
        "Command {state} after {} (pid {}){suffix}",
        duration_label(elapsed),
        python_str(pid)
      ),
    })])
  }
}

impl Translate for CharlieCodeTranslator {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    self.translate_event(&raw)
  }
}

/// The `context_reading` event of a context event. Its model is the event's model, or "" when the model is falsy.
fn context_reading(event: &Map<String, Value>) -> anyhow::Result<Value> {
  let model = match event.get("model") {
    Some(Value::String(model)) => model.as_str(),
    Some(other) if truthy(other) => bail!("charlie-code context model must be a string, got {other}"),
    _ => "",
  };
  Ok(make_context_reading_event(
    model,
    context_reading_int(event_types::CONTEXT_TOKENS, event.get("prompt_tokens")),
    context_reading_int(event_types::CONTEXT_FULL, event.get("context_window")),
    context_reading_int(event_types::CONTEXT_COMPACT_AT, event.get("compact_threshold")),
  ))
}

/// One numeric context field. An integer passes, and null is a declared unknown. Any other value logs a warning and
/// becomes unknown, so a wrong number never reaches the panel.
fn context_reading_int(field: &str, value: Option<&Value>) -> Option<i64> {
  match value {
    None | Some(Value::Null) => None,
    Some(value) => match value.as_i64() {
      Some(number) => Some(number),
      None => {
        tracing::warn!(target: "charliebot_agents", field, value = %value, "charlie_code_context_reading_bad_field");
        None
      }
    },
  }
}

/// A key the event must carry, whose value is an integer or null (`event[key]`).
fn nullable_int(event: &Map<String, Value>, key: &str) -> anyhow::Result<Option<i64>> {
  match event.get(key) {
    None => bail!("charlie-code compact event lacks {key}"),
    Some(Value::Null) => Ok(None),
    Some(value) => {
      value.as_i64().map(Some).with_context(|| format!("event field {key} must be an integer or null, got {value}"))
    }
  }
}

/// A key the event must carry, whose value is a number (`event[key]`).
fn required_number(event: &Map<String, Value>, key: &str) -> anyhow::Result<f64> {
  match event.get(key) {
    None => bail!("charlie-code event lacks {key}"),
    Some(Value::Number(number)) => number.as_f64().with_context(|| format!("event field {key} is not a number")),
    Some(other) => bail!("event field {key} must be a number, got {other}"),
  }
}

/// `_duration_label`: progress ticks read in minutes from one minute up (`1.25 min`), else in seconds (`5 s`).
fn duration_label(seconds: f64) -> String {
  if seconds >= 60.0 { format!("{} min", format_g(seconds / 60.0)) } else { format!("{} s", format_g(seconds)) }
}

/// `" ".join(raw.split())[:80]`: the command with its whitespace runs collapsed, cut to its first 80 characters.
fn command_note(raw: &str) -> String {
  pystr::split_whitespace(raw).join(" ").chars().take(COMMAND_NOTE_CHARS).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_success_stream_keeps_the_tool_pair_ids_and_the_usage() {
    let mut translator = CharlieCodeTranslator::default();
    assert_eq!(
      translator.translate_event(&json!({"type": "command", "step": 1, "id": "s-1", "command": "pwd"})).unwrap(),
      vec![json!({"type": "tool_use", "name": "Bash", "input": {"command": "pwd"}, "id": "s-1"})]
    );
    assert_eq!(
      translator
        .translate_event(&json!({"type": "observation", "step": 1, "id": "s-1", "returncode": 0, "output": "/tmp\n"}))
        .unwrap(),
      vec![json!({"type": "tool_result", "tool_name": "Bash", "content": "/tmp\n", "tool_use_id": "s-1"})]
    );
    assert_eq!(
      translator
        .translate_event(
          &json!({"type": "result", "completed": true, "usage": {"input_tokens": 123, "output_tokens": 45}})
        )
        .unwrap(),
      vec![json!({
        "type": "result",
        "result": "",
        "usage": {
          "input_tokens": 123,
          "output_tokens": 45,
          "cache_read_input_tokens": 0,
          "cache_creation_input_tokens": 0
        },
        "total_cost_usd": null
      })]
    );
  }

  #[test]
  fn a_failure_keeps_the_error_message() {
    let mut translator = CharlieCodeTranslator::default();
    assert_eq!(
      translator.translate_event(&json!({"type": "error", "message": "rate limit: retry later"})).unwrap(),
      vec![json!({"type": "error", "message": "rate limit: retry later", "content": "rate limit: retry later"})]
    );
  }

  #[test]
  fn a_progress_note_quotes_its_command_in_minutes_or_seconds() {
    let mut translator = CharlieCodeTranslator::default();
    translator.translate_event(&json!({"type": "command", "id": "s-1", "command": "make   build\nall"})).unwrap();
    let note = translator
      .translate_event(
        &json!({"type": "command_progress", "id": "s-1", "killed": false, "elapsed_seconds": 75.0, "pid": 42}),
      )
      .unwrap()[0]["content"]
      .to_string();
    assert!(note.contains("1.25 min") && note.contains("(pid 42): make build all"), "{note}");
    let note = translator
      .translate_event(
        &json!({"type": "command_progress", "id": "gone", "killed": true, "elapsed_seconds": 5, "pid": 42}),
      )
      .unwrap()[0]["content"]
      .to_string();
    assert!(note.contains("5 s") && !note.contains(": "), "{note}");
  }

  #[test]
  fn a_context_field_that_is_not_an_integer_reads_as_unknown() {
    let mut translator = CharlieCodeTranslator::default();
    let events = translator
      .translate_event(&json!({
        "type": "context",
        "model": "m",
        "prompt_tokens": "bad",
        "context_window": 1000,
        "compact_threshold": null
      }))
      .unwrap();
    assert_eq!(
      events,
      vec![json!({
        "type": "system",
        "subtype": "context_reading",
        "context_reading": {
          "model": "m",
          "context_tokens": null,
          "context_full": 1000,
          "context_compact_at": null
        }
      })]
    );
  }
}
