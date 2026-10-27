//! Claude Code's login directories, and the process environment they resolve against.

use std::env;
use std::path::PathBuf;

use anyhow::Context;

pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
pub const CREDENTIALS_FILE: &str = ".credentials.json";

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

/// Expand a leading `~` or `~/` in an account configuration path against the supplied home, as `Path.expanduser`
/// does. The home's trailing slashes go first, as `os.path.expanduser` strips them; `~user` stays as written.
pub fn expanduser(path: &str, home: Option<&str>) -> anyhow::Result<PathBuf> {
  if path == "~" || path.starts_with("~/") {
    let home = home.ok_or_else(|| anyhow::anyhow!("HOME is required to expand {path}"))?;
    if home == "/" {
      return Ok(PathBuf::from(if path == "~" { "/" } else { &path[1..] }));
    }
    let home = home.trim_end_matches('/');
    return Ok(PathBuf::from(shellexpand::tilde_with_context(path, || Some(home)).into_owned()));
  }
  Ok(PathBuf::from(path))
}

/// The process environment the Claude usage source reads: the home directory for the default login directory and the
/// pool's `~`, and `CLAUDE_CONFIG_DIR` for the sweep's second transcript root.
#[derive(Clone, Debug)]
pub struct ClaudeHost {
  pub home: PathBuf,
  pub claude_config_dir: PathBuf,
}

impl ClaudeHost {
  /// The host of the running server. HOME must be set, as `Path.home()` needs it.
  pub fn from_process() -> anyhow::Result<Self> {
    let home = std::env::var_os("HOME").context("HOME must be set for the Claude Code usage source")?;
    Ok(Self { home: PathBuf::from(home), claude_config_dir: claude_config_dir()? })
  }

  /// `Path.home() / ".claude"`: the default login directory.
  pub fn default_dir(&self) -> PathBuf {
    self.home.join(".claude")
  }

  /// `Path(raw).expanduser()` against this host's home: a leading `~` or `~/` names the home.
  pub fn expand(&self, raw: &str) -> PathBuf {
    match raw.strip_prefix('~') {
      Some("") => self.home.clone(),
      Some(rest) => match rest.strip_prefix('/') {
        Some(under_home) => self.home.join(under_home),
        None => PathBuf::from(raw),
      },
      None => PathBuf::from(raw),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_leading_tilde_names_the_supplied_home() {
    assert_eq!(expanduser("~/.claude-b", Some("/synthetic/home")).unwrap(), PathBuf::from("/synthetic/home/.claude-b"));
    assert_eq!(expanduser("/abs/.claude", Some("/synthetic/home")).unwrap(), PathBuf::from("/abs/.claude"));
    assert_eq!(expanduser("~user/.claude", Some("/synthetic/home")).unwrap(), PathBuf::from("~user/.claude"));
    assert_eq!(expanduser("~", Some("/synthetic/home")).unwrap(), PathBuf::from("/synthetic/home"));
    assert_eq!(
      expanduser("~/.claude-b", Some("/synthetic/home/")).unwrap(),
      PathBuf::from("/synthetic/home/.claude-b")
    );
    assert_eq!(expanduser("~", Some("/")).unwrap(), PathBuf::from("/"));
    assert_eq!(expanduser("~/claude", Some("/")).unwrap(), PathBuf::from("/claude"));
    assert!(expanduser("~/.claude-b", None).is_err());
  }
}
