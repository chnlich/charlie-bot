//! Chat event builders shared by backend streams and manager turns.

use serde_json::{Map, Value, json};

use charliebot_core::event_types;

pub fn make_text_event(text: &str) -> Value {
  json!({"type": event_types::ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}})
}

pub fn make_error_event(message: &str) -> Value {
  json!({"type": event_types::ERROR, "message": message, "content": message})
}

pub fn make_master_done_event(exit_code: i32, still_thinking: bool) -> Value {
  json!({
    "type": event_types::MASTER_DONE,
    "exit_code": exit_code,
    event_types::STILL_THINKING: still_thinking
  })
}

/// The result event of one call. `cost` is the JSON value of `total_cost_usd`: a float, null, or an integer such as the
/// `0` that Python's default carries, which a float would serialize as `0.0`.
pub fn make_result_event(
  input_tokens: i64,
  output_tokens: i64,
  cache_read: i64,
  cache_creation: i64,
  cost: Value,
  context_snapshot: Option<Value>,
) -> Value {
  let mut event = Map::from_iter([
    ("type".to_string(), json!(event_types::RESULT)),
    ("result".to_string(), json!("")),
    (
      "usage".to_string(),
      json!({
        event_types::USAGE_INPUT_TOKENS: input_tokens,
        event_types::USAGE_OUTPUT_TOKENS: output_tokens,
        event_types::USAGE_CACHE_READ_INPUT_TOKENS: cache_read,
        event_types::USAGE_CACHE_CREATION_INPUT_TOKENS: cache_creation
      }),
    ),
    (event_types::RESULT_TOTAL_COST_USD.to_string(), cost),
  ]);
  if let Some(snapshot) = context_snapshot {
    event.insert("context_snapshot".to_string(), snapshot);
  }
  Value::Object(event)
}

pub fn make_tool_use_event(name: &str, input: Value) -> Value {
  json!({"type": event_types::TOOL_USE, "name": name, "input": input})
}

pub fn make_tool_result_event(tool_name: &str, content: &str) -> Value {
  json!({"type": event_types::TOOL_RESULT, "tool_name": tool_name, "content": content})
}

pub fn make_context_reading_event(
  model: &str,
  context_tokens: Option<i64>,
  context_full: Option<i64>,
  context_compact_at: Option<i64>,
) -> Value {
  json!({
    "type": event_types::SYSTEM,
    "subtype": event_types::CONTEXT_READING,
    event_types::CONTEXT_READING: {
      "model": model,
      event_types::CONTEXT_TOKENS: context_tokens,
      event_types::CONTEXT_FULL: context_full,
      event_types::CONTEXT_COMPACT_AT: context_compact_at
    }
  })
}

pub fn make_compact_boundary_event(trigger: &str, pre_tokens: Option<i64>) -> Value {
  json!({
    "type": event_types::SYSTEM,
    "subtype": event_types::COMPACT_BOUNDARY,
    event_types::COMPACT_METADATA: {
      "trigger": trigger,
      event_types::COMPACT_PRE_TOKENS: pre_tokens
    }
  })
}

pub fn make_context_compacted_event(trigger: &str, compact_metadata: Option<Value>, model: Option<&str>) -> Value {
  let mut event = Map::from_iter([
    ("type".to_string(), json!(event_types::CONTEXT_COMPACTED)),
    ("trigger".to_string(), json!(trigger)),
  ]);
  if let Some(metadata) = compact_metadata {
    event.insert(event_types::COMPACT_METADATA.to_string(), metadata);
  }
  if let Some(model) = model {
    event.insert("model".to_string(), json!(model));
  }
  Value::Object(event)
}

pub fn make_context_compact_failed_event(error: Option<&str>, model: Option<&str>, trigger: Option<&str>) -> Value {
  let mut event = Map::from_iter([
    ("type".to_string(), json!(event_types::CONTEXT_COMPACT_FAILED)),
    ("error".to_string(), json!(error)),
  ]);
  if let Some(model) = model {
    event.insert("model".to_string(), json!(model));
  }
  if let Some(trigger) = trigger {
    event.insert("trigger".to_string(), json!(trigger));
  }
  Value::Object(event)
}
