//! The `ext_usage` service (`src/features/usage/ext_usage.py` `_poller`, `start_service` and `stop_service`): the
//! poll loop runs for the server's lifetime, and the server's own config, registry and store feed each round.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use charliebot_registry::FeatureContext;
use charliebot_registry::usage::QuotaAccount;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use charliebot_store::SIDEBAR_CHANNEL;
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::ext_usage::{PANEL, PollHost, poll_loop};

/// The service's one loop task. Starting creates it, and stopping cancels and drains it.
#[derive(Default)]
pub struct ExtUsagePoller {
  task: Mutex<Option<JoinHandle<()>>>,
}

impl Service for ExtUsagePoller {
  fn name(&self) -> &'static str {
    "ext_usage"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let host: Arc<dyn PollHost> = Arc::new(ServerHost(ctx.features));
      *self.task.lock().expect("the poller task lock is never poisoned") =
        Some(tokio::spawn(poll_loop(Arc::clone(&PANEL), host)));
      tracing::info!("ext_usage_poller_started");
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let task = self.task.lock().expect("the poller task lock is never poisoned").take();
      let Some(task) = task else {
        return Ok(());
      };
      task.abort();
      match task.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(error.into()),
      }
      tracing::info!("ext_usage_poller_stopped");
      Ok(())
    })
  }
}

/// The loop's host in the server: the accounts of the live config, the server clock, and the sidebar channel.
struct ServerHost(Arc<FeatureContext>);

impl PollHost for ServerHost {
  fn quota_accounts(&self) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    let cfg = self.0.config.get().context("loading config.yaml for the quota panel")?;
    self.0.registry.quota_accounts(&cfg)
  }

  fn now(&self) -> UtcDateTime {
    UtcDateTime::now()
  }

  fn broadcast_sidebar(&self, frame: Value) -> BoxFuture<'_, anyhow::Result<()>> {
    Box::pin(async move {
      self.0.store.broadcast(SIDEBAR_CHANNEL, frame).await?;
      Ok(())
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn stop_without_start_returns_at_once() {
    ExtUsagePoller::default().stop().await.unwrap();
  }
}
