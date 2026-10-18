//! `src/features/diag`: its port has not landed, so `register` makes no registrations yet.

use charliebot_registry::{Registry, RegistryError};

pub fn register(_registry: &mut Registry) -> Result<(), RegistryError> {
  Ok(())
}
