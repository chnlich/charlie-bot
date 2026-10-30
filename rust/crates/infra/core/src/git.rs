//! Git facts the server shares: the reads it makes in the repository root, and the name of the quarantine
//! directory beside the worktrees.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// `git rev-parse --short HEAD` timeout at startup (`timeouts.SUBPROCESS_GIT_SHA_TIMEOUT`).
pub const SHA_TIMEOUT: Duration = Duration::from_secs(2);
/// The page version's git timeout (`timeouts.SUBPROCESS_GIT_VERSION_TIMEOUT`).
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The quarantine directory name under `paths.worktree_dir`. The live-worktree list skips it, and
/// `charliebot gc-trash` purges it.
pub const TRASH_DIR_NAME: &str = ".trash";

/// The stripped stdout of `git <args>` run in `repo_root`, or `None` when git is missing, exits
/// non-zero, prints nothing, or outlives `timeout`. Each caller picks its own sentinel for `None`.
pub fn run(repo_root: &Path, args: &[&str], timeout: Duration) -> Option<String> {
  let mut child = Command::new("git")
    .args(args)
    .current_dir(repo_root)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .ok()?;
  let status = match child.wait_timeout(timeout) {
    Ok(Some(status)) => status,
    Ok(None) | Err(_) => {
      // The git process is abandoned either way; a failed kill leaves nothing to report beyond None.
      let _ = child.kill();
      let _ = child.wait();
      return None;
    }
  };
  if !status.success() {
    return None;
  }
  let mut stdout = String::new();
  std::io::Read::read_to_string(&mut child.stdout.take()?, &mut stdout).ok()?;
  let trimmed = stdout.trim();
  if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// `buildinfo.read_repo_head_sha`: the short HEAD sha.
pub fn head_sha(repo_root: &Path, timeout: Duration) -> Option<String> {
  run(repo_root, &["rev-parse", "--short", "HEAD"], timeout)
}

#[cfg(test)]
mod tests {
  use std::process::Command;

  use super::*;

  /// A repository with one empty commit, made by the git CLI that the server runs.
  fn repo_with_one_commit() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
      let status = Command::new("git").args(args).current_dir(repo.path()).status().unwrap();
      assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&[
      "-c",
      "user.name=synthetic",
      "-c",
      "user.email=synthetic@example.test",
      "commit",
      "-q",
      "--allow-empty",
      "-m",
      "synthetic",
    ]);
    repo
  }

  #[test]
  fn run_returns_the_trimmed_stdout_of_a_successful_command() {
    let repo = repo_with_one_commit();
    let timeout = Duration::from_secs(30);
    assert_eq!(run(repo.path(), &["rev-parse", "--is-inside-work-tree"], timeout).as_deref(), Some("true"));
    let short = Command::new("git").args(["rev-parse", "--short", "HEAD"]).current_dir(repo.path()).output().unwrap();
    let expected = String::from_utf8(short.stdout).unwrap().trim().to_string();
    assert_eq!(head_sha(repo.path(), timeout), Some(expected));
  }

  #[test]
  fn run_is_none_when_the_command_fails() {
    let not_a_repo = tempfile::tempdir().unwrap();
    assert_eq!(run(not_a_repo.path(), &["rev-parse", "--short", "HEAD"], Duration::from_secs(30)), None);
  }
}
