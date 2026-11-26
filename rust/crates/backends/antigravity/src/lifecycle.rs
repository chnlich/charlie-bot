//! The antigravity lifecycle: a gated manager turn names its Run to the process, which then takes live input from it.

use charliebot_registry::backend::{BackendLifecycle, Launch, LaunchContext, LaunchKind, LaunchRefused, MemberFailure};
use charliebot_registry::midturn_delivery::{RUN_ID_KWARG, launch_kwargs};
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::RunKind;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

pub struct AntigravityLifecycle;

impl BackendLifecycle for AntigravityLifecycle {
  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned");
      let backend_kwargs =
        self.midturn_launch_kwargs(&meta, ctx.kind == LaunchKind::Turn, ctx.run_kind, ctx.run_id.as_deref()).map_err(
          |error| LaunchRefused { message: error.to_string(), failure: MemberFailure::Other, resets_at: None },
        )?;
      Ok(Launch {
        backend_kwargs,
        resume_id: ctx.held_native_id.clone(),
        prompt: None,
        account_label: None,
        private: None,
      })
    })
  }

  fn midturn_launch_kwargs(
    &self,
    meta: &SessionMetadata,
    turn: bool,
    run_kind: RunKind,
    run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    Ok(match launch_kwargs(meta, turn, run_kind, run_id)? {
      Some(run_id) => Map::from_iter([(RUN_ID_KWARG.to_string(), Value::String(run_id))]),
      None => Map::new(),
    })
  }
}
