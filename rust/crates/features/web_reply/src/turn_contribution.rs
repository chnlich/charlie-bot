//! The web-reply package's turn contribution (`src/features/web_reply/turn_contribution.py`). It renders the
//! `web_reply`, `web_reply_notice` and `web_status` events as chat lines. The reply rules, the round-end audit and the
//! unread claim are not ported yet.

use charliebot_core::event_types::{WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS};
use charliebot_registry::turn::{EventRenderer, TurnContribution};
use serde_json::{Value, json};

/// The web-reply turn contribution.
pub struct WebReplyTurnContribution;

impl TurnContribution for WebReplyTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(WEB_REPLY, web_reply_message), (WEB_REPLY_NOTICE, web_reply_notice_message), (WEB_STATUS, web_status_message)]
  }
}

/// `_web_reply_message`: the chat line of a `web_reply` event.
fn web_reply_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY, "content": content(event)})
}

/// `_web_reply_notice_message`: the chat line of a `web_reply_notice` event.
fn web_reply_notice_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY_NOTICE, "content": content(event)})
}

/// `_web_status_message`: the chat line of a `web_status` event, with the event's status value, or `null`.
fn web_status_message(event: &Value) -> Value {
  json!({"role": WEB_STATUS, "content": content(event), "web_status": event.get(WEB_STATUS)})
}

/// The event's `content`, or `""` when the event has none (`event.get("content", "")`).
fn content(event: &Value) -> Value {
  event.get("content").cloned().unwrap_or_else(|| json!(""))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn web_reply_renders_its_content() {
    let event = json!({"content": "Hi"});
    assert_eq!(web_reply_message(&event).to_string(), r#"{"role":"web_reply","content":"Hi"}"#);
  }

  #[test]
  fn web_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(web_reply_message(&event).to_string(), r#"{"role":"web_reply","content":""}"#);
  }

  #[test]
  fn web_reply_notice_renders_its_content() {
    let event = json!({"content": "Queued"});
    assert_eq!(web_reply_notice_message(&event).to_string(), r#"{"role":"web_reply_notice","content":"Queued"}"#);
  }

  #[test]
  fn web_reply_notice_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(web_reply_notice_message(&event).to_string(), r#"{"role":"web_reply_notice","content":""}"#);
  }

  #[test]
  fn web_status_renders_its_content_and_status() {
    let event = json!({"content": "Working", "web_status": "running"});
    assert_eq!(
      web_status_message(&event).to_string(),
      r#"{"role":"web_status","content":"Working","web_status":"running"}"#
    );
  }

  #[test]
  fn web_status_without_content_or_status_renders_empty_and_null() {
    let event = json!({});
    assert_eq!(web_status_message(&event).to_string(), r#"{"role":"web_status","content":"","web_status":null}"#);
  }
}
