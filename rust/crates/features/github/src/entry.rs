//! The GitHub package's registration: the platform's draft support.

use charliebot_registry::{Registry, RegistryError};

use crate::draft;

/// `register`: the platform's draft support.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.draft_platform("github", draft::platform_factory())
}
