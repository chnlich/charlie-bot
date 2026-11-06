//! The switch's delegation check (delegation_check of scripts/switch-server.sh), run for real against a stand-in
//! server that answers the delegate, task and archive routes with the behavior each test gives. The check runs on
//! the test's runtime clone, so the worktree the server makes is a real worktree of it, and each test can hold the
//! check to leaving no worktree and no branch behind. The delegation wait and the probe cancel wait are one second.

use std::fs;

use charliebot_shell_harness::Run;
use serde_json::{Value, json};

use super::world::{World, fake_charliebot, replace_charliebot, write_behavior};

const DELEGATE: &str = "POST /api/internal/delegate";
const DELEGATION_CANCEL: &str = "POST /api/sessions/delegation-child/runs/task-run/cancel";
const DELEGATION_ARCHIVE: &str = "POST /api/sessions/delegation-session/archive";

/// Starts the old release on a pane, gives the new release's stand-in `behavior`, and runs the switch to the new
/// release. The delegation wait is two seconds, so a check that never passes fails within the test.
fn switch_to_new_release(world: &mut World, behavior: &[(&str, Value)]) -> Run {
  world.env.insert("CHARLIEBOT_SWITCH_DELEGATION_WAIT_S".to_string(), "2".to_string());
  let pane = world.start_pane();
  world.start_server(&pane);
  let new_sha = world.new_sha.clone();
  write_behavior(&world.releases.join(&new_sha), behavior);
  world.switch(&["rust", &new_sha])
}

/// What the check left in the clone: a worktree beyond the clone or a branch beyond main. A switch leaves the clone on
/// a detached HEAD, which `git branch` lists as a pseudo-branch, so that line is not a leftover.
fn task_left_behind(world: &World) -> Vec<String> {
  world.left_behind().into_iter().filter(|item| !item.starts_with("(HEAD detached")).collect()
}

/// The requests the stand-in of the `sha` release served past its version probe, as its calls.log lists them.
fn release_calls(world: &World, sha: &str) -> Vec<String> {
  fs::read_to_string(world.releases.join(sha).join("calls.log"))
    .expect("the release's calls.log")
    .lines()
    .map(String::from)
    .collect()
}

#[test]
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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

#[test]
#[ignore = "integration"]
fn test_switch_passes_the_delegation_on_the_first_command_while_the_run_still_runs() {
  // The worker's first command succeeds in its worktree, then the run sends nothing more and does not end. The check
  // passes on that command and stops the run it passed on.
  let mut world = World::new();
  let run = switch_to_new_release(
    &mut world,
    &[("task_state", json!("running")), ("first_command", json!("ok")), ("cancel", json!("settles"))],
  );

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(world.report(&run).contains("delegation: first command succeeded in "), "{}", world.report(&run));
  let new_sha = world.new_sha.clone();
  assert!(release_calls(&world, &new_sha).contains(&DELEGATION_CANCEL.to_string()), "{}", world.report(&run));
  assert!(world.serves_release(&new_sha), "{}", world.report(&run));
  assert_eq!(task_left_behind(&world), Vec::<String>::new(), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_switch_restores_when_the_first_tool_result_is_a_failed_command() {
  // A command that errored is not a pass, even though the run goes on; the check fails at its wait's end.
  let mut world = World::new();
  let run = switch_to_new_release(&mut world, &[("task_state", json!("running")), ("first_command", json!("failed"))]);

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(
    world.report(&run).contains(
      "delegation: step run failed: run task-run did not end within 2s; its first tool result was command-failed in "
    ),
    "{}",
    world.report(&run)
  );
  world.assert_untouched();
  assert_eq!(task_left_behind(&world), Vec::<String>::new(), "{}", world.report(&run));
}

#[test]
#[ignore = "integration"]
fn test_switch_restores_when_the_first_tool_result_is_not_a_command() {
  // A first tool result from another tool is not a command, so the check does not pass on it either.
  let mut world = World::new();
  let run =
    switch_to_new_release(&mut world, &[("task_state", json!("running")), ("first_command", json!("not-command"))]);

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(
    world.report(&run).contains(
      "delegation: step run failed: run task-run did not end within 2s; its first tool result was not-command in "
    ),
    "{}",
    world.report(&run)
  );
  world.assert_untouched();
}

#[test]
#[ignore = "integration"]
fn test_switch_restores_when_the_run_makes_no_tool_call_and_names_its_backend() {
  // A worker that never calls a tool leaves its log without a result, so the check names the backend whose log that is.
  let mut world = World::new();
  let run = switch_to_new_release(&mut world, &[("task_state", json!("running")), ("first_command", json!("none"))]);

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(
    world.report(&run).contains(
      "delegation: step run failed: run task-run did not end within 2s; backend fake-backend's event log holds no init \
       cwd or no tool result"
    ),
    "{}",
    world.report(&run)
  );
  world.assert_untouched();
}

#[test]
#[ignore = "integration"]
fn test_switch_reads_with_the_current_release_when_the_target_lacks_a_read() {
  // A release built between the switch-helper group and run-first-command still works as a switch's target: its
  // binary carries the group but not that read, so the switch reads with the binary of the release current names.
  let mut world = World::new();
  let band = format!(
    "if [ \"$1\" = switch-helper ] && [ \"$2\" = run-first-command ]; then\n  \
     echo 'unknown switch helper' >&2\n  exit 2\nfi\nexec {} \"$@\"\n",
    fake_charliebot().display()
  );
  replace_charliebot(&world.releases.join(&world.new_sha), &band);

  let run = switch_to_new_release(&mut world, &[]);

  assert_eq!(run.code, 0, "{}", world.report(&run));
  assert!(world.report(&run).contains("SWITCH COMPLETE"), "{}", world.report(&run));
  assert!(world.serves_release(&world.new_sha), "{}", world.report(&run));
  assert_eq!(task_left_behind(&world), Vec::<String>::new(), "{}", world.report(&run));
}
