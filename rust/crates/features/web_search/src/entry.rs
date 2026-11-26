//! The web-search package's registration: the MCP endpoint under the server's access gate.

use std::sync::Arc;

use axum::Router;
use charliebot_compat::routing::post_only;
use charliebot_core::web_search::MCP_PATH;
use charliebot_registry::{FeatureContext, Registry, RegistryError};

use crate::endpoint::WebSearch;
use crate::mcp;

/// `register`: the MCP endpoint under the server's access gate.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)
}

fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(MCP_PATH, post_only(mcp::post))
    .with_state(WebSearch::new(features.config.clone(), features.credentials.clone()))
}

/// Whether a path is the MCP endpoint, whatever the method.
pub(crate) fn matches(path: &str) -> bool {
  path == MCP_PATH
}
