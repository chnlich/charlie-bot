//! The event-fold portion of `session_usage.py`, without backend usage resolvers.

use charliebot_core::event_types;
use charliebot_registry::Registry;
use charliebot_registry::backend::ContextLimits;
use charliebot_store::{StoreError, StoreResult};
use serde_json::{Map, Value, json};

const READING_CLAUDE: &str = "claude";
const READING_SNAPSHOT: &str = "snapshot";
const READING_RESOLVED: &str = "resolved";

#[derive(Default)]
struct UsageFold {
  chosen_prompt_tokens: i64,
  chosen_model: String,
  post_compact_tokens: Option<i64>,
  model_windows: Map<String, Value>,
  snapshot: Option<Map<String, Value>>,
  reading_kind: String,
  reading: Option<Map<String, Value>>,
  total_cost: f64,
  unknown_cost: bool,
}

impl UsageFold {
  fn feed(&mut self, events: &[Value]) -> StoreResult<()> {
    for event in events {
      match event.get("type").and_then(Value::as_str) {
        Some(event_types::ASSISTANT) => self.feed_assistant(event)?,
        Some(event_types::SYSTEM) => self.feed_system(event)?,
        Some(event_types::RESULT) => self.feed_result(event)?,
        _ => {}
      }
    }
    Ok(())
  }

  fn feed_assistant(&mut self, event: &Value) -> StoreResult<()> {
    if truthy(event.get("parent_tool_use_id")) {
      return Ok(());
    }
    let message = truthy_object(event.get("message"), "assistant.message")?;
    let usage = truthy_object(message.get("usage"), "assistant.message.usage")?;
    let tokens = add_int_fields(
      usage,
      &[
        event_types::USAGE_INPUT_TOKENS,
        event_types::USAGE_CACHE_CREATION_INPUT_TOKENS,
        event_types::USAGE_CACHE_READ_INPUT_TOKENS,
      ],
    )?;
    if tokens > 0 {
      self.chosen_prompt_tokens = tokens;
      self.chosen_model =
        message.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
      self.post_compact_tokens = None;
      self.reading_kind = READING_CLAUDE.to_string();
    }
    Ok(())
  }

  fn feed_system(&mut self, event: &Value) -> StoreResult<()> {
    match event.get("subtype").and_then(Value::as_str) {
      Some(event_types::COMPACT_BOUNDARY) if self.reading_kind == READING_CLAUDE => {
        let compact = truthy_object(event.get(event_types::COMPACT_METADATA), "system.compact_metadata")?;
        if let Some(tokens) = compact.get(event_types::COMPACT_POST_TOKENS).and_then(python_integer) {
          self.post_compact_tokens = Some(tokens);
        }
      }
      Some(event_types::CONTEXT_READING) => {
        if let Some(reading) = event.get(event_types::CONTEXT_READING).and_then(Value::as_object) {
          self.reading_kind = READING_RESOLVED.to_string();
          self.reading = Some(reading.clone());
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn feed_result(&mut self, event: &Value) -> StoreResult<()> {
    let model_usage = truthy_object(event.get("modelUsage"), "result.modelUsage")?;
    for (model_name, info_value) in model_usage {
      let info = truthy_object(Some(info_value), "result.modelUsage entry")?;
      if let Some(window) = info.get("contextWindow").and_then(python_integer) {
        self.model_windows.insert(model_name.clone(), Value::from(window));
      }
    }
    if let Some(snapshot) = event.get("context_snapshot").and_then(Value::as_object) {
      self.snapshot = Some(snapshot.clone());
      self.reading_kind = READING_SNAPSHOT.to_string();
    }
    match event.get(event_types::RESULT_TOTAL_COST_USD) {
      Some(Value::Null) => self.unknown_cost = true,
      Some(value) => self.total_cost += python_number(value, "result.total_cost_usd")?,
      None => {}
    }
    Ok(())
  }

  fn cost(&self) -> Option<f64> {
    if self.unknown_cost || self.total_cost <= 0.0 {
      None
    } else {
      Some((self.total_cost * 10_000.0).round_ties_even() / 10_000.0)
    }
  }
}

/// Fold session events through the source tiers, without invoking a registered backend usage resolver.
pub fn fold_usage(events: &[Value], registry: &Registry) -> StoreResult<Option<Value>> {
  let mut fold = UsageFold::default();
  fold.feed(events)?;
  if events.is_empty() {
    return Ok(None);
  }
  let cost = fold.cost();
  let usage = match fold.reading_kind.as_str() {
    READING_CLAUDE => Some(resolve_claude(&fold, registry, cost)?),
    READING_SNAPSHOT => Some(resolve_snapshot(&fold, registry, cost)?),
    READING_RESOLVED => Some(resolve_reading(&fold, cost)),
    _ => Some(json!({
      "context_tokens": null,
      "context_full": null,
      "context_compact_at": null,
      "total_cost_usd": cost,
      "model": "",
    })),
  };
  Ok(usage)
}

fn resolve_claude(fold: &UsageFold, registry: &Registry, cost: Option<f64>) -> StoreResult<Value> {
  let context_tokens = fold.post_compact_tokens.unwrap_or(fold.chosen_prompt_tokens);
  let limits = registry.reading_limits_for(READING_CLAUDE);
  let (context_full, compact_at) = match limits {
    Some(limits) => {
      let declared = limits.declared_window.map(|value| value as i64);
      let model_window = fold.model_windows.get(&fold.chosen_model).and_then(python_integer);
      let full = match (model_window, declared) {
        (Some(model), Some(declared)) => Some(model.min(declared)),
        (_, declared) => declared,
      };
      (full, subtract_reserve(full, limits))
    }
    None => (None, None),
  };
  Ok(usage_value(Some(context_tokens), context_full, compact_at, &fold.chosen_model, cost))
}

fn resolve_snapshot(fold: &UsageFold, registry: &Registry, cost: Option<f64>) -> StoreResult<Value> {
  let snapshot = fold.snapshot.as_ref().expect("snapshot reading kind carries a snapshot");
  let tokens = truthy_object(snapshot.get("tokens"), "context_snapshot.tokens")?;
  let token_sum = add_int_fields(tokens, &["input", "output", "reasoning", "cache_read", "cache_write"])?;
  let (context_full, compact_at) = match snapshot.get("limit").and_then(Value::as_object) {
    Some(limit) => {
      let input = limit.get("input").and_then(python_integer);
      let output =
        limit.get("output").and_then(python_integer).or_else(|| (!limit.contains_key("output")).then_some(0));
      let context = limit.get("context").and_then(python_integer);
      match (input, output, context) {
        (Some(input), Some(output), _) => {
          let reserve = registry.reading_limits_for(READING_SNAPSHOT).and_then(|limits| limits.compact_reserve);
          let compact_at =
            reserve.map(|reserve| input - i64::try_from(reserve).expect("compact reserve fits i64").min(output));
          (Some(input), compact_at)
        }
        (_, Some(output), Some(context)) => (Some(context - output), None),
        _ => (None, None),
      }
    }
    None => (None, None),
  };
  let model =
    snapshot.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
  Ok(usage_value(Some(token_sum), context_full, compact_at, &model, cost))
}

fn resolve_reading(fold: &UsageFold, cost: Option<f64>) -> Value {
  let reading = fold.reading.as_ref().expect("resolved reading kind carries a reading");
  let number = |key: &str| {
    reading.get(key).and_then(|value| match value {
      Value::Bool(_) => None,
      value => python_integer(value),
    })
  };
  let model = reading.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
  usage_value(
    number(event_types::CONTEXT_TOKENS),
    number(event_types::CONTEXT_FULL),
    number(event_types::CONTEXT_COMPACT_AT),
    &model,
    cost,
  )
}

fn subtract_reserve(full: Option<i64>, limits: ContextLimits) -> Option<i64> {
  let reserve = i64::try_from(limits.compact_reserve?).ok()?;
  Some(full? - reserve)
}

fn usage_value(
  context_tokens: Option<i64>,
  context_full: Option<i64>,
  context_compact_at: Option<i64>,
  model: &str,
  cost: Option<f64>,
) -> Value {
  json!({
    "context_tokens": context_tokens,
    "context_full": context_full,
    "context_compact_at": context_compact_at,
    "total_cost_usd": cost,
    "model": model,
  })
}

fn truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(value)) => *value,
    Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(value)) => !value.is_empty(),
    Some(Value::Object(value)) => !value.is_empty(),
  }
}

fn truthy_object<'a>(value: Option<&'a Value>, field: &str) -> StoreResult<&'a Map<String, Value>> {
  if !truthy(value) {
    return Ok(empty_object());
  }
  value.and_then(Value::as_object).ok_or_else(|| StoreError::new(format!("{field} must be an object")))
}

fn empty_object() -> &'static Map<String, Value> {
  static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
  &EMPTY
}

fn python_integer(value: &Value) -> Option<i64> {
  match value {
    Value::Bool(value) => Some(i64::from(*value)),
    Value::Number(number) => number.as_i64().or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok())),
    _ => None,
  }
}

fn add_int_fields(object: &Map<String, Value>, fields: &[&str]) -> StoreResult<i64> {
  fields.iter().try_fold(0_i64, |sum, field| match object.get(*field) {
    None => Ok(sum),
    Some(value) => python_integer(value)
      .map(|value| sum + value)
      .ok_or_else(|| StoreError::new(format!("usage field {field} is not an integer"))),
  })
}

fn python_number(value: &Value, field: &str) -> StoreResult<f64> {
  match value {
    Value::Bool(value) => Ok(f64::from(u8::from(*value))),
    Value::Number(value) => {
      value.as_f64().ok_or_else(|| StoreError::new(format!("usage field {field} is not numeric")))
    }
    _ => Err(StoreError::new(format!("usage field {field} is not numeric"))),
  }
}

fn value_to_python_string(value: &Value) -> String {
  match value {
    Value::String(value) => value.clone(),
    Value::Null => "None".to_string(),
    Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
    _ => value.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn registry() -> Registry {
    Registry::default()
  }

  #[test]
  fn claude_tier_uses_assistant_event_tokens_not_result_cumulative() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3}}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 100}}, "total_cost_usd": 0.25}),
    ];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 15);
    assert_eq!(usage["model"], "model-a");
    assert_eq!(usage["total_cost_usd"], 0.25);
  }

  #[test]
  fn claude_tier_ignores_subagent_and_synthetic_assistant_events() {
    let events = vec![
      json!({"type": "assistant", "parent_tool_use_id": "tool-1", "message": {"model": "subagent", "usage": {"input_tokens": 100}}}),
      json!({"type": "assistant", "message": {"model": "empty", "usage": {"input_tokens": 0}}}),
      json!({"type": "assistant", "message": {"model": "main", "usage": {"input_tokens": 7}}}),
    ];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 7);
    assert_eq!(usage["model"], "main");
  }

  #[test]
  fn total_cost_sums_results_and_rounds_to_four_places() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.12345}), json!({"type": "result", "total_cost_usd": 0.00005})];
    assert_eq!(fold_usage(&events, &registry()).unwrap().unwrap()["total_cost_usd"], 0.1235);
  }

  #[test]
  fn context_reading_tier_beats_cumulative_result_usage() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 100}}}),
      json!({"type": "system", "subtype": "context_reading", "context_reading": {"context_tokens": 31, "context_full": 80, "context_compact_at": 70, "model": "resolved"}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 1000}}}),
    ];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 31);
    assert_eq!(usage["model"], "resolved");
  }

  #[test]
  fn empty_slot_keeps_context_unknown() {
    let events = vec![json!({"type": "user", "content": "hello"})];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert!(usage["context_tokens"].is_null());
    assert!(usage["context_full"].is_null());
    assert!(usage["context_compact_at"].is_null());
    assert_eq!(usage["model"], "");
  }

  #[test]
  fn empty_event_list_has_no_usage_value() {
    assert!(fold_usage(&[], &registry()).unwrap().is_none());
  }
}
