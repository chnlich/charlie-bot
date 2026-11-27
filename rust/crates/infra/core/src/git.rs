//! Git facts the server shares: the reads it makes in the repository root, the plain run whose exit status and
//! streams a caller reads, the timed run that an async caller awaits, the arguments of the object-existence probe,
//! of the merge-base queries and of the worktree add, remove and prune commands, and the name of the quarantine
//! directory beside the worktrees.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use wait_timeout::ChildExt;

/// The timeout of a best-effort `git rev-parse --short HEAD` read: the server's startup SHA and the CLI's
/// version-skew hint both accept no SHA past it.
pub const SHA_TIMEOUT: Duration = Duration::from_secs(2);
/// The page version's git timeout.
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

/// The exit status and both captured streams of `git <args>` run in `repo_root`, with `env` set and no timeout. A spawn
/// failure is the error. A nonzero exit is not: the caller reads the status and words its own failure. The directory
/// goes in with `-C`, so a missing directory reaches the caller as a failed status that git reports.
pub fn output(repo_root: &Path, args: &[&str], env: &[(&str, &str)]) -> std::io::Result<std::process::Output> {
  Command::new("git").arg("-C").arg(repo_root).args(args).envs(env.iter().copied()).output()
}

/// The output of `git <args>` run in `repo_root`, with `stdin` piped in when it is `Some` and `env` set on the child.
/// `None` means `timeout` passed first. The timeout drops the child, and `kill_on_drop` kills it then. A spawn failure
/// is the error, and a nonzero exit is not: the caller reads the status.
///
/// The input is written while the output is drained, so a git that fills its stdout before it reads all its input
/// cannot block both pipes. A git that exits before it reads its input fails the write with a broken pipe; the exit
/// status carries that failure, so the broken pipe is not an error here.
pub async fn output_within(
  repo_root: &Path,
  args: &[&str],
  stdin: Option<Vec<u8>>,
  env: &[(&str, &str)],
  timeout: Duration,
) -> std::io::Result<Option<Output>> {
  let mut command = tokio::process::Command::new("git");
  command
    .args(args)
    .current_dir(repo_root)
    .envs(env.iter().copied())
    .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  let mut child = command.spawn()?;
  let child_stdin = child.stdin.take();
  let write = async move {
    let Some(bytes) = stdin else {
      return Ok(());
    };
    let mut child_stdin = child_stdin.expect("git stdin is piped when input is present");
    match child_stdin.write_all(&bytes).await {
      Ok(()) => Ok(()),
      Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
      Err(error) => Err(error),
    }
  };
  let finished = async { tokio::join!(write, child.wait_with_output()) };
  match tokio::time::timeout(timeout, finished).await {
    Ok((written, output)) => {
      written?;
      output.map(Some)
    }
    Err(_) => Ok(None),
  }
}

/// The short HEAD sha.
pub fn head_sha(repo_root: &Path, timeout: Duration) -> Option<String> {
  run(repo_root, &["rev-parse", "--short", "HEAD"], timeout)
}

/// The arguments that probe whether the object `spec` names exists: `git cat-file -e <spec>`.
///
/// The probe prints nothing and answers in its exit status: 0 when the object is there, 1 when a well-formed name
/// matches no object, and 128 when git cannot resolve the spec, as for `<revision>:<path>` whose path is absent at
/// that revision. A caller reads that status, so [`run`], which folds a non-zero status and empty stdout into
/// `None`, cannot answer the probe.
pub fn object_exists_args(spec: &str) -> [&str; 3] {
  ["cat-file", "-e", spec]
}

/// The arguments that name the merge base of `base` and `head`: `git merge-base <base> <head>`.
pub fn merge_base_args<'a>(base: &'a str, head: &'a str) -> [&'a str; 3] {
  ["merge-base", base, head]
}

/// The arguments that test whether `ancestor` is reachable from `descendant`: `git merge-base --is-ancestor
/// <ancestor> <descendant>`. The test prints nothing and answers in its exit status: 0 when the ancestor is
/// reachable, 1 when it is not.
pub fn ancestry_args<'a>(ancestor: &'a str, descendant: &'a str) -> [&'a str; 4] {
  ["merge-base", "--is-ancestor", ancestor, descendant]
}

/// The arguments that add a worktree at `path` on a new branch `branch` started at `start`:
/// `git worktree add -b <branch> <path> <start>`.
pub fn worktree_add_new_branch_args<'a>(branch: &'a str, path: &'a str, start: &'a str) -> [&'a str; 6] {
  ["worktree", "add", "-b", branch, path, start]
}

/// The arguments that add a worktree at `path` on the existing branch `branch`: `git worktree add <path> <branch>`.
pub fn worktree_add_existing_branch_args<'a>(path: &'a str, branch: &'a str) -> [&'a str; 4] {
  ["worktree", "add", path, branch]
}

/// The arguments that add a detached worktree at `path`, checked out at `commit`: `git worktree add --detach <path>
/// <commit>`. No branch is checked out in it, so it never collides with a branch another worktree holds.
pub fn worktree_add_detached_args<'a>(path: &'a str, commit: &'a str) -> [&'a str; 5] {
  ["worktree", "add", "--detach", path, commit]
}

/// The arguments that remove the worktree at `path`: `git worktree remove <path>`. Without `--force`, git refuses a
/// worktree with modified tracked files or untracked files, and it makes that check at the moment of the removal.
pub fn worktree_remove_args(path: &str) -> [&str; 3] {
  ["worktree", "remove", path]
}

/// The arguments that remove the worktree at `path` even when it holds local changes: `git worktree remove --force
/// <path>`.
pub fn worktree_remove_forced_args(path: &str) -> [&str; 4] {
  ["worktree", "remove", "--force", path]
}

/// The arguments that drop the records of worktrees whose directories are gone: `git worktree prune`.
pub fn worktree_prune_args() -> [&'static str; 2] {
  ["worktree", "prune"]
}

#[cfg(test)]
mod tests {
  use std::process::Command;

  use super::*;

  #[test]
  fn the_object_probe_is_cat_file_with_the_existence_flag() {
    assert_eq!(object_exists_args("HEAD^{commit}"), ["cat-file", "-e", "HEAD^{commit}"]);
  }

  #[test]
  fn the_merge_base_args_name_the_two_commits() {
    assert_eq!(merge_base_args("origin/main", "HEAD"), ["merge-base", "origin/main", "HEAD"]);
  }

  #[test]
  fn the_ancestry_args_carry_the_is_ancestor_flag() {
    assert_eq!(ancestry_args("origin/main", "HEAD"), ["merge-base", "--is-ancestor", "origin/main", "HEAD"]);
  }

  #[test]
  fn the_worktree_remove_args_add_force_only_in_the_forced_form() {
    assert_eq!(worktree_remove_args("/w/a"), ["worktree", "remove", "/w/a"]);
    assert_eq!(worktree_remove_forced_args("/w/a"), ["worktree", "remove", "--force", "/w/a"]);
  }

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

  #[ignore = "integration"]
  #[test]
  fn run_returns_the_trimmed_stdout_of_a_successful_command() {
    let repo = repo_with_one_commit();
    let timeout = Duration::from_secs(30);
    assert_eq!(run(repo.path(), &["rev-parse", "--is-inside-work-tree"], timeout).as_deref(), Some("true"));
    let short = Command::new("git").args(["rev-parse", "--short", "HEAD"]).current_dir(repo.path()).output().unwrap();
    let expected = String::from_utf8(short.stdout).unwrap().trim().to_string();
    assert_eq!(head_sha(repo.path(), timeout), Some(expected));
  }

  #[ignore = "integration"]
  #[test]
  fn run_is_none_when_the_command_fails() {
    let not_a_repo = tempfile::tempdir().unwrap();
    assert_eq!(run(not_a_repo.path(), &["rev-parse", "--short", "HEAD"], Duration::from_secs(30)), None);
  }

  /// A git that outlives the timeout answers `None`, not an error: a caller bounds a hung git with it. The alias sleeps
  /// five seconds, far past the 200 ms timeout, so the answer does not depend on how fast git starts. It waits out the
  /// timeout, so it is an integration test and stays out of the unit-test time gate.
  #[ignore = "integration"]
  #[test]
  fn output_within_is_none_when_git_outlives_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let outcome = runtime.block_on(output_within(
      dir.path(),
      &["-c", "alias.nap=!sleep 5", "nap"],
      None,
      &[],
      Duration::from_millis(200),
    ));
    assert!(outcome.unwrap().is_none());
  }
}
