//! The improve loop: `charliebot improve` runs an iterative worker loop on the task tree as one sequence (`src/features/improve/`).

pub mod api;
pub mod command;
pub mod controller;
pub mod judgment;
pub mod landing;
pub mod sequence;

use charliebot_registry::{Registry, RegistryError};

/// Register the improve routes and the improve sequence controller.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  controller::register(registry)
}
