//! The Discord package's turn contribution (`src/features/discord/turn_contribution.py`). It renders `discord_reply`
//! events as chat lines. The round-end audit, the thread reply contract and the other turn steps are not ported yet.

use charliebot_core::pyfmt;
use charliebot_registry::turn::{EventRenderer, TurnContribution};
use serde_json::{Value, json};

use crate::event_types::DISCORD_REPLY;

/// The Discord turn contribution.
pub struct DiscordTurnContribution;

impl TurnContribution for DiscordTurnContribution {
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(DISCORD_REPLY, discord_reply_message)]
  }
}

/// `_discord_reply_message`: the chat line of a `discord_reply` event.
fn discord_reply_message(event: &Value) -> Value {
  json!({"role": "system", "content": format!("Posted to Discord: {}", content_text(event))})
}

/// The event's `content` as Python text, or `""` when the event has none (`event.get("content", "")`).
fn content_text(event: &Value) -> String {
  event.get("content").map(pyfmt::python_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn discord_reply_renders_its_content() {
    let event = json!({"content": "Done"});
    assert_eq!(discord_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Discord: Done"}"#);
  }

  #[test]
  fn discord_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(discord_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Discord: "}"#);
  }

  #[test]
  fn discord_reply_uses_python_string_conversion_for_non_string_content() {
    let event = json!({"content": {"message": "Done", "sent": true}});
    assert_eq!(
      discord_reply_message(&event).to_string(),
      r#"{"role":"system","content":"Posted to Discord: {'message': 'Done', 'sent': True}"}"#
    );
  }
}
