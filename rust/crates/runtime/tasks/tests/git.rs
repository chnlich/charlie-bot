//! Ports of the Python git-helper test suites (`tests/test_git_worktree_base.py` and
//! `tests/test_git_worktree_remove.py`).
//!
//! The repositories and their `origin` remotes are built in temporary directories with the
//! real `git`, as the Python fixtures do. A test that needs a hung or failing git writes a
//! `/bin/sh` script (no execute bit) that intercepts one verb and delegates every other
//! command to `/usr/bin/git`, and routes the module's subprocesses through it via the
//! crate's `test-hooks` program override. The hook values also shrink the worktree-add and
//! per-attempt remote limits to 300 ms so the hung-git cases stay fast.
//!
//! Tests of one binary run concurrently, and the program override is process-global, so
//! every test holds [`GIT_TEST_LOCK`] for its whole body.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use charliebot_tasks::git::{
  BaseBranchResolutionError, BaseResolution, git_create_worktree, git_current_branch,
  git_remote_default_branch_and_tip, git_verify_commit_landed, git_worktree_dir_name, git_worktree_remove,
};

static GIT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use charliebot_core::test_support::LogCapture;

/// Emits one event at this call site; the capture test below reaches it from two threads.
fn emit_probe() {
  tracing::warn!("capture probe");
}

/// A capture keeps an event whose call site another thread reached first. The uncaptured thread registers the call site
/// while the capture is installed, and the capturing thread emits it afterwards.
#[test]
fn a_capture_keeps_an_event_whose_call_site_another_thread_reached_first() {
  let logs = LogCapture::default();
  let (installed_tx, installed_rx) = std::sync::mpsc::channel();
  let (reached_tx, reached_rx) = std::sync::mpsc::channel();
  let capturing = {
    let logs = logs.clone();
    std::thread::spawn(move || {
      let _guard = logs.install();
      installed_tx.send(()).expect("the main thread waits for the install");
      reached_rx.recv().expect("the main thread sends the reach signal");
      emit_probe();
    })
  };
  installed_rx.recv().expect("the capturing thread installs");
  std::thread::spawn(emit_probe).join().expect("the uncaptured thread emits");
  reached_tx.send(()).expect("the capturing thread waits for the reach signal");
  capturing.join().expect("the capturing thread finishes");
  assert!(logs.text().contains("capture probe"), "the capture lost the event: {}", logs.text());
}

/// The per-repository and per-test git fixture: a bare `origin`, a `seed` clone that authors
/// commits, and the `main_checkout` working clone. Both clones start on branch `feature` at
/// one shared commit.
struct RepoFixture {
  _dir: tempfile::TempDir,
  seed: PathBuf,
  main_checkout: PathBuf,
}

fn run_git(dir: &Path, args: &[&str]) -> std::process::Output {
  Command::new("git").arg("-C").arg(dir).args(args).output().expect("git is installed")
}

fn git(dir: &Path, args: &[&str]) {
  let output = run_git(dir, args);
  assert!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    dir.display(),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
  String::from_utf8(run_git(dir, args).stdout).expect("git stdout is UTF-8").trim().to_string()
}

/// Write a file, commit it, and return the new commit's SHA (Python `_commit`).
fn commit(cwd: &Path, filename: &str, content: &str, message: &str) -> String {
  fs::write(cwd.join(filename), content).expect("synthetic file is written");
  git(cwd, &["add", filename]);
  git(cwd, &["commit", "-m", message]);
  git_out(cwd, &["rev-parse", "HEAD"])
}

impl RepoFixture {
  fn create() -> Self {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let origin = dir.path().join("origin.git");
    let seed = dir.path().join("seed");
    let main_checkout = dir.path().join("main_checkout");
    git(dir.path(), &["init", "--bare", &origin.display().to_string()]);
    git(dir.path(), &["clone", &origin.display().to_string(), &seed.display().to_string()]);
    git(&seed, &["config", "user.email", "test@example.com"]);
    git(&seed, &["config", "user.name", "Test"]);
    git(&seed, &["checkout", "-b", "feature"]);
    commit(&seed, "README.md", "seed\n", "seed");
    git(&seed, &["push", "-u", "origin", "feature"]);
    git(
      dir.path(),
      &["clone", "--branch", "feature", &origin.display().to_string(), &main_checkout.display().to_string()],
    );
    git(&main_checkout, &["config", "user.email", "test@example.com"]);
    git(&main_checkout, &["config", "user.name", "Test"]);
    Self { _dir: dir, seed, main_checkout }
  }

  fn tmp(&self) -> &Path {
    self._dir.path()
  }
}

/// The origin fixture for the base-less launch: bare origin whose HEAD points at `main`,
/// plus a clone of it (`remote_default_repo`).
struct RemoteDefaultRepo {
  _dir: tempfile::TempDir,
  seed: PathBuf,
  clone: PathBuf,
}

impl RemoteDefaultRepo {
  fn create() -> Self {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let origin = dir.path().join("origin.git");
    let seed = dir.path().join("seed");
    let clone = dir.path().join("clone");
    git(dir.path(), &["init", "--bare", &origin.display().to_string()]);
    git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(dir.path(), &["clone", &origin.display().to_string(), &seed.display().to_string()]);
    git(&seed, &["config", "user.email", "test@example.com"]);
    git(&seed, &["config", "user.name", "Test"]);
    git(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    commit(&seed, "README.md", "seed\n", "seed main");
    git(&seed, &["push", "-u", "origin", "main"]);
    git(dir.path(), &["clone", &origin.display().to_string(), &clone.display().to_string()]);
    git(&clone, &["config", "user.email", "test@example.com"]);
    git(&clone, &["config", "user.name", "Test"]);
    Self { _dir: dir, seed, clone }
  }

  fn tmp(&self) -> &Path {
    self._dir.path()
  }
}

/// The interception options a fake git script bakes in.
#[derive(Default)]
struct FakeGitOptions {
  verb: &'static str,
  hangs: usize,
  fails: usize,
  code: i32,
  lock_err: &'static str,
  lock_times: usize,
  merge_base_code: Option<i32>,
  overlap_dir: Option<PathBuf>,
  delegate: bool,
  stdout: String,
}

/// The fake git: a `/bin/sh` script written without an execute bit, wired in through the
/// `test-hooks` program override. Counts its intercepted invocations in `calls`.
struct FakeGit {
  _dir: tempfile::TempDir,
  calls: PathBuf,
}

impl FakeGit {
  fn new(options: &FakeGitOptions) -> Self {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let calls = dir.path().join("fake_git_calls");
    let code = if options.code == 0 { 128 } else { options.code };
    let merge_base_code = options.merge_base_code.map(|code| code.to_string()).unwrap_or_default();
    let overlap =
      options.overlap_dir.as_ref().map(|path| format!("'{}'", path.display())).unwrap_or_else(|| String::from("''"));
    let script = format!(
      r#"#!/bin/sh
# charliebot-tasks test double for git: intercepts one verb, delegates the rest.
real=/usr/bin/git
verb='{verb}'
calls='{calls}'
hangs='{hangs}'
fails='{fails}'
code='{code}'
lock_err='{lock_err}'
lock_times='{lock_times}'
merge_base_code='{merge_base_code}'
overlap_dir={overlap}
delegate='{delegate}'
stdout_line='{stdout}'
if [ -n "$verb" ]; then
  case "$*" in
    (*"$verb"*)
      n=$(( $(cat "$calls" 2>/dev/null || echo 0) + 1 ))
      echo "$n" > "$calls"
      if [ "$hangs" -ge "$n" ]; then exec sleep 30; fi
      if [ "$fails" -ge "$n" ]; then
        echo "fatal: unable to access remote (attempt $n)" >&2
        exit "$code"
      fi
      if [ -n "$lock_err" ] && [ "$n" -le "$lock_times" ]; then
        echo "$lock_err" >&2
        exit "$code"
      fi
      if [ -n "$merge_base_code" ]; then
        echo "fatal: bad object" >&2
        exit "$merge_base_code"
      fi
      if [ -n "$overlap_dir" ]; then
        (
          flock 9
          now=$(cat "$overlap_dir/now" 2>/dev/null || echo 0)
          now=$((now + 1))
          echo "$now" > "$overlap_dir/now"
          most=$(cat "$overlap_dir/most" 2>/dev/null || echo 0)
          if [ "$now" -gt "$most" ]; then echo "$now" > "$overlap_dir/most"; fi
          sleep 0.1
          echo $((now - 1)) > "$overlap_dir/now"
        ) 9>"$overlap_dir/lock"
        exec "$real" "$@"
      fi
      if [ "$delegate" = "true" ]; then exec "$real" "$@"; fi
      printf '%s' "$stdout_line"
      exit 0
      ;;
  esac
fi
exec "$real" "$@"
"#,
      verb = options.verb,
      calls = calls.display(),
      hangs = options.hangs,
      fails = options.fails,
      code = code,
      lock_err = options.lock_err,
      lock_times = options.lock_times,
      merge_base_code = merge_base_code,
      overlap = overlap,
      delegate = options.delegate,
      stdout = options.stdout,
    );
    let script_path = dir.path().join("fake_git.sh");
    let mut file = fs::File::create(&script_path).expect("fake git script is written");
    file.write_all(script.as_bytes()).expect("fake git script is written");
    drop(file);
    // The script carries no execute bit; the override starts it through /bin/sh.
    charliebot_tasks::git::set_git_program_override("/bin/sh", &[&script_path.display().to_string()]);
    Self { _dir: dir, calls }
  }

  fn call_count(&self) -> usize {
    fs::read_to_string(&self.calls).expect("call count file is written").trim().parse().expect("call count is a number")
  }
}

impl Drop for FakeGit {
  fn drop(&mut self) {
    charliebot_tasks::git::clear_git_program_override();
  }
}

fn worktree_head(wt_path: &Path) -> String {
  git_out(wt_path, &["rev-parse", "HEAD"])
}

// --- base-branch resolution matrix -----------------------------------------------------------

#[tokio::test]
async fn local_equals_origin_uses_origin_tip() {
  // Python: tests/test_git_worktree_base.py::test_local_equals_origin_uses_origin_tip.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let expected = git_out(&repo.main_checkout, &["rev-parse", "feature"]);

  let wt_path = repo.tmp().join("wt-equal");
  let resolution = git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-equal", &wt_path, None)
    .await
    .expect("worktree is created");

  assert_eq!(worktree_head(&wt_path), expected);
  assert_eq!(
    resolution,
    BaseResolution {
      canonical: "feature".to_string(),
      start_point: "origin/feature".to_string(),
      detail: format!("branch feature at origin tip {}", &expected[..12]),
    }
  );
}

#[tokio::test]
async fn local_behind_origin_starts_from_origin_tip() {
  // Python: tests/test_git_worktree_base.py::test_local_behind_origin_starts_from_origin_tip.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let origin_tip = commit(&repo.seed, "advance.txt", "advance\n", "advance origin");
  git(&repo.seed, &["push", "origin", "feature"]);
  let local_tip = git_out(&repo.main_checkout, &["rev-parse", "feature"]);
  assert_ne!(local_tip, origin_tip, "the local branch is behind");

  let wt_path = repo.tmp().join("wt-behind");
  let resolution = git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-behind", &wt_path, None)
    .await
    .expect("worktree is created");

  assert_eq!(worktree_head(&wt_path), origin_tip);
  assert_eq!(git_out(&repo.main_checkout, &["rev-parse", &resolution.start_point]), origin_tip);
  assert_eq!(git_out(&repo.main_checkout, &["rev-parse", "feature"]), local_tip, "the local ref never moved");
  assert_eq!(resolution.canonical, "feature");
  assert_eq!(resolution.start_point, "origin/feature");
  assert_eq!(
    resolution.detail,
    format!(
      "branch feature at origin tip {} (local feature at {} is behind, unused)",
      &origin_tip[..12],
      &local_tip[..12]
    )
  );
}

#[tokio::test]
async fn merge_base_unexpected_exit_code_raises() {
  // Python: tests/test_git_worktree_base.py::test_merge_base_unexpected_exit_code_raises.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  commit(&repo.seed, "advance.txt", "advance\n", "advance origin");
  git(&repo.seed, &["push", "origin", "feature"]); // local feature is now behind: the probe runs
  let _fake =
    FakeGit::new(&FakeGitOptions { verb: "merge-base --is-ancestor", merge_base_code: Some(2), ..Default::default() });

  let error = git_create_worktree(
    &repo.main_checkout,
    "feature",
    "charliebot/task-mb-fail",
    &repo.tmp().join("wt-mb-fail"),
    None,
  )
  .await
  .expect_err("a broken merge-base probe is a hard error");
  let message = error.to_string();
  assert!(error.downcast_ref::<BaseBranchResolutionError>().is_some(), "resolution error type, got: {message}");
  assert!(message.contains("merge-base --is-ancestor"), "{message}");
  assert!(message.contains("exit code 2"), "{message}");
  assert!(message.contains("fatal: bad object"), "{message}");
}

#[tokio::test]
async fn worktree_add_retries_a_transient_config_lock() {
  // Python: tests/test_git_worktree_base.py::test_worktree_add_retries_a_transient_config_lock.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let expected = git_out(&repo.main_checkout, &["rev-parse", "feature"]);
  let fake = FakeGit::new(&FakeGitOptions {
    verb: "worktree add",
    lock_err: "fatal: could not lock config file .git/config: File exists",
    lock_times: 1,
    delegate: true,
    ..Default::default()
  });

  let wt_path = repo.tmp().join("wt-retry");
  let resolution = git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-retry", &wt_path, None)
    .await
    .expect("the retried add succeeds");

  assert_eq!(fake.call_count(), 2);
  assert_eq!(worktree_head(&wt_path), expected);
  assert_eq!(resolution.canonical, "feature");
}

#[tokio::test]
async fn concurrent_worktree_adds_in_one_repo_never_overlap() {
  // Python: tests/test_git_worktree_base.py::test_concurrent_worktree_adds_in_one_repo_never_overlap.
  let _guard = GIT_TEST_LOCK.lock().await;
  // Each timed add runs the probe's 0.1 s sleep before a real git call, which leaves the 300 ms hook value about
  // 200 ms for git. This test alone raises the write limit so that a loaded host does not time out an add.
  let _write_timeout = charliebot_tasks::git::override_git_write_timeout(Duration::from_secs(10));
  let repo = RepoFixture::create();
  let overlap_dir = repo.tmp().join("overlap");
  fs::create_dir(&overlap_dir).expect("overlap directory is created");
  let _fake = FakeGit::new(&FakeGitOptions {
    verb: "worktree add",
    overlap_dir: Some(overlap_dir.clone()),
    delegate: true,
    ..Default::default()
  });

  let paths: Vec<PathBuf> = (0..3).map(|i| repo.tmp().join(format!("wt-parallel-{i}"))).collect();
  let (a, b, c) = tokio::join!(
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-parallel-0", &paths[0], None),
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-parallel-1", &paths[1], None),
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-parallel-2", &paths[2], None),
  );
  a.expect("first add succeeds");
  b.expect("second add succeeds");
  c.expect("third add succeeds");

  let most: usize =
    fs::read_to_string(overlap_dir.join("most")).expect("overlap file is written").trim().parse().expect("a number");
  assert_eq!(most, 1, "the adds of one repository run one at a time");
  for path in &paths {
    assert!(path.is_dir());
  }
}

#[tokio::test]
async fn worktree_add_persistent_lock_error_raises_after_three_retries() {
  // Python: tests/test_git_worktree_base.py::test_worktree_add_persistent_lock_error_raises_after_three_retries.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let fake = FakeGit::new(&FakeGitOptions {
    verb: "worktree add",
    lock_err: "fatal: could not lock config file .git/config: File exists",
    lock_times: 99,
    ..Default::default()
  });

  let error =
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-locked", &repo.tmp().join("wt-locked"), None)
      .await
      .expect_err("a lock that never clears fails loudly");

  assert_eq!(fake.call_count(), 4, "the first invocation plus 3 retries");
  assert_eq!(charliebot_tasks::git::test_worktree_retry_backoff(1), Duration::from_millis(100));
  assert_eq!(charliebot_tasks::git::test_worktree_retry_backoff(2), Duration::from_millis(200));
  assert_eq!(charliebot_tasks::git::test_worktree_retry_backoff(3), Duration::from_millis(400));
  assert!(error.to_string().contains("could not lock config file .git/config: File exists"), "{error}");
}

#[tokio::test]
async fn worktree_add_non_lock_failure_raises_immediately() {
  // Python: tests/test_git_worktree_base.py::test_worktree_add_non_lock_failure_raises_immediately.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let fake = FakeGit::new(&FakeGitOptions {
    verb: "worktree add",
    lock_err: "fatal: invalid reference: nope",
    lock_times: 1,
    ..Default::default()
  });
  let started = Instant::now();

  let error = git_create_worktree(
    &repo.main_checkout,
    "feature",
    "charliebot/task-bad-object",
    &repo.tmp().join("wt-bad-object"),
    None,
  )
  .await
  .expect_err("a non-lock failure raises on the first attempt");

  assert_eq!(fake.call_count(), 1);
  assert!(started.elapsed() < Duration::from_secs(2), "a non-lock failure must not back off");
  assert!(error.to_string().contains("fatal: invalid reference: nope"), "{error}");
}

#[tokio::test]
async fn baseless_launch_starts_at_remote_default_tip() {
  // Python: tests/test_git_worktree_base.py::test_baseless_launch_starts_at_remote_default_tip.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RemoteDefaultRepo::create();
  let origin_tip = commit(&repo.seed, "advance.txt", "advance\n", "advance origin main");
  git(&repo.seed, &["push", "origin", "main"]);
  assert_ne!(git_out(&repo.clone, &["rev-parse", "main"]), origin_tip, "local main is behind");

  git(&repo.clone, &["checkout", "-b", "local-only"]);

  let wt_path = repo.tmp().join("wt-baseless");
  let resolution = git_create_worktree(&repo.clone, "origin/main", "charliebot/task-baseless", &wt_path, None)
    .await
    .expect("worktree is created");

  assert_eq!(git_out(&repo.clone, &["rev-parse", &resolution.start_point]), origin_tip);
  assert!(wt_path.is_dir());
  assert_eq!(worktree_head(&wt_path), origin_tip);

  // The old fallback (the checkout's current branch) must never land on origin's main tip:
  // it either hard-errors or starts from a different commit.
  let old_base = git_current_branch(&repo.clone).await.expect("current branch resolves");
  assert_eq!(old_base, "local-only");
  let old_wt = repo.tmp().join("wt-old-fallback");
  match git_create_worktree(&repo.clone, &old_base, "charliebot/task-old-fallback", &old_wt, None).await {
    Err(error) => assert!(error.downcast_ref::<BaseBranchResolutionError>().is_some(), "{error}"),
    Ok(_) => assert_ne!(worktree_head(&old_wt), origin_tip),
  }
}

// --- remote operations: a per-attempt limit, retried at once on failure ----------------------

#[tokio::test]
async fn remote_op_hung_first_attempt_succeeds_on_retry() {
  // Python: tests/test_git_worktree_base.py::test_remote_op_hung_first_attempt_succeeds_on_retry.
  let _guard = GIT_TEST_LOCK.lock().await;
  let dir = tempfile::tempdir().expect("temporary directory is available");
  let sha = "a".repeat(40);
  let fake = FakeGit::new(&FakeGitOptions {
    verb: "ls-remote",
    hangs: 1,
    stdout: format!("ref: refs/heads/main\tHEAD\n{sha}\tHEAD\n{sha}\trefs/heads/main\n"),
    ..Default::default()
  });
  let logs = LogCapture::default();
  let _subscriber = logs.install();

  let (branch, tip) = git_remote_default_branch_and_tip(dir.path()).await.expect("default branch resolves");

  assert_eq!(branch, "main");
  assert_eq!(tip.as_deref(), Some(sha.as_str()));
  assert_eq!(fake.call_count(), 2, "a hung first attempt is killed at the per-attempt limit");
  let text = logs.text();
  assert!(text.contains("git_remote_attempt_failed"), "{text}");
  assert!(text.contains("attempt=1"), "{text}");
  assert!(text.contains("git ls-remote timed out after 0.3s"), "{text}");
}

#[tokio::test]
async fn remote_op_failing_every_attempt_returns_timeout_error() {
  // Python: tests/test_git_worktree_base.py::test_remote_op_failing_every_attempt_returns_single_attempt_error[hang].
  let _guard = GIT_TEST_LOCK.lock().await;
  let dir = tempfile::tempdir().expect("temporary directory is available");
  let fake = FakeGit::new(&FakeGitOptions { verb: "fetch", hangs: 99, fails: 99, ..Default::default() });
  let logs = LogCapture::default();
  let _subscriber = logs.install();

  let (verified, reason) = git_verify_commit_landed(dir.path(), "origin/main", &"a".repeat(40)).await;

  assert!(!verified);
  assert_eq!(
    reason,
    format!("could not fetch origin/main in {}: git fetch timed out after 0.3s", dir.path().display())
  );
  assert_eq!(fake.call_count(), 3, "every attempt runs under the per-attempt limit");
  let text = logs.text();
  assert_eq!(text.matches("git_remote_attempt_failed").count(), 3, "{text}");
  for attempt in 1..=3 {
    assert!(text.contains(&format!("attempt={attempt}")), "{text}");
  }
  assert!(text.contains("git fetch timed out after 0.3s"), "{text}");
}

#[tokio::test]
async fn remote_op_failing_every_attempt_returns_last_stderr() {
  // Python: tests/test_git_worktree_base.py::test_remote_op_failing_every_attempt_returns_single_attempt_error[exit].
  let _guard = GIT_TEST_LOCK.lock().await;
  let dir = tempfile::tempdir().expect("temporary directory is available");
  let fake = FakeGit::new(&FakeGitOptions { verb: "fetch", fails: 99, code: 128, ..Default::default() });
  let logs = LogCapture::default();
  let _subscriber = logs.install();

  let (verified, reason) = git_verify_commit_landed(dir.path(), "origin/main", &"a".repeat(40)).await;

  assert!(!verified);
  assert_eq!(
    reason,
    format!("could not fetch origin/main in {}: fatal: unable to access remote (attempt 3)", dir.path().display())
  );
  assert_eq!(fake.call_count(), 3);
  let text = logs.text();
  assert_eq!(text.matches("git_remote_attempt_failed").count(), 3, "{text}");
  for attempt in 1..=3 {
    assert!(text.contains(&format!("attempt={attempt}")), "{text}");
  }
}

#[tokio::test]
async fn worktree_add_keeps_the_write_limit() {
  // Python: tests/test_git_worktree_base.py::test_local_git_ops_keep_write_limit_while_remote_ops_take_attempt_limit.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let _fake = FakeGit::new(&FakeGitOptions { verb: "worktree add", hangs: 99, ..Default::default() });

  let error =
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-limits", &repo.tmp().join("wt-limits"), None)
      .await
      .expect_err("a hung worktree add is killed at its own limit");

  assert!(error.to_string().contains("git worktree add timed out after 0.3s for charliebot/task-limits"), "{error}");
}

#[tokio::test]
async fn ls_remote_takes_the_attempt_limit() {
  // Python: tests/test_git_worktree_base.py::test_local_git_ops_keep_write_limit_while_remote_ops_take_attempt_limit.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  let _fake = FakeGit::new(&FakeGitOptions { verb: "ls-remote", hangs: 99, ..Default::default() });

  let error =
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-limits", &repo.tmp().join("wt-limits"), None)
      .await
      .expect_err("a hung ls-remote is killed at the per-attempt limit");

  let message = error.to_string();
  assert!(error.downcast_ref::<BaseBranchResolutionError>().is_some(), "{message}");
  assert!(
    message.contains("cannot reach origin to resolve base branch 'feature': git ls-remote timed out after 0.3s"),
    "{message}"
  );
}

#[tokio::test]
async fn fetch_takes_the_attempt_limit() {
  // Python: tests/test_git_worktree_base.py::test_local_git_ops_keep_write_limit_while_remote_ops_take_attempt_limit.
  let _guard = GIT_TEST_LOCK.lock().await;
  let repo = RepoFixture::create();
  commit(&repo.seed, "advance.txt", "advance\n", "advance origin");
  git(&repo.seed, &["push", "origin", "feature"]); // local feature is now behind: the resolution fetches
  let _fake = FakeGit::new(&FakeGitOptions { verb: "fetch", hangs: 99, ..Default::default() });

  let error =
    git_create_worktree(&repo.main_checkout, "feature", "charliebot/task-limits", &repo.tmp().join("wt-limits"), None)
      .await
      .expect_err("a hung fetch is killed at the per-attempt limit");

  let message = error.to_string();
  assert!(error.downcast_ref::<BaseBranchResolutionError>().is_some(), "{message}");
  assert!(message.contains("git fetch origin feature failed: git fetch timed out after 0.3s"), "{message}");
}

// --- worktree removal ------------------------------------------------------------------------

/// The suite's standard remove call: the fake repo path, the shared thread id, and the worktrees parent.
async fn remove_worktree(tmp: &Path, wt_path: &Path, expected_residue_name: &str) -> anyhow::Result<bool> {
  git_worktree_remove(
    &tmp.join("repo").display().to_string(),
    wt_path,
    "thread-id",
    &tmp.join("worktrees"),
    expected_residue_name,
  )
  .await
}

#[tokio::test]
async fn git_worktree_remove_does_not_delete_residue_after_git_failure() {
  // Python: tests/test_git_worktree_remove.py::test_git_worktree_remove_does_not_delete_residue_after_git_failure.
  let _guard = GIT_TEST_LOCK.lock().await;
  let tmp = tempfile::tempdir().expect("temporary directory is available");
  // The fake git runs with the repo path as its working directory, so the directory exists.
  fs::create_dir(tmp.path().join("repo")).expect("repository directory is created");
  let wt_path = tmp.path().join("worktrees").join("charliebot-task-leftover");
  fs::create_dir_all(&wt_path).expect("worktree directory is created");
  fs::write(wt_path.join("scratch.log"), "leftover\n").expect("scratch file is written");
  let _fake = FakeGit::new(&FakeGitOptions {
    verb: "worktree remove",
    lock_err: "not a worktree",
    lock_times: 1,
    code: 1,
    ..Default::default()
  });

  let removed = remove_worktree(tmp.path(), &wt_path, "charliebot-task-leftover").await.expect("no raise");

  assert!(!removed);
  assert!(wt_path.exists());
  assert!(wt_path.join("scratch.log").exists());
}

#[tokio::test]
async fn git_worktree_remove_refuses_path_outside_allowed_parent() {
  // Python: tests/test_git_worktree_remove.py::test_git_worktree_remove_refuses_path_outside_allowed_parent.
  let _guard = GIT_TEST_LOCK.lock().await;
  let tmp = tempfile::tempdir().expect("temporary directory is available");
  let wt_path = tmp.path().join("other").join("charliebot-task-elsewhere");
  fs::create_dir_all(&wt_path).expect("worktree directory is created");

  let error = remove_worktree(tmp.path(), &wt_path, "charliebot-task-elsewhere").await.expect_err("the remove refuses");

  assert!(error.to_string().contains("outside allowed parent"), "{error}");
  assert!(wt_path.exists());
}

#[tokio::test]
async fn git_worktree_remove_refuses_repo_root() {
  // Python: tests/test_git_worktree_remove.py::test_git_worktree_remove_refuses_repo_root.
  let _guard = GIT_TEST_LOCK.lock().await;
  let tmp = tempfile::tempdir().expect("temporary directory is available");
  let worktree_parent = tmp.path().join("worktrees");
  let repo_path = worktree_parent.join("charliebot-task-repo-root");
  fs::create_dir_all(&repo_path).expect("repo directory is created");

  let error = git_worktree_remove(
    &repo_path.display().to_string(),
    &repo_path,
    "thread-id",
    &worktree_parent,
    "charliebot-task-repo-root",
  )
  .await
  .expect_err("the remove refuses");

  assert!(error.to_string().contains("repo root"), "{error}");
  assert!(repo_path.exists());
}

#[tokio::test]
async fn git_worktree_remove_refuses_symlink_target() {
  // Python: tests/test_git_worktree_remove.py::test_git_worktree_remove_refuses_symlink_target.
  let _guard = GIT_TEST_LOCK.lock().await;
  let tmp = tempfile::tempdir().expect("temporary directory is available");
  let worktree_parent = tmp.path().join("worktrees");
  let real_target = tmp.path().join("real-target");
  fs::create_dir_all(&real_target).expect("real target directory is created");
  fs::create_dir_all(&worktree_parent).expect("worktree parent is created");
  let wt_path = worktree_parent.join("charliebot-task-symlink");
  std::os::unix::fs::symlink(&real_target, &wt_path).expect("symlink is created");

  let error = remove_worktree(tmp.path(), &wt_path, "charliebot-task-symlink").await.expect_err("the remove refuses");

  assert!(error.to_string().contains("symlink"), "{error}");
  assert!(wt_path.is_symlink());
  assert!(real_target.exists());
}

#[test]
fn worktree_dir_name_replaces_slashes() {
  // Python: tests/test_git_worktree_base.py resolves worktree names through git_worktree_dir_name.
  assert_eq!(git_worktree_dir_name("charliebot/task-equal"), "charliebot-task-equal");
}
