use std::collections::HashSet;

use super::*;
use serde_json::json;

fn workflow(path: &str, body: &str) -> (String, String) {
  (path.to_string(), body.to_string())
}

fn run(path: &str, status: &str, conclusion: Option<&str>) -> Value {
  let mut run = json!({ "path": path, "event": "pull_request", "status": status, "name": "test" });
  if let Some(conclusion) = conclusion {
    run["conclusion"] = json!(conclusion);
  }
  run
}

#[test]
fn a_removed_line_is_left_out_of_the_added_text() {
  let patch = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old fictionalmarker\n+new text\n";
  assert_eq!(added_text(patch), "diff --git a/x b/x\n+++ b/x\nnew text\n");
}

#[test]
fn one_commit_supplies_its_subject_and_trimmed_body() {
  let commits = parse_commit_messages("Add the route\n\nExplain the contract.\n\0");
  let text = resolve_pr_text(&commits, &LandOptions::default()).expect("one commit resolves");
  assert_eq!(text.title, "Add the route");
  assert_eq!(text.body, "Explain the contract.");
}

#[test]
fn several_commits_need_both_an_explicit_title_and_body() {
  let commits =
    parse_commit_messages("Add the route\n\nExplain the contract.\n\0Handle errors\n\nName the failure.\n\0");
  let refusal = resolve_pr_text(&commits, &LandOptions::default()).expect_err("two commits need explicit text");
  let refusal = format!("{refusal:#}");
  assert!(refusal.contains("2 commits"), "{refusal}");
  assert!(refusal.contains("--title"), "{refusal}");
  assert!(refusal.contains(&pr_text_genre_path()), "{refusal}");
  let title_only = LandOptions { title: Some("Routing".to_string()), body: None, evidence_dir: None };
  assert!(resolve_pr_text(&commits, &title_only).is_err());
  let body_only = LandOptions { title: None, body: Some("Why.".to_string()), evidence_dir: None };
  assert!(resolve_pr_text(&commits, &body_only).is_err());
  let explicit = LandOptions { title: Some("Routing".to_string()), body: Some("Why.".to_string()), evidence_dir: None };
  assert_eq!(
    resolve_pr_text(&commits, &explicit).expect("explicit text resolves"),
    ResolvedPrText { title: "Routing".to_string(), body: "Why.".to_string() }
  );
}

#[test]
fn explicit_pr_text_replaces_commit_derived_values() {
  let options = LandOptions {
    title: Some("Resolved title".to_string()),
    body: Some("Resolved body".to_string()),
    evidence_dir: None,
  };
  let text = resolve_pr_text(&[], &options).expect("explicit values need no commit range");
  assert_eq!(text, ResolvedPrText { title: "Resolved title".to_string(), body: "Resolved body".to_string() });
}

#[test]
fn an_empty_explicit_title_fails_validation() {
  let options = LandOptions { title: Some(" \t\n".to_string()), body: None, evidence_dir: None };
  assert!(options.validate().is_err());
  assert!(resolve_pr_text(&[], &options).is_err());
}

#[test]
fn an_evidence_directory_that_is_not_a_directory_fails_validation() {
  let dir = tempfile::tempdir().expect("temporary directory is available");
  let missing = LandOptions { evidence_dir: Some(dir.path().join("absent")), ..LandOptions::default() };
  assert!(missing.validate().is_err());
  let present = LandOptions { evidence_dir: Some(dir.path().to_path_buf()), ..LandOptions::default() };
  present.validate().expect("an existing directory passes");
}

#[test]
fn a_missing_commit_title_fails_when_the_range_is_empty() {
  assert!(resolve_pr_text(&[], &LandOptions::default()).is_err());
}

#[test]
fn a_green_sha_passes_when_every_expected_workflow_completed_successfully() {
  let expected = [".github/workflows/ci.yml".to_string(), ".github/workflows/integration.yml".to_string()];
  let runs = json!([
    run(".github/workflows/ci.yml", "completed", Some("success")),
    run(".github/workflows/integration.yml", "completed", Some("success")),
  ]);
  let check_runs = json!([
    { "name": "rust-test", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:00:00Z" },
    { "name": "lint", "status": "completed", "conclusion": "skipped", "completed_at": "2026-11-10T03:01:00Z" },
  ]);
  let statuses = json!([{ "context": "docs", "state": "success" }]);
  let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 300);
  assert_eq!(verdict, CiVerdict::Pass);
}

#[test]
fn a_failed_check_names_itself_and_its_url() {
  let expected = [".github/workflows/ci.yml".to_string()];
  let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
  let check_runs = json!([
    { "name": "rust-test", "status": "completed", "conclusion": "failure", "html_url": "https://runs/1" },
  ]);
  let statuses = json!([]);
  let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 300);
  assert_eq!(
    verdict,
    CiVerdict::Failed {
      failing: vec![CheckRow {
        name: "rust-test".to_string(),
        conclusion: Some("failure".to_string()),
        url: Some("https://runs/1".to_string()),
        completed_at: None,
      }]
    }
  );
}

#[test]
fn a_running_check_waits_and_times_out_after_ninety_minutes() {
  let expected = [".github/workflows/ci.yml".to_string()];
  let runs = json!([run(".github/workflows/ci.yml", "in_progress", None)]);
  let check_runs = json!([]);
  let statuses = json!([]);
  assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
  assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, TIMEOUT_SECS + 1), CiVerdict::Timeout);
}

#[test]
fn no_check_at_all_waits_until_the_not_started_clock_expires() {
  let expected = [".github/workflows/ci.yml".to_string()];
  let runs = json!([]);
  let check_runs = json!([]);
  let statuses = json!([]);
  assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
  assert_eq!(
    judge_ci(&expected, &runs, &check_runs, &statuses, NOT_STARTED_SECS + 1),
    CiVerdict::NotStarted { workflow: ".github/workflows/ci.yml".to_string() }
  );
}

#[test]
fn one_expected_workflow_not_yet_registered_is_not_started_past_ten_minutes() {
  let expected = [".github/workflows/ci.yml".to_string(), ".github/workflows/integration.yml".to_string()];
  let runs = json!([
    run(".github/workflows/ci.yml", "completed", Some("success")),
    { "path": ".github/workflows/ci.yml", "event": "push", "status": "completed", "conclusion": "success" },
  ]);
  let check_runs = json!([{ "name": "rust-test", "status": "completed", "conclusion": "success" }]);
  let statuses = json!([]);
  assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
  assert_eq!(
    judge_ci(&expected, &runs, &check_runs, &statuses, NOT_STARTED_SECS + 1),
    CiVerdict::NotStarted { workflow: ".github/workflows/integration.yml".to_string() }
  );
}

#[test]
fn a_failing_commit_status_fails_the_sha() {
  let expected = [".github/workflows/ci.yml".to_string()];
  let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
  let check_runs = json!([]);
  let statuses = json!([{ "context": "coverage", "state": "error", "target_url": "https://ci/coverage" }]);
  let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 60);
  assert_eq!(
    verdict,
    CiVerdict::Failed {
      failing: vec![CheckRow {
        name: "coverage".to_string(),
        conclusion: Some("error".to_string()),
        url: Some("https://ci/coverage".to_string()),
        completed_at: None,
      }]
    }
  );
}

#[test]
fn only_unfiltered_pull_request_workflows_are_expected() {
  let workflows = [
    workflow(
      ".github/workflows/ci.yml",
      "name: CI\non:\n  pull_request:\n  push:\n    branches:\n      - main\njobs: {}\n",
    ),
    workflow(
      ".github/workflows/docs.yml",
      "name: Docs\non:\n  pull_request:\n    paths:\n      - 'docs/**'\njobs: {}\n",
    ),
    workflow(".github/workflows/nightly.yml", "name: Nightly\non:\n  schedule:\n    - cron: '0 3 * * *'\njobs: {}\n"),
    workflow(".github/workflows/triage.yml", "name: Triage\non: [push, workflow_dispatch]\njobs: {}\n"),
    workflow(".github/workflows/legacy.yml", "name: Legacy\ntriggers: [push]\njobs: {}\n"),
  ];
  assert_eq!(expected_workflow_paths(&workflows).unwrap(), vec![".github/workflows/ci.yml".to_string()]);
  // A filtered pull_request trigger still declares the trigger; a workflow without one does not.
  for (path, content) in &workflows[..2] {
    assert!(pull_request_trigger_of(path, content).unwrap().is_some());
  }
  for (path, content) in &workflows[2..] {
    assert!(pull_request_trigger_of(path, content).unwrap().is_none());
  }
}

#[test]
fn a_workflow_that_does_not_parse_fails_the_gate_instead_of_dropping_out() {
  let broken = [workflow(".github/workflows/ci.yml", "on: [pull_request\n")];
  assert!(expected_workflow_paths(&broken).is_err());
  assert!(pull_request_trigger_of(".github/workflows/ci.yml", "on: [pull_request\n").is_err());
}

#[test]
fn a_pending_commit_status_waits_instead_of_failing_the_sha() {
  let expected = [".github/workflows/ci.yml".to_string()];
  let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
  let statuses = json!([{ "context": "coverage", "state": "pending" }]);
  assert_eq!(judge_ci(&expected, &runs, &json!([]), &statuses, 60), CiVerdict::Waiting);
  assert_eq!(judge_ci(&expected, &runs, &json!([]), &statuses, TIMEOUT_SECS + 1), CiVerdict::Timeout);
}

#[test]
fn an_unfiltered_null_or_listed_pull_request_trigger_is_not_filtered() {
  assert_eq!(
    pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: pull_request").unwrap()),
    Some(false)
  );
  assert_eq!(
    pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on:\n  pull_request:\njobs: {}").unwrap()),
    Some(false)
  );
  assert_eq!(
    pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: [push, pull_request]").unwrap()),
    Some(false)
  );
  assert_eq!(
    pull_request_trigger(
      &serde_yaml::from_str::<serde_yaml::Value>("on:\n  pull_request:\n    branches-ignore:\n      - wip-*").unwrap()
    ),
    Some(true)
  );
  assert_eq!(pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: push").unwrap()), None);
}

fn record(state: &str, sha: &str, head_pushed_at: &str, rebase_retries: i64) -> LandingRecord {
  LandingRecord {
    state: state.to_string(),
    repo: "owner/repo".to_string(),
    base: "main".to_string(),
    sha: sha.to_string(),
    pr_url: None,
    checks: Vec::new(),
    head_pushed_at: Some(head_pushed_at.to_string()),
    ci_passed_at: None,
    pushed_at: None,
    rebase_retries,
    queue_ref: None,
  }
}

#[test]
fn a_rebase_that_rewrote_the_head_restarts_both_clocks_and_counts_a_retry() {
  let prev = record(STATE_WAITING, "a".repeat(40).as_str(), "2026-11-10T01:00:00Z", 2);
  // The same head sha keeps the previous round's push time and retry count.
  let sha = "a".repeat(40);
  assert_eq!(carried_pushed_at(Some(&prev), &sha, "2026-11-10T02:00:00Z"), Some("2026-11-10T01:00:00Z".to_string()));
  // A rebase's rewrite starts both clocks again and counts one retry; the caller asks only
  // about a rebase that rewrote the head, so a round without one never reaches the question.
  let new_sha = "b".repeat(40);
  assert_eq!(
    carried_pushed_at(Some(&prev), &new_sha, "2026-11-10T02:00:00Z"),
    Some("2026-11-10T02:00:00Z".to_string())
  );
  assert!(rewrite_counts_as_retry(Some(&prev), true));
  // The first rebase of a task without a previous round is the initial base, not a retry.
  assert!(!rewrite_counts_as_retry(None, true));
  // A terminal record is dropped before this point, but a fresh round over one keeps its retries.
  let landed = record(STATE_LANDED, new_sha.as_str(), "2026-11-10T01:30:00Z", 3);
  assert_eq!(
    carried_pushed_at(Some(&landed), new_sha.as_str(), "2026-11-10T04:00:00Z"),
    Some("2026-11-10T01:30:00Z".to_string())
  );
}

#[test]
fn push_rejections_split_a_verified_move_from_the_terminal_classes() {
  // Repository rules keep their pr-merge-required answer.
  assert_eq!(
    push_rejection_kind("remote: error: GH013: Repository rule violations found for refs/heads/main"),
    PushRejection::RepositoryRules
  );
  assert_eq!(
    push_rejection_kind("! [remote rejected] main -> main (repository rules prohibit push)"),
    PushRejection::RepositoryRules
  );
  // Only git's own report of a stale remote tip classifies as a possible base move.
  assert_eq!(
    push_rejection_kind(
      "To github.com:owner/repo.git\n ! [rejected] abc123 -> main (non-fast-forward)\nerror: failed to push some refs"
    ),
    PushRejection::NonFastForward
  );
  assert_eq!(
    push_rejection_kind(
      "To github.com:owner/repo.git\n ! [rejected] abc123 -> main (fetch first)\nerror: failed to push some refs"
    ),
    PushRejection::NonFastForward
  );
  // A lease check that found the ref already moved names the same possible base move.
  assert_eq!(
    push_rejection_kind(
      "remote: error: cannot lock ref 'refs/heads/main': is at 1111111 but expected 2222222\n\
         To https://github.com/owner/repo.git\n ! [remote rejected] abc123 -> main (cannot lock ref)\n\
         error: failed to push some refs"
    ),
    PushRejection::NonFastForward
  );
  // A hook refusal, an SSH reset, a timeout and an auth error each carry their own class.
  let hook = "pre-push: push to dev is restricted; use merge_to_dev.sh <branch>\n\
                error: failed to push some refs to 'gitee.com:owner/repo.git'";
  assert_eq!(push_rejection_kind(hook), PushRejection::Refused);
  assert_eq!(
    push_rejection_kind(
      "remote: push to main is restricted; use merge_to_dev.sh <branch>\nTo ../origin.git\n\
         ! [remote rejected] abc123 -> main (pre-receive hook declined)\n\
         error: failed to push some refs to '../origin.git'"
    ),
    PushRejection::Refused
  );
  // A ref-lock failure without the lease wording is a local or hook lock conflict, not a move.
  assert_eq!(
    push_rejection_kind(
      "remote: error: cannot lock ref 'refs/heads/main': Unable to create 'refs/heads/main.lock': File exists"
    ),
    PushRejection::Refused
  );
  assert_eq!(
    push_rejection_kind(
      "kex_exchange_identification: read: Connection reset by peer\nfatal: Could not read from remote repository."
    ),
    PushRejection::Transport
  );
  assert_eq!(push_rejection_kind("git push timed out after 120.0s"), PushRejection::Timeout);
  assert_eq!(
    push_rejection_kind("ssh: connect to host gitee.com port 22: Connection timed out"),
    PushRejection::Timeout
  );
  assert_eq!(
    push_rejection_kind("git@gitee.com: Permission denied (publickey).\nfatal: Could not read from remote repository."),
    PushRejection::Auth
  );
}

#[test]
fn landing_states_carry_distinct_exit_codes_with_the_landings_at_zero() {
  let states = [
    (STATE_LANDED, 0),
    (STATE_LANDED_NO_CI, 0),
    (STATE_WAITING, 8),
    (STATE_REBASE_CONFLICT, 3),
    (STATE_CI_FAILED, 4),
    (STATE_CI_NOT_STARTED, 5),
    (STATE_CI_TIMEOUT, 6),
    (STATE_PR_MERGE_REQUIRED, 7),
  ];
  let mut codes = Vec::new();
  for (state, expected) in states {
    let code = LandState::try_from(state).expect("state parses").exit_code();
    assert_eq!(code, expected, "state {state}");
    codes.push(code);
  }
  let nonzero: HashSet<i32> = codes.into_iter().filter(|code| *code != 0).collect();
  assert_eq!(nonzero.len(), 6);
}

#[test]
fn the_landing_record_round_trips_through_landing_json() {
  let session = tempfile::tempdir().expect("temporary directory is available");
  let record = LandingRecord {
    state: STATE_LANDED.to_string(),
    repo: "owner/repo".to_string(),
    base: "main".to_string(),
    sha: "a".repeat(40),
    pr_url: Some("https://github.com/owner/repo/pull/7".to_string()),
    checks: vec![CheckRow {
      name: "rust-test".to_string(),
      conclusion: Some("success".to_string()),
      url: Some("https://runs/1".to_string()),
      completed_at: Some("2026-11-10T03:00:00Z".to_string()),
    }],
    head_pushed_at: Some("2026-11-10T02:00:00Z".to_string()),
    ci_passed_at: Some("2026-11-10T03:01:00Z".to_string()),
    pushed_at: Some("2026-11-10T03:02:00Z".to_string()),
    rebase_retries: 1,
    queue_ref: None,
  };
  write_landing_record(session.path(), &record).expect("the record is written");
  assert_eq!(read_landing_record(session.path()).expect("the record is read"), Some(record));
}

#[test]
fn a_malformed_landing_record_is_an_error_and_a_missing_one_is_none() {
  let session = tempfile::tempdir().expect("temporary directory is available");
  assert_eq!(read_landing_record(session.path()).expect("no record reads as none"), None);
  std::fs::write(session.path().join(LANDING_FILE_NAME), "{not json").expect("the malformed record is written");
  assert!(read_landing_record(session.path()).is_err());
}

#[test]
fn the_ci_passed_at_is_the_last_completed_check() {
  let evaluation = CiEvaluation {
    runs: Value::Array(Vec::new()),
    check_runs: json!([
      { "name": "a", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:00:00Z" },
      { "name": "b", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:04:00Z" },
    ]),
    statuses: json!([]),
  };
  assert_eq!(evaluation.ci_passed_at(), Some("2026-11-10T03:04:00Z".to_string()));
}

#[test]
fn the_remote_url_names_the_owner_repo_path() {
  assert_eq!(git::repo_from_remote_url("https://github.com/owner/repo.git"), Some("owner/repo".to_string()));
  assert_eq!(git::repo_from_remote_url("git@github.com:owner/repo.git"), Some("owner/repo".to_string()));
  assert_eq!(git::repo_from_remote_url("ssh://git@github.com/owner/repo"), Some("owner/repo".to_string()));
  assert_eq!(git::repo_from_remote_url("ssh://git@github.com:22/owner/repo.git"), Some("owner/repo".to_string()));
  assert_eq!(git::repo_from_remote_url("https://github.com/owner/repo/"), Some("owner/repo".to_string()));
  assert_eq!(git::repo_from_remote_url("not-a-remote"), None);
}

/// The check reads each changed file that still exists, and a changed Rust file's whole crate, so a `cfg(test)`
/// module declared in an unchanged parent is test code. A file the diff removed is left out.
#[test]
fn the_literal_check_reads_the_changed_files_and_the_crates_of_changed_rust_files() {
  use std::fs;
  let root = tempfile::tempdir().unwrap();
  let worktree = root.path();
  let crate_dir = worktree.join("rust/crates/x/y");
  fs::create_dir_all(crate_dir.join("src")).unwrap();
  fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
  fs::write(crate_dir.join("src/lib.rs"), "").unwrap();
  fs::write(crate_dir.join("src/extra.rs"), "").unwrap();
  fs::create_dir_all(worktree.join("scripts")).unwrap();
  fs::write(worktree.join("scripts/run.sh"), "").unwrap();
  fs::write(worktree.join("scripts/notes.txt"), "").unwrap();
  let changed =
    ["rust/crates/x/y/src/extra.rs".to_string(), "scripts/run.sh".to_string(), "scripts/removed.sh".to_string()];
  assert_eq!(literal_check_paths(worktree, &changed).unwrap(), vec![crate_dir, worktree.join("scripts/run.sh")]);
  // A path under a regular file fails to stat with something other than "not found": that is an error, not a skip.
  assert!(literal_check_paths(worktree, &["scripts/notes.txt/run.sh".to_string()]).is_err());
}

/// A changed file that holds a backend literal refuses the landing and names its file and line; once the literal is
/// gone the same file passes.
#[test]
fn a_backend_literal_in_a_changed_file_refuses_the_landing_and_a_clean_file_passes() {
  use std::fs;
  let root = tempfile::tempdir().unwrap();
  let worktree = root.path();
  fs::create_dir_all(worktree.join("scripts")).unwrap();
  fs::write(worktree.join("scripts/run.sh"), "echo ok\nmodel=claude-opus-5-5\n").unwrap();
  let paths = [worktree.join("scripts/run.sh")];
  let added = AddedLines::from([(PathBuf::from("scripts/run.sh"), BTreeSet::from([2]))]);
  let text = format!(
    "{:#}",
    refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths, &added).unwrap_err()
  );
  assert!(text.contains("scripts/run.sh:2") && text.contains("claude-opus-5-5"), "{text}");

  fs::write(worktree.join("scripts/run.sh"), "echo ok\n").unwrap();
  refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths, &added)
    .expect("a clean file passes");
}
