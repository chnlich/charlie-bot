//! The memory store's root (`src/features/memory/store_root.py`): one derivation for the run instructions and the diff
//! view.

use std::path::PathBuf;

use charliebot_core::config::CharlieBotConfig;

/// The store root under the profile home: `<home>/memory` (`store_root.memory_dir`).
pub fn memory_dir(cfg: &CharlieBotConfig) -> PathBuf {
  cfg.charliebot_home.join("memory")
}
