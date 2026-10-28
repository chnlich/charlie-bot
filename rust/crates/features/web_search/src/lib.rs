//! The web search an agent runs through the server: `POST /api/web-search/mcp`, an MCP streamable-HTTP endpoint with
//! one tool, `web_search`.
//!
//! The endpoint backends disallow Claude Code's built-in web search, because their endpoint either refuses it or
//! answers without a search. This endpoint serves the search instead: Google's `google_search` grounding over
//! `gemini-3.8-flash`, with each grounding redirect resolved to its real URL. The endpoint sits under the server's
//! access gate, and the `--mcp-config` that mounts it carries a run token, so no credential travels in an argument.

mod endpoint;
mod mcp;
mod search;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;
use charliebot_compat::routing::post_only;
use charliebot_core::web_search::MCP_PATH;
use charliebot_registry::{FeatureContext, Registry, RegistryError};

use endpoint::WebSearch;

/// `register`: the MCP endpoint under the server's access gate.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)
}

fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route(MCP_PATH, post_only(mcp::post)).with_state(WebSearch::new(features.credentials.clone()))
}

/// Whether a path is the MCP endpoint, whatever the method.
fn matches(path: &str) -> bool {
  path == MCP_PATH
}
