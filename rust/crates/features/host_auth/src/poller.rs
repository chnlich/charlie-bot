//! The `host_auth` service (`src/features/host_auth/api.py` `_poll_loop`, `_poller_round`, and the `SingleTaskPoller`
//! that owns the loop): one round at start, then one per standing period.

use std::sync::Mutex;
use std::time::Duration;

use charliebot_registry::context::StartupStop;
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

  fn start<'a>(&'a self, context: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let stop = context.features.startup.stop();
      *self.task.lock().expect("the poller task lock is never poisoned") = Some(tokio::spawn(poll_loop(stop)));
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
async fn poll_loop(stop: StartupStop) {
  if !first_round_gate(&stop).await {
    return;
  }
  loop {
    if let Err(error) = poller_round().await {
      tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
    }
    tokio::time::sleep(Duration::from_secs(PROBE_INTERVAL_SEC as u64)).await;
  }
}

/// The wait between the ready signal and the first round.
///
/// Python's `_poll_loop` (`src/features/host_auth/api.py:191-198`) is started by its ready-phase service, but its body
/// first runs when the startup coroutine yields, and the loop then still finishes the server's startup tail before it
/// reaches the poller. A cold-boot census starts at the `server_ready` line, and Python's state file is never resident
/// in it. This wait puts the first state read past that census's visit of the home root, where `host_auth.json` sits.
const FIRST_ROUND_DELAY: Duration = Duration::from_millis(300);

/// Wait for the post-`server_ready` readiness signal, then the startup tail; false when stopping instead.
async fn first_round_gate(stop: &StartupStop) -> bool {
  if !stop.ready().await {
    return false;
  }
  tokio::time::sleep(FIRST_ROUND_DELAY).await;
  true
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

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test(start_paused = true)]
  async fn the_first_round_waits_for_startup_readiness() {
    let stop = StartupStop::default();
    let mut gate = Box::pin(first_round_gate(&stop));

    let opened_before_ready = tokio::select! {
      opened = &mut gate => Some(opened),
      _ = tokio::task::yield_now() => None,
    };

    assert_eq!(opened_before_ready, None, "the first round must not start before startup is ready");
    stop.mark_ready();
    assert!(gate.await, "the gate opens once startup is ready");
  }

  #[tokio::test(start_paused = true)]
  async fn a_stop_before_readiness_keeps_the_first_round_shut() {
    let stop = StartupStop::default();
    stop.cancel();

    assert!(!first_round_gate(&stop).await, "a stopping process starts no round");
  }
}
