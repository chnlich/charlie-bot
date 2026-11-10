//! The Feishu long-connection service and its configuration gate.

use charliebot_chat_threads::channel_service::{ChannelService, ChatChannel};
use charliebot_core::credentials::{CredentialValue, Credentials};
use charliebot_registry::FeatureContext;

use crate::config::FeishuConfig;
use crate::listener::FeishuListener;
use crate::package::NAME;

/// The Feishu long connection and boot backfill, started when the app's credentials and an owner are configured.
pub(crate) type FeishuService = ChannelService<FeishuListener>;

impl ChatChannel for FeishuListener {
  const NAME: &'static str = crate::package::NAME;

  fn listener() -> Self {
    FeishuListener
  }

  fn allows(features: &FeatureContext) -> anyhow::Result<bool> {
    let config = features.config.get()?;
    let credentials = features.credentials.get()?;
    Ok(configured(config.section::<FeishuConfig>(NAME), &credentials))
  }
}

fn configured(config: &FeishuConfig, credentials: &Credentials) -> bool {
  ["app_id", "app_secret"].iter().all(|key| credentials.get(NAME, key).is_some_and(CredentialValue::is_truthy))
    && !config.allowed_users.is_empty()
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use charliebot_core::credentials::load_credentials;

  use super::*;

  fn credentials(text: &str) -> (tempfile::TempDir, Credentials) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("credentials.yaml"), text).unwrap();
    let credentials = load_credentials(dir.path()).unwrap();
    (dir, credentials)
  }

  #[test]
  fn the_entrypoint_requires_the_app_credentials_and_an_owner() {
    let (_dir, app) = credentials("feishu:\n  app_id: cli_synthetic\n  app_secret: synthetic-secret\n");
    let (_dir, no_secret) = credentials("feishu:\n  app_id: cli_synthetic\n  app_secret: \"\"\n");
    let allowed = FeishuConfig {
      allowed_users: BTreeMap::from([(
        "ou_0000000000000000000000000000000000000001".to_string(),
        "tester".to_string(),
      )]),
    };
    assert!(!configured(&FeishuConfig::default(), &app));
    assert!(!configured(&allowed, &no_secret));
    assert!(configured(&allowed, &app));
  }
}
