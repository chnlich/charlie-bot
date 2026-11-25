//! The explain feature's entry for the package registry. `register` adds the one router, and the registry reads
//! the paths it claims from `ROUTE_PATTERNS`.

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use charliebot_compat::routing::{get_only, refuse_other_methods};
use charliebot_registry::{FeatureContext, Registry, RegistryError};

use crate::api;

const EXPLAIN_PATH: &str = "/api/sessions/{session_id}/explain";
const EXPLAIN_STATUS_PATH: &str = "/api/sessions/{session_id}/explain/status";

/// The paths the explain router mounts. The registry answers the trailing-slash redirect's claim from them.
const ROUTE_PATTERNS: &[&str] = &[EXPLAIN_PATH, EXPLAIN_STATUS_PATH];

/// `register`: the router under `/api/sessions`.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(routes, ROUTE_PATTERNS)
}

/// The explain routes, with the feature context as their state. The 405 body names the first route of a path that
/// matches it, so the POST route's allow list answers the explain path, and the status path names its GET.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(EXPLAIN_PATH, refuse_other_methods(post(api::post_explain).get(api::get_explain), "POST"))
    .route(EXPLAIN_STATUS_PATH, get_only(api::get_explain_status))
    .with_state(Arc::clone(features))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registers_one_router_that_claims_the_explain_paths_only() {
    let mut registry = Registry::new();
    registry.register_package("features.explain", register).expect("the explain router registers");

    let routers = registry.routers();
    assert_eq!(routers.len(), 1);
    assert!(routers[0].claims("/api/sessions/s/explain"));
    assert!(routers[0].claims("/api/sessions/s/explain/status"));
    assert!(!routers[0].claims("/api/sessions/s"));
    assert!(!routers[0].claims("/api/sessions//explain"));
    assert!(!routers[0].claims("/api/sessions/s/explain/other"));
  }
}
