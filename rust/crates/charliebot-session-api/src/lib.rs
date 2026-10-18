//! Session listing routes and the shared listing helpers.

pub mod listing;
mod prompt_routes;
mod routes;
mod search;
mod sidebar;
mod tree;

use std::sync::Arc;

use axum::Router;
use charliebot_registry::{FeatureContext, Registry, RegistryError};

/// Register the session listing router, the prompt-inspection router and their registry contributions.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes::routes, routes::matches)?;
  registry.router(prompt_routes::routes, prompt_routes::matches)
}

/// Build the route table with the shared feature context.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes::routes(features)
}
