//! The failure-triage package's registration: this stateless half contributes no route and no service.

use charliebot_registry::{Registry, RegistryError};

/// `register`: this half contributes no route and no service.
pub fn register(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}
