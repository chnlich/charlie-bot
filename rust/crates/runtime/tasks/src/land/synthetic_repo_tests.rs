use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;

use super::*;

/// One synthetic repository with a bare `origin.git` beside it, on branch *branch*.
fn synthetic_repo(root: &Path, branch: &str) -> std::path::PathBuf {
  synthetic_repo_inner(root, branch, false)
}

fn synthetic_queue_repo(root: &Path, branch: &str) -> std::path::PathBuf {
  synthetic_repo_inner(root, branch, true)
}

fn synthetic_repo_inner(root: &Path, branch: &str, scan_pushes: bool) -> std::path::PathBuf {
  let repo = root.join("repo");
  fs::create_dir(&repo).expect("repository directory is created");
  assert!(
    Command::new("git").args(["init", "-q", "-b", "main"]).arg(&repo).status().expect("git is installed").success()
  );
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(&repo)
      .args(["config", "user.email", "synthetic@example.test"])
      .status()
      .unwrap()
      .success()
  );
  assert!(
    Command::new("git").arg("-C").arg(&repo).args(["config", "user.name", "Synthetic"]).status().unwrap().success()
  );
  let origin = root.join("origin.git");
  assert!(
    Command::new("git")
      .args(["init", "-q", "--bare", "--initial-branch=main"])
      .arg(&origin)
      .status()
      .unwrap()
      .success()
  );
  if scan_pushes {
    charliebot_shell_harness::install_pre_receive_leak_scan(&origin, root)
      .expect("the origin leak scanner is installed");
  }
  fs::write(repo.join("tracked.txt"), "base\n").expect("the base file is written");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "base"]).status().unwrap().success());
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(&repo)
      .args(["remote", "add", "origin", origin.to_str().expect("the path is UTF-8")])
      .status()
      .unwrap()
      .success()
  );
  assert!(
    Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "-u", "origin", "main"]).status().unwrap().success()
  );
  if branch != "main" {
    assert!(Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", branch]).status().unwrap().success());
  }
  repo
}

/// Commits with the identity passed on the command line: a clone has no `user.*` config of its own,
/// and a CI runner has no identity git can auto-detect.
fn commit(repo: &Path, file: &str, content: &str) {
  fs::write(repo.join(file), content).expect("the file is written");
  assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(repo)
      .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
      .args(["commit", "-q", "-m", file])
      .status()
      .unwrap()
      .success()
  );
}

fn head(repo: &Path) -> String {
  String::from_utf8(Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
    .expect("the sha is UTF-8")
    .trim()
    .to_string()
}

/// A profile home whose credentials file holds `secret` as the `planted.api_key` leaf.
fn planted_home(root: &Path, secret: &str) -> PathBuf {
  let home = root.join("home");
  fs::create_dir(&home).expect("the profile home is created");
  fs::write(home.join("credentials.yaml"), format!("planted:\n  api_key: {secret}\n"))
    .expect("the credentials file is written");
  home
}

/// One commit on the checked-out branch whose message is *message*.
fn commit_with_message(repo: &Path, file: &str, content: &str, message: &str) {
  fs::write(repo.join(file), content).expect("the file is written");
  assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(repo)
      .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
      .args(["commit", "-q", "-m", message])
      .status()
      .unwrap()
      .success()
  );
}

async fn fetch_main(repo: &Path) {
  assert!(git::git_stdout(repo, &["fetch", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
}

/// A landing venue over *worktree* on *branch*: the push reads only the worktree.
fn landing_ctx(worktree: &Path, branch: &str) -> TaskLanding {
  TaskLanding {
    session_id: "synthetic-task".to_string(),
    repo: "synthetic/synthetic".to_string(),
    worktree: worktree.to_path_buf(),
    repo_path: worktree.to_path_buf(),
    base_branch: "main".to_string(),
    branch_name: branch.to_string(),
    session_dir: worktree.join("session"),
    queue_program: PathBuf::from("charliebot"),
  }
}

fn git_ok(repo: &Path, args: &[&str]) {
  let output = Command::new("git").arg("-C").arg(repo).args(args).output().expect("git runs");
  assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
}

/// GitHub deletes a merged task branch on origin, but the landing's clone keeps
/// `refs/remotes/origin/<branch>` at the merged tip. The push leases against an explicitly absent ref,
/// so the rewritten branch publishes as a new branch; the implicit lease of the tracking ref would refuse
/// it as `stale info`.
#[tokio::test]
#[ignore = "integration"]
async fn a_merged_branch_absent_from_origin_publishes_its_rewrite_as_a_new_branch() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  synthetic_repo(root.path(), "main");
  let origin = root.path().join("origin.git");
  let work = root.path().join("work");
  git_ok(root.path(), &["clone", "-q", origin.to_str().unwrap(), work.to_str().unwrap()]);
  git_ok(&work, &["switch", "-q", "-c", "task1"]);
  commit(&work, "one.txt", "one\n");
  git_ok(&work, &["push", "-q", "-u", "origin", "task1"]);
  let merged_tip = head(&work);

  // GitHub deletes the branch on origin; the clone's tracking ref stays at the merged tip.
  git_ok(&origin, &["update-ref", "-d", "refs/heads/task1"]);
  let tracking = git::git_rev_parse(&work, "refs/remotes/origin/task1").await.expect("the tracking ref survives");
  assert_eq!(tracking, merged_tip);

  // A rebase rewrote the branch, as the landing's round before the push does.
  git_ok(
    &work,
    &[
      "-c",
      "user.name=Synthetic",
      "-c",
      "user.email=synthetic@example.test",
      "commit",
      "-q",
      "--amend",
      "-m",
      "rewritten",
    ],
  );
  let rewritten = head(&work);
  assert_ne!(rewritten, merged_tip);

  push_task_branch(&landing_ctx(&work, "task1"), "task1").await.expect("the absent branch publishes as a new one");
  let published = git::git_rev_parse(&origin, "refs/heads/task1").await.expect("origin holds the new branch");
  assert_eq!(published, rewritten);
}

/// With the branch still on origin, the push leases against the tip it fetched. A second landing that
/// pushes between that fetch and this push's update transaction therefore keeps its tip, and this push
/// refuses instead of overwriting it.
#[tokio::test]
#[ignore = "integration"]
async fn a_branch_moved_on_origin_after_the_fetch_still_refuses_the_push() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_repo(root.path(), "task1");
  let origin = root.path().join("origin.git");
  commit(&repo, "one.txt", "one\n");
  git_ok(&repo, &["push", "-q", "-u", "origin", "task1"]);

  // A second landing clone holds its own commit for the same branch.
  let other = root.path().join("other");
  git_ok(root.path(), &["clone", "-q", origin.to_str().unwrap(), other.to_str().unwrap()]);
  git_ok(&other, &["switch", "-q", "-c", "task1", "origin/task1"]);
  commit(&other, "two.txt", "two\n");
  let other_tip = head(&other);

  // The race: the other push lands after this push advertised the remote refs and before its update
  // transaction, so the fetched tip the lease names is no longer the remote tip.
  let hook = repo.join(".git/hooks/pre-push");
  fs::write(
    &hook,
    format!("#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE\ngit -C '{}' push -q origin task1\n", other.display()),
  )
  .expect("the pre-push hook is written");
  fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("the hook is executable");

  git_ok(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
  let error = push_task_branch(&landing_ctx(&repo, "task1"), "task1").await.expect_err("the other push stands");
  let text = format!("{error:#}");
  assert!(text.contains("cannot lock ref") && text.contains("but expected"), "{text}");
  assert_eq!(git::git_rev_parse(&origin, "refs/heads/task1").await.expect("origin holds the branch"), other_tip);
}

#[tokio::test]
#[ignore = "integration"]
async fn a_credential_value_in_the_diff_refuses_the_landing() {
  const SECRET: &str = "fake-landing-diff-value-0001";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = planted_home(root.path(), SECRET);
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", &format!("the key is {SECRET}\n"));
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the diff holds the value");
  let text = format!("{error:#}");
  assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
  assert!(text.contains("commit ") && text.contains(" diff"), "the field is named: {text}");
  assert!(!text.contains(SECRET), "the value never appears: {text}");
}

/// The literal check reads the files the branch changes. A literal that main already holds is not a site of this
/// landing; one that the branch adds is, and the refusal names its file and line.
#[tokio::test]
#[ignore = "integration"]
async fn a_backend_literal_the_branch_adds_refuses_the_landing_and_one_main_holds_does_not() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = root.path().join("home");
  let repo = synthetic_repo(root.path(), "main");
  fs::create_dir(repo.join("scripts")).expect("the scripts directory is created");
  commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\n");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", "feature"]).status().unwrap().success());
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  backend_literal_check(&Registry::new(), &home, &repo, "origin/main").await.expect("the branch adds no literal");

  commit(&repo, "scripts/new.sh", "model=claude-opus-5-5\n");
  let error =
    backend_literal_check(&Registry::new(), &home, &repo, "origin/main").await.expect_err("the branch adds a literal");
  let text = format!("{error:#}");
  assert!(text.contains("scripts/new.sh:1") && text.contains("claude-opus-5-5"), "{text}");
  assert!(!text.contains("old.sh"), "{text}");
}

/// A task branch on an older feature base, `origin/older`, that main has since passed. Main holds a model id in
/// `scripts/old.sh`; the task branch merged main into its own work, so that file reaches the branch with main's line.
fn merged_older_feature(root: &Path) -> std::path::PathBuf {
  let repo = synthetic_repo(root, "main");
  git_ok(&repo, &["switch", "-q", "-c", "older"]);
  commit(&repo, "older.txt", "older\n");
  git_ok(&repo, &["push", "-q", "-u", "origin", "older"]);
  git_ok(&repo, &["switch", "-q", "main"]);
  fs::create_dir(repo.join("scripts")).expect("the scripts directory is created");
  commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\n");
  git_ok(&repo, &["push", "-q", "origin", "main"]);
  git_ok(&repo, &["switch", "-q", "-c", "feature", "origin/older"]);
  git_ok(&repo, &["merge", "-q", "--no-edit", "origin/main"]);
  commit(&repo, "notes.txt", "clean\n");
  repo
}

/// The literal main holds reaches the branch only through the merge, and the branch leaves that file as main has it:
/// the literal is no site of this landing, so the clean branch passes.
#[tokio::test]
#[ignore = "integration"]
async fn a_literal_the_default_branch_holds_is_no_site_of_a_merged_feature_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = merged_older_feature(root.path());
  backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/older")
    .await
    .expect("main's literal is not a site of this landing");
}

/// A literal that the branch adds on a new line is refused on that line, and main's literal is not named.
#[tokio::test]
#[ignore = "integration"]
async fn a_literal_the_branch_adds_on_a_merged_feature_landing_is_refused_on_its_line() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = merged_older_feature(root.path());
  commit(&repo, "scripts/new run.sh", "echo ok\nmodel=claude-opus-5-5\n");
  let error = backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/older")
    .await
    .expect_err("the branch adds a literal");
  let text = format!("{error:#}");
  assert!(text.contains("new run.sh:2") && text.contains("claude-opus-5-5"), "{text}");
  assert!(!text.contains("old.sh"), "{text}");
}

/// The branch edits main's file without touching main's literal line, which passes. A second literal in that file is
/// refused on its own line only.
#[tokio::test]
#[ignore = "integration"]
async fn a_second_literal_in_a_file_main_holds_is_refused_on_its_own_line() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = merged_older_feature(root.path());
  let home = root.path().join("home");
  commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\necho done\n");
  backend_literal_check(&Registry::new(), &home, &repo, "origin/older")
    .await
    .expect("main's literal line is not a site of this landing");

  commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\necho done\nmodel=claude-haiku-5-5\n");
  let error = backend_literal_check(&Registry::new(), &home, &repo, "origin/older")
    .await
    .expect_err("the branch adds a second literal");
  let text = format!("{error:#}");
  assert!(text.contains("old.sh:3") && text.contains("claude-haiku-5-5"), "{text}");
  assert!(!text.contains("old.sh:1"), "{text}");
}

/// The comparison base is the remote's default branch, so a remote whose default branch cannot be read fails the
/// check instead of narrowing it to the task base.
#[tokio::test]
#[ignore = "integration"]
async fn a_backend_literal_check_whose_default_branch_cannot_be_read_fails() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_repo(root.path(), "feature");
  let missing = root.path().join("missing.git");
  git_ok(&repo, &["remote", "set-url", "origin", missing.to_str().expect("the path is UTF-8")]);
  let error = backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/main")
    .await
    .expect_err("the default branch cannot be read");
  assert!(format!("{error:#}").contains("default branch"), "{error:#}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_credential_value_in_a_commit_message_refuses_the_landing() {
  const SECRET: &str = "fake-landing-message-value-0002";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = planted_home(root.path(), SECRET);
  let repo = synthetic_repo(root.path(), "feature");
  commit_with_message(&repo, "notes.txt", "clean\n", &format!("notes {SECRET}"));
  fetch_main(&repo).await;
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the message holds the value");
  let text = format!("{error:#}");
  assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
  assert!(text.contains(" message"), "the field is named: {text}");
  assert!(!text.contains(SECRET), "the value never appears: {text}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_credential_value_in_the_pr_body_refuses_before_the_pr_is_created() {
  const SECRET: &str = "fake-landing-body-value-0003";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = planted_home(root.path(), SECRET);
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let text = ResolvedPrText { title: "Clean title".to_string(), body: format!("Evidence with {SECRET}\n") };
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
    .await
    .expect_err("the body holds the value");
  let message = format!("{error:#}");
  assert!(message.contains("PR body"), "the field is named: {message}");
  assert!(!message.contains(SECRET), "the value never appears: {message}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_clean_branch_passes_and_a_value_under_the_minimum_length_passes() {
  // Eleven characters: below the scanner's minimum, so the same value in the diff is not a credential.
  const SHORT: &str = "fake-short1";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = planted_home(root.path(), SHORT);
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", &format!("the short value {SHORT} is not a credential\n"));
  fetch_main(&repo).await;
  let text = ResolvedPrText { title: "Clean title".to_string(), body: "Evidence.\n".to_string() };
  outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text)).await.expect("the check passes");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_pr_body_with_a_session_view_line_passes_the_landing_check() {
  // A fake session id: the landing's check reads only credential values, so the View line of a session page passes.
  const SESSION_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = planted_home(root.path(), "fake-landing-view-value-0006");
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let text = ResolvedPrText {
    title: "Clean title".to_string(),
    body: format!("## Evidence\nbefore and after.\n\nView: /?session={SESSION_ID}\n"),
  };
  outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text)).await.expect("the check passes");
}

/// A profile home whose credentials file holds both a non-secret leaf and a secret leaf, so each side of the skip
/// runs against the same file.
fn home_with_leaves(root: &Path, model: &str, secret: &str) -> PathBuf {
  let home = root.join("home");
  fs::create_dir(&home).expect("the profile home is created");
  fs::write(home.join("credentials.yaml"), format!("gemini:\n  model: {model}\nplanted:\n  api_key: {secret}\n"))
    .expect("the credentials file is written");
  home
}

#[tokio::test]
#[ignore = "integration"]
async fn a_diff_that_quotes_a_non_secret_leaf_passes_the_credential_check() {
  const MODEL: &str = "fake-model-name-0005";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_leaves(root.path(), MODEL, "fake-landing-secret-0005");
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", &format!("the model is {MODEL}\n"));
  fetch_main(&repo).await;
  outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect("the model name is not a credential");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_secret_leaf_beside_a_non_secret_leaf_refuses_the_landing() {
  const SECRET: &str = "fake-landing-secret-0006";
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_leaves(root.path(), "fake-model-name-0006", SECRET);
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", &format!("the key is {SECRET}\n"));
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the secret leaf refuses");
  let text = format!("{error:#}");
  assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
  assert!(!text.contains(SECRET), "the value never appears: {text}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_credentials_file_that_cannot_be_parsed_refuses_the_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = root.path().join("home");
  fs::create_dir(&home).expect("the profile home is created");
  fs::write(home.join("credentials.yaml"), "planted:\n  - listed-value-not-readable\n")
    .expect("the credentials file is written");
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the file does not parse");
  let text = format!("{error:#}");
  assert!(text.contains("could not read the credential values"), "the refusal is named: {text}");
  assert!(!text.contains("listed-value-not-readable"), "the content never appears: {text}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_rebase_conflict_aborts_and_leaves_the_branch_untouched() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_repo(root.path(), "feature");
  commit(&repo, "shared.txt", "feature side\n");
  assert!(
    Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "feature"]).status().unwrap().success()
  );
  // Main moves onto the same file on origin, so the branch's rebase cannot apply.
  let main_work = root.path().join("main-work");
  let clone = Command::new("git")
    .args(["clone", root.path().join("origin.git").to_str().unwrap(), main_work.to_str().unwrap()])
    .output()
    .expect("git clone runs");
  assert!(clone.status.success(), "git clone failed: {}", String::from_utf8_lossy(&clone.stderr));
  commit(&main_work, "shared.txt", "main side\n");
  assert!(
    Command::new("git").arg("-C").arg(&main_work).args(["push", "-q", "origin", "main"]).status().unwrap().success()
  );

  let before = head(&repo);
  assert!(git::git_stdout(&repo, &["fetch", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
  let step = rebase_onto_base(&repo, "origin/main").await.expect("the rebase step resolves");
  assert_eq!(step, RebaseStep::Conflict);
  abort_rebase(&repo).await;
  assert_eq!(head(&repo), before);
}

#[tokio::test]
#[ignore = "integration"]
async fn the_gate_reads_the_default_branch_workflows() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_repo(root.path(), "main");
  // No workflows at all: the no-CI repository case.
  assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::NotApplicable));
  // A workflow without a pull_request trigger stays outside the gate.
  fs::create_dir_all(repo.join(".github/workflows")).expect("the workflows directory is created");
  fs::write(repo.join(".github/workflows/nightly.yml"), "on:\n  schedule:\n    - cron: '0 3 * * *'\njobs: {}\n")
    .expect("the workflow is written");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "nightly"]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success());
  assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::NotApplicable));
  // A pull_request workflow on the default branch applies the gate; a non-default base does not.
  fs::write(
    repo.join(".github/workflows/ci.yml"),
    "on:\n  pull_request:\n  push:\n    branches:\n      - main\njobs: {}\n",
  )
  .expect("the workflow is written");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "ci"]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success());
  assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::Applies));
  assert!(matches!(gate_applicability(&repo, "other").await.unwrap(), GateApplicability::NotApplicable));
}

// ---- the declared-scanner checks: the base ref's own tree decides ----

/// A synthetic repository whose `origin/main` declares `dev/check-skills-host-leak.sh`, checked out on
/// *branch* for the task's commits.
fn declared_repo(root: &Path, branch: &str) -> std::path::PathBuf {
  let repo = synthetic_repo(root, "main");
  fs::create_dir_all(repo.join("dev")).expect("the dev directory is created");
  fs::write(repo.join("dev/check-skills-host-leak.sh"), "# the leak scanner declaration\n")
    .expect("the scanner declaration is written");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "dev"]).status().unwrap().success());
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(&repo)
      .args(["commit", "-q", "-m", "the scanner declaration"])
      .status()
      .unwrap()
      .success()
  );
  assert!(Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success());
  assert!(Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", branch]).status().unwrap().success());
  repo
}

/// A profile home with a credentials leaf and a local leak-patterns file holding `patterns`.
fn home_with_patterns(root: &Path, secret: &str, patterns: &str) -> PathBuf {
  let home = planted_home(root, secret);
  fs::write(home.join("skills_leak_patterns.local.txt"), patterns).expect("the pattern file is written");
  home
}

/// Commits a `*.gz` file whose decompressed content is `content`.
fn commit_gz(repo: &Path, file: &str, content: &[u8]) {
  use std::io::Write as _;
  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
  encoder.write_all(content).expect("the content compresses");
  let bytes = encoder.finish().expect("the gzip stream finishes");
  fs::write(repo.join(file), bytes).expect("the gz file is written");
  assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(repo)
      .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
      .args(["commit", "-q", "-m", file])
      .status()
      .unwrap()
      .success()
  );
}

#[tokio::test]
#[ignore = "integration"]
async fn a_local_pattern_in_the_diff_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0010", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "the notes carry fictionalmarker\n");
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the diff holds the pattern");
  let text = format!("{error:#}");
  assert!(text.contains("Leak hit in commit") && text.contains(" diff"), "the field is named: {text}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_local_pattern_in_a_commit_message_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0011", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit_with_message(&repo, "notes.txt", "clean\n", "notes on fictionalmarker");
  fetch_main(&repo).await;
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the message holds the pattern");
  assert!(format!("{error:#}").contains("message"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_local_pattern_in_a_gz_blob_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0012", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit_gz(&repo, "fixture.gz", b"compressed fictionalmarker content\n");
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the blob holds the pattern");
  assert!(format!("{error:#}").contains("fixture.gz"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_local_pattern_in_the_branch_name_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0013", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature-fictionalmarker");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let error = outbound_checks(&home, &repo, &repo, "feature-fictionalmarker", "origin/main", None)
    .await
    .expect_err("the branch name holds the pattern");
  assert!(format!("{error:#}").contains("branch name"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_local_pattern_in_the_pr_text_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0014", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let text = ResolvedPrText { title: "the fictionalmarker title".to_string(), body: "Evidence.\n".to_string() };
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
    .await
    .expect_err("the PR title holds the pattern");
  assert!(format!("{error:#}").contains("PR title"), "{}", format!("{error:#}"));
  let text = ResolvedPrText { title: "Clean".to_string(), body: "Evidence on fictionalmarker.\n".to_string() };
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
    .await
    .expect_err("the PR body holds the pattern");
  assert!(format!("{error:#}").contains("PR body"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_generic_leak_in_a_touched_doc_refuses_a_declared_landing() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0015", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  fs::create_dir_all(repo.join("skills/demo")).expect("the skill directory is created");
  commit_with_message(&repo, "skills/demo/notes.md", "see /home/someone/notes\n", "the doc");
  fetch_main(&repo).await;
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the touched doc holds a generic leak");
  assert!(format!("{error:#}").contains("doc skills/demo/notes.md"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_declared_landing_refuses_a_missing_or_empty_pattern_file() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  // No pattern file at all: the required local patterns cannot be loaded.
  let home = planted_home(root.path(), "fake-landing-secret-0016");
  let repo = declared_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the missing pattern file refuses");
  assert!(format!("{error:#}").contains("exit 2"), "{}", format!("{error:#}"));

  // A comments-only file holds no pattern: the refusal is the same.
  fs::write(home.join("skills_leak_patterns.local.txt"), "# only comments\n").expect("the pattern file is written");
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the empty pattern file refuses");
  assert!(format!("{error:#}").contains("exit 2"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_declared_landing_refuses_a_stale_hooks_path() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0017", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean\n");
  fetch_main(&repo).await;
  // The checkout pins a hooks directory that is gone: every push would skip the hooks.
  assert!(
    Command::new("git")
      .arg("-C")
      .arg(&repo)
      .args(["config", "core.hooksPath", "/nonexistent/hooks"])
      .status()
      .unwrap()
      .success()
  );
  let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
    .await
    .expect_err("the stale hooks path refuses");
  assert!(format!("{error:#}").contains("core.hooksPath"), "{}", format!("{error:#}"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_clean_declared_landing_passes() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0018", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  commit(&repo, "notes.txt", "clean notes\n");
  fetch_main(&repo).await;
  let text = ResolvedPrText {
    title: "Clean title".to_string(),
    body: "Evidence.\n\nView: /diff?session=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n".to_string(),
  };
  outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text)).await.expect("the declared checks pass");
}

#[tokio::test]
#[ignore = "integration"]
async fn an_undeclared_repo_keeps_only_the_credential_checks() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  // A repository whose base declares no scanner: the local patterns are never required or applied, so the
  // same text that would refuse a declared landing lands here — the private repo's own names stay private.
  let home = planted_home(root.path(), "fake-landing-secret-0019");
  let repo = synthetic_repo(root.path(), "feature");
  commit_with_message(&repo, "notes.txt", "the notes carry fictionalmarker\n", "notes on fictionalmarker");
  fetch_main(&repo).await;
  let text = ResolvedPrText { title: "fictionalmarker title".to_string(), body: "Evidence.\n".to_string() };
  outbound_checks(&home, &repo, &repo, "feature-fictionalmarker", "origin/main", Some(&text))
    .await
    .expect("the undeclared landing skips the pattern checks");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_corrupt_gz_blob_refuses_the_check() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let home = home_with_patterns(root.path(), "fake-landing-secret-0020", "fictionalmarker\n");
  let repo = declared_repo(root.path(), "feature");
  fs::write(repo.join("fixture.gz"), b"not a gzip member").expect("the corrupt gz is written");
  assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
  assert!(
    Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "the fixture"]).status().unwrap().success()
  );
  fetch_main(&repo).await;
  let error =
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the corrupt blob refuses");
  assert!(format!("{error:#}").contains("decompress"), "{}", format!("{error:#}"));
}
// -- the landing queue over a real bare origin --

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use charliebot_store::run_records::RunKind;
use serde_json::json;

use crate::test_fixtures;

static MANUAL_MOVES: AtomicU64 = AtomicU64::new(0);

/// A repository whose base branch declares a pull_request workflow, so the CI gate applies, checked out on
/// *branch*.
fn gated_repo(root: &Path, branch: &str) -> PathBuf {
  let repo = synthetic_queue_repo(root, "main");
  fs::create_dir_all(repo.join(".github/workflows")).expect("the workflows directory is created");
  fs::write(repo.join(".github/workflows/ci.yml"), "on:\n  pull_request:\njobs: {}\n")
    .expect("the workflow is written");
  git_ok(&repo, &["add", "."]);
  git_ok(
    &repo,
    &["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test", "commit", "-q", "-m", "workflow"],
  );
  git_ok(&repo, &["push", "-q", "origin", "main"]);
  if branch != "main" {
    git_ok(&repo, &["switch", "-q", "-c", branch]);
  }
  repo
}

/// One task's clone of the synthetic origin, on its own branch, with one commit pushed.
fn task_clone(root: &Path, name: &str) -> PathBuf {
  let origin = root.join("origin.git");
  let work = root.join(name);
  git_ok(
    root,
    &[
      "clone",
      "-q",
      origin.to_str().expect("the origin path is UTF-8"),
      work.to_str().expect("the work path is UTF-8"),
    ],
  );
  git_ok(&work, &["switch", "-q", "-c", name]);
  // The landing rebases in this clone, and a host without a global git identity has no committer for it.
  git_ok(&work, &["config", "user.name", "Synthetic"]);
  git_ok(&work, &["config", "user.email", "synthetic@example.test"]);
  commit(&work, &format!("{name}.txt"), &format!("{name}\n"));
  git_ok(&work, &["push", "-q", "-u", "origin", name]);
  work
}

/// The landing context of a task clone, with its own session directory.
fn venue(worktree: &Path, branch: &str, session_id: &str) -> TaskLanding {
  let mut ctx = landing_ctx(worktree, branch);
  ctx.session_id = session_id.to_string();
  fs::create_dir_all(&ctx.session_dir).expect("the session directory is created");
  ctx
}

/// The waiting record `land` wrote before it registered the queued command.
fn queue_record(pr: u64, sha: &str) -> LandingRecord {
  LandingRecord {
    state: STATE_WAITING.to_string(),
    repo: "synthetic/synthetic".to_string(),
    base: "main".to_string(),
    sha: sha.to_string(),
    pr_url: Some(format!("https://example.test/owner/repo/pull/{pr}")),
    checks: Vec::new(),
    head_pushed_at: None,
    ci_passed_at: None,
    pushed_at: None,
    rebase_retries: 0,
    queue_ref: None,
  }
}

async fn git_async(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
  let (ok, out, err) = git::git_stdout(repo, args, GIT_READ_TIMEOUT, "git").await;
  if !ok {
    anyhow::bail!("git {args:?} failed: {err}");
  }
  Ok(out.trim().to_string())
}

/// Pushes one empty commit onto origin's main from *work* and returns its sha: the outside write that a landing's
/// leased main push must notice.
async fn push_main_commit(work: &Path) -> anyhow::Result<String> {
  git_async(work, &["fetch", "-q", "origin", "main"]).await?;
  let tree = git_async(work, &["rev-parse", "origin/main^{tree}"]).await?;
  let message = format!("manual main move {}", MANUAL_MOVES.fetch_add(1, Ordering::Relaxed) + 1);
  let commit = git_async(
    work,
    &[
      "-c",
      "user.name=Synthetic",
      "-c",
      "user.email=synthetic@example.test",
      "commit-tree",
      &tree,
      "-p",
      "origin/main",
      "-m",
      &message,
    ],
  )
  .await?;
  git_async(work, &["push", "-q", "origin", &format!("{commit}:refs/heads/main")]).await?;
  Ok(commit)
}

/// A scripted GitHub for a queued landing: each head sha's CI reads answer from its own queue (a sha with no queue
/// passes), every read is recorded, and the remote can move main before it answers.
struct QueueRemote {
  answers: Mutex<HashMap<String, VecDeque<CiVerdict>>>,
  passed_at: Mutex<HashMap<String, String>>,
  reads: Mutex<HashMap<String, usize>>,
  seen: Mutex<Vec<(String, CiVerdict)>>,
  /// The worktree that each CI read ran in: the landing's own git, read from the task venue it was given.
  worktrees: Mutex<Vec<PathBuf>>,
  moved: Mutex<Vec<String>>,
  work: PathBuf,
  default_passed_at: String,
  /// Moves main before answering a read of this sha (every sha when empty) at this read count (every read when 0).
  move_main: Option<(String, usize)>,
}

impl QueueRemote {
  fn new(work: &Path, passed: &[(&str, &str)], default_passed_at: &str) -> QueueRemote {
    let mut times = HashMap::new();
    for (sha, at) in passed {
      times.insert((*sha).to_string(), (*at).to_string());
    }
    QueueRemote {
      answers: Mutex::new(HashMap::new()),
      passed_at: Mutex::new(times),
      reads: Mutex::new(HashMap::new()),
      seen: Mutex::new(Vec::new()),
      worktrees: Mutex::new(Vec::new()),
      moved: Mutex::new(Vec::new()),
      work: work.to_path_buf(),
      default_passed_at: default_passed_at.to_string(),
      move_main: None,
    }
  }

  fn answer(&self, sha: &str, verdicts: Vec<CiVerdict>) {
    self.answers.lock().expect("the answers lock").insert(sha.to_string(), verdicts.into());
  }

  fn moves_main_at(&mut self, sha: &str, read: usize) {
    self.move_main = Some((sha.to_string(), read));
  }

  fn passed_reads(&self, sha: &str) -> usize {
    self
      .seen
      .lock()
      .expect("the seen lock")
      .iter()
      .filter(|(seen, verdict)| seen == sha && *verdict == CiVerdict::Pass)
      .count()
  }
}

impl LandRemote for QueueRemote {
  async fn ci_snapshot(
    &self,
    ctx: &TaskLanding,
    sha: &str,
    _head_pushed_at: Option<&str>,
  ) -> anyhow::Result<CiSnapshot> {
    let verdict = self
      .answers
      .lock()
      .expect("the answers lock")
      .get_mut(sha)
      .and_then(|answers| answers.pop_front())
      .unwrap_or(CiVerdict::Pass);
    self.seen.lock().expect("the seen lock").push((sha.to_string(), verdict.clone()));
    self.worktrees.lock().expect("the worktrees lock").push(ctx.worktree.clone());
    let count = {
      let mut reads = self.reads.lock().expect("the reads lock");
      let count = reads.entry(sha.to_string()).or_insert(0);
      *count += 1;
      *count
    };
    let move_main =
      self.move_main.as_ref().is_some_and(|(who, at)| (who.is_empty() || who == sha) && (*at == 0 || *at == count));
    let passed_at = self
      .passed_at
      .lock()
      .expect("the times lock")
      .get(sha)
      .cloned()
      .unwrap_or_else(|| self.default_passed_at.clone());
    if move_main {
      let moved = push_main_commit(&self.work).await?;
      self.moved.lock().expect("the moved lock").push(moved);
    }
    Ok(CiSnapshot { verdict, checks: Vec::new(), ci_passed_at: Some(passed_at) })
  }

  async fn close_task_prs(&self, _ctx: &TaskLanding, _published: &str, _sha: &str) -> anyhow::Result<()> {
    Ok(())
  }
}

/// An owner whose wake program is a stub: it records its arguments and, for a trigger registration, writes the
/// pending trigger file that the queue's dedupe reads; a cancel removes that file.
fn stub_owner(root: &Path, worktree: &Path, session_id: &str) -> (land_queue::Owner, PathBuf) {
  let session_dir = root.join("sessions").join(session_id);
  let triggers = layout::triggers_dir(&session_dir);
  fs::create_dir_all(&triggers).expect("the triggers directory is created");
  let log = root.join(format!("{session_id}-program.log"));
  let program = root.join(format!("{session_id}-program.sh"));
  let script = format!(
    r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case " $* " in
  *" session trigger "*)
    message=
    previous=
    for argument in "$@"; do
      if [ "$previous" = "--message" ]; then message=$argument; fi
      previous=$argument
    done
    printf '{{"id":"wake-%s","message":"%s","status":"pending"}}\n' "$$" "$message" > "{triggers}/wake-$$.json"
    ;;
  *" trigger cancel "*)
    id=
    for argument in "$@"; do id=$argument; done
    rm -f "{triggers}/"*"$id"*
    ;;
esac
exit 0
"#,
    log = log.display(),
    triggers = triggers.display()
  );
  fs::write(&program, script).expect("the stub program is written");
  let mut permissions = fs::metadata(&program).expect("the stub program exists").permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&program, permissions).expect("the stub program is executable");
  let owner =
    land_queue::Owner { session_id: session_id.to_string(), session_dir, worktree: worktree.to_path_buf(), program };
  (owner, log)
}

/// Drives a queued landing until the queue shows its turn held, so the test can start the clients that must wait
/// behind it; the run itself stays unfinished.
async fn wait_for_turn<F>(site: &land_queue::Site, run: &mut Pin<Box<F>>)
where
  F: Future<Output = anyhow::Result<QueueEnd>>,
{
  tokio::time::timeout(Duration::from_secs(30), async {
    loop {
      if site.read().await.expect("the queue reads").turn.is_some() {
        return;
      }
      tokio::select! {
        result = run.as_mut() => panic!(
          "the landing ended before it held the turn: {:?}",
          result.err().map(|error| format!("{error:#}"))
        ),
        _ = tokio::time::sleep(Duration::from_millis(5)) => {}
      }
    }
  })
  .await
  .expect("the landing took the turn within 30 s");
}

fn landed_record(end: anyhow::Result<QueueEnd>, who: &str) -> LandingRecord {
  match end.expect("the landing ends without an error") {
    QueueEnd::Record(record) => *record,
    QueueEnd::Superseded => panic!("{who} was superseded"),
  }
}

/// Drives a waiting landing until origin shows its entry for `pr` under `owner`, and returns that entry's heartbeat.
/// The run itself stays unfinished.
async fn wait_for_entry<F>(site: &land_queue::Site, owner: &str, pr: u64, run: &mut Pin<Box<F>>) -> land_queue::Beat
where
  F: Future<Output = anyhow::Result<QueueEnd>>,
{
  tokio::time::timeout(Duration::from_secs(30), async {
    loop {
      let view = site.read().await.expect("the queue reads");
      if let Some((_, beat)) = view.entry_of(pr, owner) {
        return beat.clone();
      }
      tokio::select! {
        result = run.as_mut() => panic!(
          "the landing ended before it queued: {:?}",
          result.err().map(|error| format!("{error:#}"))
        ),
        _ = tokio::time::sleep(Duration::from_millis(5)) => {}
      }
    }
  })
  .await
  .expect("the landing queued within 30 s")
}

/// Makes origin decline each push that moves the ref that `flag` names, for as long as `flag` exists. A deletion of
/// the ref passes, and each declined push is logged in the returned file. The wrapper runs in front of the leak
/// scanner that `install_pre_receive_leak_scan` installed, which still checks every other push.
fn decline_moves_of_the_flagged_ref(origin: &Path, flag: &Path) -> PathBuf {
  let scanner = origin.join("hooks/pre-receive.scan");
  fs::rename(origin.join("hooks/pre-receive"), &scanner).expect("the leak scanner hook is moved aside");
  let log = flag.with_file_name("declined.log");
  let script = format!(
    r#"#!/bin/sh
input=$(cat)
flag='{flag}'
log='{log}'
if [ -f "$flag" ]; then
  target=$(cat "$flag")
  case "$input" in
    *" $target"*)
      case "$input" in
        *" 0000000000000000000000000000000000000000 $target"*) ;;
        *)
          echo "declined by the test: $target" >> "$log"
          echo "declined by the test: $target" >&2
          exit 1
          ;;
      esac
      ;;
  esac
fi
printf '%s\n' "$input" | '{scanner}'
"#,
    flag = flag.display(),
    log = log.display(),
    scanner = scanner.display()
  );
  let hook = origin.join("hooks/pre-receive");
  fs::write(&hook, script).expect("the declining hook is written");
  let mut permissions = fs::metadata(&hook).expect("the declining hook exists").permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&hook, permissions).expect("the declining hook is executable");
  log
}

/// A task session under `home` whose metadata names `repo` as its repository, and whose one run record names `work`
/// as the worktree on `branch`. `TaskLanding::load` reads that record as the task's landing venue.
fn write_task_session(home: &Path, session_id: &str, repo: &Path, work: &Path, branch: &str) {
  let session_dir = home.join("sessions").join(session_id);
  fs::create_dir_all(&session_dir).expect("the session directory is created");
  let metadata = json!({
    "name": "task",
    "schema_version": 2,
    "profile": "worker",
    "task": { "repo_path": repo.display().to_string(), "base_branch": "main" },
  });
  fs::write(layout::metadata_path(&session_dir), metadata.to_string()).expect("the session metadata is written");
  let mut run = test_fixtures::run_record(session_id, "run-landing", RunKind::Work);
  run.repo_path = Some(repo.display().to_string());
  run.base_branch = Some("main".to_string());
  run.branch_name = Some(branch.to_string());
  run.worktree_path = Some(work.display().to_string());
  let run_dir = layout::runs_dir(&session_dir).join(&run.id);
  fs::create_dir_all(&run_dir).expect("the run directory is created");
  fs::write(run_dir.join(run_records::RUN_METADATA_NAME), run_records::run_record_text(&run))
    .expect("the run record is written");
}

/// The subjects of origin's main, oldest first, as the test clones see them.
async fn main_subjects(repo: &Path) -> Vec<String> {
  assert!(git::git_stdout(repo, &["fetch", "-q", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
  git_async(repo, &["log", "--reverse", "--format=%s", "origin/main"])
    .await
    .expect("the log reads")
    .lines()
    .map(str::to_string)
    .collect()
}

/// The merge command of a queued pull request merge: a squash-like empty commit on main.
fn merge_command(work: &Path, pr: u64) -> String {
  let dir = work.display();
  format!(
    "git -C {dir} fetch -q origin main && git -C {dir} checkout -q -B landqueue origin/main && \
       git -C {dir} -c user.name=Synthetic -c user.email=synthetic@example.test commit -q --allow-empty -m 'merged PR {pr}' && \
       git -C {dir} push -q origin landqueue:main"
  )
}

const CADENCE: Cadence = Cadence { poll: Duration::from_millis(5), beat: Duration::from_millis(30) };
const SESSION_A: &str = "00000000-0000-4000-8000-000000000001";
const SESSION_B: &str = "00000000-0000-4000-8000-000000000002";
const SESSION_C: &str = "00000000-0000-4000-8000-000000000003";
const SESSION_MERGE: &str = "00000000-0000-4000-8000-000000000004";
const SESSION_E: &str = "00000000-0000-4000-8000-000000000005";
const SESSION_F: &str = "00000000-0000-4000-8000-000000000006";
const SESSION_DIRECT: &str = "00000000-0000-4000-8000-000000000007";
const SESSION_QUEUED: &str = "00000000-0000-4000-8000-000000000008";
const SESSION_H: &str = "00000000-0000-4000-8000-000000000009";
const SESSION_REPLACEMENT: &str = "00000000-0000-4000-8000-000000000010";
const SESSION_REFUSED: &str = "00000000-0000-4000-8000-000000000011";
const SESSION_REPEAT: &str = "00000000-0000-4000-8000-000000000012";
const SESSION_EDIT: &str = "00000000-0000-4000-8000-000000000013";
const SESSION_NEW_COMMIT: &str = "00000000-0000-4000-8000-000000000014";
const SESSION_CI_FAILED: &str = "00000000-0000-4000-8000-000000000015";
const SESSION_SIGNAL: &str = "00000000-0000-4000-8000-000000000016";
const SESSION_LANDED_REPEAT: &str = "00000000-0000-4000-8000-000000000017";
const SESSION_WORKTREE_GONE: &str = "00000000-0000-4000-8000-000000000018";
/// The environment that hands `queued_landing_child` its paths: the profile home, the queued sha, the remote's work
/// clone, and the file the child writes its outcome to.
const CHILD_HOME_ENV: &str = "CHARLIEBOT_LAND_TEST_CHILD_HOME";
const CHILD_SHA_ENV: &str = "CHARLIEBOT_LAND_TEST_CHILD_SHA";
const CHILD_REMOTE_ENV: &str = "CHARLIEBOT_LAND_TEST_CHILD_REMOTE";
const CHILD_RESULT_ENV: &str = "CHARLIEBOT_LAND_TEST_CHILD_RESULT";

#[tokio::test]
#[ignore = "integration"]
async fn landings_and_a_merge_reach_main_in_ci_pass_order() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let a_work = task_clone(root.path(), "task-a");
  let b_work = task_clone(root.path(), "task-b");
  let c_work = task_clone(root.path(), "task-c");
  let merge_work = task_clone(root.path(), "merge-work");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let a = venue(&a_work, "task-a", SESSION_A);
  let b = venue(&b_work, "task-b", SESSION_B);
  let c = venue(&c_work, "task-c", SESSION_C);
  let (sha_a, sha_b, sha_c) = (head(&a_work), head(&b_work), head(&c_work));
  for (ctx, pr, sha) in [(&a, 101, &sha_a), (&b, 102, &sha_b), (&c, 103, &sha_c)] {
    write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
  }
  let remote = QueueRemote::new(
    &remote_work,
    &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:02Z"), (&sha_c, "2026-11-27T00:00:05Z")],
    "2026-11-27T00:00:09Z",
  );
  // The first landing holds the turn while its CI is still running, which gives the other clients time to queue.
  remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Pass]);
  let registry = Registry::new();
  let merge_site = land_queue::Site { worktree: merge_work.clone() };
  let (owner, _log) = stub_owner(root.path(), &merge_work, SESSION_MERGE);
  let merge_head = head(&merge_work);
  let merge_cmd = merge_command(&merge_work, 104);
  let merge_key = rfc3339_millis("2026-11-27T00:00:03Z").expect("the merge pass time parses");

  let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
  wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
  let gate = tokio::time::timeout(
    Duration::from_secs(10),
    land_queue::pr_merge_gate(&merge_site, Some(&owner), 104, &merge_head, Some(&merge_cmd), None, CADENCE, merge_key),
  )
  .await
  .expect("the merge gate answers")
  .expect("the merge gate reads the queue");
  assert_eq!(gate, land_queue::MergeGate::Queued, "the held turn queues the merge");
  let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
  let c_run = Box::pin(queued_landing(&c, &registry, &home, &sha_c, &remote, CADENCE, Duration::from_secs(60)));
  let merge_run = Box::pin(land_queue::run_pr_merge_queued(&merge_site, &owner, 104, &merge_cmd, CADENCE));

  let (a_end, b_end, c_end, merge_end) = tokio::join!(a_run, b_run, c_run, merge_run);
  let (a_record, b_record, c_record) =
    (landed_record(a_end, "A"), landed_record(b_end, "B"), landed_record(c_end, "C"));
  assert_eq!(merge_end.expect("the queued merge ends without an error"), 0);
  assert_eq!(a_record.state, STATE_LANDED);
  assert_eq!(b_record.state, STATE_LANDED);
  assert_eq!(c_record.state, STATE_LANDED);

  // The merge key sits between B's and C's, so main takes exactly the queue's order.
  assert_eq!(
    main_subjects(&a_work).await,
    vec![
      "base".to_string(),
      "workflow".to_string(),
      "task-a.txt".to_string(),
      "task-b.txt".to_string(),
      "merged PR 104".to_string(),
      "task-c.txt".to_string()
    ]
  );
  // Each landed commit passed its own CI read on itself.
  for record in [&a_record, &b_record, &c_record] {
    assert!(remote.passed_reads(&record.sha) > 0, "{} never passed a CI read", record.sha);
  }
  let view = land_queue::Site { worktree: a_work.clone() }.read().await.expect("the queue reads");
  assert!(view.turn.is_none() && view.entries.is_empty(), "the queue is empty after the landings");
}

#[tokio::test]
#[ignore = "integration"]
async fn an_earlier_ci_pass_written_later_goes_first_without_interrupting_the_holder() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let a_work = task_clone(root.path(), "task-a");
  let b_work = task_clone(root.path(), "task-b");
  let c_work = task_clone(root.path(), "task-c");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let a = venue(&a_work, "task-a", SESSION_A);
  let b = venue(&b_work, "task-b", SESSION_B);
  let c = venue(&c_work, "task-c", SESSION_C);
  let (sha_a, sha_b, sha_c) = (head(&a_work), head(&b_work), head(&c_work));
  for (ctx, pr, sha) in [(&a, 201, &sha_a), (&b, 202, &sha_b), (&c, 203, &sha_c)] {
    write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
  }
  let remote = QueueRemote::new(
    &remote_work,
    &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:05Z"), (&sha_c, "2026-11-27T00:00:03Z")],
    "2026-11-27T00:00:09Z",
  );
  remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Pass]);
  let registry = Registry::new();
  let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
  wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
  // B writes its entry first; C, whose CI passed earlier, writes second and takes the place ahead of B.
  let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
  tokio::time::sleep(Duration::from_millis(20)).await;
  let c_run = Box::pin(queued_landing(&c, &registry, &home, &sha_c, &remote, CADENCE, Duration::from_secs(60)));
  let (a_end, b_end, c_end) = tokio::join!(a_run, b_run, c_run);
  for (end, who) in [(a_end, "A"), (b_end, "B"), (c_end, "C")] {
    assert_eq!(landed_record(end, who).state, STATE_LANDED);
  }
  assert_eq!(
    main_subjects(&a_work).await,
    vec![
      "base".to_string(),
      "workflow".to_string(),
      "task-a.txt".to_string(),
      "task-c.txt".to_string(),
      "task-b.txt".to_string()
    ]
  );
}

#[tokio::test]
#[ignore = "integration"]
async fn a_repeat_landing_call_registers_nothing_and_a_new_head_replaces_the_entry() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let ctx = venue(&work, "task", SESSION_C);
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(301, &sha)).expect("the landing record is written");
  let site = land_queue::Site { worktree: work.clone() };
  let (owner, log) = stub_owner(root.path(), &work, SESSION_C);
  let remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
  remote
    .answer(&sha, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Waiting, CiVerdict::Waiting, CiVerdict::Pass]);
  assert_eq!(
    land_queue::arm_landing(&site, &owner, 301, &sha).await.expect("the first call registers"),
    land_queue::Armed::Registered
  );
  let registry = Registry::new();
  let mut run = Box::pin(queued_landing(&ctx, &registry, &home, &sha, &remote, CADENCE, Duration::from_secs(60)));
  wait_for_turn(&site, &mut run).await;
  // A repeat call for the same head returns the current state and registers no second command.
  assert_eq!(
    land_queue::arm_landing(&site, &owner, 301, &sha).await.expect("the repeat reads"),
    land_queue::Armed::Existing
  );
  let registered = fs::read_to_string(&log).expect("the program log reads");
  assert_eq!(registered.matches("session trigger").count(), 1, "no second trigger was registered: {registered}");
  // A new head replaces the old entry and registers the new head's command. `land` writes the new head's record
  // before it arms the command, and the old command reads that record to see that it was superseded.
  commit(&work, "two.txt", "two\n");
  let new_sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(301, &new_sha)).expect("the new landing record is written");
  assert_eq!(
    land_queue::arm_landing(&site, &owner, 301, &new_sha).await.expect("the new head registers"),
    land_queue::Armed::Registered
  );
  let view = site.read().await.expect("the queue reads");
  let session_c = land_queue::owner_token(SESSION_C).expect("the session id names a token");
  assert!(view.entry_of(301, &session_c).is_none(), "the replaced head's entry was removed");
  assert!(matches!(run.await.expect("the old command ends"), QueueEnd::Superseded), "the moved head supersedes it");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_repeat_land_call_keeps_a_queued_landing_while_main_moves_behind_a_holder() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_queue_repo(root.path(), "main");
  // `TaskLanding::load` reads the repository's owner and name from its origin URL, which a file URL carries.
  let origin_url = format!("file://{}", root.path().join("origin.git").display());
  git_ok(&repo, &["remote", "set-url", "origin", &origin_url]);
  let work = task_clone(root.path(), "task");
  // The repository a worktree is cut from holds the task's commits, so the queued landing reads the head from it.
  git_ok(&repo, &["fetch", "-q", "origin", "task"]);
  let holder_work = task_clone(root.path(), "holder");
  let outside = task_clone(root.path(), "outside");
  // The profile home is the root: its `sessions` directory holds the task session and the stub owner's session.
  let home = root.path();
  write_task_session(home, SESSION_REPEAT, &repo, &work, "task");
  let mut ctx = TaskLanding::load(home, SESSION_REPEAT).await.expect("the task session loads");
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(601, &sha)).expect("the landing record is written");
  let site = land_queue::Site { worktree: work.clone() };
  let (owner, program_log) = stub_owner(root.path(), &work, SESSION_REPEAT);
  // The wake of each queued command runs the stub program, which records its calls.
  ctx.queue_program = owner.program.clone();
  let owner_key = land_queue::owner_token(SESSION_REPEAT).expect("the session id names a token");
  assert_eq!(
    land_queue::arm_landing(&site, &owner, 601, &sha).await.expect("the first call registers"),
    land_queue::Armed::Registered
  );
  // A holder takes the turn, so the queued command waits behind it.
  let holder_site = land_queue::Site { worktree: holder_work.clone() };
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&holder_site, 600, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  let registry = Registry::new();
  let remote = QueueRemote::new(&outside, &[], "2026-11-27T00:00:09Z");
  let mut run = Box::pin(queued_landing(&ctx, &registry, home, &sha, &remote, CADENCE, Duration::from_secs(60)));
  let entry = wait_for_entry(&site, &owner_key, 601, &mut run).await;
  // Another landing moves main while the queued command waits.
  let moved = push_main_commit(&outside).await.expect("the other landing moves main");
  git_async(&holder_work, &["fetch", "-q", "origin", "main"]).await.expect("the main fetch runs");
  assert_eq!(git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads"), moved);

  // A repeat call returns the waiting record and leaves the entry, its key and the task branch head as they were.
  let repeated = land_in(&ctx, &registry, home, &LandOptions::default()).await.expect("the repeat call returns");
  assert_eq!((repeated.state.as_str(), repeated.sha.as_str()), (STATE_WAITING, sha.as_str()));
  assert_eq!(head(&work), sha, "the task branch keeps its head");
  let view = site.read().await.expect("the queue reads");
  let (name, beat) = view.entry_of(601, &owner_key).expect("the entry still waits");
  assert_eq!(name.refname(), entry.refname, "the entry keeps its key");
  assert_eq!(beat.parent, sha, "the entry still names the landed head");
  let registered = fs::read_to_string(&program_log).expect("the program log reads");
  assert_eq!(registered.matches("session trigger").count(), 1, "no second command was registered: {registered}");

  // Once the holder releases the turn, the queued command takes it and lands the head on the moved main.
  let (released, end) = tokio::join!(holder.release(), run);
  released.expect("the holder releases the turn");
  assert_eq!(landed_record(end, "the queued landing").state, STATE_LANDED_NO_CI);
}

/// A queued command's record is written only while the session's record still names its head, so a stale command
/// never overwrites the record of the newer call that replaced it.
#[test]
fn a_queued_record_never_overwrites_a_newer_calls_record() {
  let session = tempfile::tempdir().expect("temporary directory is available");
  let mut ctx = landing_ctx(session.path(), "task");
  ctx.session_dir = session.path().to_path_buf();
  write_landing_record(&ctx.session_dir, &queue_record(801, "newer-sha")).expect("the newer record is written");
  assert!(
    !write_queued_record(&ctx, "queued-sha", &queue_record(801, "queued-sha")).expect("the stale write reads"),
    "a stale command's record is not written"
  );
  let kept = read_landing_record(&ctx.session_dir).expect("the record reads").expect("the record exists");
  assert_eq!(kept.sha, "newer-sha", "the newer call's record stays");
  assert!(
    write_queued_record(&ctx, "newer-sha", &queue_record(801, "newer-sha")).expect("the current write reads"),
    "the current command's record is written"
  );
}

/// The scratch worktrees the home holds: one entry per worktree directory that a queued landing has not removed.
fn scratch_worktrees(home: &Path) -> Vec<PathBuf> {
  let root = landing_worktree::root_in(home);
  match fs::read_dir(&root) {
    Ok(entries) => entries.map(|entry| entry.expect("the scratch entry reads").path()).collect(),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => panic!("{} cannot be read: {error}", root.display()),
  }
}

/// A gated task whose one pushed commit sits in the session worktree `work`, with the commit's waiting record in
/// the session's landing record. The task's repository fetches the commit, as the repository that a production
/// worktree is cut from holds it. Returns the profile home, the session worktree and the queued sha.
async fn queued_task(root: &Path, session_id: &str) -> (PathBuf, PathBuf, String) {
  let repo = gated_repo(root, "main");
  let origin_url = format!("file://{}", root.join("origin.git").display());
  git_ok(&repo, &["remote", "set-url", "origin", &origin_url]);
  let work = task_clone(root, "task");
  git_ok(&repo, &["fetch", "-q", "origin", "task"]);
  let home = root.join("home");
  fs::create_dir_all(&home).expect("the home is created");
  write_task_session(&home, session_id, &repo, &work, "task");
  let ctx = TaskLanding::load(&home, session_id).await.expect("the task session loads");
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(701, &sha)).expect("the landing record is written");
  (home, work, sha)
}

/// A queued landing takes its turn and publishes its commit rebased onto the main that moved while it waited,
/// although `change` alters the session's worktree first. The holder keeps the turn until the change and the move
/// are made.
async fn queued_landing_publishes_past_a_session_worktree_change(session_id: &str, change: impl FnOnce(&Path)) {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let (home, work, sha) = queued_task(root.path(), session_id).await;
  let ctx = TaskLanding::load(&home, session_id).await.expect("the task session loads");
  let holder_work = task_clone(root.path(), "holder");
  let outside = task_clone(root.path(), "outside");
  let site = land_queue::Site { worktree: work.clone() };
  let owner_key = land_queue::owner_token(session_id).expect("the session id names a token");
  let holder_site = land_queue::Site { worktree: holder_work.clone() };
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&holder_site, 700, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  let remote = QueueRemote::new(&outside, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
  let registry = Registry::new();
  let mut run = Box::pin(queued_landing(&ctx, &registry, &home, &sha, &remote, CADENCE, Duration::from_secs(60)));
  wait_for_entry(&site, &owner_key, 701, &mut run).await;
  change(&work);
  let session_head = head(&work);
  let session_status = git_async(&work, &["status", "--porcelain"]).await.expect("the status reads");
  let moved = push_main_commit(&outside).await.expect("the other landing moves main");

  let (released, end) = tokio::join!(holder.release(), run);
  released.expect("the holder releases the turn");
  let record = landed_record(end, "the queued landing");
  assert_eq!(record.state, STATE_LANDED);
  assert_ne!(record.sha, sha, "the commit is published rebased onto the moved main");
  let main = git_async(&outside, &["ls-remote", "origin", "refs/heads/main"]).await.expect("main reads");
  assert_eq!(main.split_whitespace().next(), Some(record.sha.as_str()), "the landed commit is main's tip");
  let subjects = main_subjects(&work).await;
  assert!(subjects.contains(&"task.txt".to_string()), "{subjects:?}");
  let ancestry = Command::new("git")
    .arg("-C")
    .arg(&work)
    .args(["merge-base", "--is-ancestor", &moved, &record.sha])
    .status()
    .expect("git runs");
  assert!(ancestry.success(), "the outside commit stays under the landing");
  let branch = git_async(&work, &["ls-remote", "origin", "refs/heads/task"]).await.expect("the branch reads");
  assert_eq!(
    branch.split_whitespace().next(),
    Some(record.sha.as_str()),
    "origin's task branch holds the landed commit"
  );
  let view = site.read().await.expect("the queue reads");
  assert!(view.turn.is_none() && view.entries.is_empty(), "the publish removed the turn and the entry");
  assert_eq!(head(&work), session_head, "the session's worktree keeps its head");
  assert_eq!(git_async(&work, &["status", "--porcelain"]).await.expect("the status reads"), session_status);
  assert!(scratch_worktrees(&home).is_empty(), "the scratch worktree is removed: {:?}", scratch_worktrees(&home));
  let reads = remote.worktrees.lock().expect("the worktrees lock").clone();
  assert!(!reads.is_empty(), "the CI read ran");
  for read in reads {
    assert!(read.starts_with(landing_worktree::root_in(&home)), "CI ran in {}, not a scratch worktree", read.display());
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn a_queued_landing_publishes_past_uncommitted_edits_in_the_session_worktree() {
  queued_landing_publishes_past_a_session_worktree_change(SESSION_EDIT, |work| {
    fs::write(work.join("task.txt"), "an uncommitted edit\n").expect("the edit is written");
  })
  .await;
}

#[tokio::test]
#[ignore = "integration"]
async fn a_queued_landing_publishes_past_a_new_commit_in_the_session_worktree() {
  queued_landing_publishes_past_a_session_worktree_change(SESSION_NEW_COMMIT, |work| {
    commit(work, "two.txt", "two\n");
  })
  .await;
}

#[tokio::test]
#[ignore = "integration"]
async fn a_queued_landing_removes_its_scratch_worktree_after_a_failed_ci() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let (home, work, sha) = queued_task(root.path(), SESSION_CI_FAILED).await;
  let ctx = TaskLanding::load(&home, SESSION_CI_FAILED).await.expect("the task session loads");
  let outside = task_clone(root.path(), "outside");
  let remote = QueueRemote::new(&outside, &[], "2026-11-27T00:00:09Z");
  remote.answer(&sha, vec![CiVerdict::Failed { failing: Vec::new() }]);
  let end = queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60)).await;
  assert_eq!(landed_record(end, "the queued landing").state, STATE_CI_FAILED);
  assert!(scratch_worktrees(&home).is_empty(), "the scratch worktree is removed: {:?}", scratch_worktrees(&home));
  let reads = remote.worktrees.lock().expect("the worktrees lock").clone();
  assert!(!reads.is_empty(), "the CI read ran");
  for read in reads {
    assert!(read.starts_with(landing_worktree::root_in(&home)), "CI ran in {}, not a scratch worktree", read.display());
  }
  assert_eq!(head(&work), sha, "the session's worktree keeps its head");
}

/// Task completion can remove the session's worktree while its landing waits. The queued landing reads the task record
/// and the repository only, so it still publishes.
#[tokio::test]
#[ignore = "integration"]
async fn a_queued_landing_completes_after_its_session_worktree_is_removed() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let (home, work, sha) = queued_task(root.path(), SESSION_WORKTREE_GONE).await;
  fs::remove_dir_all(&work).expect("the session worktree is removed");
  let ctx = TaskLanding::load(&home, SESSION_WORKTREE_GONE).await.expect("the task loads without its worktree");
  let outside = task_clone(root.path(), "outside");
  let remote = QueueRemote::new(&outside, &[], "2026-11-27T00:00:09Z");
  let end = queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60)).await;
  let record = landed_record(end, "the queued landing");
  assert_eq!(record.state, STATE_LANDED);
  let main = git_async(&outside, &["ls-remote", "origin", "refs/heads/main"]).await.expect("main reads");
  assert_eq!(main.split_whitespace().next(), Some(record.sha.as_str()), "the landed commit is main's tip");
  assert!(scratch_worktrees(&home).is_empty(), "the scratch worktree is removed: {:?}", scratch_worktrees(&home));
}

/// The child process of `a_stop_signal_ends_a_queued_landing_and_removes_its_scratch_worktree`: runs one queued
/// landing over the paths the environment names and writes how it ended to the result file. Without that
/// environment the test does nothing, so only the parent starts it.
#[tokio::test]
#[ignore = "integration"]
async fn queued_landing_child() {
  let Some(home) = std::env::var_os(CHILD_HOME_ENV) else {
    return;
  };
  let home = PathBuf::from(home);
  let sha = std::env::var(CHILD_SHA_ENV).expect("the child receives the queued sha");
  let remote_work = PathBuf::from(std::env::var_os(CHILD_REMOTE_ENV).expect("the child receives the remote's clone"));
  let result = PathBuf::from(std::env::var_os(CHILD_RESULT_ENV).expect("the child receives its result file"));
  let ctx = TaskLanding::load(&home, SESSION_SIGNAL).await.expect("the task session loads");
  let remote = QueueRemote::new(&remote_work, &[], "2026-11-27T00:00:09Z");
  let outcome =
    match queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60)).await {
      Ok(QueueEnd::Record(record)) => format!("record {}", record.state),
      Ok(QueueEnd::Superseded) => "superseded".to_string(),
      Err(error) => format!("error: {error:#}"),
    };
  fs::write(&result, outcome).expect("the child writes its outcome");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_stop_signal_ends_a_queued_landing_and_removes_its_scratch_worktree() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let (home, work, sha) = queued_task(root.path(), SESSION_SIGNAL).await;
  let holder_work = task_clone(root.path(), "holder");
  let outside = task_clone(root.path(), "outside");
  let holder_site = land_queue::Site { worktree: holder_work.clone() };
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&holder_site, 700, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);

  // The child waits behind the holder in its own process group, so the signal reaches it alone.
  let result = root.path().join("child-result.txt");
  let mut child = std::process::Command::new(std::env::current_exe().expect("the test binary path reads"))
    .args(["land::synthetic_repo_tests::queued_landing_child", "--exact", "--ignored", "--nocapture"])
    .env(CHILD_HOME_ENV, &home)
    .env(CHILD_SHA_ENV, &sha)
    .env(CHILD_REMOTE_ENV, &outside)
    .env(CHILD_RESULT_ENV, &result)
    .process_group(0)
    .spawn()
    .expect("the child starts");
  let site = land_queue::Site { worktree: work.clone() };
  let owner_key = land_queue::owner_token(SESSION_SIGNAL).expect("the session id names a token");
  tokio::time::timeout(Duration::from_secs(60), async {
    while site.read().await.expect("the queue reads").entry_of(701, &owner_key).is_none() {
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .expect("the child queued within 60 s");
  assert_eq!(scratch_worktrees(&home).len(), 1, "the child's scratch worktree exists while it waits");

  let group = libc::pid_t::try_from(child.id()).expect("a pid fits pid_t");
  // SAFETY: `group` is the child's pid, and the child leads its own process group, so the negative pid names only it
  // and the children it runs.
  let sent = unsafe { libc::kill(-group, libc::SIGTERM) };
  assert_eq!(sent, 0, "the stop signal is sent to the child's group");
  let status =
    tokio::task::spawn_blocking(move || child.wait()).await.expect("the wait task ends").expect("the child exits");
  assert!(status.success(), "the child writes its outcome and exits: {status}");
  let outcome = fs::read_to_string(&result).expect("the child wrote its outcome");
  assert!(outcome.contains("stopped by SIGTERM"), "the child ended by the signal: {outcome}");
  assert!(
    scratch_worktrees(&home).is_empty(),
    "the signal removed the scratch worktree: {:?}",
    scratch_worktrees(&home)
  );
  assert_eq!(head(&work), sha, "the session's worktree keeps its head");
  holder.release().await.expect("the holder releases the turn");
}

/// A repeat `land` call after a landing returns the landed record. The queued command rebased the head onto a moved
/// main, so the session's head still holds the landed patch under its old sha and must not start another landing.
#[tokio::test]
#[ignore = "integration"]
async fn a_repeat_land_call_after_a_landing_returns_the_landed_record() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = synthetic_queue_repo(root.path(), "main");
  let origin_url = format!("file://{}", root.path().join("origin.git").display());
  git_ok(&repo, &["remote", "set-url", "origin", &origin_url]);
  let work = task_clone(root.path(), "task");
  git_ok(&repo, &["fetch", "-q", "origin", "task"]);
  let remote_work = task_clone(root.path(), "remote-work");
  // The profile home is the root, as in the repeat-call test above.
  let home = root.path();
  write_task_session(home, SESSION_LANDED_REPEAT, &repo, &work, "task");
  let mut ctx = TaskLanding::load(home, SESSION_LANDED_REPEAT).await.expect("the task session loads");
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(801, &sha)).expect("the landing record is written");
  let (owner, program_log) = stub_owner(root.path(), &work, SESSION_LANDED_REPEAT);
  ctx.queue_program = owner.program.clone();
  // Main moves before the queued landing, so the landed commit is the head rebased onto it.
  push_main_commit(&remote_work).await.expect("the other landing moves main");
  let remote = QueueRemote::new(&remote_work, &[], "2026-11-27T00:00:09Z");
  let registry = Registry::new();
  let landed = landed_record(
    queued_landing(&ctx, &registry, home, &sha, &remote, CADENCE, Duration::from_secs(60)).await,
    "the queued landing",
  );
  assert_eq!(landed.state, STATE_LANDED_NO_CI);
  assert_ne!(landed.sha, sha, "the landed commit is the head rebased onto the moved main");

  let repeated = land_in(&ctx, &registry, home, &LandOptions::default()).await.expect("the repeat call returns");
  assert_eq!((repeated.state.as_str(), repeated.sha.as_str()), (STATE_LANDED_NO_CI, landed.sha.as_str()));
  assert_eq!(head(&work), sha, "the session's worktree keeps its head");
  let registered =
    if program_log.exists() { fs::read_to_string(&program_log).expect("the program log reads") } else { String::new() };
  assert!(!registered.contains("session trigger"), "no second command was registered: {registered}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_waiter_whose_heartbeat_push_is_declined_keeps_its_entry_and_takes_the_turn() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let holder_work = task_clone(root.path(), "holder");
  let b_work = task_clone(root.path(), "task-b");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let b = venue(&b_work, "task-b", SESSION_B);
  let sha_b = head(&b_work);
  write_landing_record(&b.session_dir, &queue_record(502, &sha_b)).expect("the landing record is written");
  let flag = root.path().join("decline-flag");
  let declined = decline_moves_of_the_flagged_ref(&root.path().join("origin.git"), &flag);
  let remote = QueueRemote::new(&remote_work, &[(&sha_b, "2026-11-27T00:00:02Z")], "2026-11-27T00:00:09Z");
  let site = land_queue::Site { worktree: b_work.clone() };
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&land_queue::Site { worktree: holder_work.clone() }, 501, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  let registry = Registry::new();
  let mut b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
  let b_owner = land_queue::owner_token(SESSION_B).expect("the session id names a token");
  let entry = wait_for_entry(&site, &b_owner, 502, &mut b_run).await;
  // B's heartbeat pushes move its entry, so they are declined while the entry still holds B's sha.
  fs::write(&flag, &entry.refname).expect("the decline flag is written");
  tokio::time::timeout(Duration::from_secs(30), async {
    while !declined.exists() {
      tokio::select! {
        result = b_run.as_mut() => panic!(
          "the waiter ended after its heartbeat was declined: {:?}",
          result.err().map(|error| format!("{error:#}"))
        ),
        _ = tokio::time::sleep(Duration::from_millis(5)) => {}
      }
    }
  })
  .await
  .expect("a heartbeat push was declined within 30 s");
  // The declining stops, so B's take of the turn is not declined; B keeps its place until the holder releases.
  fs::remove_file(&flag).expect("the decline flag is removed");
  let (released, end) = tokio::join!(holder.release(), b_run);
  released.expect("the holder releases the turn");
  assert_eq!(landed_record(end, "B").state, STATE_LANDED);
  assert_eq!(main_subjects(&b_work).await.last().map(String::as_str), Some("task-b.txt"));
}

#[tokio::test]
#[ignore = "integration"]
async fn a_waiter_whose_entry_is_gone_when_its_heartbeat_is_declined_reports_the_removal_with_git_stderr() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let holder_work = task_clone(root.path(), "holder");
  let b_work = task_clone(root.path(), "task-b");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let b = venue(&b_work, "task-b", SESSION_B);
  let sha_b = head(&b_work);
  write_landing_record(&b.session_dir, &queue_record(503, &sha_b)).expect("the landing record is written");
  let remote = QueueRemote::new(&remote_work, &[(&sha_b, "2026-11-27T00:00:02Z")], "2026-11-27T00:00:09Z");
  let site = land_queue::Site { worktree: b_work.clone() };
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&land_queue::Site { worktree: holder_work.clone() }, 504, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  let registry = Registry::new();
  let mut b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
  let b_owner = land_queue::owner_token(SESSION_B).expect("the session id names a token");
  let entry = wait_for_entry(&site, &b_owner, 503, &mut b_run).await;
  // Another client removes B's entry, so B's next heartbeat finds it gone.
  let removed = site
    .push(&[land_queue::Change::Delete { refname: entry.refname.clone(), from: entry.sha.clone() }])
    .await
    .expect("the entry is removed");
  assert_eq!(removed, land_queue::Pushed::Done);
  let error = b_run.await.err().expect("the waiter reports the removal");
  let text = format!("{error:#}");
  assert!(text.contains("was removed while it waited"), "{text}");
  assert!(text.contains("failed to push"), "the error names git's reason: {text}");
  holder.release().await.expect("the holder releases the turn");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_round_waiter_whose_heartbeats_are_declined_reaches_its_bound_and_removes_its_entry() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  synthetic_queue_repo(root.path(), "main");
  let round_work = task_clone(root.path(), "round");
  let holder_work = task_clone(root.path(), "holder");
  let site = land_queue::Site { worktree: round_work.clone() };
  let round_head = head(&round_work);
  // The round's owner key is its launch id, which the entry carries as it is.
  let launch = "20261127T120000-4242-fc8d9d";
  let ci_passed_ms = 1_795_000_000_000;
  let main_tip = git_async(&holder_work, &["rev-parse", "origin/main"]).await.expect("the main tip reads");
  let mut holder = land_queue::Claim::direct(&land_queue::Site { worktree: holder_work.clone() }, 700, &main_tip);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  // The round's entry is queued first, so its heartbeat pushes move it while it still holds the round's sha.
  let name = land_queue::EntryName::new(ci_passed_ms, 701, launch);
  land_queue::Claim::enter(&site, &name, &round_head).await.expect("the round's entry enters");
  let flag = root.path().join("decline-flag");
  let declined = decline_moves_of_the_flagged_ref(&root.path().join("origin.git"), &flag);
  fs::write(&flag, name.refname()).expect("the decline flag is written");

  let bound = Duration::from_millis(900);
  let started = Instant::now();
  let acquired = land_queue::acquire(&site, launch, 701, &round_head, ci_passed_ms, bound, CADENCE)
    .await
    .expect("the round's wait ends at its bound");
  let waited = started.elapsed();
  assert_eq!(acquired, land_queue::Acquired::BoundReached);
  assert!(waited >= bound && waited < bound + Duration::from_secs(3), "the wait ended at its bound: {waited:?}");
  assert!(declined.exists(), "the heartbeat pushes were declined while the entry held the round's sha");
  let view = site.read().await.expect("the queue reads");
  assert!(view.entry_of(701, launch).is_none(), "the bound removed the round's entry");
  holder.release().await.expect("the holder releases the turn");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_failed_holder_ci_releases_the_turn_to_the_next_entry() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let a_work = task_clone(root.path(), "task-a");
  let b_work = task_clone(root.path(), "task-b");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let a = venue(&a_work, "task-a", SESSION_A);
  let b = venue(&b_work, "task-b", SESSION_B);
  let (sha_a, sha_b) = (head(&a_work), head(&b_work));
  for (ctx, pr, sha) in [(&a, 401, &sha_a), (&b, 402, &sha_b)] {
    write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
  }
  let remote = QueueRemote::new(
    &remote_work,
    &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:02Z")],
    "2026-11-27T00:00:09Z",
  );
  remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Failed { failing: Vec::new() }]);
  let registry = Registry::new();
  let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
  wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
  let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
  let (a_end, b_end) = tokio::join!(a_run, b_run);
  assert_eq!(landed_record(a_end, "A").state, STATE_CI_FAILED);
  assert_eq!(landed_record(b_end, "B").state, STATE_LANDED);
  let view = land_queue::Site { worktree: a_work.clone() }.read().await.expect("the queue reads");
  assert!(view.turn.is_none() && view.entries.is_empty());
}

#[tokio::test]
#[ignore = "integration"]
async fn a_stale_heartbeat_is_pruned_from_the_entry_and_the_turn() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  let repo = gated_repo(root.path(), "main");
  let parent = head(&repo);
  let stale = |refname: &str| {
    let stamp = format!("@{} +0000", OffsetDateTime::now_utc().unix_timestamp() - 600);
    let output = Command::new("git")
      .arg("-C")
      .arg(&repo)
      .args(["-c", "commit.gpgsign=false", "commit-tree", "--no-gpg-sign", "4b825dc642cb6eb9a060e54bf8d69288fbee4904"])
      .args(["-p", &parent, "-m", refname])
      .env("GIT_AUTHOR_NAME", "charliebot land queue")
      .env("GIT_AUTHOR_EMAIL", "land-queue@charliebot.invalid")
      .env("GIT_COMMITTER_NAME", "charliebot land queue")
      .env("GIT_COMMITTER_EMAIL", "land-queue@charliebot.invalid")
      .env("GIT_AUTHOR_DATE", &stamp)
      .env("GIT_COMMITTER_DATE", &stamp)
      .output()
      .expect("git commit-tree runs");
    assert!(output.status.success(), "commit-tree failed: {}", String::from_utf8_lossy(&output.stderr));
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    git_ok(&repo, &["push", "-q", "--force", "origin", &format!("{sha}:{refname}")]);
  };
  let session_e = land_queue::owner_token(SESSION_E).expect("the session id names a token");
  let entry_e = land_queue::EntryName::new(1_795_000_000_000, 7, &session_e);
  stale(&entry_e.refname());
  stale(land_queue::TURN_REF);
  let site = land_queue::Site { worktree: repo.clone() };
  let view = site.read().await.expect("the queue reads and prunes");
  assert!(view.turn.is_none(), "the stale turn is pruned");
  assert!(view.entries.is_empty(), "the stale entry is pruned");
  let left = git_async(&repo, &["ls-remote", "origin", land_queue::TURN_REF, "refs/land-queue/*"])
    .await
    .expect("ls-remote runs");
  assert!(left.is_empty(), "origin holds no stale queue refs: {left}");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_main_push_from_outside_starts_another_rebase_round() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let ctx = venue(&work, "task", SESSION_F);
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(501, &sha)).expect("the landing record is written");
  let mut remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
  // The turn holder's first CI read is answered after main moves from outside.
  remote.moves_main_at(&sha, 2);
  let end = queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60))
    .await
    .expect("the landing recovers");
  let record = landed_record(Ok(end), "A");
  assert_eq!(record.state, STATE_LANDED);
  let moved = remote.moved.lock().expect("the moved lock").clone();
  assert_eq!(moved.len(), 1, "one outside push moved main");
  let subjects = main_subjects(&work).await;
  assert!(subjects.contains(&"task.txt".to_string()), "{subjects:?}");
  let tip = git_async(&work, &["rev-parse", "origin/main"]).await.expect("the tip reads");
  assert_eq!(tip, record.sha, "the landed commit is main's tip");
  let ancestry = Command::new("git")
    .arg("-C")
    .arg(&work)
    .args(["merge-base", "--is-ancestor", &moved[0], &record.sha])
    .status()
    .expect("git runs");
  assert!(ancestry.success(), "the outside commit stays under the landing");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_pr_merge_takes_the_turn_directly_and_completes_from_the_queue() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let site = land_queue::Site { worktree: work.clone() };
  let head = head(&work);
  let direct_marker = root.path().join("direct-marker");
  let direct_cmd = format!("printf '%s' \"$CHARLIEBOT_LAND_TURN\" > {}", direct_marker.display());
  let (direct_owner, _log) = stub_owner(root.path(), &work, SESSION_DIRECT);
  let gate = land_queue::pr_merge_gate(&site, Some(&direct_owner), 601, &head, Some(&direct_cmd), None, CADENCE, 1)
    .await
    .expect("the gate answers");
  assert_eq!(gate, land_queue::MergeGate::Ran(0));
  assert_eq!(fs::read_to_string(&direct_marker).expect("the marker reads"), "direct pr 601");
  assert!(site.read().await.expect("the queue reads").turn.is_none(), "the direct merge released the turn");

  // With the turn held, the same call queues and its wake completes the merge.
  let mut holder = land_queue::Claim::direct(&site, 602, &head);
  assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
  let queued_marker = root.path().join("queued-marker");
  let queued_cmd = format!("printf '%s' \"$CHARLIEBOT_LAND_TURN\" > {}", queued_marker.display());
  let (queued_owner, _log) = stub_owner(root.path(), &work, SESSION_QUEUED);
  let gate = land_queue::pr_merge_gate(&site, Some(&queued_owner), 601, &head, Some(&queued_cmd), None, CADENCE, 5)
    .await
    .expect("the gate answers");
  assert_eq!(gate, land_queue::MergeGate::Queued);
  let view = site.read().await.expect("the queue reads");
  let queued_key = land_queue::owner_token(SESSION_QUEUED).expect("the session id names a token");
  let (entry, _) = view.entry_of(601, &queued_key).expect("the queued merge entered the queue").clone();
  let mut runner = Box::pin(land_queue::run_pr_merge_queued(&site, &queued_owner, 601, &queued_cmd, CADENCE));
  tokio::select! {
    result = &mut runner => panic!("the merge ran before the turn was free: {:?}", result.err().map(|error| format!("{error:#}"))),
    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
  }
  holder.release().await.expect("the holder releases");
  assert_eq!(runner.await.expect("the queued merge ends"), 0);
  assert_eq!(fs::read_to_string(&queued_marker).expect("the marker reads"), entry.refname());
  let view = site.read().await.expect("the queue reads");
  assert!(view.turn.is_none() && view.entries.is_empty(), "the queued merge released the turn and its entry");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_pr_merge_replaces_its_entry_when_the_head_changes() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let site = land_queue::Site { worktree: work.clone() };
  let old_head = head(&work);
  let (owner, log) = stub_owner(root.path(), &work, SESSION_REPLACEMENT);
  let key = owner.token().expect("the session id names a token");
  let mut blocker = land_queue::Claim::direct(&site, 602, &old_head);
  assert_eq!(blocker.take_turn().await.expect("the blocker takes the turn"), land_queue::Take::Taken);

  let first_ms = 1_795_000_000_000;
  assert_eq!(
    land_queue::pr_merge_gate(&site, Some(&owner), 601, &old_head, Some("true"), None, CADENCE, first_ms)
      .await
      .expect("the first merge queues"),
    land_queue::MergeGate::Queued
  );
  let first_view = site.read().await.expect("the first entry reads");
  let (first_name, _) = first_view.entry_of(601, &key).expect("the first entry belongs to its owner").clone();

  commit(&work, "second.txt", "second\n");
  let new_head = head(&work);
  assert_eq!(
    land_queue::pr_merge_gate(&site, Some(&owner), 601, &new_head, Some("true"), None, CADENCE, first_ms + 1,)
      .await
      .expect("the changed head replaces the entry"),
    land_queue::MergeGate::Queued
  );
  let view = site.read().await.expect("the replacement entry reads");
  let (name, beat) = view.entry_of(601, &key).expect("the replacement entry belongs to its owner");
  assert_eq!(name.ci_passed_ms, first_ms + 1, "the replacement uses the new head's CI time");
  assert_eq!(beat.parent, new_head, "the replacement points at the new head");
  assert_ne!(name.refname(), first_name.refname(), "the replacement has a new queue key");
  let program_log = fs::read_to_string(log).expect("the wake log reads");
  assert!(program_log.contains("trigger cancel"), "the old wake was cancelled: {program_log}");

  blocker.release().await.expect("the blocker releases the turn");
  land_queue::release(&site, &key, 601).await.expect("the replacement entry is removed");
  let view = site.read().await.expect("the queue reads after release");
  assert!(view.turn.is_none() && view.entries.is_empty(), "both queue refs were released");
}

#[tokio::test]
#[ignore = "integration"]
async fn a_refused_release_keeps_its_refs_and_reports_the_failure() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let site = land_queue::Site { worktree: work.clone() };
  let sha = head(&work);
  let key = land_queue::owner_token(SESSION_REFUSED).expect("the session id names a token");
  let name = land_queue::EntryName::new(1_795_000_000_000, 701, &key);
  let mut claim = land_queue::Claim::enter(&site, &name, &sha).await.expect("the entry is created");
  assert_eq!(claim.take_turn().await.expect("the claim takes the turn"), land_queue::Take::Taken);

  let patterns = root.path().join("land-queue-leak-scan/patterns.txt");
  let mut local_patterns = fs::read_to_string(&patterns).expect("the local pattern file reads");
  local_patterns.push_str(&format!("{}\n", name.refname()));
  fs::write(&patterns, local_patterns).expect("the holder pattern is added");

  let error = claim.release().await.expect_err("the scanner refuses the release push");
  let message = format!("{error:#}");
  assert!(message.contains("could not release the landing queue refs"), "{message}");
  assert!(message.contains("pre-receive hook declined"), "{message}");
  let view = site.read().await.expect("the queue reads after the refused release");
  assert!(view.turn.is_some() && view.entry_of(701, &key).is_some(), "the refused push leaves both refs in place");
}

/// The queue fixture scans every received ref name and newly received commit message. This full cycle covers the
/// landing, queued merge, direct merge and round paths against one origin.
#[tokio::test]
#[ignore = "integration"]
async fn a_full_queue_cycle_pushes_only_text_the_leak_scan_passes() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  synthetic_queue_repo(root.path(), "main");
  let land = task_clone(root.path(), "task-a");
  let merge = task_clone(root.path(), "merge-work");
  let site_land = land_queue::Site { worktree: land.clone() };
  let site_merge = land_queue::Site { worktree: merge.clone() };
  let base = git_async(&land, &["rev-parse", "origin/main"]).await.expect("origin's main reads");
  let sha_land = head(&land);
  let sha_merge = head(&merge);
  let (owner_land, _) = stub_owner(root.path(), &land, "5e551011-6f0a-4c1d-9b2e-0a1b2c3d4e5f");
  let (owner_merge, _) = stub_owner(root.path(), &merge, "0d0e1e2a-7b3c-4d4e-8f5a-6b7c8d9e0f1a");
  let ms = 1_795_000_000_000;

  // The land path: the entry joins the queue under the session's token, which takes the turn.
  let land_name = land_queue::EntryName::new(ms, 101, &owner_land.token().expect("the session names a token"));
  let mut land_claim =
    land_queue::Claim::enter_or_resume(&site_land, &land_name, &sha_land).await.expect("the land path enqueues");
  assert_eq!(land_claim.take_turn().await.expect("the turn is taken"), land_queue::Take::Taken);

  // The pr-merge path queues behind the held turn, and its wake carries no session id.
  let gate =
    land_queue::pr_merge_gate(&site_merge, Some(&owner_merge), 104, &sha_merge, Some("true"), None, CADENCE, ms + 1)
      .await
      .expect("the merge gate queues");
  assert_eq!(gate, land_queue::MergeGate::Queued, "the held turn queues the merge");

  // The landing beats, publishes main with its turn and entry removed, and the queued merge runs from the turn.
  assert_eq!(land_claim.beat(&sha_land).await.expect("the heartbeat pushes"), land_queue::Pushed::Done);
  let publish = land_claim.publish_changes("refs/heads/main", &base, &sha_land);
  assert_eq!(site_land.push(&publish).await.expect("the publish pushes"), land_queue::Pushed::Done);
  land_claim.landed();
  let merged = land_queue::run_pr_merge_queued(&site_merge, &owner_merge, 104, "true", CADENCE)
    .await
    .expect("the queued merge runs");
  assert_eq!(merged, 0, "the queued merge exits 0");

  // A free queue merges directly, and a round acquires and releases under its launch id.
  let direct =
    land_queue::pr_merge_gate(&site_merge, Some(&owner_merge), 105, &sha_merge, Some("true"), None, CADENCE, 1)
      .await
      .expect("the direct merge runs");
  assert_eq!(direct, land_queue::MergeGate::Ran(0));
  let round = land_queue::acquire(
    &site_merge,
    "20261127T083000Z-12345-23456",
    106,
    &sha_merge,
    ms,
    Duration::from_secs(60),
    CADENCE,
  )
  .await
  .expect("the round acquires the turn");
  assert_eq!(round, land_queue::Acquired::Held);
  land_queue::release(&site_merge, "20261127T083000Z-12345-23456", 106).await.expect("the round releases");

  assert!(site_land.read().await.expect("the queue reads").turn.is_none(), "the cycle released the turn");
  let main = git_async(&land, &["ls-remote", "origin", "refs/heads/main"]).await.expect("main reads");
  assert!(main.starts_with(&sha_land), "the landing published main: {main}");
}

#[tokio::test]
#[ignore = "integration"]
async fn main_that_keeps_moving_past_five_rounds_releases_the_turn() {
  let root = tempfile::tempdir().expect("temporary directory is available");
  gated_repo(root.path(), "main");
  let work = task_clone(root.path(), "task");
  let remote_work = task_clone(root.path(), "remote-work");
  let home = root.path().join("home");
  fs::create_dir_all(&home).expect("the home is created");
  let ctx = venue(&work, "task", SESSION_H);
  let sha = head(&work);
  write_landing_record(&ctx.session_dir, &queue_record(701, &sha)).expect("the landing record is written");
  let mut remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
  // Every CI read moves main, so every round's leased main push is refused.
  remote.moves_main_at("", 0);
  let error = match queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60)).await
  {
    Ok(_) => panic!("the landing gives up after five rounds"),
    Err(error) => error,
  };
  assert!(format!("{error:#}").contains("kept moving past 5"), "{error:#}");
  let view = land_queue::Site { worktree: work.clone() }.read().await.expect("the queue reads");
  assert!(view.turn.is_none() && view.entries.is_empty(), "the give-up released the turn and the entry");
}
