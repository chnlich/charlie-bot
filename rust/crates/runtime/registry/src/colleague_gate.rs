//! The colleague gate: the chat platform's check on which requests may start agent work in a thread a colleague posted
//! in, and which wakes may start a run. A platform registers one `ColleagueGate` in startup state. With none registered
//! no chat platform is active, so no session holds a colleague message and every request and wake passes.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use futures_util::future::BoxFuture;
use serde_json::json;

use crate::FeatureContext;
use crate::caller::CallerIdentity;

/// The platform's colleague check. Both methods run on every request or wake and decide it.
pub trait ColleagueGate: Send + Sync {
  /// The refusal detail when `caller`'s request to start, queue or schedule agent work must be refused, or None when
  /// the request may proceed. An operator caller always proceeds.
  fn refuse_agent_work<'a>(
    &'a self,
    features: &'a FeatureContext,
    caller: &'a CallerIdentity,
  ) -> BoxFuture<'a, Option<&'static str>>;

  /// Whether a wake of `session_id` by the trigger `label` may start a run.
  fn admits_wake<'a>(
    &'a self,
    features: &'a FeatureContext,
    session_id: &'a str,
    label: &'a str,
  ) -> BoxFuture<'a, bool>;
}

/// The 403 response for an agent-work request that the gate refuses, or None when the request may proceed.
pub async fn refuse_colleague_work(features: &FeatureContext, caller: &CallerIdentity) -> Option<Response> {
  let gate = features.startup.get::<Arc<dyn ColleagueGate>>()?;
  let detail = gate.refuse_agent_work(features, caller).await?;
  Some(responses::json(StatusCode::FORBIDDEN, &json!({ "detail": detail })))
}

/// Whether a wake of `session_id` by `label` may start a run. Without a registered gate every wake may start one.
pub async fn admits_wake(features: &FeatureContext, session_id: &str, label: &str) -> bool {
  match features.startup.get::<Arc<dyn ColleagueGate>>() {
    Some(gate) => gate.admits_wake(features, session_id, label).await,
    None => true,
  }
}
