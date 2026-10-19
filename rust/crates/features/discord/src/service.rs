//! The Discord gateway service and `service.py`'s configuration gate.

use charliebot_chat_threads::channel_service::ChannelService;
use charliebot_core::credentials::{CredentialValue, Credentials};
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;

use crate::config::DiscordConfig;
use crate::discord_listener::DiscordListener;

/// Starts the Discord gateway and boot backfill when the bot token and an allowed user are configured.
pub(crate) struct DiscordService {
  entrypoint: ChannelService<DiscordListener>,
}

impl Default for DiscordService {
  fn default() -> Self {
    Self { entrypoint: ChannelService::new("discord", DiscordListener) }
  }
}

impl Service for DiscordService {
  fn name(&self) -> &'static str {
    "discord"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let config = ctx.features.config.get()?;
      let credentials = ctx.features.credentials.get()?;
      let discord = config.section::<DiscordConfig>("discord");
      self.entrypoint.start(ctx, configured(discord, &credentials));
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { self.entrypoint.stop().await })
  }
}

fn configured(config: &DiscordConfig, credentials: &Credentials) -> bool {
  credentials.get("discord", "bot_token").is_some_and(CredentialValue::is_truthy) && !config.allowed_users.is_empty()
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
  fn the_entrypoint_requires_a_truthy_token_and_an_allowed_user() {
    let (_dir, token) = credentials("discord:\n  bot_token: synthetic-token\n");
    let (_dir, no_token) = credentials("discord:\n  bot_token: \"\"\n");
    let allowed =
      DiscordConfig { allowed_users: BTreeMap::from([("700000000000000001".to_string(), "tester".to_string())]) };
    assert!(!configured(&DiscordConfig::default(), &token));
    assert!(!configured(&allowed, &no_token));
    assert!(configured(&allowed, &token));
  }
}
