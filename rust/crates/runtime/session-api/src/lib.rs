//! Session listing routes and the shared listing helpers.

pub mod api;
mod concurrency;
pub mod detail;
mod fork_routes;
pub mod listing;
mod prompt_routes;
mod route_support;
mod routes;
mod search;
mod sidebar;
#[cfg(test)]
mod test_support;
mod tree;
mod websocket;
mod worker_transcript;
mod write_routes;
mod writes;

use std::sync::Arc;

use axum::Router;
use charliebot_registry::{FeatureContext, Registry, RegistryError};

/// Register the session listing router, the prompt-inspection router and their registry contributions.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(routes::routes, routes::ROUTE_PATTERNS)?;
  registry.router_patterns(concurrency::routes, concurrency::ROUTE_PATTERNS)?;
  registry.router(prompt_routes::routes, prompt_routes::matches)?;
  registry.websocket("/ws/sessions/*", websocket::session_websocket)
}

/// Build the route table with the shared feature context.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes::routes(features)
}
