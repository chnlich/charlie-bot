//! Delayed and watched triggers that admit scheduled task inputs.

pub mod triggers;

use charliebot_registry::{Registry, RegistryError};

/// Register trigger routes and the post-recovery trigger step.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(triggers::routes, triggers::matches)?;
  registry.trigger_step("triggers", triggers::trigger_step)
}
