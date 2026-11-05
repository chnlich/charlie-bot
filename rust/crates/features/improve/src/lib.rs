//! The improve loop: `charliebot improve start` runs an iterative worker loop on the task tree as one sequence.

pub mod api;
pub mod cli;
pub mod command;
pub mod controller;
pub mod judgment;
pub mod landing;
pub mod sequence;

use charliebot_registry::{Registry, RegistryError};

/// The improve routes and the improve sequence controller, registered together.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  controller::register(registry)
}
