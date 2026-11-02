//! The memory store's root: one derivation for the run instructions and the diff
//! view.

use std::path::PathBuf;

use charliebot_core::config::CharlieBotConfig;

/// The store root under the profile home: `<home>/memory`.
pub fn memory_dir(cfg: &CharlieBotConfig) -> PathBuf {
  cfg.charliebot_home.join("memory")
}
