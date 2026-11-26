//! The `slack:` section: the allow-list for Slack summons and the colleague list. The `slack` section of
//! `credentials.yaml` has the same name, and its readers are here too.

use charliebot_core::config::{ConfigError, ConfigSection, SectionModel};
use charliebot_core::credentials::{CredentialValue, Credentials};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LEGACY_KEYS: &[(&str, &str)] = &[
  ("slack_allowed_user_ids", "slack.allowed_user_ids"),
  ("credentials: slack_bot_token", "slack.bot_token"),
  ("credentials: slack_app_token", "slack.app_token"),
  ("credentials: slack_user_token", "slack.user_token"),
];

/// Slack user ids allowed to summon a session (the operator), and colleague ids whose summons the operator's presence
/// in their thread admits. The fields of `RawSlackConfig` document each key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawSlackConfig")]
pub(crate) struct SlackConfig {
  pub allowed_user_ids: Vec<String>,
  pub colleague_user_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlackConfig {
  /// Set this to the Slack user ids of the operators. A mention or thread message from a listed id may start or
  /// continue a session. A message from a user in neither list is ignored. While this list is empty, the Slack listener
  /// does not start. The first id is the host owner, and a failed backup sends that id a DM.
  #[serde(default)]
  allowed_user_ids: Vec<String>,
  /// Set this to the Slack user ids of colleagues whose mentions and thread messages may start or continue a session.
  /// Such a message is admitted only when an operator is a member of the channel, and the thread holds a message from
  /// an operator or one that mentions an operator. A message that fails this check is dropped and logged.
  /// An id that is also in `allowed_user_ids`, or that is listed twice here, fails the config load, and the error names
  /// the id. In a thread that a colleague has posted in, a run whose inputs are not all from an operator refuses its
  /// requests for agent work.
  #[serde(default)]
  colleague_user_ids: Vec<String>,
}

impl SlackConfig {
  /// The role of a thread message's sender: `operator` for an allowed id, `colleague` for a listed colleague, and
  /// `other` for everyone else, including a message with no sender.
  pub(crate) fn role_of(&self, message: &Value) -> &'static str {
    match message.get("user").and_then(Value::as_str) {
      Some(user) if self.allowed_user_ids.iter().any(|id| id == user) => "operator",
      Some(user) if self.colleague_user_ids.iter().any(|id| id == user) => "colleague",
      _ => "other",
    }
  }
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

/// The name of the registered `slack:` config section, and of the `slack` section of `credentials.yaml`.
pub(crate) const NAME: &str = "slack";

/// The `slack` credential field of the app-level token, which Socket Mode connects with.
pub(crate) const APP_TOKEN_FIELD: &str = "app_token";

/// The `slack` credential field `field`, or `None` when `credentials.yaml` leaves it unset.
pub(crate) fn credential_value<'a>(credentials: &'a Credentials, field: &str) -> Option<&'a CredentialValue> {
  credentials.get(NAME, field)
}

/// The `slack` credential field `field`, which must be set.
pub(crate) fn require_credential(credentials: &Credentials, field: &str) -> Result<String, ConfigError> {
  credentials.require(NAME, field)
}

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
