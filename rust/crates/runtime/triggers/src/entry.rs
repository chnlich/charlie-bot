//! The triggers package's registration: the trigger routes and the post-recovery trigger step.

use charliebot_registry::{Registry, RegistryError};

use crate::triggers;

/// `register`: the trigger routes and the post-recovery trigger step.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(triggers::routes, triggers::ROUTE_PATTERNS)?;
  registry.trigger_step("triggers", triggers::trigger_step)
}
