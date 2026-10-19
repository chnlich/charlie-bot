//! The chat-channel entrypoint service (`src/features/chat_threads/channel_service.py`): one gateway listener task plus
//! the boot backfill task. A channel's crate keeps its own service wiring and its configuration gate; the task
//! bookkeeping lives here.

use std::sync::{Arc, Mutex};

use charliebot_registry::FeatureContext;
use charliebot_registry::wiring::ServiceContext;
use futures_util::future::BoxFuture;
use tokio::task::JoinHandle;

use crate::backfill::run_backfill;
use crate::thread_entry::spawn_logged;

/// The listener surface a chat channel hands the entrypoint service (`ChannelListener`).
pub trait ChannelListener: Send + Sync + 'static {
  /// The gateway loop. It runs until its task is cancelled.
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>;

  /// The boot backfill over the summons lost across the restart; returns how many notices it posted.
  fn backfill_lost_summons(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<usize>>;
}

/// One chat channel's entrypoint tasks: the gateway listener and the boot backfill.
///
/// `start` launches nothing and logs `<channel>_entrypoint_off` unless `configured` holds. A started service runs its
/// tasks as `<channel>-listener` and `<channel>-backfill` and logs `<channel>_entrypoint_started`. `stop` cancels what
/// `start` launched, and returns at once for a service that never started.
pub struct ChannelService<L: ChannelListener> {
  channel: &'static str,
  listener: Arc<L>,
  listener_task: Mutex<Option<JoinHandle<()>>>,
  backfill_task: Mutex<Option<JoinHandle<()>>>,
}

impl<L: ChannelListener> ChannelService<L> {
  /// A service for `channel`, not yet started.
  pub fn new(channel: &'static str, listener: L) -> Self {
    Self { channel, listener: Arc::new(listener), listener_task: Mutex::new(None), backfill_task: Mutex::new(None) }
  }

  /// Launch the listener and the backfill when the channel is configured.
  pub fn start(&self, ctx: ServiceContext, configured: bool) {
    if !configured {
      tracing::info!(event = %format!("{}_entrypoint_off", self.channel));
      return;
    }
    let listener = self.listener.run_listener(Arc::clone(&ctx.features));
    let listener_task = spawn_logged(format!("{}-listener", self.channel), listener);
    let listener = Arc::clone(&self.listener);
    let backfill_features = Arc::clone(&ctx.features);
    let backfill =
      run_backfill(ctx.recovery_done, move || listener.backfill_lost_summons(backfill_features), self.channel);
    let backfill_task = spawn_logged(format!("{}-backfill", self.channel), backfill);
    *self.listener_task.lock().expect("the listener task slot is never poisoned") = Some(listener_task);
    *self.backfill_task.lock().expect("the backfill task slot is never poisoned") = Some(backfill_task);
    tracing::info!(event = %format!("{}_entrypoint_started", self.channel));
  }

  /// Cancel the listener and the backfill, and wait until both have stopped.
  pub async fn stop(&self) -> anyhow::Result<()> {
    let listener = self.listener_task.lock().expect("the listener task slot is never poisoned").take();
    let backfill = self.backfill_task.lock().expect("the backfill task slot is never poisoned").take();
    cancel_and_wait(listener).await?;
    cancel_and_wait(backfill).await
  }
}

/// Cancel one task and wait for it. A task that stopped by cancellation is the expected end; a panic is an error.
async fn cancel_and_wait(task: Option<JoinHandle<()>>) -> anyhow::Result<()> {
  let Some(task) = task else {
    return Ok(());
  };
  task.abort();
  match task.await {
    Ok(()) => Ok(()),
    Err(error) if error.is_cancelled() => Ok(()),
    Err(error) => Err(error.into()),
  }
}
