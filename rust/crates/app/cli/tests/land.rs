//! The `charliebot land` process tests. Each runs the built `charliebot land` on a synthetic task whose origin is a bare
//! repository on disk, with the GitHub stand-in `fixtures/land/gh` first on `PATH`, and reads back the PR the landing
//! leaves on the task branch.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};
use serde_json::{Value, json};
use tempfile::TempDir;

const GH_STUB: &str = include_str!("fixtures/land/gh");
const TASK_SESSION: &str = "5f0c2a8e-1b2c-4d3e-8f40-516273849500";
const TASK_RUN: &str = "9a8b7c6d-1b2c-4d3e-8f40-516273849500";
const TASK_BRANCH: &str = "charliebot/task-1794400000-9a8b7c6d";

/// Git isolated from the host's global and system configuration, so a host's hooks or signing never reach the test.
fn git_command(dir: &Path) -> Command {
  let mut command = Command::new("git");
  command.arg("-C").arg(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
  command
}

fn git(dir: &Path, args: &[&str]) -> String {
  let output = git_command(dir).args(args).output().expect("git runs");
  assert!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    dir.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  String::from_utf8(output.stdout).expect("git prints UTF-8").trim().to_string()
}

fn commit(dir: &Path, file: &str) -> String {
  fs::write(dir.join(file), format!("{file}\n")).expect("the file is written");
  git(dir, &["add", file]);
  git(dir, &["commit", "-q", "-m", file]);
  git(dir, &["rev-parse", "HEAD"])
}

/// One landing venue: a bare origin, the main checkout, and a task worktree with one commit on the task branch.
/// The venue also holds the task's session in a synthetic profile home and the gh stand-in's state.
struct Venue {
  _root: TempDir,
  origin: PathBuf,
  repo: PathBuf,
  worktree: PathBuf,
  home: PathBuf,
  stub_bin: PathBuf,
  gh_state: PathBuf,
}

impl Venue {
  /// *with_ci* commits a `pull_request` workflow to main, which puts the landing under the CI gate.
  fn new(with_ci: bool) -> Venue {
    let root = tempfile::tempdir().expect("a temporary directory");
    let origin = root.path().join("owner").join("repo.git");
    fs::create_dir_all(&origin).expect("the origin directory is created");
    git(&origin, &["init", "-q", "--bare", "--initial-branch=main"]);

    let repo = root.path().join("repo");
    fs::create_dir(&repo).expect("the repository directory is created");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Synthetic"]);
    git(&repo, &["config", "user.email", "synthetic@example.test"]);
    if with_ci {
      fs::create_dir_all(repo.join(".github/workflows")).expect("the workflows directory is created");
      fs::write(repo.join(".github/workflows/ci.yml"), "on:\n  pull_request:\njobs: {}\n")
        .expect("the workflow is written");
      git(&repo, &["add", ".github"]);
    }
    commit(&repo, "base.txt");
    git(&repo, &["remote", "add", "origin", &format!("file://{}", origin.display())]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let worktree = root.path().join("worktree");
    git(&repo, &["worktree", "add", "-q", "-b", TASK_BRANCH, worktree.to_str().expect("the path is UTF-8"), "main"]);
    commit(&worktree, "feature.txt");

    let home = root.path().join("home");
    let session_dir = home.join("sessions").join(TASK_SESSION);
    let run_dir = session_dir.join("data").join("runs").join(TASK_RUN);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut metadata = SessionMetadata::new(TASK_SESSION, "land a synthetic task", TaskProfile::Worker);
    metadata.task = Some(TaskSpec {
      goal: "land a synthetic task".to_string(),
      repo_path: Some(repo.display().to_string()),
      base_branch: Some("main".to_string()),
      ..TaskSpec::default()
    });
    fs::write(session_dir.join("metadata.json"), metadata.to_json_string(&[], false)).expect("the metadata is written");
    let run = json!({
      "id": TASK_RUN,
      "session_id": TASK_SESSION,
      "repo_path": repo.display().to_string(),
      "base_branch": "main",
      "branch_name": TASK_BRANCH,
      "worktree_path": worktree.display().to_string(),
    });
    fs::write(run_dir.join("metadata.json"), run.to_string()).expect("the run record is written");

    let stub_bin = root.path().join("stub-bin");
    fs::create_dir(&stub_bin).expect("the stub directory is created");
    fs::write(stub_bin.join("gh"), GH_STUB).expect("the gh stand-in is written");
    fs::set_permissions(stub_bin.join("gh"), fs::Permissions::from_mode(0o755)).expect("the gh stand-in is executable");
    let gh_state = root.path().join("gh-state");
    fs::create_dir(&gh_state).expect("the gh state directory is created");

    Venue { _root: root, origin, repo, worktree, home, stub_bin, gh_state }
  }

  fn path_with_stub(&self) -> String {
    format!("{}:{}", self.stub_bin.display(), std::env::var("PATH").expect("PATH is set"))
  }

  fn gh(&self, args: &[&str]) -> String {
    let output = Command::new(self.stub_bin.join("gh"))
      .args(args)
      .env("PATH", self.path_with_stub())
      .env("GH_STUB_STATE", &self.gh_state)
      .env("GH_STUB_ORIGIN", &self.origin)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .output()
      .expect("the gh stand-in runs");
    assert!(output.status.success(), "gh {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("gh prints UTF-8").trim().to_string()
  }

  /// The worker's own landing request: the task branch pushed and a PR opened on it.
  fn open_worker_pr(&self) -> String {
    git(&self.worktree, &["push", "-q", "origin", TASK_BRANCH]);
    self.gh(&["pr", "create", "--repo", "owner/repo", "--head", TASK_BRANCH, "--base", "main", "--title", "feature"]);
    self.origin_tip(TASK_BRANCH)
  }

  fn origin_tip(&self, branch: &str) -> String {
    git(&self.origin, &["rev-parse", &format!("refs/heads/{branch}")])
  }

  fn pr_state(&self) -> String {
    let view: Value = serde_json::from_str(&self.gh(&[
      "pr",
      "view",
      TASK_BRANCH,
      "--repo",
      "owner/repo",
      "--json",
      "number,url,state",
    ]))
    .expect("gh pr view prints JSON");
    view["state"].as_str().expect("the PR has a state").to_string()
  }

  fn gh_calls(&self) -> String {
    fs::read_to_string(self.gh_state.join("calls")).unwrap_or_default()
  }

  /// Run `charliebot land --task` from the task worktree; returns the process output.
  fn land_output(&self) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .args(["land", "--task", TASK_SESSION])
      .current_dir(&self.worktree)
      .env("CHARLIEBOT_HOME", &self.home)
      .env_remove("CHARLIEBOT_SESSION_ID")
      .env("PATH", self.path_with_stub())
      .env("GH_STUB_STATE", &self.gh_state)
      .env("GH_STUB_ORIGIN", &self.origin)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .output()
      .expect("charliebot land runs")
  }

  /// Run `charliebot land --task` from the task worktree; returns the exit code and the printed landing record.
  fn land(&self) -> (i32, Value) {
    let output = self.land_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let record = serde_json::from_str(&stdout).unwrap_or_else(|error| {
      panic!(
        "charliebot land printed no landing record ({error}); stdout: {stdout}; stderr: {}; gh calls:\n{}",
        String::from_utf8_lossy(&output.stderr),
        self.gh_calls()
      )
    });
    (output.status.code().expect("charliebot land exits with a code"), record)
  }
}

/// The path that left rebase-landed PRs open: a worker pushes the task branch and opens a PR, then main moves
/// before the base push. The landing rebases and pushes a patch-equivalent copy under a new sha. GitHub never
/// marks that PR merged, so the landing closes it.
#[test]
#[ignore = "integration"]
fn a_rebased_landing_closes_the_pr_whose_head_it_never_pushed() {
  let venue = Venue::new(false);
  let pr_head = venue.open_worker_pr();
  commit(&venue.repo, "moved-main.txt");
  git(&venue.repo, &["push", "-q", "origin", "main"]);

  let (code, record) = venue.land();

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed-no-ci");
  let landed = record["sha"].as_str().expect("the record names the landed sha").to_string();
  assert_eq!(venue.origin_tip("main"), landed);
  assert_ne!(pr_head, landed, "the landing rebased onto the moved main");
  assert_eq!(
    venue.origin_tip(TASK_BRANCH),
    pr_head,
    "the ungated landing leaves the task branch where the worker left it"
  );
  assert_eq!(venue.pr_state(), "CLOSED");
  let comment = fs::read_to_string(venue.gh_state.join("prs/1/comment")).expect("the close leaves a comment");
  assert!(comment.contains(&landed), "the closing comment names the landed sha: {comment}");
}

/// Under the gate, main moves between the task-branch push and the base push. The landing rebases, pushes the task
/// branch again, and lands that head, so GitHub marks the PR merged and the landing closes nothing.
#[test]
#[ignore = "integration"]
fn a_gated_landing_that_main_overtakes_ends_with_its_pr_merged() {
  let venue = Venue::new(true);
  venue.open_worker_pr();
  let moved_main = commit(&venue.repo, "moved-main.txt");
  git(&venue.repo, &["push", "-q", "origin", "main:refs/heads/moved-main"]);
  fs::write(venue.gh_state.join("advance-main"), &moved_main).expect("the main move is staged");

  let (code, record) = venue.land();

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed");
  assert_eq!(record["rebase_retries"], 1, "main moved once between the branch push and the base push");
  let landed = record["sha"].as_str().expect("the record names the landed sha").to_string();
  assert_eq!(venue.origin_tip("main"), landed);
  assert_eq!(venue.origin_tip(TASK_BRANCH), landed);
  assert_eq!(venue.pr_state(), "MERGED");
  assert!(!venue.gh_calls().contains("pr close"), "a PR GitHub merges is not closed:\n{}", venue.gh_calls());
}

/// A local pre-push hook that refuses pushes to the base branch — the worktree shares the main
/// checkout's hooks directory, as a task worktree does. The refusal is not a moved base, so the
/// landing ends on the first round with the hook's own message.
#[test]
#[ignore = "integration"]
fn a_hook_refused_base_push_fails_on_the_first_round_with_the_hooks_message() {
  let venue = Venue::new(false);
  let hook_calls = venue.home.join("pre-push-calls");
  let hook = venue.repo.join(".git/hooks/pre-push");
  let script = [
    "#!/usr/bin/env bash",
    &format!("echo call >>{}", hook_calls.display()),
    "while read -r _local_ref _local_sha remote_ref _remote_sha; do",
    "  if [ \"$remote_ref\" = refs/heads/main ]; then",
    "    echo 'pre-push: push to main is restricted; use merge_to_main.sh <branch>' >&2",
    "    exit 1",
    "  fi",
    "done",
  ]
  .join("\n");
  fs::write(&hook, script).expect("the pre-push hook is written");
  fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("the hook is executable");

  let output = venue.land_output();

  assert_eq!(output.status.code(), Some(1));
  let stderr = String::from_utf8_lossy(&output.stderr);
  let error = serde_json::from_str::<Value>(&stderr).expect("the failure prints a JSON error")["error"]
    .as_str()
    .expect("the error is a string")
    .to_string();
  assert!(error.contains("push to main is restricted"), "error: {error}");
  assert!(!error.contains("kept moving"), "error: {error}");
  let calls = fs::read_to_string(&hook_calls).expect("the hook recorded the push");
  assert_eq!(calls.lines().count(), 1, "the hook ran on the landing's only push: {calls}");
}
