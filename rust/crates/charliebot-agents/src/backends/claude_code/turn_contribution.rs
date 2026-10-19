//! Chat rendering for account login notices from Claude Code.

use serde_json::{Value, json};

use charliebot_registry::turn::TurnContribution;

use super::event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED;

pub struct ClaudeCodeTurnContribution;

pub fn render_login_required(_event: &Value) -> Value {
  json!({
    "role": "system",
    "kind": CLAUDE_ACCOUNT_LOGIN_REQUIRED,
    "content": "One account in the Claude pool needs a new login; see the usage panel."
  })
}

impl TurnContribution for ClaudeCodeTurnContribution {
  fn class_name(&self) -> &'static str {
    "ClaudeCodeTurnContribution"
  }

  fn event_renderers(&self) -> Vec<(&'static str, charliebot_registry::turn::EventRenderer)> {
    vec![(CLAUDE_ACCOUNT_LOGIN_REQUIRED, render_login_required)]
  }
}
