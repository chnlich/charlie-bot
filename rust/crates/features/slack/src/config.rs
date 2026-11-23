//! The `slack:` section: the allow-list for Slack summons and the colleague list.

use charliebot_core::config::{ConfigSection, SectionModel};
use serde::{Deserialize, Serialize};

const LEGACY_KEYS: &[(&str, &str)] = &[
  ("slack_allowed_user_ids", "slack.allowed_user_ids"),
  ("credentials: slack_bot_token", "slack.bot_token"),
  ("credentials: slack_app_token", "slack.app_token"),
  ("credentials: slack_user_token", "slack.user_token"),
];

/// Slack user ids allowed to summon a session (the operator), and colleague ids whose summons the operator's presence
/// in their thread admits. A config that lists one id in both fails at load, and so does an id listed twice in the
/// colleague list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawSlackConfig")]
pub(crate) struct SlackConfig {
  pub allowed_user_ids: Vec<String>,
  pub colleague_user_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlackConfig {
  #[serde(default)]
  allowed_user_ids: Vec<String>,
  #[serde(default)]
  colleague_user_ids: Vec<String>,
}

impl TryFrom<RawSlackConfig> for SlackConfig {
  type Error = String;

  fn try_from(raw: RawSlackConfig) -> Result<Self, Self::Error> {
    let mut colleagues = std::collections::HashSet::new();
    for id in &raw.colleague_user_ids {
      if !colleagues.insert(id.as_str()) {
        return Err(format!("colleague_user_ids lists {id} more than once"));
      }
      if raw.allowed_user_ids.contains(id) {
        return Err(format!("{id} is in both allowed_user_ids and colleague_user_ids"));
      }
    }
    Ok(Self { allowed_user_ids: raw.allowed_user_ids, colleague_user_ids: raw.colleague_user_ids })
  }
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

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};

  use super::SlackConfig;

  fn load(raw: Value) -> Result<SlackConfig, serde_json::Error> {
    serde_json::from_value(raw)
  }

  #[test]
  fn an_id_in_both_lists_fails_the_load() {
    let error = load(json!({"allowed_user_ids": ["U_OPERATOR"], "colleague_user_ids": ["U_OPERATOR"]}))
      .expect_err("an id in both lists is refused");
    assert!(error.to_string().contains("U_OPERATOR is in both"), "{error}");
  }

  #[test]
  fn a_colleague_id_listed_twice_fails_the_load() {
    let error =
      load(json!({"colleague_user_ids": ["U_PEER", "U_PEER"]})).expect_err("a repeated colleague id is refused");
    assert!(error.to_string().contains("U_PEER more than once"), "{error}");
  }

  #[test]
  fn disjoint_lists_load_together() {
    let config =
      load(json!({"allowed_user_ids": ["U_OPERATOR"], "colleague_user_ids": ["U_PEER"]})).expect("disjoint lists load");
    assert_eq!(config.allowed_user_ids, ["U_OPERATOR"]);
    assert_eq!(config.colleague_user_ids, ["U_PEER"]);
  }
}
