//! The chat messages the three draft events render as.
//!
//! The card message carries the registered body whole: the browser shows it in full, never folded or truncated, and
//! its button sends exactly that text. `draft_sent` and `draft_failed` carry the state the card applies to the draft
//! it names.

use charliebot_core::event_types;
use serde_json::{Value, json};

use crate::log::{
  BODY, CREATED_AT, DRAFT_ID, ERROR, OUTCOME, PLATFORM, REPLACES, SENDER, SENT_AT, SHA256, TARGET, TARGET_LABEL, URL,
};

/// The card of one `draft_registered` event.
pub fn draft_registered_message(event: &Value) -> Value {
  json!({
    "role": "draft",
    "draft_id": field(event, DRAFT_ID),
    (PLATFORM): field(event, PLATFORM),
    (TARGET): field(event, TARGET),
    "target_label": field(event, TARGET_LABEL),
    (BODY): field(event, BODY),
    (REPLACES): field(event, REPLACES),
    "sha256": field(event, SHA256),
    "created_at": field(event, CREATED_AT),
  })
}

/// The chat line of one `draft_sent` event; the card it names loses its button.
pub fn draft_sent_message(event: &Value) -> Value {
  json!({
    "role": event_types::DRAFT_SENT,
    "draft_id": field(event, DRAFT_ID),
    "sha256": field(event, SHA256),
    "sender": field(event, SENDER),
    "url": field(event, URL),
    "sent_at": field(event, SENT_AT),
  })
}

/// The chat line of one `draft_failed` event; the card it names keeps its button and shows the failure.
pub fn draft_failed_message(event: &Value) -> Value {
  json!({
    "role": event_types::DRAFT_FAILED,
    "draft_id": field(event, DRAFT_ID),
    "error": field(event, ERROR),
    "outcome": field(event, OUTCOME),
  })
}

fn field(event: &Value, name: &str) -> Value {
  event.get(name).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn long_body() -> String {
    (0..400).map(|index| format!("line {index} of the draft body\n")).collect()
  }

  fn registered(body: &str, replaces: Value) -> Value {
    json!({
      "type": "draft_registered",
      "draft_id": "d7",
      "platform": "slack",
      "target": {"channel": "C1", "thread": "1.0"},
      "target_label": "Slack thread C1/1.0",
      "body": body,
      "replaces": replaces,
      "sha256": "a".repeat(64),
      "created_at": "2026-11-02T00:00:00Z",
    })
  }

  #[test]
  fn a_card_carries_the_target_and_the_whole_body() {
    let body = long_body();
    let message = draft_registered_message(&registered(&body, Value::Null));

    assert_eq!(message["role"], "draft");
    assert_eq!(message["draft_id"], "d7");
    assert_eq!(message["platform"], "slack");
    assert_eq!(message["target"], json!({"channel": "C1", "thread": "1.0"}));
    assert_eq!(message["target_label"], "Slack thread C1/1.0");
    assert_eq!(message["body"], body, "the body reaches the card byte for byte");
    assert_eq!(message["sha256"], "a".repeat(64));
    assert_eq!(message["replaces"], Value::Null);
  }

  #[test]
  fn a_replacing_draft_names_the_card_it_supersedes() {
    let message = draft_registered_message(&registered("replacement", json!("d6")));
    assert_eq!(message["replaces"], "d6");
  }

  #[test]
  fn the_sent_line_carries_the_sender_and_url() {
    let event = json!({
      "type": "draft_sent",
      "draft_id": "d7",
      "sha256": "b".repeat(64),
      "sender": "U0SYNTHETIC",
      "url": "https://chat.example/archives/C1/p1",
      "sent_at": "2026-11-02T00:01:00Z",
    });
    let message = draft_sent_message(&event);

    assert_eq!(message["role"], "draft_sent");
    assert_eq!(message["draft_id"], "d7");
    assert_eq!(message["sender"], "U0SYNTHETIC");
    assert_eq!(message["url"], "https://chat.example/archives/C1/p1");
    assert_eq!(message["sha256"], "b".repeat(64));
  }

  #[test]
  fn the_failed_line_carries_the_outcome() {
    let event = json!({"type": "draft_failed", "draft_id": "d7", "error": "missing_scope", "outcome": "rejected"});
    let message = draft_failed_message(&event);

    assert_eq!(message["role"], "draft_failed");
    assert_eq!(message["error"], "missing_scope");
    assert_eq!(message["outcome"], "rejected");
  }
}
