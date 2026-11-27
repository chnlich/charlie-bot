//! Task-branch push diagnostics, including a dry run with the same lease.

use std::path::Path;
use std::time::Duration;

use crate::git;
use crate::land::{PushRejection, push_rejection_kind};

pub(crate) async fn push_task_branch(
  worktree: &Path,
  remote: &str,
  branch: &str,
  lease: &str,
  timeout: Duration,
) -> anyhow::Result<()> {
  let (ok, stdout, stderr) =
    git::git_output(worktree, &["push", lease, remote, branch], timeout, "git push task branch").await;
  if ok {
    return Ok(());
  }
  let (dry_ok, dry_stdout, dry_stderr) =
    git::git_output(worktree, &["push", "--dry-run", lease, remote, branch], timeout, "git push --dry-run").await;
  let (kind, detail) = if dry_ok {
    (PushRejection::Refused, "server-side refusal; not NonFastForward, Transport, or Auth")
  } else {
    (push_rejection_kind(&format!("{stderr}\n{dry_stderr}")), "push and dry-run both failed")
  };
  anyhow::bail!(
    "pushing task branch {branch} failed ({kind:?}): {detail}\n\
     push stdout:\n{stdout}\npush stderr:\n{stderr}\n\
     dry-run stdout:\n{dry_stdout}\ndry-run stderr:\n{dry_stderr}"
  )
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::{Path, PathBuf};
  use std::process::Command;
  use std::time::Duration;

  use super::push_task_branch;

  fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
  }

  fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let origin = root.join("origin.git");
    git(root, &["init", "--bare", "--initial-branch=main", origin.to_str().unwrap()]);
    let work = root.join("work");
    git(root, &["clone", origin.to_str().unwrap(), work.to_str().unwrap()]);
    fs::write(work.join("tracked"), "content\n").unwrap();
    git(&work, &["add", "tracked"]);
    git(&work, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "test"]);
    git(&work, &["switch", "-c", "task"]);
    (origin, work)
  }

  fn hook(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn client_hook_output_is_retained_in_both_attempts() {
    let root = tempfile::tempdir().unwrap();
    let (_, work) = fixture(root.path());
    hook(
      &work.join(".git/hooks/pre-push"),
      "#!/bin/sh\nprintf '  client stdout\\n'\nprintf '  client stderr\\n' >&2\nexit 1\n",
    );
    let error =
      push_task_branch(&work, "origin", "task", "--force-with-lease=refs/heads/task:", Duration::from_secs(30))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Refused"), "{error}");
    assert!(!error.contains("NonFastForward") && !error.contains("base moved"), "{error}");
    assert_eq!(error.matches("  client stdout\n").count(), 2, "{error}");
    assert_eq!(error.matches("  client stderr\n").count(), 2, "{error}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn server_hook_refusal_is_reported_when_dry_run_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let (origin, work) = fixture(root.path());
    hook(&origin.join("hooks/pre-receive"), "#!/bin/sh\necho server-refused\nexit 1\n");
    hook(&work.join(".git/hooks/pre-push"), "#!/bin/sh\necho client-ok\necho client-note >&2\n");
    let error =
      push_task_branch(&work, "origin", "task", "--force-with-lease=refs/heads/task:", Duration::from_secs(30))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Refused") && error.contains("server-side refusal"), "{error}");
    assert!(error.contains("not NonFastForward, Transport, or Auth"), "{error}");
    assert!(error.contains("server-refused"), "{error}");
    assert_eq!(error.matches("client-ok\n").count(), 2, "{error}");
    assert_eq!(error.matches("client-note\n").count(), 2, "{error}");
  }
}
