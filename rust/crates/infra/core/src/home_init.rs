//! The profile home's directories and seeded files. The server's startup and
//! `charliebot setup` both run this one provisioning path.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::config::CharlieBotConfig;
use crate::repo_root::CONFIGS_DIR_NAME;
use crate::str_methods::universal_newlines;

/// The default `config.yaml` for a checkout without the template.
const DEFAULT_CONFIG_YAML: &str = "paths:\n  workspace_dirs:\n  - ~/workspace\n  worktree_dir: ~/worktrees\n";

/// The status a seed reports for an item that already exists.
pub const SEED_EXISTS: &str = "exists";
/// The status a dry-run seed reports for an item it would create.
pub const SEED_WOULD_CREATE: &str = "would-create";
/// The status a seed reports for an item this run created.
pub const SEED_CREATED: &str = "created";

/// The provisioned home: `sessions/` and `config.d/` created, and `config.yaml` and `credentials.yaml` (mode 0600)
/// seeded from the checkout's `configs/` templates when missing.
pub fn init_charliebot_home(cfg: &CharlieBotConfig, repo_root: &Path) -> io::Result<()> {
  for directory in [cfg.charliebot_home.clone(), cfg.sessions_dir(), cfg.config_d_dir()] {
    fs::create_dir_all(&directory)?;
  }
  let config_file = cfg.config_file();
  if !config_file.exists() {
    let template = repo_root.join(CONFIGS_DIR_NAME).join("config.example.yaml");
    let text = if template.exists() {
      universal_newlines(&fs::read_to_string(&template)?)
    } else {
      DEFAULT_CONFIG_YAML.to_string()
    };
    fs::write(&config_file, text)?;
  }
  let credentials_file = cfg.credentials_file();
  if !credentials_file.exists() {
    fs::copy(repo_root.join(CONFIGS_DIR_NAME).join("credentials.example.yaml"), &credentials_file)?;
    fs::set_permissions(&credentials_file, fs::Permissions::from_mode(0o600))?;
  }
  Ok(())
}
