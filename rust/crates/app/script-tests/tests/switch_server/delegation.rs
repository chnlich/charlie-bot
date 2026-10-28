//! The switch's delegation check (delegation_check of scripts/switch-server.sh), run for real against a stand-in
//! server that answers the delegate, task and archive routes with the behavior each test gives. The check runs on
//! the test's runtime clone, so the worktree the server makes is a real worktree of it, and each test can hold the
//! check to leaving no worktree and no branch behind. The delegation wait and the probe cancel wait are one second.

use serde_json::json;

use super::world::World;

const DELEGATE: &str = "POST /api/internal/delegate";
const DELEGATION_CANCEL: &str = "POST /api/sessions/delegation-child/runs/task-run/cancel";
const DELEGATION_ARCHIVE: &str = "POST /api/sessions/delegation-session/archive";

#[test]
fn test_delegation_passes_when_the_run_succeeds_and_its_worktree_exists() {
  let world = World::new();

  let (run, calls) = world.delegation_check(&[("task_state", json!("success")), ("worktree", json!("created"))]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("delegation: passed: run task-run ended success; worktree "), "{}", run.describe());
  assert_eq!(calls.iter().filter(|call| **call == DELEGATE).count(), 1, "{calls:?}");
  // A run that ended needs no stop.
  assert!(!calls.iter().any(|call| call.ends_with("/cancel")), "{calls:?}");
  assert_eq!(calls.last(), Some(&DELEGATION_ARCHIVE.to_string()), "{calls:?}");
  assert_eq!(world.left_behind(), Vec::<String>::new(), "{}", run.describe());
}

#[test]
fn test_delegation_fails_with_the_run_error_and_leaves_nothing_behind() {
  let world = World::new();
  let error = "RuntimeError: git worktree add timed out after 0.3s for charliebot/task-switch-check";

  let (run, calls) = world.delegation_check(&[
    ("task_state", json!("failed")),
    ("worktree", json!("missing")),
    ("task_error", json!(error)),
  ]);

  assert_eq!(run.code, 1, "{}", run.describe());
  let failed = format!("delegation: step run failed: run task-run ended failed; the server's error: {error}");
  assert!(run.stdout.contains(&failed), "{}", run.describe());
  assert_eq!(calls.last(), Some(&DELEGATION_ARCHIVE.to_string()), "{calls:?}");
  assert_eq!(world.left_behind(), Vec::<String>::new(), "{}", run.describe());
}

#[test]
fn test_delegation_refused_by_the_server_names_the_server_answer() {
  let world = World::new();

  let (run, calls) = world.delegation_check(&[("delegate", json!("refused"))]);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stdout.contains("delegation: step delegate failed: the server answered 400:"), "{}", run.describe());
  assert!(run.stdout.contains("backend router failed"), "{}", run.describe());
  assert_eq!(calls.last(), Some(&DELEGATION_ARCHIVE.to_string()), "{calls:?}");
  assert_eq!(world.left_behind(), Vec::<String>::new(), "{}", run.describe());
}

#[test]
fn test_delegation_times_out_then_stops_the_run_and_removes_its_worktree() {
  let world = World::new();

  let (run, calls) = world.delegation_check(&[
    ("task_state", json!("running")),
    ("worktree", json!("created")),
    ("cancel", json!("settles")),
  ]);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stdout.contains("delegation: step run failed: run task-run did not end within 1s"), "{}", run.describe());
  assert!(calls.contains(&DELEGATION_CANCEL.to_string()), "{calls:?}");
  assert_eq!(calls.last(), Some(&DELEGATION_ARCHIVE.to_string()), "{calls:?}");
  assert_eq!(world.left_behind(), Vec::<String>::new(), "{}", run.describe());
}

#[test]
fn test_switch_restores_and_exits_2_when_the_delegation_check_fails() {
  // A failed delegation check takes the path of a failed probe turn, so the switch restores the previous release.
  let mut world = World::new();
  let pid = world.spawn_live();

  let run = world.switch_forward(pid, "", "return 1");

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(run.stdout.contains("switch: step failed: the delegation check failed"), "{}", world.report(&run));
  assert!(!run.stdout.contains("SWITCH COMPLETE"), "{}", world.report(&run));
}
