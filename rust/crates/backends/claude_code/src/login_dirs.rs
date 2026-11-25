//! Claude Code's login directories, and the process environment they resolve against.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_core::home::CLAUDE_CONFIG_DIR_NAME;
use charliebot_core::paths;

pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub const CREDENTIALS_FILE: &str = ".credentials.json";

/// Return the default Claude login directory.
pub fn default_claude_dir() -> anyhow::Result<PathBuf> {
  let home = paths::process_home().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
  Ok(PathBuf::from(home).join(CLAUDE_CONFIG_DIR_NAME))
}

/// Resolve the login directory from CLAUDE_CONFIG_DIR, then HOME/.claude.
pub fn claude_config_dir() -> anyhow::Result<PathBuf> {
  match env::var_os(CLAUDE_CONFIG_DIR_ENV).filter(|value| !value.is_empty()) {
    Some(value) => Ok(PathBuf::from(value)),
    None => default_claude_dir(),
  }
}

/// The directory a configuration file names for a Claude config directory: a leading `~` or `~/` becomes `home`,
/// and every other path stays as written. A `~` path without a home is an error, since the path would otherwise
/// name a literal directory called `~`.
pub fn config_dir_path(path: &str, home: Option<&str>) -> anyhow::Result<PathBuf> {
  paths::expanduser_under(path, home.map(Path::new)).ok_or_else(|| anyhow::anyhow!("HOME is required to expand {path}"))
}

/// The process environment the Claude usage source reads: the home directory for the default login directory and the
/// pool's `~`, and `CLAUDE_CONFIG_DIR` for the sweep's second transcript root.
#[derive(Clone, Debug)]
pub struct ClaudeHost {
  pub home: PathBuf,
  pub claude_config_dir: PathBuf,
}

impl ClaudeHost {
  /// The host of the running server. HOME must be set, since this host's home directory comes from it.
  pub fn from_process() -> anyhow::Result<Self> {
    let home = paths::process_home().context("HOME must be set for the Claude Code usage source")?;
    Ok(Self { home: PathBuf::from(home), claude_config_dir: claude_config_dir()? })
  }

  /// The default login directory: this host's home directory joined with [`CLAUDE_CONFIG_DIR_NAME`].
  pub fn default_dir(&self) -> PathBuf {
    self.home.join(CLAUDE_CONFIG_DIR_NAME)
  }

  /// Expand a leading `~` or `~/` in `raw` against this host's home.
  pub fn expand(&self, raw: &str) -> PathBuf {
    paths::expanduser_under(raw, Some(&self.home)).expect("the host carries its home")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_tilde_path_without_a_home_is_an_error() {
    assert_eq!(
      config_dir_path("~/.claude-b", Some("/synthetic/home")).unwrap(),
      PathBuf::from("/synthetic/home/.claude-b")
    );
    assert!(config_dir_path("~/.claude-b", None).is_err());
    assert_eq!(config_dir_path("/abs/.claude", None).unwrap(), PathBuf::from("/abs/.claude"));
  }
}
