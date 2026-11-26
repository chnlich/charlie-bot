//! The session-api package's registrations: the session listing router, the prompt and backend routes and the
//! websocket.

use std::sync::Arc;

use axum::Router;
use charliebot_registry::{FeatureContext, Registry, RegistryError};

use crate::{backend_routes, concurrency, prompt_routes, websocket};

/// The crate's registration entry point: the session listing router, the prompt-inspection router and their
/// registry contributions.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(crate::routes::routes, crate::routes::ROUTE_PATTERNS)?;
  registry.router_patterns(concurrency::routes, concurrency::ROUTE_PATTERNS)?;
  registry.router(prompt_routes::routes, prompt_routes::matches)?;
  registry.router(backend_routes::routes, backend_routes::matches)?;
  registry.websocket("/ws/sessions/*", websocket::session_websocket)
}

/// The route table over the shared feature context.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  crate::routes::routes(features)
}
