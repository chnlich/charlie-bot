//! The scratch worktree a queued landing works in: a detached checkout at the queued commit, made under the task's
//! repository and removed when its guard drops.
//!
//! The queued command rebases, pushes and waits for CI in this checkout, never in the session's worktree, so the
//! session's checkout, edits and commits cannot stop a landing that waits in the queue. The checkout is detached, so it
//! holds no branch, and it lives under the home's state directory, outside the task worktree directory that the
//! disk sweep reads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use charliebot_core::git::{worktree_add_detached_args, worktree_prune_args, worktree_remove_forced_args};
use charliebot_core::home::state_dir;
use uuid::Uuid;

use crate::git;

/// The directory under the home's state directory that holds the scratch worktrees.
const LANDING_WORKTREE_DIR_NAME: &str = "land-worktrees";
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// The directory that holds every scratch worktree of the home.
pub fn root_in(home: &Path) -> PathBuf {
  state_dir(home).join(LANDING_WORKTREE_DIR_NAME)
}

/// A detached worktree of `repo` at one commit. Dropping the guard removes the worktree on every exit path, a panic
/// included; a removal that fails is logged, because a drop cannot return an error.
#[derive(Debug)]
pub struct LandingWorktree {
  repo: PathBuf,
  path: PathBuf,
}

impl LandingWorktree {
  /// Adds a detached worktree of `repo` at `commit` under `root`, named by a fresh UUID so no two landings share one.
  pub async fn add(repo: &Path, root: &Path, commit: &str) -> anyhow::Result<Self> {
    std::fs::create_dir_all(root).with_context(|| format!("could not create {}", root.display()))?;
    let path = root.join(Uuid::new_v4().to_string());
    let path_text = path.display().to_string();
    let (ok, _, err) =
      git::git_stdout(repo, &worktree_add_detached_args(&path_text, commit), WRITE_TIMEOUT, "git worktree add").await;
    if !ok {
      anyhow::bail!("git worktree add --detach {path_text} {commit} failed in {}: {err}", repo.display());
    }
    Ok(Self { repo: repo.to_path_buf(), path })
  }

  pub fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for LandingWorktree {
  fn drop(&mut self) {
    let path = self.path.display().to_string();
    if let Err(error) = git_sync(&self.repo, &worktree_remove_forced_args(&path)) {
      tracing::error!(target: "charliebot_tasks", path = %path, error = %error, "landing_worktree_remove_failed");
    }
    if self.path.exists()
      && let Err(error) = std::fs::remove_dir_all(&self.path)
    {
      tracing::error!(target: "charliebot_tasks", path = %path, error = %error, "landing_worktree_dir_remove_failed");
    }
    if let Err(error) = git_sync(&self.repo, &worktree_prune_args()) {
      tracing::error!(
        target: "charliebot_tasks",
        repo = %self.repo.display(),
        error = %error,
        "landing_worktree_prune_failed"
      );
    }
  }
}

/// Runs `git -C <repo> <args>` and returns git's stderr when it fails. A drop cannot await, so this is the synchronous
/// form of the call.
fn git_sync(repo: &Path, args: &[&str]) -> Result<(), String> {
  let output =
    std::process::Command::new("git").arg("-C").arg(repo).args(args).output().map_err(|error| error.to_string())?;
  if output.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&output.stderr).trim().to_string()) }
}
