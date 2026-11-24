//! The memory store's root: one derivation for the run instructions, the diff
//! view and the memory CLI.

use std::path::{Path, PathBuf};

use charliebot_core::config::CharlieBotConfig;

/// The store root under a profile home: `<home>/memory`. The CLI passes the home it resolves from the environment.
pub fn memory_dir_in(home: &Path) -> PathBuf {
  home.join("memory")
}

/// The store root under the profile home: `<home>/memory`.
pub fn memory_dir(cfg: &CharlieBotConfig) -> PathBuf {
  memory_dir_in(&cfg.charliebot_home)
}
