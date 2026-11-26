//! The diag package's registration: the `/api/diag` router.

use charliebot_registry::{Registry, RegistryError};

use crate::api;

/// `register`: the `/api/diag` router, nothing else.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)
}
