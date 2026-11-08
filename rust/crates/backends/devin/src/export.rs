//! The devin export: the trajectory JSON that `devin --export` writes after each turn, read into the runtime's events.
//!
//! This module is the one place that names the export's fields. The names come from the CLI binary's strings
//! (`ATIF-v1.x`), not from a Devin document. The names are `session_id`, `steps[]` with `source`, `message`,
//! `tool_calls[]`, `observation.results[]` and each agent step's `metrics`. A live export that disagrees with them is
//! fixed here.
//!
//! A compaction summary is the system step that devin writes when it compacts a long conversation. The summary
//! replaces the earlier steps, the current turn's user step included, so that turn's prompt lives only inside the
//! summary. The step is marked by `extra.telemetry` with `source` "compaction" and `operation` "summary", as the
//! exports of CLI 3000.12.17 (`ATIF-v1.7`) show. The agent steps devin keeps from before the compaction follow the
//! summary step.

use std::collections::HashMap;

use anyhow::{Context, bail};
use charliebot_agents::event_builders::{
  make_result_event, make_text_event, make_tool_result_event, make_tool_use_event,
};
use charliebot_core::event_types;
use serde::Deserialize;
use serde_json::{Value, json};

/// The `source` of a step the agent wrote. A user step's message is the prompt, so it becomes no text event.
const AGENT_SOURCE: &str = "agent";
/// The `source` of a step that carries a turn's prompt. Each turn's user step starts the steps that turn added, unless
/// a compaction summary replaced it.
const USER_SOURCE: &str = "user";
/// The telemetry `source` and `operation` of a compaction summary step.
const COMPACTION_SOURCE: &str = "compaction";
const SUMMARY_OPERATION: &str = "summary";

#[derive(Deserialize)]
struct Export {
  session_id: Option<String>,
  steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
  source: String,
  message: String,
  #[serde(default)]
  tool_calls: Vec<ToolCall>,
  observation: Option<Observation>,
  metrics: Option<StepMetrics>,
  extra: Option<StepExtra>,
}

impl Step {
  /// Whether the steps after this one belong to one turn: a user step holds that turn's prompt, and a compaction
  /// summary holds it after a compaction.
  fn starts_turn(&self) -> bool {
    self.source == USER_SOURCE || self.is_compaction_summary()
  }

  fn is_compaction_summary(&self) -> bool {
    let Some(telemetry) = self.extra.as_ref().and_then(|extra| extra.telemetry.as_ref()) else {
      return false;
    };
    telemetry.source.as_deref() == Some(COMPACTION_SOURCE) && telemetry.operation.as_deref() == Some(SUMMARY_OPERATION)
  }
}

/// The CLI's own metadata of a step. Only its telemetry is read here.
#[derive(Deserialize)]
struct StepExtra {
  telemetry: Option<Telemetry>,
}

/// What produced a step, such as `source` "assistant" with `operation` "inference" for a model call.
#[derive(Deserialize)]
struct Telemetry {
  source: Option<String>,
  operation: Option<String>,
}

#[derive(Deserialize)]
struct ToolCall {
  tool_call_id: String,
  function_name: String,
  arguments: Value,
}

#[derive(Deserialize)]
struct Observation {
  results: Vec<ObservationResult>,
}

#[derive(Deserialize)]
struct ObservationResult {
  source_call_id: Option<String>,
  content: Option<String>,
}

/// The token counts an agent step reports. ATIF marks every field here optional, and the CLI omits `cached_tokens`
/// when a turn has no prompt caching. `prompt_tokens` counts all input, and `cached_tokens` is the part of it that was
/// a cache hit. An omitted count adds zero to the turn's usage.
#[derive(Deserialize)]
struct StepMetrics {
  prompt_tokens: Option<i64>,
  completion_tokens: Option<i64>,
  cached_tokens: Option<i64>,
}

/// The token counts of one turn: the sum of its steps' metrics.
#[derive(Default)]
struct TurnUsage {
  prompt_tokens: i64,
  completion_tokens: i64,
  cached_tokens: i64,
}

/// The events of one turn, in the order the session page shows them: the attached session, each step's text and tool
/// events, the final answer, and the usage result.
///
/// The export holds every turn's steps, except those a compaction replaced with its summary. Only the steps after the
/// last user step or compaction summary, whichever comes later, become events. `answer` is the stdout text, the final
/// answer. The last agent message of that turn carries the same answer, so that message is not repeated as a text
/// event. The usage is the sum of those steps' metrics, not the export's `final_metrics`, which covers every turn. A
/// compaction drops the compacted steps with their metrics, so a compacted turn's usage leaves out the tokens spent
/// before the compaction. The usage result has a null cost, because Devin bills in credits and reports no dollar
/// figure.
pub fn turn_events(export_text: &str, answer: &str) -> anyhow::Result<Vec<Value>> {
  let export: Export = serde_json::from_str(export_text).context("the export is not the trajectory JSON")?;
  let session_id = match export.session_id {
    Some(session_id) if !session_id.is_empty() => session_id,
    _ => bail!("devin export has no session_id"),
  };
  let Some(turn_start) = export.steps.iter().rposition(Step::starts_turn) else {
    bail!("devin export has no user step and no compaction summary");
  };
  let steps = &export.steps[turn_start + 1..];
  let usage = turn_usage(steps)?;
  let final_message = steps.iter().rposition(|step| step.source == AGENT_SOURCE && !step.message.is_empty());

  let mut events = vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": session_id})];
  let mut tool_names: HashMap<String, String> = HashMap::new();
  for (index, step) in steps.iter().enumerate() {
    if step.source == AGENT_SOURCE && final_message != Some(index) && !step.message.is_empty() {
      events.push(make_text_event(&step.message));
    }
    for call in &step.tool_calls {
      tool_names.insert(call.tool_call_id.clone(), call.function_name.clone());
      events.push(make_tool_use_event(&call.function_name, call.arguments.clone()));
    }
    for result in step.observation.iter().flat_map(|observation| &observation.results) {
      let Some(call_id) = &result.source_call_id else {
        bail!("devin export observation result has no source_call_id, so its tool name is unknown");
      };
      let Some(name) = tool_names.get(call_id) else {
        bail!("devin export observation answers tool call {call_id} that no step made");
      };
      events.push(make_tool_result_event(name, result.content.as_deref().unwrap_or("")));
    }
  }

  events.push(make_text_event(answer));
  // ATIF's `prompt_tokens` is the whole input, with `cached_tokens` as a subset of it (Harbor's
  // rfcs/0001-trajectory-format.md, MetricsSchema). The result keeps that OpenAI-style convention, as codex's
  // translator does: `input_tokens` is the prompt sum with the cached part. `cache_read_input_tokens` carries the
  // cached count beside it. Devin's usage source is run-logs-only, whose ledger record reads `input_tokens` as the
  // whole input, so each input token is counted once.
  events.push(make_result_event(
    usage.prompt_tokens,
    usage.completion_tokens,
    usage.cached_tokens,
    0,
    Value::Null,
    None,
  ));
  Ok(events)
}

/// The turn's tokens, summed over its steps' metrics. Only agent steps carry metrics, so an agent step without them
/// would leave its tokens out of the sum, and the turn fails instead.
fn turn_usage(steps: &[Step]) -> anyhow::Result<TurnUsage> {
  let mut usage = TurnUsage::default();
  for step in steps {
    match (&step.metrics, step.source.as_str()) {
      (Some(metrics), _) => {
        usage.prompt_tokens += metrics.prompt_tokens.unwrap_or(0);
        usage.completion_tokens += metrics.completion_tokens.unwrap_or(0);
        usage.cached_tokens += metrics.cached_tokens.unwrap_or(0);
      }
      (None, AGENT_SOURCE) => bail!("devin export has an agent step without metrics in the current turn"),
      (None, _) => {}
    }
  }
  Ok(usage)
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIXTURE: &str = include_str!("../tests/fixtures/export.json");
  const MULTITURN_FIXTURE: &str = include_str!("../tests/fixtures/export_multiturn.json");
  const COMPACTED_FIXTURE: &str = include_str!("../tests/fixtures/export_compacted.json");

  #[test]
  fn a_trajectory_becomes_the_session_tool_text_and_usage_events() {
    let events = turn_events(FIXTURE, "The number is 41.").unwrap();
    assert_eq!(
      events,
      vec![
        json!({"type": "session_attached", "session_id": "devin-session-7f3a"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "I will read the file."}]}}),
        json!({"type": "tool_use", "name": "read_file", "input": {"path": "number.txt"}}),
        json!({"type": "tool_result", "tool_name": "read_file", "content": "41"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "The number is 41."}]}}),
        json!({
          "type": "result",
          "result": "",
          "usage": {
            "input_tokens": 1200,
            "output_tokens": 85,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 0
          },
          "total_cost_usd": null
        }),
      ]
    );
  }

  #[test]
  fn only_the_steps_after_the_last_user_step_become_events() {
    let events = turn_events(MULTITURN_FIXTURE, "Fourth turn answer.").unwrap();
    assert_eq!(
      events,
      vec![
        json!({"type": "session_attached", "session_id": "devin-session-2c9e"}),
        json!({"type": "tool_use", "name": "exec", "input": {"command": "echo fourth"}}),
        json!({"type": "tool_result", "tool_name": "exec", "content": "fourth"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Fourth turn answer."}]}}),
        json!({
          "type": "result",
          "result": "",
          "usage": {
            "input_tokens": 2400,
            "output_tokens": 160,
            "cache_read_input_tokens": 1200,
            "cache_creation_input_tokens": 0
          },
          "total_cost_usd": null
        }),
      ]
    );
  }

  #[test]
  fn a_compacted_turn_becomes_events_from_the_steps_after_the_compaction_summary() {
    let events = turn_events(COMPACTED_FIXTURE, "The sum is 42.").unwrap();
    assert_eq!(
      events,
      vec![
        json!({"type": "session_attached", "session_id": "devin-session-5b1d"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Next I read b.txt."}]}}),
        json!({"type": "tool_use", "name": "read_file", "input": {"path": "b.txt"}}),
        json!({"type": "tool_result", "tool_name": "read_file", "content": "23"}),
        json!({"type": "tool_use", "name": "exec", "input": {"command": "expr 19 + 23"}}),
        json!({"type": "tool_result", "tool_name": "exec", "content": "42"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "The sum is 42."}]}}),
        json!({
          "type": "result",
          "result": "",
          "usage": {
            "input_tokens": 3200,
            "output_tokens": 90,
            "cache_read_input_tokens": 2800,
            "cache_creation_input_tokens": 0
          },
          "total_cost_usd": null
        }),
      ]
    );
  }

  #[test]
  fn a_turn_after_a_compacted_turn_starts_after_its_user_step() {
    let mut export: Value = serde_json::from_str(COMPACTED_FIXTURE).unwrap();
    let steps = export["steps"].as_array_mut().unwrap();
    steps.push(json!({"step_id": 9, "source": "user", "message": "Double the sum."}));
    steps.push(json!({
      "step_id": 10,
      "source": "agent",
      "message": "84.",
      "metrics": {"prompt_tokens": 1300, "completion_tokens": 5, "cached_tokens": 1200}
    }));
    let events = turn_events(&export.to_string(), "84.").unwrap();
    assert_eq!(
      events,
      vec![
        json!({"type": "session_attached", "session_id": "devin-session-5b1d"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "84."}]}}),
        json!({
          "type": "result",
          "result": "",
          "usage": {
            "input_tokens": 1300,
            "output_tokens": 5,
            "cache_read_input_tokens": 1200,
            "cache_creation_input_tokens": 0
          },
          "total_cost_usd": null
        }),
      ]
    );
  }

  #[test]
  fn a_step_metrics_without_cached_tokens_counts_zero_cached_tokens() {
    let mut export: Value = serde_json::from_str(MULTITURN_FIXTURE).unwrap();
    for step in export["steps"].as_array_mut().unwrap() {
      if let Some(metrics) = step.get_mut("metrics") {
        metrics.as_object_mut().unwrap().remove("cached_tokens");
      }
    }
    let events = turn_events(&export.to_string(), "Fourth turn answer.").unwrap();
    let result = events.last().unwrap();
    assert_eq!(result["usage"]["input_tokens"], 2400);
    assert_eq!(result["usage"]["output_tokens"], 160);
    assert_eq!(result["usage"]["cache_read_input_tokens"], 0);
  }

  #[test]
  fn an_export_without_a_user_step_or_a_compaction_summary_is_refused() {
    let without_user = MULTITURN_FIXTURE.replace("\"source\": \"user\"", "\"source\": \"system\"");
    let error = turn_events(&without_user, "x").unwrap_err();
    assert_eq!(error.to_string(), "devin export has no user step and no compaction summary");
  }

  #[test]
  fn an_export_without_a_step_message_is_refused() {
    let mut export: Value = serde_json::from_str(MULTITURN_FIXTURE).unwrap();
    let last_step = export["steps"].as_array_mut().unwrap().last_mut().unwrap();
    last_step.as_object_mut().unwrap().remove("message");
    let error = turn_events(&export.to_string(), "x").unwrap_err();
    assert_eq!(error.to_string(), "the export is not the trajectory JSON");
  }

  #[test]
  fn an_export_without_a_session_id_is_refused() {
    let without = FIXTURE.replace("\"session_id\": \"devin-session-7f3a\",", "");
    let error = turn_events(&without, "x").unwrap_err();
    assert_eq!(error.to_string(), "devin export has no session_id");
    let empty = FIXTURE.replace("devin-session-7f3a", "");
    assert_eq!(turn_events(&empty, "x").unwrap_err().to_string(), "devin export has no session_id");
  }

  #[test]
  fn an_observation_that_answers_no_call_of_the_turn_is_refused() {
    let orphan = FIXTURE.replace("\"source_call_id\": \"call-1\"", "\"source_call_id\": \"call-9\"");
    let error = turn_events(&orphan, "x").unwrap_err();
    assert!(error.to_string().contains("call-9"), "the error names the call: {error}");
  }

  #[test]
  fn a_current_turn_agent_step_without_metrics_is_refused() {
    let mut export: Value = serde_json::from_str(MULTITURN_FIXTURE).unwrap();
    let last_step = export["steps"].as_array_mut().unwrap().last_mut().unwrap();
    last_step.as_object_mut().unwrap().remove("metrics");
    let error = turn_events(&export.to_string(), "x").unwrap_err();
    assert_eq!(error.to_string(), "devin export has an agent step without metrics in the current turn");
  }
}
