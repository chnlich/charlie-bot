//! The `slack:` section: the allow-list for Slack summons.

use charliebot_core::config::{ConfigSection, SectionModel};
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

/// The name of the registered `slack:` config section.
pub(crate) const NAME: &str = "slack";

impl SectionModel for SlackConfig {
  const KEY: &'static str = NAME;
}

/// The registered `slack:` config section.
pub(crate) fn section() -> ConfigSection {
  ConfigSection::of::<SlackConfig>(LEGACY_KEYS)
}
