//! The Slack entrypoint service and its configuration gate.

use charliebot_chat_threads::channel_service::{ChannelService, ChatChannel};
use charliebot_core::credentials::{CredentialValue, Credentials};
use charliebot_registry::FeatureContext;

use crate::config::SlackConfig;
use crate::slack_listener::SlackListener;

/// Starts Socket Mode and the boot backfill when Slack credentials and users are configured.
pub(crate) type SlackService = ChannelService<SlackListener>;

impl ChatChannel for SlackListener {
  const NAME: &'static str = crate::config::NAME;

  fn listener() -> Self {
    SlackListener
  }

  fn allows(features: &FeatureContext) -> anyhow::Result<bool> {
    let config = features.config.get()?;
    let credentials = features.credentials.get()?;
    Ok(configured(config.section::<SlackConfig>(Self::NAME), &credentials))
  }
}

fn configured(config: &SlackConfig, credentials: &Credentials) -> bool {
  credentials.get(crate::config::NAME, "bot_token").is_some_and(CredentialValue::is_truthy)
    && credentials.get(crate::config::NAME, "app_token").is_some_and(CredentialValue::is_truthy)
    && !config.allowed_user_ids.is_empty()
}

#[cfg(test)]
mod tests {
  use charliebot_core::credentials::load_credentials;

  use super::*;

  fn credentials(text: &str) -> (tempfile::TempDir, Credentials) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.yaml"), text).unwrap();
    let credentials = load_credentials(dir.path()).unwrap();
    (dir, credentials)
  }

  #[test]
  fn the_entrypoint_requires_both_tokens_and_at_least_one_allowed_user() {
    let (_dir, complete_credentials) = credentials("slack:\n  bot_token: bot\n  app_token: app\n");
    assert!(!configured(&SlackConfig::default(), &complete_credentials));
    assert!(!configured(
      &SlackConfig { allowed_user_ids: vec!["U0SYNTHETIC".to_string()] },
      &credentials("slack:\n  bot_token: bot\n").1,
    ));
    assert!(configured(&SlackConfig { allowed_user_ids: vec!["U0SYNTHETIC".to_string()] }, &complete_credentials,));
  }
}
