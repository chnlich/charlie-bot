//! `src/features/memory`: the labeled-entry memory store's server half. It reads and scaffolds the store, selects the
//! memory each run's instructions take, and exposes the store root to the diff view. The `memory` CLI and
//! `memory_proposal.py` stay in Python.

pub mod memory;
mod store_root;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};
use turn_contribution::MemoryTurn;

/// `src/features/memory/__init__.py`, without the CLI command: the store's diff root, then its turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.diff_root("memory", store_root::memory_dir)?;
  registry.turn_contribution("memory", Arc::new(MemoryTurn))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_core::config::{
    BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use charliebot_registry::Registry;

  use super::register;

  #[test]
  fn the_diff_root_is_the_home_memory_dir() {
    let mut registry = Registry::new();
    registry.register_package("src.features.memory", register).unwrap();
    let cfg = CharlieBotConfig {
      charliebot_home: Path::new("/synthetic/home").to_path_buf(),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    };
    let roots: Vec<_> = registry.diff_roots().map(|(name, root)| (name, root(&cfg))).collect();
    assert_eq!(roots, [("memory", Path::new("/synthetic/home/memory").to_path_buf())]);
  }
}
