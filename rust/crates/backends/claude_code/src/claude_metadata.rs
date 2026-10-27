//! Accessors for Claude-owned session and thread metadata fields.

use charliebot_store::metadata::{SessionMetadata, ThreadMetadata};
use serde_json::Value;

pub fn account_of(meta: &SessionMetadata) -> Option<String> {
  meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
}

pub fn set_account(meta: &mut SessionMetadata, label: Option<&str>) {
  meta.set_extra("claude_account", label.map_or(Value::Null, |value| Value::String(value.to_string())));
}

pub fn session_id_of(thread: &ThreadMetadata) -> Option<String> {
  thread.extra.get("claude_session_id").and_then(Value::as_str).map(str::to_string)
}

pub fn set_session_id(thread: &mut ThreadMetadata, session_id: Option<&str>) {
  thread
    .extra
    .insert("claude_session_id".to_string(), session_id.map_or(Value::Null, |value| Value::String(value.to_string())));
}

#[cfg(test)]
mod tests {
  use charliebot_store::metadata::TaskProfile;
  use serde_json::json;

  use super::*;

  #[test]
  fn accessors_preserve_the_registered_slot_values() {
    let mut session = SessionMetadata::new("s", "S", TaskProfile::Manager);
    set_account(&mut session, Some("synthetic"));
    assert_eq!(account_of(&session).as_deref(), Some("synthetic"));
    set_account(&mut session, None);
    assert_eq!(account_of(&session), None);

    let mut thread: ThreadMetadata = serde_json::from_value(json!({
      "id":"t", "session_id":"s", "description":"synthetic"
    }))
    .unwrap();
    set_session_id(&mut thread, Some("native"));
    assert_eq!(session_id_of(&thread).as_deref(), Some("native"));
  }
}
