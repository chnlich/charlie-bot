//! The shared service of the chat channels: the entrypoint tasks (one gateway listener task plus the boot backfill
//! task) and the answer a refused thread entry gets on the internal routes. A channel's crate keeps its own service
//! wiring, configuration gate and route paths; the task bookkeeping and the answer shape live here.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_registry::FeatureContext;
use charliebot_registry::wiring::ServiceContext;
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
