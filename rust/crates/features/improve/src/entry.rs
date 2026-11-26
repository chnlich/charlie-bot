//! The improve package's registration: the improve routes and the sequence controller.

use charliebot_registry::{Registry, RegistryError};

use crate::{api, controller};

/// The improve routes and the improve sequence controller, registered together.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  controller::register(registry)
}
