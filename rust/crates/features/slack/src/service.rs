//! The Slack entrypoint service and its Python `service.py` configuration gate.

use charliebot_chat_threads::channel_service::ChannelService;
use charliebot_core::credentials::{CredentialValue, Credentials};
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;

use crate::config::SlackConfig;
use crate::slack_listener::SlackListener;

/// Starts Socket Mode and the boot backfill when Slack credentials and users are configured.
pub(crate) struct SlackService {
  entrypoint: ChannelService<SlackListener>,
}

impl Default for SlackService {
  fn default() -> Self {
    Self { entrypoint: ChannelService::new("slack", SlackListener) }
  }
}

impl Service for SlackService {
  fn name(&self) -> &'static str {
    "slack"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let config = ctx.features.config.get()?;
      let credentials = ctx.features.credentials.get()?;
      let slack = config.section::<SlackConfig>("slack");
      self.entrypoint.start(ctx, configured(slack, &credentials));
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { self.entrypoint.stop().await })
  }
}

fn configured(config: &SlackConfig, credentials: &Credentials) -> bool {
  credentials.get("slack", "bot_token").is_some_and(CredentialValue::is_truthy)
    && credentials.get("slack", "app_token").is_some_and(CredentialValue::is_truthy)
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
