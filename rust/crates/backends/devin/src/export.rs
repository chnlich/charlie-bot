//! The devin export: the trajectory JSON that `devin --export` writes after each turn, read into the runtime's events.
//!
//! This module is the one place that names the export's fields. The names come from the CLI binary's strings
//! (`ATIF-v1.x`), not from a Devin document: `session_id`, `steps[]` with `source`, `message`, `tool_calls[]` and
//! `observation.results[]`, and `final_metrics`. A live export that disagrees with them is fixed here.

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

#[derive(Deserialize)]
struct Export {
  session_id: Option<String>,
  steps: Vec<Step>,
  final_metrics: Metrics,
}

#[derive(Deserialize)]
struct Step {
  source: String,
  message: Option<String>,
  #[serde(default)]
  tool_calls: Vec<ToolCall>,
  observation: Option<Observation>,
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
  source_call_id: String,
  content: Option<String>,
}

#[derive(Deserialize)]
struct Metrics {
  total_prompt_tokens: i64,
  total_completion_tokens: i64,
  total_cached_tokens: i64,
}

/// The events of one turn, in the order the session page shows them: the attached session, each step's text and tool
/// events, the final answer, and the usage result.
///
/// `answer` is the stdout text, the final answer. The last agent message of the trajectory carries the same answer, so
/// that message is not repeated as a text event. The usage result has a null cost, because Devin bills in credits and
/// reports no dollar figure.
pub fn turn_events(export_text: &str, answer: &str) -> anyhow::Result<Vec<Value>> {
  let export: Export = serde_json::from_str(export_text).context("the export is not the trajectory JSON")?;
  let session_id = match export.session_id {
    Some(session_id) if !session_id.is_empty() => session_id,
    _ => bail!("devin export has no session_id"),
  };
  let final_message = export.steps.iter().rposition(|step| step.source == AGENT_SOURCE && has_text(&step.message));

  let mut events = vec![json!({"type": event_types::SESSION_ATTACHED, "session_id": session_id})];
  let mut tool_names: HashMap<String, String> = HashMap::new();
  for (index, step) in export.steps.iter().enumerate() {
    if let Some(message) = step.message.as_deref()
      && step.source == AGENT_SOURCE
      && final_message != Some(index)
      && !message.is_empty()
    {
      events.push(make_text_event(message));
    }
    for call in &step.tool_calls {
      tool_names.insert(call.tool_call_id.clone(), call.function_name.clone());
      events.push(make_tool_use_event(&call.function_name, call.arguments.clone()));
    }
    for result in step.observation.iter().flat_map(|observation| &observation.results) {
      let Some(name) = tool_names.get(&result.source_call_id) else {
        bail!("devin export observation answers tool call {} that no step made", result.source_call_id);
      };
      events.push(make_tool_result_event(name, result.content.as_deref().unwrap_or("")));
    }
  }

  events.push(make_text_event(answer));
  let metrics = export.final_metrics;
  events.push(make_result_event(
    metrics.total_prompt_tokens,
    metrics.total_completion_tokens,
    metrics.total_cached_tokens,
    0,
    Value::Null,
    None,
  ));
  Ok(events)
}

fn has_text(message: &Option<String>) -> bool {
  message.as_deref().is_some_and(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIXTURE: &str = include_str!("../tests/fixtures/export.json");

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
  fn a_missing_metrics_block_is_refused() {
    let without = FIXTURE.replace("\"final_metrics\"", "\"metrics\"");
    assert!(turn_events(&without, "x").is_err());
  }
}
