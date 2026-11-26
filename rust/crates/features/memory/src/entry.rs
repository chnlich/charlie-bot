//! The memory package's registration: the store's diff root and its turn contribution.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

use crate::store_root;
use crate::turn_contribution::MemoryTurn;

/// `register`: the store's diff root, then its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.diff_root("memory", store_root::memory_dir)?;
  registry.turn_contribution("memory", Arc::new(MemoryTurn))
}
