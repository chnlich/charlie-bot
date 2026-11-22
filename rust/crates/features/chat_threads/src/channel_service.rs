//! The shared service of the chat channels: the `Service` lifecycle over the entrypoint tasks (one gateway listener
//! task plus the boot backfill task), and the answer a refused thread entry gets on the internal routes. A channel's
//! crate keeps its configuration gate and route paths. The lifecycle, the task bookkeeping and the answer shape live
//! here.

use std::future::Future;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_cli_common::channel_verb::REPLY_BODY;
use charliebot_cli_common::fields::{SESSION_ID, TEXT};
use charliebot_compat::request_body::{self, BodyModel, text_field};
use charliebot_compat::responses;
use charliebot_registry::wiring::{Phase, Service, ServiceContext, cancel_and_wait};
use charliebot_registry::{FeatureContext, caller};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::backfill::run_backfill;
use crate::thread_entry::{ThreadDetail, ThreadReplyError, spawn_logged};

/// The listener surface a chat channel hands the entrypoint service (`ChannelListener`).
pub trait ChannelListener: Send + Sync + 'static {
  /// The gateway loop. It runs until its task is cancelled.
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>;

  /// The boot backfill over the summons lost across the restart. It returns how many notices it posted.
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
    if let Some(task) = listener {
      cancel_and_wait(task).await?;
    }
    if let Some(task) = backfill {
      cancel_and_wait(task).await?;
    }
    Ok(())
  }
}

/// The answer of an internal channel route: the readback with 200, a refusal with its status and `{"detail": ...}`,
/// or the unhandled failure's 500. The Discord and Feishu routes both return [`ThreadReplyError`] and answer this
/// shape, so it lives beside the error's owner. The Slack routes keep their own answer, whose 500 carries the
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

/// The answer of a channel's reply route: reads and validates the body against [`REPLY_BODY`], resolves the caller's
/// reply context under `verb`, calls `post` with `(session_id, text, request_id, run_started_at)`, and answers through
/// [`reply_response`]. The Discord and Feishu reply routes share this request flow and answer shape, so the handler
/// body lives here.
pub async fn post_reply_response<F, Fut>(
  features: &FeatureContext,
  request: Request<Body>,
  verb: &str,
  post: F,
) -> Response
where
  F: FnOnce(String, String, Option<String>, Option<String>) -> Fut,
  Fut: Future<Output = Result<Value, ThreadReplyError>>,
{
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match body.declared_fields(&REPLY_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, SESSION_ID);
  let text = text_field(&fields, TEXT);
  let identity = match caller::resolve_caller(&parts.headers, features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  let (request_id, run_started_at) = match identity.reply_context(&features.store, verb, &session_id, &text) {
    Ok(context) => context,
    Err(response) => return response,
  };
  reply_response(post(session_id, text, request_id, run_started_at).await)
}

/// The check route's request model: the request carries no fields.
pub const CHECK_BODY: BodyModel = BodyModel::required(&[]).forbid_extra();

/// The answer of a channel's check route: the request's body must carry no fields, then `check`'s result answers
/// through [`reply_response`]. The Discord and Feishu routes mount the same check over their own bot, so the body model
/// and the handler body live here.
pub async fn check_response(
  request: Request<Body>,
  check: impl Future<Output = Result<Value, ThreadReplyError>>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  if let Err(response) = body.declared_fields(&CHECK_BODY) {
    return *response;
  }
  reply_response(check.await)
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
