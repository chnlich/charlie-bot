//! Compose the text and uploads for one task Run's durable input events.
//!
//! Live mid-turn delivery and the next Run share this composer, so a delivered message and a queued message read the
//! same.

use charliebot_core::event_types;
use charliebot_core::text_repr::{field_text, truthy_field_text};
use charliebot_registry::context::FeatureContext;
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::SessionMetadata;
use serde_json::{Map, Value};

/// The parent's typed task input for a child report: a header naming the task, its provenance and outcome, then the
/// summary.
pub fn child_report_text(report: &Value, provenance: Option<&str>) -> String {
  let mut header = format!("[Report from task {}", field_text(report.get(event_types::CHILD_SESSION_ID)));
  if let Some(provenance) = provenance {
    header.push_str(&format!(" ({provenance})"));
  }
  format!("{header} | outcome {}] {}", field_text(report.get("outcome")), truthy_field_text(report.get("summary")))
}

/// The text an input event carries, without the framing line the prompt adds for its kind. A child report has no
/// content, so its text is its rendered report; every other input's text is its content. The turn-end reminder quotes
/// this text as well, so the prompt and the reminder read one renderer.
pub fn input_text(event: &Value, provenance: Option<&str>) -> String {
  if event["type"] == event_types::CHILD_REPORT {
    return child_report_text(event, provenance);
  }
  event["content"].as_str().unwrap_or_default().to_string()
}

/// The provenance fragment of a child report event's child, or `None` for any other input.
fn report_provenance_of<'a>(event: &Value, report_provenance: &'a Map<String, Value>) -> Option<&'a str> {
  if event["type"] != event_types::CHILD_REPORT {
    return None;
  }
  let child_id = field_text(event.get(event_types::CHILD_SESSION_ID));
  report_provenance.get(&child_id).and_then(Value::as_str)
}

/// Compose the manager input prompt and uploaded-file list from the claimed facts.
pub fn compose_input_prompt(
  features: &FeatureContext,
  meta: &SessionMetadata,
  events: &[ChatEvent],
  report_provenance: &Map<String, Value>,
) -> anyhow::Result<(String, Vec<Value>)> {
  let mut parts = Vec::new();
  let mut uploads = Vec::new();
  for event in events {
    let event_type = event.get("type").and_then(CompactValue::as_str).unwrap_or_default();
    let value = event.to_value();
    let body = input_text(&value, report_provenance_of(&value, report_provenance));
    let text = match event_type {
      event_types::USER => match features.registry.input_preamble(meta, &value) {
        Some(preamble) => format!("{preamble}\n{body}"),
        None => body,
      },
      event_types::AGENT_MESSAGE => {
        let source = event
          .get(event_types::FROM_SESSION_NAME)
          .and_then(CompactValue::as_str)
          .filter(|name| !name.is_empty())
          .or_else(|| event.get(event_types::FROM_SESSION).and_then(CompactValue::as_str).filter(|id| !id.is_empty()))
          .unwrap_or("unknown");
        format!("[Message from session {source}] {body}")
      }
      event_types::SCHEDULED_TRIGGER => format!("[Scheduled trigger] {body}"),
      event_types::DRAFT_SENT => format!("[Draft sent] {body}"),
      _ => body,
    };
    let event_uploads = event
      .get(event_types::UPLOADED_FILES)
      .and_then(CompactValue::as_array)
      .map(|files| files.iter().map(CompactValue::to_value).collect::<Vec<_>>())
      .unwrap_or_default();
    parts.push(charliebot_store::message_events::build_agent_input_content(&text, &event_uploads));
    uploads.extend(event_uploads);
  }
  Ok((parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"), uploads))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_store::metadata::TaskProfile;
  use serde_json::json;

  use super::*;
  use crate::test_fixtures;

  /// The features of a server that registers the web reply and voice packages, in the server's order.
  fn features(home: &tempfile::TempDir) -> Arc<FeatureContext> {
    test_fixtures::features_with_packages(
      &home.path().join("home"),
      "",
      &[("features.web_reply", charliebot_web_reply::register), ("features.voice", charliebot_voice::register)],
    )
  }

  fn user_message(id: &str, content: &str) -> ChatEvent {
    ChatEvent::from(json!({"id": id, "type": event_types::USER, "content": content}))
  }

  #[test]
  fn each_user_message_of_a_web_session_opens_with_the_user_message_line() {
    let home = tempfile::tempdir().unwrap();
    let meta = SessionMetadata::new("session", "Web", TaskProfile::Manager);
    let events = [user_message("909c6592-first", "first"), user_message("a1b2c3d4-second", "second")];

    let (content, _) = compose_input_prompt(&features(&home), &meta, &events, &Map::new()).unwrap();

    assert_eq!(content, "[User message]\nfirst\n\n[User message]\nsecond");
  }

  #[test]
  fn a_dictated_web_message_carries_the_voice_note_then_the_user_message_line() {
    let home = tempfile::tempdir().unwrap();
    let meta = SessionMetadata::new("session", "Web", TaskProfile::Manager);
    let dictated = ChatEvent::from(json!({
      "id": "909c6592-dictated",
      "type": event_types::USER,
      "content": "the dictated text",
      "input_mode": "voice",
    }));

    let (content, _) = compose_input_prompt(&features(&home), &meta, &[dictated], &Map::new()).unwrap();

    let mut lines = content.lines();
    assert_eq!(lines.next(), Some("[User message]"));
    assert!(lines.next().is_some_and(|note| note.starts_with("[Voice input: ")), "{content}");
    assert_eq!(lines.next(), Some("the dictated text"));
    assert_eq!(lines.next(), None);
  }

  #[test]
  fn a_thread_message_keeps_its_text_without_a_tag_line() {
    // A Slack session is a thread only while some package has registered `slack_origin` as a
    // thread-origin field; the chat packages do that at server start.
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
    let home = tempfile::tempdir().unwrap();
    let mut thread = SessionMetadata::new("thread", "Slack", TaskProfile::Manager);
    thread.extra.insert("slack_origin".to_string(), json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1.0"}));

    let (content, _) =
      compose_input_prompt(&features(&home), &thread, &[user_message("909c6592-typed", "the typed text")], &Map::new())
        .unwrap();

    assert_eq!(content, "[User message]\nthe typed text");
  }
}
