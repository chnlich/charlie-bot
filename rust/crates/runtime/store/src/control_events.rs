//! Stable task-tree identifiers and the ordered headers for durable control facts.

use charliebot_core::event_types::{self, EventFields};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::metadata::UtcDateTime;
use crate::{StoreError, StoreResult};

const TASK_ID_NAMESPACE_NAME: &str = "charliebot/session-task-tree";
pub const ACTOR_USER: &str = "user";
pub const ACTOR_AGENT: &str = "agent";
pub const ACTOR_SYSTEM: &str = "system";
/// The prefix of the request id of every round that a scheduled task starts: `cron:<task>:<time>:leaf`. A delegation
/// outside a bound task may not carry it, so a round in flight is only ever a scheduled task's.
pub const CRON_REQUEST_ID_PREFIX: &str = "cron:";

/// The field of a run record and a task-created event that carries the hash of the task spec.
pub const TASK_SPEC_HASH_FIELD: &str = "task_spec_hash";

/// Fields that belong to the request-event header or its shared task-input payload.
pub const REQUEST_EVENT_CORE_FIELDS: &[&str] = &[
  "id",
  "type",
  "timestamp",
  "actor",
  "source_session_id",
  "content",
  "request_id",
  event_types::UPLOADED_FILES,
  event_types::FROM_SESSION,
  event_types::FROM_SESSION_NAME,
  event_types::INPUT_MODE,
  event_types::EVENT_INDEX,
];

fn task_id_namespace() -> Uuid {
  Uuid::new_v5(&Uuid::NAMESPACE_URL, TASK_ID_NAMESPACE_NAME.as_bytes())
}

fn stable_id(name: &str) -> String {
  Uuid::new_v5(&task_id_namespace(), name.as_bytes()).to_string()
}

/// A fresh request id for a delivery that has no caller-supplied retry identity.
pub fn new_request_id() -> String {
  Uuid::new_v4().to_string()
}

/// The task id for one `(parent, request_id)` pair.
pub fn stable_task_id(task_parent_id: Option<&str>, request_id: &str) -> String {
  stable_id(&format!("task:{}:{request_id}", task_parent_id.unwrap_or("root")))
}

/// The run id for one `(session_id, request_id)` pair.
pub fn stable_run_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("run:{session_id}:{request_id}"))
}

/// The stable task-close-request fact id for one `(session_id, request_id)` pair.
pub fn stable_close_request_event_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("close-request:{session_id}:{request_id}"))
}

/// The stable blocked-close notice id for one `(session_id, request_id)` pair.
pub fn stable_close_blocked_notice_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("close-blocked-notice:{session_id}:{request_id}"))
}

/// The stable task-closed fact id for one `(session_id, request_id)` pair.
pub fn stable_close_event_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("task-closed:{session_id}:{request_id}"))
}

/// The stable task-reopened fact id for one `(session_id, request_id)` pair.
pub fn stable_reopen_event_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("task-reopened:{session_id}:{request_id}"))
}

/// The stable input-acknowledgement fact id for one `(session_id, request_id)` pair.
pub fn stable_input_ack_event_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("input-ack:{session_id}:{request_id}"))
}

/// The input event id bound to one `(session_id, request_id)` pair.
pub fn stable_request_event_id(session_id: &str, request_id: &str) -> String {
  stable_id(&format!("request:{session_id}:{request_id}"))
}

/// The request id one `(run_id, agent-send verb, target, text)` binds to across retries.
pub fn derived_agent_send_request_id(run_id: &str, verb: &str, target: &str, text: &str) -> String {
  let material = [run_id, verb, target, text].join("\0");
  format!("agent-send-{}", &sha256_hex(&material)[..24])
}

/// Return the stored event for a replay, or a conflict when its request payload differs. The events are
/// searched in place, so a compact cache needs no owned copy unless a replay matches.
pub fn replay_request_event<'a, E: EventFields + 'a>(
  mut events: impl Iterator<Item = &'a E>,
  event_id: &str,
  request_id: &str,
  event_type: &str,
  content: &str,
  fields: Option<&Value>,
) -> StoreResult<Option<Value>> {
  let Some(existing) = events.find(|event| event.field("id").and_then(EventFields::as_str_field) == Some(event_id))
  else {
    return Ok(None);
  };
  let existing = existing.to_owned_value();
  let extra_fields_match = match fields {
    None => true,
    Some(fields) => {
      let fields = fields.as_object().ok_or_else(|| StoreError::invalid("request event fields must be an object"))?;
      let existing_fields = existing.as_object().expect("stored request events are objects");
      let existing_extra_fields = existing_fields
        .iter()
        .filter(|(key, _)| !REQUEST_EVENT_CORE_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<String, Value>>();
      &existing_extra_fields == fields
    }
  };
  let matches = existing.get("request_id").and_then(Value::as_str) == Some(request_id)
    && existing.get("type").and_then(Value::as_str) == Some(event_type)
    && existing.get("content").and_then(Value::as_str) == Some(content)
    && extra_fields_match;
  if !matches {
    return Err(StoreError::conflict(vec![format!(
      "request_id {request_id} is already event {event_id} with a different type or content"
    )]));
  }
  Ok(Some(existing.clone()))
}

/// The stable report id for one source event delivered to one recipient.
pub fn stable_child_report_id(child_session_id: &str, source_event_id: &str, recipient_session_id: &str) -> String {
  stable_id(&format!("child-report:{child_session_id}:{source_event_id}:{recipient_session_id}"))
}

/// The stable withheld-launch fact id for one `(run_id, reason)` pair.
pub fn stable_withheld_event_id(run_id: &str, reason: &str) -> String {
  stable_id(&format!("run-launch-withheld:{run_id}:{reason}"))
}

/// The stable turn-launch notice fact id for one `(run_id, reason)` pair.
pub fn stable_turn_launch_event_id(run_id: &str, reason: &str) -> String {
  stable_id(&format!("turn-launch:{run_id}:{reason}"))
}

/// The SHA-256 digest of UTF-8 text as lowercase hexadecimal.
pub fn sha256_hex(text: &str) -> String {
  sha256_bytes_hex(text.as_bytes())
}

/// The SHA-256 digest of bytes as lowercase hexadecimal.
pub fn sha256_bytes_hex(bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(bytes))
}

/// The default request id for one delegation description.
pub fn derived_delegate_request_id(session_id: &str, task_type: &str, description: &str) -> String {
  let material = [session_id, task_type, description].join("\0");
  format!("delegate-{}", &sha256_hex(&material)[..24])
}

/// Build a control event in header and payload insertion order.
pub fn build_control_event(
  event_type: &str,
  actor: &str,
  source_session_id: &str,
  event_id: Option<&str>,
  timestamp: Option<UtcDateTime>,
  payload: impl IntoIterator<Item = (String, Value)>,
) -> Value {
  let mut event = Map::new();
  event.insert("id".to_string(), Value::String(event_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_string)));
  event.insert("type".to_string(), Value::String(event_type.to_string()));
  event.insert("timestamp".to_string(), Value::String(timestamp.unwrap_or_else(UtcDateTime::now).to_isoformat_text()));
  event.insert("actor".to_string(), Value::String(actor.to_string()));
  event.insert("source_session_id".to_string(), Value::String(source_session_id.to_string()));
  for (key, value) in payload {
    event.insert(key, value);
  }
  Value::Object(event)
}

/// Build the task-created fact with the shared task-tree header.
pub fn build_task_created_event(
  actor: &str,
  task_id: &str,
  request_id: &str,
  task_parent_id: Option<&str>,
  task_spec_hash: Option<&str>,
  event_id: Option<&str>,
  timestamp: Option<UtcDateTime>,
) -> Value {
  build_control_event(
    event_types::TASK_CREATED,
    actor,
    task_id,
    event_id,
    timestamp,
    [
      ("request_id".to_string(), Value::String(request_id.to_string())),
      (
        crate::metadata::TASK_PARENT_ID_FIELD.to_string(),
        task_parent_id.map_or(Value::Null, |value| Value::String(value.to_string())),
      ),
      (TASK_SPEC_HASH_FIELD.to_string(), task_spec_hash.map_or(Value::Null, |value| Value::String(value.to_string()))),
    ],
  )
}

/// The fields of one task-close fact: its outcome and summary, the report recipient, and the archive or sequence
/// detail its outcome carries.
pub struct CloseFact<'a> {
  pub session_id: &'a str,
  pub request_id: &'a str,
  pub outcome: &'a str,
  pub summary: &'a str,
  pub result_refs: &'a [String],
  pub run_ids: &'a [String],
  pub report_to: Option<&'a str>,
  pub sequence_outcome: Option<&'a str>,
  pub archived_with: Option<&'a str>,
  pub archived_by: Option<&'a str>,
  pub actor: &'a str,
}

/// Build the task-close fact from its fields. Every writer of a close fact passes through here, so the payload's
/// field names and order stay the store's.
pub fn close_event(fact: CloseFact<'_>) -> Value {
  let mut payload = vec![
    ("request_id".to_string(), json!(fact.request_id)),
    ("outcome".to_string(), json!(fact.outcome)),
    ("summary".to_string(), json!(fact.summary)),
    (event_types::RESULT_REFS.to_string(), json!(fact.result_refs)),
    ("run_ids".to_string(), json!(fact.run_ids)),
    ("report_to".to_string(), fact.report_to.map_or(Value::Null, |parent| json!(parent))),
  ];
  if let Some(archived_with) = fact.archived_with {
    payload.push(("archived_with".to_string(), json!(archived_with)));
  }
  if let Some(archived_by) = fact.archived_by {
    payload.push(("archived_by".to_string(), json!(archived_by)));
  }
  if let Some(sequence_outcome) = fact.sequence_outcome {
    payload.push((event_types::SEQUENCE_OUTCOME.to_string(), json!(sequence_outcome)));
  }
  build_control_event(
    event_types::TASK_CLOSED,
    fact.actor,
    fact.session_id,
    Some(&stable_close_event_id(fact.session_id, fact.request_id)),
    None,
    payload,
  )
}

/// Build an archived task-close fact with its report recipient and the subtree that archives it.
#[allow(clippy::too_many_arguments)]
pub fn archived_close_event(
  session_id: &str,
  request_id: &str,
  summary: &str,
  report_to: Option<&str>,
  archived_with: Option<&str>,
  archived_by: Option<&str>,
  actor: &str,
  result_refs: &[String],
  run_ids: &[String],
) -> Value {
  close_event(CloseFact {
    session_id,
    request_id,
    outcome: "archived",
    summary,
    result_refs,
    run_ids,
    report_to,
    sequence_outcome: None,
    archived_with,
    archived_by,
    actor,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn stable_run_id_matches_the_uuid5_namespace() {
    assert_eq!(stable_run_id("session-a", "request-1"), "1a41849d-357a-5529-96ac-75565f82bd39");
  }

  #[test]
  fn stable_turn_launch_event_id_matches_the_uuid5_namespace() {
    assert_eq!(stable_turn_launch_event_id("run-a", "reason"), "49a20cae-8223-5a11-bf89-46aa225aa122");
  }

  #[test]
  fn control_event_keeps_header_and_payload_order() {
    let event = build_control_event(
      "run_stop_requested",
      ACTOR_SYSTEM,
      "session-a",
      Some("event-a"),
      Some(UtcDateTime::parse("2025-01-02T03:04:05.123456Z").unwrap()),
      [
        ("request_id".to_string(), Value::String("stop-a".to_string())),
        ("run_id".to_string(), Value::String("run-a".to_string())),
      ],
    );
    assert_eq!(
      charliebot_core::text_repr::json_dumps(
        &event,
        &charliebot_core::text_repr::JsonStyle {
          ensure_ascii: false,
          sort_keys: false,
          item_separator: ", ",
          key_separator: ": ",
          indent: None,
        }
      ),
      "{\"id\": \"event-a\", \"type\": \"run_stop_requested\", \"timestamp\": \"2025-01-02T03:04:05.123456+00:00\", \"actor\": \"system\", \"source_session_id\": \"session-a\", \"request_id\": \"stop-a\", \"run_id\": \"run-a\"}"
    );
  }
}
