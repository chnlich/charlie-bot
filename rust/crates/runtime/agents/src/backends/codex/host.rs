//! The codex package's process environment: the PATH that finds the codex binary, and the home directory.

use std::path::PathBuf;

use anyhow::Context;

/// The process environment the codex package reads: PATH to find the codex binary, and HOME for `~/.local/bin` and
/// `~/.codex`.
#[derive(Clone, Debug)]
pub struct CodexHost {
  pub path: String,
  pub home: PathBuf,
}

impl CodexHost {
  /// The host of the running server. PATH defaults to Python's `os.defpath` when unset; HOME must be set.
  pub fn from_process() -> anyhow::Result<Self> {
    let home = std::env::var("HOME").context("HOME must be set for the codex package")?;
    let path = match std::env::var("PATH") {
      Ok(path) => path,
      Err(std::env::VarError::NotPresent) => "/bin:/usr/bin".to_string(),
      Err(error) => return Err(error).context("PATH must be valid Unicode for the codex package"),
    };
    Ok(Self { path, home: PathBuf::from(home) })
  }

  /// `Path.home() / ".local" / "bin"`: the directory the child PATH gains, and the binary's fallback.
  pub fn user_local_bin(&self) -> PathBuf {
    self.home.join(".local").join("bin")
  }

  /// `Path.home() / ".codex"`: the default codex home that holds the rollouts.
  pub fn codex_home(&self) -> PathBuf {
    self.home.join(".codex")
  }
}
