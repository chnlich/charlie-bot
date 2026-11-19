//! The agent `ssh` guard: every ssh that an agent process starts by name is non-interactive and bounded.
//!
//! The guard script is `rust/crates/runtime/agents/ssh_guard/ssh`. `agent_environment` puts its directory on the PATH
//! of every agent process, right after the gh wrapper, so a name lookup of `ssh` finds the guard first.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// The repository directory that holds the guard script.
pub const GUARD_REPO_DIR: &str = "rust/crates/runtime/agents/ssh_guard";

/// The guard directory under `repo_root`.
pub fn dir_under(repo_root: &Path) -> PathBuf {
  repo_root.join(GUARD_REPO_DIR)
}

/// Refuse a start without an executable guard script, the way `gh_wrapper::require_installed` does for the wrapper.
pub fn require_installed(dir: &Path) -> anyhow::Result<()> {
  let script = dir.join("ssh");
  let metadata =
    fs::metadata(&script).with_context(|| format!("the agent ssh guard must exist at {}", script.display()))?;
  if !charliebot_core::paths::metadata_is_executable(&metadata) {
    bail!("the agent ssh guard must be an executable file at {}", script.display());
  }
  Ok(())
}
