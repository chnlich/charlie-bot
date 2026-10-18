//! The `host_auth` service (`src/features/host_auth/api.py` `_poll_loop`, `_poller_round`, and the `SingleTaskPoller`
//! that owns the loop): one round at start, then one per standing period.

use std::sync::Mutex;
use std::time::Duration;

use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;
use tokio::task::JoinHandle;

use crate::host_auth::{self, PROBE_INTERVAL_SEC, RoundSlot};

/// The service's one loop task. Starting creates it, and stopping cancels and drains it.
#[derive(Default)]
pub struct HostAuthPoller {
  task: Mutex<Option<JoinHandle<()>>>,
}

impl Service for HostAuthPoller {
  fn name(&self) -> &'static str {
    "host_auth"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, _context: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      *self.task.lock().expect("the poller task lock is never poisoned") = Some(tokio::spawn(poll_loop()));
      tracing::info!("host_auth_poller_started");
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
      tracing::info!("host_auth_poller_stopped");
      Ok(())
    })
  }
}

/// One round at once, then one per standing period. A failed tick is logged and the loop continues.
async fn poll_loop() {
  loop {
    if let Err(error) = poller_round().await {
      tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
    }
    tokio::time::sleep(Duration::from_secs(PROBE_INTERVAL_SEC as u64)).await;
  }
}

/// One scheduled round. It is skipped while a manual round holds the flag.
async fn poller_round() -> anyhow::Result<()> {
  let Some(_slot) = RoundSlot::claim() else {
    return Ok(());
  };
  let (state_path, config_path) = host_auth::default_paths()?;
  host_auth::run_round(false, &state_path, &config_path).await?;
  Ok(())
}
