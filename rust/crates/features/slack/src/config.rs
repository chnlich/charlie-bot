//! The `slack:` section (`src/features/slack/config.py`): the allow-list for Slack summons.

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use serde::{Deserialize, Serialize};

const LEGACY_KEYS: &[(&str, &str)] = &[
  ("slack_allowed_user_ids", "slack.allowed_user_ids"),
  ("credentials: slack_bot_token", "slack.bot_token"),
  ("credentials: slack_app_token", "slack.app_token"),
  ("credentials: slack_user_token", "slack.user_token"),
];

/// Slack user ids allowed to summon a session.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlackConfig {
  #[serde(default)]
  pub allowed_user_ids: Vec<String>,
}

/// The registered `slack:` config section.
pub(crate) fn section() -> ConfigSection {
  ConfigSection { key: "slack", legacy_keys: LEGACY_KEYS, parse: parse_section, dump: dump_section }
}

/// Render the section as its model dumps it (`charliebot config get slack`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<SlackConfig>().ok_or("the slack section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: SlackConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("slack: {error}"))?;
  Ok(Arc::new(settings))
}
