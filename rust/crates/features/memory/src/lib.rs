//! The labeled-entry memory store's server half. It reads and scaffolds the store, selects the
//! memory each run's instructions take, and exposes the store root to the diff view. The `memory` CLI lives in
//! this crate beside it.

pub mod cli;
mod entry;
pub mod memory;
mod store_root;
mod turn_contribution;

pub use entry::register;

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
