use charliebot_core::event_types;
use serde_json::{Value, json};

use crate::metadata::SessionMetadata;
use crate::projection::{self, MessageProjection};
use crate::{ChatEvent, SharedChatEvent, Store, StoreResult};

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
    "type": event_types::SCHEDULED_TRIGGER,
    "content": content,
    "timestamp": charliebot_core::timefmt::utc_now_iso(),
  })
  .into()
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
  .into()
}

/// Return the chat-event ids answered by one master_done event.
pub fn master_done_input_event_ids<T: charliebot_core::event_types::EventFields>(event: &T) -> Vec<String> {
  if let Some(ids) = event.field(event_types::INPUT_EVENT_IDS) {
    return ids.string_items().into_iter().map(str::to_string).collect();
  }
  event
    .field(event_types::INPUT_EVENT_ID)
    .and_then(charliebot_core::event_types::EventFields::as_str_field)
    .filter(|id| !id.is_empty())
    .map(|id| vec![id.to_string()])
    .unwrap_or_default()
}

impl Store {
  /// Build an agent-relay event for the target session's event log.
  pub fn build_agent_message_event(&self, _session_id: &str, text: &str, provenance: MessageProvenance) -> ChatEvent {
    build_agent_message_event(text, provenance)
  }

  /// Return the global input-event ids answered by a master_done event.
  pub fn master_done_input_event_ids<T: charliebot_core::event_types::EventFields>(&self, event: &T) -> Vec<String> {
    master_done_input_event_ids(event)
  }

  /// Convert events with this store's feature renderers.
  pub fn events_to_messages(&self, events: &[ChatEvent]) -> StoreResult<Vec<Value>> {
    projection::events_to_messages(events, 0, self.event_renderers.clone())
  }

  /// Convert a shared event-cache slice without copying event payloads.
  pub fn events_to_messages_shared(&self, events: &[SharedChatEvent]) -> StoreResult<Vec<Value>> {
    projection::events_to_messages(events, 0, self.event_renderers.clone())
  }

  /// Convert events with this store's feature renderers in the global event-index space.
  pub fn events_to_messages_at(&self, events: &[ChatEvent], event_index_offset: usize) -> StoreResult<Vec<Value>> {
    projection::events_to_messages(events, event_index_offset, self.event_renderers.clone())
  }

  /// Convert events with this store's feature renderers and preserve a live draft.
  pub fn events_to_view(&self, events: &[ChatEvent]) -> StoreResult<(Vec<Value>, Option<Value>)> {
    projection::events_to_view(events, 0, self.event_renderers.clone())
  }

  /// Convert events to a message view with the supplied global event-index offset.
  pub fn events_to_view_at(
    &self,
    events: &[ChatEvent],
    event_index_offset: usize,
  ) -> StoreResult<(Vec<Value>, Option<Value>)> {
    projection::events_to_view(events, event_index_offset, self.event_renderers.clone())
  }

  /// Build a message projection over derived events with this store's feature renderers.
  pub fn build_message_projection(
    &self,
    events: &[SharedChatEvent],
    event_index_offset: usize,
  ) -> StoreResult<MessageProjection> {
    MessageProjection::new(events, event_index_offset, self.event_renderers.clone())
  }

  /// Return one session or an error carrying the route's 404 detail.
  pub async fn require_session(&self, session_id: &str) -> StoreResult<SessionMetadata> {
    self.get_session(session_id).await?.ok_or_else(|| crate::StoreError::not_found(SESSION_NOT_FOUND_DETAIL))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::compact_events;
  use crate::store::test_support::{assistant_text_event, make_test_store, queued_user_reorder_events};

  #[test]
  fn agent_message_event_carries_only_the_cross_session_provenance() {
    let event = build_agent_message_event(
      "Hello from the survey.",
      MessageProvenance { from_session: "source".to_string(), from_session_name: "Survey".to_string() },
    );
    assert_eq!(event["type"].as_str(), Some("agent_message"));
    assert_eq!(event["content"].as_str(), Some("Hello from the survey."));
    assert_eq!(event["from_session"].as_str(), Some("source"));
    assert_eq!(event["from_session_name"].as_str(), Some("Survey"));
    assert!(event["timestamp"].as_str().is_some());
    assert_eq!(event.as_object().unwrap().len(), 5);
  }

  #[test]
  fn scheduled_trigger_event_has_no_user_attachment_fields() {
    let event = build_scheduled_trigger_event("wake for the due digest");
    assert_eq!(event["type"].as_str(), Some("scheduled_trigger"));
    assert_eq!(event["content"].as_str(), Some("wake for the due digest"));
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

  fn timed(mut event: Value, timestamp: &str) -> Value {
    event["timestamp"] = json!(timestamp);
    event
  }

  #[test]
  fn queued_input_renders_inside_the_turn_that_answers_it() {
    let events = vec![
      json!({"type": "user", "content": "first question", "timestamp": "t1"}),
      timed(assistant_text_event("first answer"), "t2"),
      json!({"type": "user", "content": "second question", "timestamp": "t3"}),
      timed(assistant_text_event("mid-round answer"), "t4"),
      json!({"type": "master_done", "still_thinking": true, "timestamp": "t5"}),
      timed(assistant_text_event("second answer"), "t6"),
      json!({"type": "master_done", "thinking_seconds": 7, "timestamp": "t7"}),
    ];

    let messages = projection::events_to_messages(&compact_events(&events), 0, Default::default()).unwrap();
    let (view_messages, pending) = projection::events_to_view(&compact_events(&events), 0, Default::default()).unwrap();

    let roles: Vec<&str> = messages.iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "separator", "assistant", "separator"]);
    let ids: Vec<&str> = messages.iter().map(|message| message["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["legacy:0", "legacy:1", "legacy:2", "legacy:3", "legacy:4", "legacy:5", "legacy:6"]);
    assert!(messages[4]["thinking_seconds"].is_null());
    assert_eq!(messages[6]["thinking_seconds"], 7);
    assert_eq!(view_messages, messages);
    assert!(pending.is_none());
  }

  #[test]
  fn stable_history_orders_queued_user_between_completed_runs() {
    let events = queued_user_reorder_events();

    let messages = projection::events_to_messages(&compact_events(&events), 0, Default::default()).unwrap();
    let (view_messages, pending) = projection::events_to_view(&compact_events(&events), 0, Default::default()).unwrap();

    assert_eq!(view_messages, messages);
    assert!(pending.is_none());
    let roles: Vec<&str> = messages.iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["assistant", "separator", "user", "assistant", "separator"]);
    let ids: Vec<&str> = messages.iter().map(|message| message["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["assistant-1", "done-1", "queued-user", "assistant-2", "done-2"]);
    assert_eq!(messages[0]["thinking"], "final thought");
    assert_eq!(messages[0]["tools"][0]["output"], "report contents");
  }
}
