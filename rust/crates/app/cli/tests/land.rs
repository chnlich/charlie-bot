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

#[path = "common/stand_in.rs"]
mod stand_in;

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
  commit_with_message(dir, file, file, "")
}

fn commit_with_message(dir: &Path, file: &str, subject: &str, body: &str) -> String {
  let path = dir.join(file);
  fs::create_dir_all(path.parent().expect("a committed file has a parent directory"))
    .expect("the file's directory is created");
  fs::write(path, format!("{file}\n")).expect("the file is written");
  git(dir, &["add", file]);
  let mut args = vec!["commit", "-q", "-m", subject];
  if !body.is_empty() {
    args.extend(["-m", body]);
  }
  git(dir, &args);
  git(dir, &["rev-parse", "HEAD"])
}

/// Writes a file that carries a PNG signature and an IHDR chunk of `width` and `height`, which is all the
/// screenshot gate reads, and returns its file name, which the PR body names it by.
fn write_screenshot(dir: &Path, name: &str, width: u32, height: u32) -> String {
  let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
  bytes.extend_from_slice(&13u32.to_be_bytes());
  bytes.extend_from_slice(b"IHDR");
  bytes.extend_from_slice(&width.to_be_bytes());
  bytes.extend_from_slice(&height.to_be_bytes());
  fs::write(dir.join(name), bytes).expect("the screenshot is written");
  name.to_string()
}

/// Writes one PNG per window size and returns their file names, in the order the set lists them.
fn write_window_screenshots(dir: &Path) -> Vec<String> {
  charliebot_tasks::web_screenshots::WINDOW_SIZES
    .iter()
    .map(|(width, height)| write_screenshot(dir, &format!("shot-{width}x{height}.png"), *width, *height))
    .collect()
}

/// The `--evidence-dir` argument that names the directory `shots`.
fn evidence_arg(shots: &Path) -> String {
  shots.display().to_string()
}

/// A served-page change on the task branch, committed with an empty message, so a test supplies the PR body.
fn web_change(venue: &Venue) -> std::path::PathBuf {
  let shots = venue.home.join("shots");
  fs::create_dir(&shots).expect("the screenshot directory is created");
  commit_with_message(&venue.worktree, "web/static/js/tray.js", "Open the choice tray", "");
  shots
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
    Self::with_feature_message(with_ci, "feature.txt", "feature.txt", "")
  }

  /// *file* is the one file the task branch's commit changes, so a test sets the diff the landing gates.
  fn with_feature_message(with_ci: bool, file: &str, subject: &str, body: &str) -> Venue {
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
    commit_with_message(&worktree, file, subject, body);

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
    self.open_worker_pr_with_text("feature", "")
  }

  fn open_worker_pr_with_text(&self, title: &str, body: &str) -> String {
    git(&self.worktree, &["push", "-q", "origin", TASK_BRANCH]);
    self.gh(&[
      "pr",
      "create",
      "--repo",
      "owner/repo",
      "--head",
      TASK_BRANCH,
      "--base",
      "main",
      "--title",
      title,
      "--body",
      body,
    ]);
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

  fn pr_text(&self) -> (String, String) {
    (
      fs::read_to_string(self.gh_state.join("prs/1/title")).expect("the PR title is stored"),
      fs::read_to_string(self.gh_state.join("prs/1/body")).expect("the PR body is stored"),
    )
  }

  fn origin_has_task_branch(&self) -> bool {
    Command::new("git")
      .args(["--git-dir"])
      .arg(&self.origin)
      .args(["show-ref", "--verify", "--quiet"])
      .arg(format!("refs/heads/{TASK_BRANCH}"))
      .status()
      .expect("git checks the origin ref")
      .success()
  }

  fn gh_calls(&self) -> String {
    fs::read_to_string(self.gh_state.join("calls")).unwrap_or_default()
  }

  /// Run `charliebot land --task` from the task worktree; returns the process output.
  fn land_output(&self) -> std::process::Output {
    self.land_output_with_args(&[])
  }

  /// Starts a stand-in live server that knows `sessions`, and points the home's config and credential at it, so the
  /// landing's session check reads that server. The landing makes one session check per `/?session=` View line.
  fn live_server_knows(&self, sessions: &[&str]) {
    let routes: Vec<String> = sessions.iter().map(|session| format!("/api/sessions/{session}")).collect();
    let (port, _server) =
      stand_in::serve(1, move |path| if routes.iter().any(|route| route == path) { 200 } else { 404 });
    fs::write(self.home.join("config.yaml"), format!("server:\n  port: {port}\n")).expect("the config is written");
    fs::write(self.home.join("credentials.yaml"), "charliebot:\n  access_key: test-key\n")
      .expect("the credentials are written");
  }

  fn land_output_with_args(&self, extra_args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .args(["land", "--task", TASK_SESSION])
      .args(extra_args)
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
    self.land_with_args(&[])
  }

  fn land_with_args(&self, args: &[&str]) -> (i32, Value) {
    let output = self.land_output_with_args(args);
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

#[test]
#[ignore = "integration"]
fn a_single_commit_supplies_the_draft_pr_title_and_body() {
  let venue = Venue::with_feature_message(true, "feature.txt", "Add routing support", "Keep callers on a stable path.");

  let (code, record) = venue.land();

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed");
  assert_eq!(venue.pr_text(), ("Add routing support".to_string(), "Keep callers on a stable path.".to_string()));
  let calls = venue.gh_calls();
  let create = calls
    .lines()
    .find(|call| call.starts_with("pr create "))
    .unwrap_or_else(|| panic!("the landing creates a PR: {calls}"));
  assert!(create.contains("--draft"), "the created PR is a draft: {create}");
}

#[test]
#[ignore = "integration"]
fn explicit_title_and_body_file_override_the_commit_defaults() {
  let venue = Venue::new(true);
  let body_file = venue.home.join("description.md");
  fs::write(&body_file, "The reviewed change keeps the routing contract stable.\n")
    .expect("the PR description is written");
  let body_file = body_file.display().to_string();

  let (code, record) = venue.land_with_args(&["--title", "Reviewed routing change", "--body-file", &body_file]);

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(
    venue.pr_text(),
    ("Reviewed routing change".to_string(), "The reviewed change keeps the routing contract stable.\n".to_string())
  );
}

#[test]
#[ignore = "integration"]
fn several_commits_without_explicit_pr_text_are_refused_before_any_push() {
  let venue = Venue::new(true);
  commit_with_message(&venue.worktree, "second.txt", "Second change", "");

  let output = venue.land_output();

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("2 commits"), "stderr: {stderr}");
  assert!(stderr.contains("--title"), "stderr: {stderr}");
  assert!(stderr.contains("commits-and-prs.md"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn several_commits_land_with_explicit_title_and_body_file() {
  let venue = Venue::new(true);
  commit_with_message(&venue.worktree, "second.txt", "Second change", "");
  let body_file = venue.home.join("several-commits.md");
  fs::write(&body_file, "Both commits make one routing change.\n").expect("the PR description is written");
  let body_file = body_file.display().to_string();

  let (code, record) = venue.land_with_args(&["--title", "Routing change", "--body-file", &body_file]);

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed");
  assert_eq!(venue.pr_text(), ("Routing change".to_string(), "Both commits make one routing change.\n".to_string()));
}

#[test]
#[ignore = "integration"]
fn a_stale_open_pr_is_updated_to_the_resolved_title_and_body() {
  let venue = Venue::new(true);
  venue.open_worker_pr_with_text("Previous commit title", "Description from an earlier commit.");

  let (code, record) = venue.land();

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(venue.pr_text(), ("feature.txt".to_string(), String::new()));
  let calls = venue.gh_calls();
  assert!(
    calls.lines().any(|call| call.starts_with("api repos/") && call.contains("/pulls/1 -X PATCH ")),
    "the stale PR is patched through the REST pull endpoint: {calls}"
  );
}

#[test]
#[ignore = "integration"]
fn an_unreadable_body_file_fails_before_push_or_pr_creation() {
  let venue = Venue::new(true);
  let missing_file = venue.home.join("missing-description.md").display().to_string();

  let output = venue.land_output_with_args(&["--body-file", &missing_file]);

  assert_ne!(output.status.code(), Some(0));
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty());
}

#[test]
#[ignore = "integration"]
fn an_empty_body_file_fails_before_push_or_pr_creation() {
  let venue = Venue::new(true);
  let body_file = venue.home.join("empty-description.md");
  fs::write(&body_file, " \n\t").expect("the empty description is written");
  let body_file = body_file.display().to_string();

  let output = venue.land_output_with_args(&["--body-file", &body_file]);

  assert_ne!(output.status.code(), Some(0));
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty());
}

#[test]
#[ignore = "integration"]
fn an_empty_title_fails_before_push_or_pr_creation() {
  let venue = Venue::new(true);

  let output = venue.land_output_with_args(&["--title", " \t\n"]);

  assert_ne!(output.status.code(), Some(0));
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty());
}

#[test]
#[ignore = "integration"]
fn ungated_landing_accepts_but_does_not_use_explicit_pr_text() {
  let venue = Venue::new(false);

  let (code, record) = venue.land_with_args(&["--title", "Unused title", "--body", "Unused body"]);

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed-no-ci");
  assert!(!venue.gh_calls().contains("pr create"));
}

/// Writes the demo file a passing body names into the evidence directory `shots`, and returns its file name.
fn demo_file(shots: &Path) -> String {
  fs::write(shots.join("demo.html"), "<html>the approved tray demo</html>").expect("the demo is written");
  "demo.html".to_string()
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_without_a_demo_line_is_refused() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let body = format!("View: /?session=tray\n{}", write_window_screenshots(&shots).join(" "));

  let output = venue.land_output_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("Demo"), "stderr: {stderr}");
  assert!(stderr.contains("1536x900"), "the refusal names the window sizes: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_without_a_view_line_is_refused() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  let body = format!("Demo: {demo}\n{}", write_window_screenshots(&shots).join(" "));

  let output = venue.land_output_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("View"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_with_a_screenshot_of_the_wrong_height_is_refused() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  let mut named = write_window_screenshots(&shots);
  // A 1440x1600 review screenshot and a 125%-scaled 1920 px wide capture are both outside the set.
  named.remove(0);
  named.push(write_screenshot(&shots, "review-1440x1600.png", 1440, 1600));
  named.push(write_screenshot(&shots, "scaled-1920x1125.png", 1920, 1125));
  let body = format!("Demo: {demo}\nView: /?session=tray\n{}", named.join(" "));

  let output = venue.land_output_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("1536x900"), "the refusal names the missing size: {stderr}");
  assert!(stderr.contains("1440x1600"), "the refusal names the mismatched file: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_lands_with_demo_view_and_each_window_size_and_carries_the_view_trailer() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  venue.live_server_knows(&["tray"]);
  let body = format!("Demo: {demo}\nView: /?session=tray\n{}", write_window_screenshots(&shots).join(" "));

  let (code, record) = venue.land_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed-no-ci");
  let landed = git(&venue.origin, &["log", "-1", "--format=%B", "refs/heads/main"]);
  assert!(
    landed.lines().any(|line| line == "View: /?session=tray"),
    "the landed commit carries the View trailer: {landed}"
  );
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_naming_a_session_the_live_server_does_not_know_is_refused_before_the_merge() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  venue.live_server_knows(&["tray"]);
  let body = format!("Demo: {demo}\nView: /?session=unknown\n{}", write_window_screenshots(&shots).join(" "));

  let output = venue.land_output_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("must name a view on the live server"), "stderr: {stderr}");
  assert!(stderr.contains("does not know session unknown"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_commit_message_view_with_an_empty_session_id_is_refused_before_the_push() {
  let venue = Venue::with_feature_message(false, "feature.txt", "Open the tray", "View: /?session=");

  let output = venue.land_output();

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("must name a view on the live server"), "stderr: {stderr}");
  assert!(stderr.contains("is not a session id"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_commit_message_view_naming_a_session_the_live_server_does_not_know_is_refused_before_the_push() {
  let venue = Venue::with_feature_message(false, "feature.txt", "Open the tray", "View: /?session=unknown");
  venue.live_server_knows(&["tray"]);

  let output = venue.land_output();

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("must name a view on the live server"), "stderr: {stderr}");
  assert!(stderr.contains("does not know session unknown"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_naming_a_file_the_evidence_directory_lacks_is_refused_before_the_push() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  let mut named = write_window_screenshots(&shots);
  named.push("missing-review.png".to_string());
  let body = format!("Demo: {demo}\nView: /?session=tray\n{}", named.join(" "));

  let output = venue.land_output_with_args(&["--body", &body, "--evidence-dir", &evidence_arg(&shots)]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("missing-review.png"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

#[test]
#[ignore = "integration"]
fn a_web_page_change_without_an_evidence_directory_is_refused_before_the_push() {
  let venue = Venue::new(false);
  let shots = web_change(&venue);
  let demo = demo_file(&shots);
  let body = format!("Demo: {demo}\nView: /?session=tray\n{}", write_window_screenshots(&shots).join(" "));

  let output = venue.land_output_with_args(&["--body", &body]);

  assert_ne!(output.status.code(), Some(0));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("--evidence-dir"), "stderr: {stderr}");
  assert!(!venue.origin_has_task_branch());
  assert!(venue.gh_calls().is_empty(), "gh calls:\n{}", venue.gh_calls());
}

/// A web test file is no served page, so its landing carries no Demo, View or screenshot and lands.
#[test]
#[ignore = "integration"]
fn a_change_to_web_tests_lands_without_demo_view_or_screenshots() {
  let venue =
    Venue::with_feature_message(false, "web/tests/websocket_reconnect_poke.test.js", "Cover the reconnect poke", "");

  let (code, record) = venue.land();

  assert_eq!(code, 0, "record: {record}");
  assert_eq!(record["state"], "landed-no-ci");
  let landed = record["sha"].as_str().expect("the record names the landed sha").to_string();
  assert_eq!(venue.origin_tip("main"), landed);
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

#[test]
#[ignore = "integration"]
fn the_help_names_every_window_size() {
  let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args(["land", "--help"])
    .output()
    .expect("charliebot land --help runs");

  assert_eq!(output.status.code(), Some(0));
  let help = String::from_utf8_lossy(&output.stdout);
  for size in charliebot_tasks::web_screenshots::WINDOW_SIZES {
    let label = format!("{}x{}", size.0, size.1);
    assert!(help.contains(&label), "the help names {label}: {help}");
  }
}
