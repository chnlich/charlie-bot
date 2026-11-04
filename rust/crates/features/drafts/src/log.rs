//! Derive one draft's state from the session log.
//!
//! The log is the only truth the send path reads: a registered draft with no `draft_sent` and no later draft that
//! names it in `replaces` can be sent. Nothing else is stored.

use charliebot_core::event_types;
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use serde_json::Value;

/// The event field names the drafts package owns.
pub const DRAFT_ID: &str = "draft_id";
pub const TARGET_LABEL: &str = "target_label";
pub const SHA256: &str = "sha256";
pub const CREATED_AT: &str = "created_at";
pub const SENDER: &str = "sender";
pub const URL: &str = "url";
pub const SENT_AT: &str = "sent_at";
pub const ERROR: &str = "error";
pub const OUTCOME: &str = "outcome";

/// The registration payload's field names. `charliebot_cli_common::draft` owns them because it builds the payload, and
/// the register route reads the same payload into the draft events.
pub const SESSION_ID: &str = charliebot_cli_common::draft::SESSION_ID;
pub const PLATFORM: &str = charliebot_cli_common::draft::PLATFORM;
pub const TARGET: &str = charliebot_cli_common::draft::TARGET;
pub const BODY: &str = charliebot_cli_common::draft::BODY;
pub const REPLACES: &str = charliebot_cli_common::draft::REPLACES;

/// One registered draft, in the form the send path hands to its platform.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredDraft {
  pub draft_id: String,
  pub platform: String,
  pub target: Value,
  pub body: String,
  pub sha256: String,
}

/// What the log says about one draft id.
#[derive(Debug, Clone, PartialEq)]
pub enum Sendability {
  /// No `draft_sent` and no replacing draft: the operator may send it.
  Sendable(RegisteredDraft),
  /// A `draft_sent` for this draft is already logged.
  AlreadySent,
  /// A later registered draft names this one in `replaces`.
  Replaced,
  /// The log holds no `draft_registered` with this id.
  Missing,
}

/// Read one session's log and derive `draft_id`'s sendability.
pub fn sendability<'a>(events: impl Iterator<Item = &'a ChatEvent>, draft_id: &str) -> Sendability {
  let mut registered: Option<RegisteredDraft> = None;
  let mut sent = false;
  let mut replaced = false;
  for event in events {
    match event.get("type").and_then(CompactValue::as_str) {
      Some(event_types::DRAFT_REGISTERED) => {
        if text(event, DRAFT_ID) == draft_id {
          registered = Some(registered_draft(event));
        } else if text(event, REPLACES) == draft_id {
          replaced = true;
        }
      }
      Some(event_types::DRAFT_SENT) if text(event, DRAFT_ID) == draft_id => sent = true,
      _ => {}
    }
  }
  match registered {
    None => Sendability::Missing,
    Some(_) if sent => Sendability::AlreadySent,
    Some(_) if replaced => Sendability::Replaced,
    Some(draft) => Sendability::Sendable(draft),
  }
}

/// One event's string field, or `""` when it is absent or not a string.
pub fn text<'a>(event: &'a ChatEvent, field: &str) -> &'a str {
  event.get(field).and_then(CompactValue::as_str).unwrap_or_default()
}

/// One `draft_registered` event's send-side fields.
fn registered_draft(event: &ChatEvent) -> RegisteredDraft {
  RegisteredDraft {
    draft_id: text(event, DRAFT_ID).to_string(),
    platform: text(event, PLATFORM).to_string(),
    target: event.get(TARGET).map(CompactValue::to_value).unwrap_or(Value::Null),
    body: text(event, BODY).to_string(),
    sha256: text(event, SHA256).to_string(),
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn event(value: Value) -> ChatEvent {
    ChatEvent::from_value(&value)
  }

  fn registered(draft_id: &str, replaces: Option<&str>) -> ChatEvent {
    event(json!({
      "type": event_types::DRAFT_REGISTERED,
      "draft_id": draft_id,
      "platform": "synthetic",
      "target": {"channel": "C1"},
      "target_label": "Channel C1",
      "body": "the body",
      "replaces": replaces,
      "sha256": "abc",
    }))
  }

  #[test]
  fn a_lone_registration_is_sendable() {
    let events = [registered("d1", None)];
    assert_eq!(
      sendability(events.iter(), "d1"),
      Sendability::Sendable(RegisteredDraft {
        draft_id: "d1".to_string(),
        platform: "synthetic".to_string(),
        target: json!({"channel": "C1"}),
        body: "the body".to_string(),
        sha256: "abc".to_string(),
      })
    );
  }

  #[test]
  fn a_sent_draft_is_no_longer_sendable() {
    let events = [registered("d1", None), event(json!({"type": event_types::DRAFT_SENT, "draft_id": "d1"}))];
    assert_eq!(sendability(events.iter(), "d1"), Sendability::AlreadySent);
  }

  #[test]
  fn a_draft_named_by_a_later_replaces_is_superseded() {
    let events = [registered("d1", None), registered("d2", Some("d1"))];
    assert_eq!(sendability(events.iter(), "d1"), Sendability::Replaced);
    assert!(matches!(sendability(events.iter(), "d2"), Sendability::Sendable(_)));
  }

  #[test]
  fn an_unknown_id_is_missing() {
    let events = [registered("d1", None)];
    assert_eq!(sendability(events.iter(), "other"), Sendability::Missing);
  }

  #[test]
  fn a_sent_and_replaced_draft_reports_sent() {
    let events = [
      registered("d1", None),
      event(json!({"type": event_types::DRAFT_SENT, "draft_id": "d1"})),
      registered("d2", Some("d1")),
    ];
    assert_eq!(sendability(events.iter(), "d1"), Sendability::AlreadySent);
  }
}
