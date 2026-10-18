//! The Slack package's turn contribution (`src/features/slack/turn_contribution.py`). It renders `slack_reply` events
//! as chat lines. The round-end audit, the thread reply contract and the other turn steps are not ported yet.

use charliebot_registry::turn::{EventRenderer, TurnContribution};
use serde_json::{Value, json};

use crate::event_types::SLACK_REPLY;

/// The Slack turn contribution.
pub struct SlackTurnContribution;

impl TurnContribution for SlackTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(SLACK_REPLY, slack_reply_message)]
  }
}

/// `_slack_reply_message`: the chat line of a `slack_reply` event.
fn slack_reply_message(event: &Value) -> Value {
  json!({"role": "system", "content": format!("Posted to Slack: {}", content_text(event))})
}

/// The event's `content` as text, or `""` when the event has none (`event.get("content", "")`). A non-string
/// content renders as its JSON text.
fn content_text(event: &Value) -> String {
  match event.get("content") {
    None => String::new(),
    Some(Value::String(text)) => text.clone(),
    Some(other) => other.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn slack_reply_renders_its_content() {
    let event = json!({"content": "Done"});
    assert_eq!(slack_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Slack: Done"}"#);
  }

  #[test]
  fn slack_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(slack_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Slack: "}"#);
  }
}
