//! The shared service of the chat channels: the `Service` lifecycle over the entrypoint tasks (one gateway listener
//! task plus the boot backfill task), and the answer a refused thread entry gets on the internal routes. A channel's
//! crate keeps its configuration gate and route paths; the lifecycle, the task bookkeeping and the answer shape live
//! here.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_registry::FeatureContext;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::backfill::run_backfill;
use crate::thread_entry::{ThreadDetail, ThreadReplyError, spawn_logged};

/// The listener surface a chat channel hands the entrypoint service (`ChannelListener`).
pub trait ChannelListener: Send + Sync + 'static {
  /// The gateway loop. It runs until its task is cancelled.
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>;

  /// The boot backfill over the summons lost across the restart; returns how many notices it posted.
  fn backfill_lost_summons(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<usize>>;
}

/// A chat channel's entrypoint identity: the listener its service runs, the platform name its credentials section
/// and service name spell, and the gate its stored settings pass.
///
/// The channel's crate implements this for its listener and names the registered service by the alias
/// `type <Channel>Service = ChannelService<Listener>`. The `Service` lifecycle is implemented once, here.
pub trait ChatChannel: ChannelListener + Sized {
  /// The platform name that the credentials section, the service name and the task prefixes spell.
  const NAME: &'static str;

  /// The listener value the entrypoint service runs.
  fn listener() -> Self;

  /// Whether the channel's stored settings and credentials start its entrypoint.
  fn allows(features: &FeatureContext) -> anyhow::Result<bool>;
}

/// One chat channel's entrypoint tasks: the gateway listener and the boot backfill.
///
/// `Service::start` launches nothing and logs `<channel>_entrypoint_off` unless the channel's settings allow it. A
/// started service runs its tasks as `<channel>-listener` and `<channel>-backfill` and logs
/// `<channel>_entrypoint_started`. `Service::stop` cancels what `start` launched, and returns at once for a service
/// that never started.
pub struct ChannelService<L: ChannelListener> {
  channel: &'static str,
  listener: Arc<L>,
  listener_task: Mutex<Option<JoinHandle<()>>>,
  backfill_task: Mutex<Option<JoinHandle<()>>>,
}

impl<L: ChatChannel> Default for ChannelService<L> {
  fn default() -> Self {
    Self {
      channel: L::NAME,
      listener: Arc::new(L::listener()),
      listener_task: Mutex::new(None),
      backfill_task: Mutex::new(None),
    }
  }
}

impl<L: ChatChannel> Service for ChannelService<L> {
  fn name(&self) -> &'static str {
    self.channel
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let configured = L::allows(&ctx.features)?;
      self.start_tasks(ctx, configured);
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { self.stop_tasks().await })
  }
}

impl<L: ChannelListener> ChannelService<L> {
  /// Launch the listener and the backfill when the channel is configured.
  fn start_tasks(&self, ctx: ServiceContext, configured: bool) {
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
  async fn stop_tasks(&self) -> anyhow::Result<()> {
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

/// The answer of an internal channel route: the readback with 200, a refusal with its status and `{"detail": ...}`,
/// or the unhandled failure's 500. The Discord and Feishu routes both return [`ThreadReplyError`] and answer this
/// shape, so it lives beside the error's owner; the Slack routes keep their own answer, whose 500 carries the
/// refusal's detail as JSON instead of logging it.
pub fn reply_response(result: Result<Value, ThreadReplyError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(error) if error.status == StatusCode::INTERNAL_SERVER_ERROR => {
      responses::internal_error("RuntimeError", error.to_string())
    }
    Err(ThreadReplyError { status, detail }) => {
      let detail = match detail {
        ThreadDetail::Text(text) => Value::String(text),
        ThreadDetail::Structured(value) => value,
      };
      responses::json(status, &json!({"detail": detail}))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct FakeChannel;

  impl ChannelListener for FakeChannel {
    fn run_listener(&self, _features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
      Box::pin(async { Ok(()) })
    }

    fn backfill_lost_summons(&self, _features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<usize>> {
      Box::pin(async { Ok(0) })
    }
  }

  impl ChatChannel for FakeChannel {
    const NAME: &'static str = "fake";

    fn listener() -> Self {
      FakeChannel
    }

    fn allows(_features: &FeatureContext) -> anyhow::Result<bool> {
      Ok(false)
    }
  }

  #[test]
  fn the_service_takes_its_name_and_phase_from_the_channel() {
    let service = ChannelService::<FakeChannel>::default();
    assert_eq!(service.name(), "fake");
    assert_eq!(service.phase(), Phase::Ready);
  }
}
