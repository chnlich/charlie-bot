//! The gemini stream-json translator (`GeminiCliBackend.translate_event`). Assistant text accumulates until a non-delta
//! message, or a tool, error or result event, flushes it as one text event.

use anyhow::bail;
use charliebot_core::event_types;
use charliebot_core::pyfmt::python_str;
use charliebot_registry::backend::Translate;
use serde_json::{Map, Value, json};

use crate::cli_base::{int_field, str_field, truthy};
use crate::event_builders::{
  make_error_event, make_result_event, make_text_event, make_tool_result_event, make_tool_use_event,
};

/// One gemini stream's translator. It holds the assistant text that the stream has not flushed yet.
#[derive(Default)]
pub struct GeminiTranslator {
  text_buffer: String,
}

impl GeminiTranslator {
  /// Translates one stream-json event. A type this translator does not handle flushes the buffer, as Python's fallthrough.
  pub fn translate_event(&mut self, event: &Value) -> anyhow::Result<Vec<Value>> {
    let Some(event) = event.as_object() else {
      bail!("gemini event is not a JSON object: {event}");
    };
    match event.get("type").and_then(Value::as_str) {
      Some("init") => {
        let session_id = str_field(event, "session_id", "")?;
        Ok(vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": session_id})])
      }
      Some("message") => self.message(event),
      Some("tool_use") => {
        let mut events = self.flush();
        let input = event.get("parameters").cloned().unwrap_or_else(|| json!({}));
        events.push(make_tool_use_event(str_field(event, "tool_name", "")?, input));
        Ok(events)
      }
      Some("tool_result") => {
        let mut events = self.flush();
        let tool_id = str_field(event, "tool_id", "")?;
        match str_field(event, "status", "")? {
          "success" => events.push(make_tool_result_event(tool_id, str_field(event, "output", "")?)),
          "error" => {
            let error = event.get("error").cloned().unwrap_or_else(|| json!({}));
            events.push(make_tool_result_event(tool_id, &error_message(&error)?));
          }
          _ => {}
        }
        Ok(events)
      }
      Some("error") => {
        let mut events = self.flush();
        events.push(make_error_event(str_field(event, "message", "")?));
        Ok(events)
      }
      Some("result") => {
        let mut events = self.flush();
        let empty = Map::new();
        let stats = match event.get("stats") {
          None => &empty,
          Some(Value::Object(stats)) => stats,
          Some(other) => bail!("gemini result stats must be a JSON object, got {other}"),
        };
        events.push(make_result_event(
          int_field(stats, "input_tokens")?,
          int_field(stats, "output_tokens")?,
          int_field(stats, "cached")?,
          0,
          json!(0),
          None,
        ));
        Ok(events)
      }
      other => {
        tracing::debug!(target: "charliebot_agents", r#type = ?other, "gemini_event_unhandled");
        Ok(self.flush())
      }
    }
  }

  /// A user message is dropped without a flush. An assistant message joins the buffer, and a message without `delta`
  /// flushes it. Any other role falls through to the unhandled flush.
  fn message(&mut self, event: &Map<String, Value>) -> anyhow::Result<Vec<Value>> {
    match str_field(event, "role", "")? {
      "user" => Ok(Vec::new()),
      "assistant" => {
        self.text_buffer.push_str(str_field(event, "content", "")?);
        if event.get("delta").is_some_and(truthy) {
          return Ok(Vec::new());
        }
        Ok(self.flush())
      }
      _ => Ok(self.flush()),
    }
  }

  fn flush(&mut self) -> Vec<Value> {
    if self.text_buffer.is_empty() {
      return Vec::new();
    }
    vec![make_text_event(&std::mem::take(&mut self.text_buffer))]
  }
}

impl Translate for GeminiTranslator {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    self.translate_event(&raw)
  }
}

/// The text of a failed tool call: the `message` of an object that carries one, else the Python `str` of the error.
fn error_message(error: &Value) -> anyhow::Result<String> {
  match error {
    Value::Object(fields) => match fields.get("message") {
      None => Ok(python_str(error)),
      Some(Value::String(message)) => Ok(message.clone()),
      Some(other) => bail!("gemini tool error message must be a string, got {other}"),
    },
    other => Ok(python_str(other)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_event_type_maps_to_the_python_translation() {
    let mut translator = GeminiTranslator::default();
    let mut translate = |event: Value| translator.translate_event(&event).unwrap();
    assert_eq!(
      translate(json!({"type": "init", "session_id": "sid"})),
      vec![json!({"type": "session_attached", "session_id": "sid"})]
    );
    assert!(translate(json!({"type": "message", "role": "user", "content": "ignored"})).is_empty());
    assert_eq!(
      translate(json!({"type": "message", "role": "assistant", "content": "hello"})),
      vec![json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hello"}]}})]
    );
    assert_eq!(
      translate(json!({"type": "tool_use", "tool_name": "Bash", "parameters": {"command": "pwd"}})),
      vec![json!({"type": "tool_use", "name": "Bash", "input": {"command": "pwd"}})]
    );
    assert_eq!(
      translate(json!({"type": "tool_result", "status": "success", "tool_id": "Bash", "output": "/tmp"})),
      vec![json!({"type": "tool_result", "tool_name": "Bash", "content": "/tmp"})]
    );
    assert_eq!(
      translate(json!({"type": "error", "message": "boom"})),
      vec![json!({"type": "error", "message": "boom", "content": "boom"})]
    );
    assert_eq!(
      translate(json!({"type": "result", "stats": {"input_tokens": 10, "output_tokens": 5, "cached": 3}})),
      vec![json!({
        "type": "result",
        "result": "",
        "usage": {
          "input_tokens": 10,
          "output_tokens": 5,
          "cache_read_input_tokens": 3,
          "cache_creation_input_tokens": 0
        },
        "total_cost_usd": 0
      })]
    );
  }

  #[test]
  fn delta_messages_join_the_buffer_until_a_flushing_event() {
    let mut translator = GeminiTranslator::default();
    assert!(
      translator
        .translate_event(&json!({"type": "message", "role": "assistant", "content": "Hel", "delta": true}))
        .unwrap()
        .is_empty()
    );
    let events =
      translator.translate_event(&json!({"type": "tool_use", "tool_name": "Read", "parameters": {}})).unwrap();
    assert_eq!(events[0], json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hel"}]}}));
    assert_eq!(events[1]["name"], "Read");
  }

  #[test]
  fn a_failed_tool_without_a_message_reports_the_python_str_of_its_error() {
    let mut translator = GeminiTranslator::default();
    let events = translator
      .translate_event(&json!({"type": "tool_result", "status": "error", "tool_id": "Bash", "error": {"code": 7}}))
      .unwrap();
    assert_eq!(events, vec![json!({"type": "tool_result", "tool_name": "Bash", "content": "{'code': 7}"})]);
  }
}
