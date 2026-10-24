//! Compose the text and uploads for one task Run's durable input events (`src/runtime/task_input_prompt.py`).
//!
//! Live mid-turn delivery and the next Run share this composer, so a delivered message and a queued message read the
//! same.

use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::SessionMetadata;
use serde_json::{Map, Value};

use crate::dispatch::{python_string, truthy_string};

/// Format a child report as the parent's typed task input (`session_dispatch.child_report_text` before it moved here).
pub fn child_report_text(report: &Value, provenance: Option<&str>) -> String {
  let mut header = format!("[Report from task {}", python_string(report.get("child_session_id")));
  if let Some(provenance) = provenance {
    header.push_str(&format!(" ({provenance})"));
  }
  format!("{header} | outcome {}] {}", python_string(report.get("outcome")), truthy_string(report.get("summary")))
}

/// Compose the manager input prompt and uploaded-file list from the claimed facts.
pub fn compose_input_prompt(
  features: &FeatureContext,
  meta: &SessionMetadata,
  events: &[Value],
  report_provenance: &Map<String, Value>,
) -> anyhow::Result<(String, Vec<Value>)> {
  let mut parts = Vec::new();
  let mut uploads = Vec::new();
  for event in events {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let content = event.get("content").and_then(Value::as_str).unwrap_or_default();
    let text = match event_type {
      event_types::USER => match features.registry.input_preamble(meta, event)? {
        Some(preamble) => format!("{preamble}\n{content}"),
        None => content.to_string(),
      },
      event_types::AGENT_MESSAGE => {
        let source = event
          .get("from_session_name")
          .and_then(Value::as_str)
          .filter(|name| !name.is_empty())
          .or_else(|| event.get("from_session").and_then(Value::as_str).filter(|id| !id.is_empty()))
          .unwrap_or("unknown");
        format!("[Message from session {source}] {content}")
      }
      event_types::CHILD_REPORT => {
        let child_id = python_string(event.get("child_session_id"));
        let provenance = report_provenance.get(&child_id).and_then(Value::as_str);
        child_report_text(event, provenance)
      }
      event_types::SCHEDULED_TRIGGER => format!("[Scheduled trigger] {content}"),
      _ => content.to_string(),
    };
    let event_uploads = event.get("uploaded_files").and_then(Value::as_array).cloned().unwrap_or_default();
    parts.push(charliebot_store::message_events::build_agent_input_content(&text, &event_uploads));
    uploads.extend(event_uploads);
  }
  Ok((parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"), uploads))
}
