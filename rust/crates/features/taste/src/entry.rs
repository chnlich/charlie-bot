//! The taste package's registration: its turn contribution.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

use crate::turn_contribution::TasteTurn;

/// The taste package's registration: its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("taste", Arc::new(TasteTurn))
}
