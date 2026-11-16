//! The labeled-entry memory store's server half. It reads and scaffolds the store, selects the
//! memory each run's instructions take, and exposes the store root to the diff view. The `memory` CLI lives in
//! this crate beside it.

pub mod cli;
pub mod memory;
mod store_root;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};
use turn_contribution::MemoryTurn;

/// `register`: the store's diff root, then its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.diff_root("memory", store_root::memory_dir)?;
  registry.turn_contribution("memory", Arc::new(MemoryTurn))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_core::test_support::synthetic_config;
  use charliebot_registry::Registry;

  use super::register;

  #[test]
  fn the_diff_root_is_the_home_memory_dir() {
    let mut registry = Registry::new();
    registry.register_package("features.memory", register).unwrap();
    let cfg = synthetic_config("/synthetic/home");
    let roots: Vec<_> = registry.diff_roots().map(|(name, root)| (name, root(&cfg))).collect();
    assert_eq!(roots, [("memory", Path::new("/synthetic/home/memory").to_path_buf())]);
  }
}
