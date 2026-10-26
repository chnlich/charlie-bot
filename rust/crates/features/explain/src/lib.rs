//! `src/features/explain`: the explain button's three routes under `/api/sessions`, the background generation they
//! start, and the results file they keep in each session.
//!
//! A divider's explanation is one tool-capable one-shot agent run over a read-only copy of the session's chat events.
//! The results file `sessions/<sid>/data/explain_results.json` is the persisted truth. The generation tasks are
//! memory-only, and a pending entry older than the one-shot budget is reaped by the first read that meets it.

mod api;
mod explain;
mod prompt;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use charliebot_compat::routing::{get_only, refuse_other_methods};
use charliebot_registry::{FeatureContext, Registry, RegistryError};

/// `register` (`src/features/explain/__init__.py`): the router under `/api/sessions`.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)
}

/// The explain routes, with the feature context as their state. Starlette's 405 names the first route of a path that
/// matches it, so the POST route's allow list answers the explain path, and the status path names its GET.
fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(
      "/api/sessions/{session_id}/explain",
      refuse_other_methods(post(api::post_explain).get(api::get_explain), "POST"),
    )
    .route("/api/sessions/{session_id}/explain/status", get_only(api::get_explain_status))
    .with_state(Arc::clone(features))
}

/// Whether a path is one of the explain routes, whatever the method. The server's trailing-slash redirect asks this
/// of the slash-toggled path.
fn matches(path: &str) -> bool {
  let Some(rest) = path.strip_prefix("/api/sessions/") else {
    return false;
  };
  match rest.split_once('/') {
    Some((session_id, tail)) => !session_id.is_empty() && matches!(tail, "explain" | "explain/status"),
    None => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registers_one_router_that_claims_the_explain_paths_only() {
    let mut registry = Registry::new();
    registry.register_package("src.features.explain", register).expect("the explain router registers");

    let routers = registry.routers();
    assert_eq!(routers.len(), 1);
    assert!(routers[0].claims("/api/sessions/s/explain"));
    assert!(routers[0].claims("/api/sessions/s/explain/status"));
    assert!(!routers[0].claims("/api/sessions/s"));
    assert!(!routers[0].claims("/api/sessions//explain"));
    assert!(!routers[0].claims("/api/sessions/s/explain/other"));
  }
}
