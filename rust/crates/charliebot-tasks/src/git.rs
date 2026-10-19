//! Git evidence checks for task completion.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use charliebot_core::pyfmt;
use tokio::process::Command;
use tokio::time::timeout;

const GIT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_REMOTE_ATTEMPTS: usize = 3;

/// Check that a commit exists in a repository and is reachable from a target branch.
pub async fn git_verify_commit_landed(repo_path: &Path, branch: &str, commit: &str) -> (bool, String) {
  if !(7..=40).contains(&commit.len())
    || !commit.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
  {
    return (false, format!("commit {} is not a git commit hash", pyfmt::str_repr(commit)));
  }

  if let Some(remote_branch) = branch.strip_prefix("origin/") {
    let mut fetch_detail = String::new();
    let mut fetched = false;
    for _ in 0..GIT_REMOTE_ATTEMPTS {
      match git_output(repo_path, &["fetch", "origin", remote_branch]).await {
        Ok(output) if output.status.success() => {
          fetched = true;
          break;
        }
        Ok(output) => fetch_detail = stderr_text(&output).unwrap_or_else(|| status_text(&output)),
        Err(error) => fetch_detail = error,
      }
    }
    if !fetched {
      return (false, format!("could not fetch {branch} in {}: {fetch_detail}", repo_path.display()));
    }
  }

  let commit_object = format!("{commit}^{{commit}}");
  let checks = [
    ("existence", vec!["cat-file", "-e", commit_object.as_str()]),
    ("ancestry", vec!["merge-base", "--is-ancestor", commit, branch]),
  ];
  for (label, args) in checks {
    match git_output(repo_path, &args).await {
      Ok(output) if output.status.success() => {}
      Ok(output) => {
        let detail = stderr_text(&output).unwrap_or_else(|| status_text(&output));
        return (false, format!("{label} check failed in {} ({branch}): {detail}", repo_path.display()));
      }
      Err(error) => return (false, format!("git {label} check for {commit} failed: {error}")),
    }
  }
  (true, String::new())
}

async fn git_output(repo_path: &Path, args: &[&str]) -> Result<Output, String> {
  let mut command = Command::new("git");
  command
    .arg("-C")
    .arg(repo_path)
    .args(args)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  match timeout(GIT_READ_TIMEOUT, command.output()).await {
    Ok(Ok(output)) => Ok(output),
    Ok(Err(error)) => Err(error.to_string()),
    Err(_) => Err("operation timed out".to_string()),
  }
}

fn stderr_text(output: &Output) -> Option<String> {
  let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
  (!detail.is_empty()).then_some(detail)
}

fn status_text(output: &Output) -> String {
  output.status.code().map_or_else(|| "-1".to_string(), |code| code.to_string())
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::process::Command;

  use super::git_verify_commit_landed;

  fn git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git").arg("-C").arg(repo).args(args).output().expect("git is installed")
  }

  #[tokio::test]
  async fn synthetic_repository_verifies_landed_and_unlanded_commits() {
    // Python: tests/test_task_execution.py::test_landing_verification_negative_cases.
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = root.path().join("repo");
    let origin = root.path().join("origin.git");
    fs::create_dir(&repo).expect("repository directory is created");
    assert!(git(&repo, &["init", "-q", "-b", "main"]).status.success());
    assert!(git(&repo, &["config", "user.email", "synthetic@example.test"]).status.success());
    assert!(git(&repo, &["config", "user.name", "Synthetic"]).status.success());
    fs::write(repo.join("tracked.txt"), "base\n").expect("synthetic file is written");
    assert!(git(&repo, &["add", "."]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "base"]).status.success());
    let landed =
      String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout).expect("git sha is UTF-8").trim().to_string();
    assert!(
      Command::new("git")
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(&origin)
        .status()
        .expect("git is installed")
        .success()
    );
    assert!(
      git(&repo, &["remote", "add", "origin", origin.to_str().expect("synthetic path is UTF-8")]).status.success()
    );
    assert!(git(&repo, &["push", "-q", "-u", "origin", "main"]).status.success());

    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &landed).await;
    assert!(verified, "{reason}");
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &"f".repeat(40)).await;
    assert!(!verified);
    assert!(reason.starts_with("existence check failed in "));

    assert!(git(&repo, &["switch", "-q", "-c", "feature"]).status.success());
    fs::write(repo.join("unlanded.txt"), "unlanded\n").expect("synthetic file is written");
    assert!(git(&repo, &["add", "."]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "unlanded"]).status.success());
    let unlanded =
      String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout).expect("git sha is UTF-8").trim().to_string();
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &unlanded).await;
    assert!(!verified);
    assert!(reason.starts_with("ancestry check failed in "));
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &landed).await;
    assert!(verified, "{reason}");
  }

  #[tokio::test]
  async fn invalid_hash_reports_python_string_repr() {
    // Python: tests/test_task_execution.py::test_landing_verification_negative_cases.
    let root = tempfile::tempdir().expect("temporary directory is available");
    let (verified, reason) = git_verify_commit_landed(root.path(), "main", "not-a-hash").await;
    assert!(!verified);
    assert_eq!(reason, "commit 'not-a-hash' is not a git commit hash");
  }
}
