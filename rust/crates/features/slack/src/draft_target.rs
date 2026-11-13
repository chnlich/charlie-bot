//! The Slack draft target: a channel post, a thread reply, a DM, or a group DM.
//!
//! The CLI validates the flags with this module before it registers, and the server validates the same target again
//! when it writes the event, so both sides share one rule.

use serde_json::{Map, Value, json};

/// The canonical form of one Slack target, `{channel, thread}` or `{users}`. The failure is the sentence that names
/// why the target is refused.
pub fn validate_target(target: &Value) -> Result<Value, String> {
  let Some(object) = target.as_object() else {
    return Err("the Slack target must be an object".to_string());
  };
  if let Some(unknown) = object.keys().find(|key| !matches!(key.as_str(), "channel" | "thread" | "users")) {
    return Err(format!("the Slack target has no field {unknown}"));
  }
  match (object.get("channel"), object.get("users")) {
    (Some(_), Some(_)) => Err("a Slack draft names either a channel or users, not both".to_string()),
    (None, None) => Err("a Slack draft names either a channel or users".to_string()),
    (Some(channel), None) => channel_target(object, channel),
    (None, Some(users)) => users_target(object, users),
  }
}

fn channel_target(object: &Map<String, Value>, channel: &Value) -> Result<Value, String> {
  let Some(channel) = channel.as_str().filter(|channel| valid_id(channel)) else {
    return Err("target.channel must be a Slack channel id".to_string());
  };
  let thread = match object.get("thread") {
    None | Some(Value::Null) => Value::Null,
    Some(Value::String(thread)) if valid_thread_ts(thread) => Value::String(thread.clone()),
    Some(_) => return Err("target.thread must be a Slack thread timestamp".to_string()),
  };
  Ok(json!({"channel": channel, "thread": thread}))
}

fn users_target(object: &Map<String, Value>, users: &Value) -> Result<Value, String> {
  if object.get("thread").is_some_and(|thread| !thread.is_null()) {
    return Err("a Slack draft replies in a thread only by channel".to_string());
  }
  let Some(users) = users.as_array() else {
    return Err("target.users must be a list of Slack user ids".to_string());
  };
  if users.is_empty() {
    return Err("target.users must name at least one Slack user".to_string());
  }
  let mut ids: Vec<&str> = Vec::with_capacity(users.len());
  for user in users {
    let Some(user) = user.as_str().filter(|user| valid_id(user)) else {
      return Err("target.users must hold Slack user ids".to_string());
    };
    if ids.contains(&user) {
      return Err(format!("target.users names {user} more than once"));
    }
    ids.push(user);
  }
  Ok(json!({"users": ids}))
}

/// The target's one-line card label.
pub fn target_label(target: &Value) -> String {
  if let Some(channel) = target.get("channel").and_then(Value::as_str) {
    return match target.get("thread").and_then(Value::as_str) {
      Some(thread) => format!("Slack thread {thread} in {channel}"),
      None => format!("Slack channel {channel}"),
    };
  }
  let users: Vec<&str> = target
    .get("users")
    .and_then(Value::as_array)
    .map(|users| users.iter().filter_map(Value::as_str).collect())
    .unwrap_or_default();
  if users.len() == 1 {
    format!("Slack DM with {}", users[0])
  } else {
    format!("Slack group DM with {}", users.join(", "))
  }
}

/// A Slack id: non-empty and made of ASCII letters and digits.
fn valid_id(id: &str) -> bool {
  !id.is_empty() && id.chars().all(|character| character.is_ascii_alphanumeric())
}

/// A Slack timestamp: digits, one dot, digits.
fn valid_thread_ts(ts: &str) -> bool {
  match ts.split_once('.') {
    Some((seconds, micros)) => {
      !seconds.is_empty()
        && !micros.is_empty()
        && seconds.chars().all(|character| character.is_ascii_digit())
        && micros.chars().all(|character| character.is_ascii_digit())
    }
    None => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_channel_post_canonicalizes_to_its_channel_with_no_thread() {
    assert_eq!(validate_target(&json!({"channel": "C123ABC"})).unwrap(), json!({"channel": "C123ABC", "thread": null}));
  }

  #[test]
  fn a_thread_reply_keeps_its_timestamp() {
    assert_eq!(
      validate_target(&json!({"channel": "C123ABC", "thread": "1700000000.000100"})).unwrap(),
      json!({"channel": "C123ABC", "thread": "1700000000.000100"})
    );
  }

  #[test]
  fn one_user_is_a_dm_and_several_are_a_group_dm() {
    assert_eq!(validate_target(&json!({"users": ["U1ABC"]})).unwrap(), json!({"users": ["U1ABC"]}));
    assert_eq!(validate_target(&json!({"users": ["U1ABC", "U2XYZ"]})).unwrap(), json!({"users": ["U1ABC", "U2XYZ"]}));
  }

  #[test]
  fn the_label_names_the_target() {
    assert_eq!(target_label(&json!({"channel": "C1", "thread": null})), "Slack channel C1");
    assert_eq!(target_label(&json!({"channel": "C1", "thread": "1.0"})), "Slack thread 1.0 in C1");
    assert_eq!(target_label(&json!({"users": ["U1"]})), "Slack DM with U1");
    assert_eq!(target_label(&json!({"users": ["U1", "U2"]})), "Slack group DM with U1, U2");
  }

  #[test]
  fn both_or_neither_target_form_fails() {
    assert!(validate_target(&json!({"channel": "C1", "users": ["U1"]})).is_err());
    assert!(validate_target(&json!({})).is_err());
    assert!(validate_target(&json!({"thread": "1.0"})).is_err());
  }

  #[test]
  fn an_unparseable_id_a_thread_and_an_empty_user_list_fail() {
    for target in [
      json!({"channel": ""}),
      json!({"channel": "C 1"}),
      json!({"channel": "C1", "thread": "not-a-ts"}),
      json!({"users": []}),
      json!({"users": ["U1", "U1"]}),
      json!({"users": [""]}),
      json!({"users": ["U1"], "thread": "1.0"}),
      json!({"channel": "C1", "note": "x"}),
    ] {
      assert!(validate_target(&target).is_err(), "{target}");
    }
  }
}
