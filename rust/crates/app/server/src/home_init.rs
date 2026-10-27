//! `init_seed.init_charliebot_home`: the profile home's directories and seeded files.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use charliebot_core::config::CharlieBotConfig;

/// `_default_config_yaml()` as `yaml_utils.save_yaml` writes it, for a checkout without the template.
const DEFAULT_CONFIG_YAML: &str = "paths:\n  workspace_dirs:\n  - ~/workspace\n  worktree_dir: ~/worktrees\n";

/// Create the home, `sessions/` and `config.d/`; seed `config.yaml` and `credentials.yaml` (mode 0600)
/// from the checkout's `configs/` templates when missing.
pub fn init_charliebot_home(cfg: &CharlieBotConfig, repo_root: &Path) -> io::Result<()> {
  for directory in [cfg.charliebot_home.clone(), cfg.sessions_dir(), cfg.config_d_dir()] {
    fs::create_dir_all(&directory)?;
  }
  let config_file = cfg.config_file();
  if !config_file.exists() {
    let template = repo_root.join("configs").join("config.example.yaml");
    let text = if template.exists() {
      universal_newlines(&fs::read_to_string(&template)?)
    } else {
      DEFAULT_CONFIG_YAML.to_string()
    };
    fs::write(&config_file, text)?;
  }
  let credentials_file = cfg.credentials_file();
  if !credentials_file.exists() {
    fs::copy(repo_root.join("configs").join("credentials.example.yaml"), &credentials_file)?;
    fs::set_permissions(&credentials_file, fs::Permissions::from_mode(0o600))?;
  }
  Ok(())
}

/// Python's text-mode read: `\r\n` and `\r` become `\n`.
fn universal_newlines(text: &str) -> String {
  text.replace("\r\n", "\n").replace('\r', "\n")
}
