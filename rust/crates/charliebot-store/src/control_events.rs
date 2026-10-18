//! Stable task-tree identifiers and the ordered headers for durable control facts.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::metadata::UtcDateTime;

const TASK_ID_NAMESPACE_NAME: &str = "charliebot/session-task-tree";
pub const ACTOR_USER: &str = "user";
pub const ACTOR_AGENT: &str = "agent";
pub const ACTOR_SYSTEM: &str = "system";

fn task_id_namespace() -> Uuid {
  Uuid::new_v5(&Uuid::NAMESPACE_URL, TASK_ID_NAMESPACE_NAME.as_bytes())
}

fn stable_id(name: &str) -> String {
  Uuid::new_v5(&task_id_namespace(), name.as_bytes()).to_string()
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

/// The stable report id for one source event delivered to one recipient.
pub fn stable_child_report_id(child_session_id: &str, source_event_id: &str, recipient_session_id: &str) -> String {
  stable_id(&format!("child-report:{child_session_id}:{source_event_id}:{recipient_session_id}"))
}

/// The stable withheld-launch fact id for one `(run_id, reason)` pair.
pub fn stable_withheld_event_id(run_id: &str, reason: &str) -> String {
  stable_id(&format!("run-launch-withheld:{run_id}:{reason}"))
}

/// The SHA-256 digest of UTF-8 text as lowercase hexadecimal.
pub fn sha256_hex(text: &str) -> String {
  format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// The default request id for one delegation description.
pub fn derived_delegate_request_id(session_id: &str, task_type: &str, description: &str) -> String {
  let material = [session_id, task_type, description].join("\0");
  format!("delegate-{}", &sha256_hex(&material)[..24])
}

/// Build a control event in Python header and payload insertion order.
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
  event
    .insert("timestamp".to_string(), Value::String(timestamp.unwrap_or_else(UtcDateTime::now).to_python_isoformat()));
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
    "task_created",
    actor,
    task_id,
    event_id,
    timestamp,
    [
      ("request_id".to_string(), Value::String(request_id.to_string())),
      ("task_parent_id".to_string(), task_parent_id.map_or(Value::Null, |value| Value::String(value.to_string()))),
      ("task_spec_hash".to_string(), task_spec_hash.map_or(Value::Null, |value| Value::String(value.to_string()))),
    ],
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn stable_run_id_matches_the_python_uuid5_namespace() {
    assert_eq!(stable_run_id("session-a", "request-1"), "1a41849d-357a-5529-96ac-75565f82bd39");
  }

  #[test]
  fn control_event_keeps_python_header_and_payload_order() {
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
      charliebot_core::pyfmt::json_dumps(
        &event,
        &charliebot_core::pyfmt::JsonStyle {
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
