//! The `host_auth` service (`src/features/host_auth/api.py` `_poll_loop`, `_poller_round`, and the `SingleTaskPoller`
//! that owns the loop): one round at start, then one per standing period.

use std::future::Future;
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
  poll_loop_with(stop, poller_round).await;
}

async fn poll_loop_with<F, Fut>(stop: StartupStop, mut round: F)
where
  F: FnMut() -> Fut,
  Fut: Future<Output = anyhow::Result<()>>,
{
  if !first_round(&stop, &mut round).await {
    return;
  }
  loop {
    tokio::time::sleep(Duration::from_secs(PROBE_INTERVAL_SEC as u64)).await;
    if let Err(error) = round().await {
      tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
    }
  }
}

/// Run the startup round after the gate, then let the caller continue its periodic loop.
async fn first_round<F, Fut>(stop: &StartupStop, round: F) -> bool
where
  F: FnOnce() -> Fut,
  Fut: Future<Output = anyhow::Result<()>>,
{
  if !first_round_gate(stop).await {
    return false;
  }
  if let Err(error) = round().await {
    tracing::error!(error = %format!("{error:#}"), "host_auth_poll_error");
  }
  true
}

/// The wait between the ready signal and the first round.
///
/// Python's `_poll_loop` (`src/features/host_auth/api.py:191-198`) is started by its ready-phase service, but its body
/// first runs when the startup coroutine yields, and the loop then still finishes the server's startup tail before it
/// reaches the poller. A cold-boot census starts at the `server_ready` line, and Python's state file is never resident
/// in it. This wait puts the first state read past that census's visit of the home root, where `host_auth.json` sits.
const FIRST_ROUND_DELAY: Duration = Duration::from_millis(300);

/// Wait for readiness, then the startup tail; false when stopping instead.
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
    let rounds = std::sync::atomic::AtomicUsize::new(0);
    let mut initial_round = Box::pin(first_round(&stop, || async {
      rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      Ok::<(), anyhow::Error>(())
    }));

    let completed_before_ready = tokio::select! {
      completed = &mut initial_round => Some(completed),
      _ = tokio::task::yield_now() => None,
    };

    assert_eq!(completed_before_ready, None, "the first round stays behind the startup gate");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 0, "no state read starts before readiness");
    stop.mark_ready();
    let completed_before_delay = tokio::select! {
      completed = &mut initial_round => Some(completed),
      _ = tokio::task::yield_now() => None,
    };
    assert_eq!(completed_before_delay, None, "the startup tail finishes before the state read");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 0, "no state read starts during the delay");
    tokio::time::advance(FIRST_ROUND_DELAY).await;
    assert!(initial_round.await, "the first round runs after the startup tail");
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "one state read starts after readiness");
  }

  #[tokio::test(start_paused = true)]
  async fn a_stop_before_readiness_keeps_the_first_round_shut() {
    let stop = StartupStop::default();
    stop.cancel();

    assert!(!first_round_gate(&stop).await, "a stopping process starts no round");
  }

  #[tokio::test(start_paused = true)]
  async fn the_first_round_sleeps_for_one_interval_before_the_next_round() {
    let stop = StartupStop::default();
    stop.mark_ready();
    let rounds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let round_count = std::sync::Arc::clone(&rounds);
    let task = tokio::spawn(poll_loop_with(stop, move || {
      let round_count = std::sync::Arc::clone(&round_count);
      async move {
        round_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok::<(), anyhow::Error>(())
      }
    }));

    tokio::task::yield_now().await;
    tokio::time::advance(FIRST_ROUND_DELAY).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "startup performs one round");

    let interval = Duration::from_secs(PROBE_INTERVAL_SEC as u64);
    tokio::time::advance(interval - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 1, "the next round waits for its interval");
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(rounds.load(std::sync::atomic::Ordering::Relaxed), 2, "the next round starts at its interval");

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
  }
}
