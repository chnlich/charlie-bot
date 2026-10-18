use serde_json::{Value, json};

use crate::metadata::SessionMetadata;
use crate::projection;
use crate::{ChatEvent, Store, StoreResult};

/// Public detail for a missing session lookup.
pub const SESSION_NOT_FOUND_DETAIL: &str = "Session not found";

/// Provenance carried by an agent-relay message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageProvenance {
  /// Session that sent the relayed message.
  pub from_session: String,
  /// Display name of the sending session.
  pub from_session_name: String,
}

/// Build the persisted event for a scheduled-trigger self-wake.
pub fn build_scheduled_trigger_event(content: &str) -> ChatEvent {
  json!({
    "type": "scheduled_trigger",
    "content": content,
    "timestamp": charliebot_core::timefmt::utc_now_iso(),
  })
}

/// Build a persisted event for a message relayed from another session.
pub fn build_agent_message_event(text: &str, provenance: MessageProvenance) -> ChatEvent {
  json!({
    "type": "agent_message",
    "content": text,
    "from_session": provenance.from_session,
    "from_session_name": provenance.from_session_name,
    "timestamp": charliebot_core::timefmt::utc_now_iso(),
  })
}

/// Return the chat-event ids answered by one master_done event.
pub fn master_done_input_event_ids(event: &Value) -> Vec<String> {
  if let Some(ids) = event.get("input_event_ids").and_then(Value::as_array) {
    return ids.iter().filter_map(Value::as_str).map(str::to_string).collect();
  }
  event
    .get("input_event_id")
    .and_then(Value::as_str)
    .filter(|id| !id.is_empty())
    .map(|id| vec![id.to_string()])
    .unwrap_or_default()
}

/// Convert raw chat events to committed render messages.
pub fn events_to_messages(events: &[Value]) -> StoreResult<Vec<Value>> {
  projection::events_to_messages(events, 0, Default::default())
}

/// Convert raw chat events to committed messages and the pending assistant draft.
pub fn events_to_view(events: &[Value]) -> StoreResult<(Vec<Value>, Option<Value>)> {
  projection::events_to_view(events, 0, Default::default())
}

impl Store {
  /// Build an agent-relay event for the target session's event log.
  pub fn build_agent_message_event(&self, _session_id: &str, text: &str, provenance: MessageProvenance) -> ChatEvent {
    build_agent_message_event(text, provenance)
  }

  /// Return the global input-event ids answered by a master_done event.
  pub fn master_done_input_event_ids(&self, event: &ChatEvent) -> Vec<String> {
    master_done_input_event_ids(event)
  }

  /// Convert events with this store's feature renderers.
  pub fn events_to_messages(&self, events: &[ChatEvent]) -> StoreResult<Vec<Value>> {
    projection::events_to_messages(events, 0, self.event_renderers.clone())
  }

  /// Convert events with this store's feature renderers and preserve a live draft.
  pub fn events_to_view(&self, events: &[ChatEvent]) -> StoreResult<(Vec<Value>, Option<Value>)> {
    projection::events_to_view(events, 0, self.event_renderers.clone())
  }

  /// Return one session or an error carrying the route's 404 detail.
  pub async fn require_session(&self, session_id: &str) -> StoreResult<SessionMetadata> {
    self.get_session(session_id).await?.ok_or_else(|| crate::StoreError::not_found(SESSION_NOT_FOUND_DETAIL))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::make_test_store;

  #[test]
  fn agent_message_event_carries_only_the_cross_session_provenance() {
    let event = build_agent_message_event(
      "Hello from the survey.",
      MessageProvenance { from_session: "source".to_string(), from_session_name: "Survey".to_string() },
    );
    assert_eq!(event["type"], "agent_message");
    assert_eq!(event["content"], "Hello from the survey.");
    assert_eq!(event["from_session"], "source");
    assert_eq!(event["from_session_name"], "Survey");
    assert!(event["timestamp"].as_str().is_some());
    assert_eq!(event.as_object().unwrap().len(), 5);
  }

  #[test]
  fn scheduled_trigger_event_has_no_user_attachment_fields() {
    let event = build_scheduled_trigger_event("wake for the due digest");
    assert_eq!(event["type"], "scheduled_trigger");
    assert_eq!(event["content"], "wake for the due digest");
    assert!(event["timestamp"].as_str().is_some());
    assert_eq!(event.as_object().unwrap().len(), 3);
  }

  #[test]
  fn plural_master_done_ids_take_precedence_and_keep_only_strings() {
    assert_eq!(
      master_done_input_event_ids(&json!({
        "input_event_ids": ["first", 4, "", null],
        "input_event_id": "legacy",
      })),
      vec!["first", ""]
    );
    assert!(master_done_input_event_ids(&json!({"input_event_ids": [], "input_event_id": "legacy"})).is_empty());
    assert_eq!(master_done_input_event_ids(&json!({"input_event_id": "legacy"})), vec!["legacy"]);
    assert!(master_done_input_event_ids(&json!({"input_event_id": ""})).is_empty());
  }

  #[tokio::test]
  async fn require_session_returns_the_route_404_detail() {
    let (_home, store) = make_test_store();
    let error = match store.require_session("missing-session").await {
      Ok(_) => panic!("missing sessions must return the route's not-found error"),
      Err(error) => error,
    };
    assert_eq!(error.status_code(), Some(404));
    assert_eq!(error.to_string(), SESSION_NOT_FOUND_DETAIL);
  }
}
