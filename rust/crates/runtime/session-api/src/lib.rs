//! Session listing routes and the shared listing helpers.

pub mod detail;
mod fork_routes;
pub mod listing;
mod prompt_routes;
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
use charliebot_store::{StoreError, StoreResult};

/// Register the session listing router, the prompt-inspection router and their registry contributions.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(routes::routes, routes::ROUTE_PATTERNS)?;
  registry.router(prompt_routes::routes, prompt_routes::matches)?;
  registry.websocket("/ws/sessions/*", websocket::session_websocket)
}

/// Build the route table with the shared feature context.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes::routes(features)
}

/// Run a handler's store work on the blocking pool, as Python's handlers run it in `asyncio.to_thread`, so a slow
/// read holds no async worker.
pub(crate) async fn blocking_store<T, F>(work: F) -> StoreResult<T>
where
  T: Send + 'static,
  F: FnOnce() -> StoreResult<T> + Send + 'static,
{
  tokio::task::spawn_blocking(work).await.map_err(|error| StoreError::new(error.to_string()))?
}
