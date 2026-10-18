//! Claude Code's default login directory and environment variable.

use std::env;
use std::path::{Path, PathBuf};

pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Return the default Claude login directory.
pub fn default_claude_dir() -> anyhow::Result<PathBuf> {
  let home = env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
  Ok(PathBuf::from(home).join(".claude"))
}

/// Resolve the login directory from CLAUDE_CONFIG_DIR, then HOME/.claude.
pub fn claude_config_dir() -> anyhow::Result<PathBuf> {
  match env::var_os(CLAUDE_CONFIG_DIR_ENV).filter(|value| !value.is_empty()) {
    Some(value) => Ok(PathBuf::from(value)),
    None => default_claude_dir(),
  }
}

/// Expand the supported leading tilde in an account configuration path.
pub fn expanduser(path: &str, home: Option<&str>) -> anyhow::Result<PathBuf> {
  if path == "~" || path.starts_with("~/") {
    let home = home.ok_or_else(|| anyhow::anyhow!("HOME is required to expand {path}"))?;
    let suffix = path.strip_prefix("~/").unwrap_or("");
    return Ok(Path::new(home).join(suffix));
  }
  Ok(PathBuf::from(path))
}
