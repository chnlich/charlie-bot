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
