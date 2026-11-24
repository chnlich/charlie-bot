//! The thread-origin fields: the session metadata fields that name the chat thread a session answers.
//!
//! A chat platform package registers its origin field once, with its platform name, beside its metadata slot
//! (`Registry::thread_origin_field`). The thread-session test and the channel label read this one list. Only the
//! Threads sidebar and the thread-reply checks use them; the web-session gates no longer do.

use std::sync::Mutex;

use anyhow::Context;
use charliebot_store::metadata::SessionMetadata;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The registered `(platform, origin field)` pairs.
static THREAD_ORIGIN_FIELDS: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());

/// The registration of `field` as the thread-origin field of `platform`. Recording a pair again changes nothing.
pub fn register_origin_field(platform: &'static str, field: &'static str) {
  let mut fields = THREAD_ORIGIN_FIELDS.lock().expect("the thread-origin field list is not poisoned");
  if !fields.contains(&(platform, field)) {
    fields.push((platform, field));
  }
}

/// True when `meta` names a chat thread: a registered origin field is present and not null.
pub fn is_thread_session(meta: &SessionMetadata) -> bool {
  platform_of(meta).is_some()
}

/// The platform whose origin field `meta` holds, or `None` for a session bound to no chat thread.
pub fn platform_of(meta: &SessionMetadata) -> Option<&'static str> {
  let fields = THREAD_ORIGIN_FIELDS.lock().expect("the thread-origin field list is not poisoned");
  fields
    .iter()
    .find(|(_, field)| meta.extra(field).is_some_and(|origin| !origin.is_null()))
    .map(|(platform, _)| *platform)
}

/// The session's origin stored under `field`, deserialized as `T`. An absent or null field reads as `None`; a
/// stored value that does not deserialize fails, and the error names `field` and `expected`.
pub fn read_origin<T: DeserializeOwned>(
  meta: &SessionMetadata,
  field: &str,
  expected: &str,
) -> anyhow::Result<Option<T>> {
  match meta.extra(field) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => T::deserialize(value).map(Some).with_context(|| format!("{field} is not {expected}")),
  }
}

#[cfg(test)]
mod tests {
  use charliebot_store::metadata::TaskProfile;
  use serde_json::json;

  use super::*;

  #[test]
  fn a_bound_session_names_its_platform_and_an_unbound_one_names_none() {
    register_origin_field("discord", "discord_origin");
    register_origin_field("slack", "slack_origin");
    let mut discord = SessionMetadata::new("s", "s", TaskProfile::Manager);
    discord.extra.insert("discord_origin".to_string(), json!({"guild_id": "G1"}));
    let mut nulled = SessionMetadata::new("s", "s", TaskProfile::Manager);
    nulled.extra.insert("slack_origin".to_string(), Value::Null);
    let plain = SessionMetadata::new("s", "s", TaskProfile::Manager);

    assert_eq!(platform_of(&discord), Some("discord"));
    assert!(is_thread_session(&discord));
    assert_eq!(platform_of(&nulled), None);
    assert_eq!(platform_of(&plain), None);
    assert!(!is_thread_session(&plain));
  }
}
