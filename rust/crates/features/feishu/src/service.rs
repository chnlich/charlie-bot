//! The Feishu long-connection service and its configuration gate.

use charliebot_chat_threads::channel_service::ChannelService;
use charliebot_core::credentials::{CredentialValue, Credentials};
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;

use crate::config::FeishuConfig;
use crate::listener::FeishuListener;

/// Starts the Feishu long connection and boot backfill when the app's credentials and an allowed user are configured.
pub(crate) struct FeishuService {
  entrypoint: ChannelService<FeishuListener>,
}

impl Default for FeishuService {
  fn default() -> Self {
    Self { entrypoint: ChannelService::new("feishu", FeishuListener) }
  }
}

impl Service for FeishuService {
  fn name(&self) -> &'static str {
    "feishu"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let config = ctx.features.config.get()?;
      let credentials = ctx.features.credentials.get()?;
      let feishu = config.section::<FeishuConfig>("feishu");
      self.entrypoint.start(ctx, configured(feishu, &credentials));
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { self.entrypoint.stop().await })
  }
}

fn configured(config: &FeishuConfig, credentials: &Credentials) -> bool {
  ["app_id", "app_secret"].iter().all(|key| credentials.get("feishu", key).is_some_and(CredentialValue::is_truthy))
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
  fn the_entrypoint_requires_the_app_credentials_and_an_allowed_user() {
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
