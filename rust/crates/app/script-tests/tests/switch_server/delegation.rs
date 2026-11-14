//! The switch's delegation check (delegation_check of scripts/switch-server.sh), run for real against a stand-in
//! server that answers the delegate, task and archive routes with the behavior each test gives. The check runs on
//! the test's runtime clone, so the worktree the server makes is a real worktree of it. Each test can then hold the
//! check to leaving no worktree and no branch behind. The delegation wait and the probe cancel wait are one second.

use std::fs;
use std::path::PathBuf;

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
fn test_switch_switches_back_and_exits_2_when_the_delegation_check_fails_on_the_server_side() {
  // A failed delegation check takes the path of a failed probe turn, so the switch switches back.
  let mut world = World::new();
  let pid = world.spawn_live();

  let run = world.switch_forward(pid, "", "return 1");

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(world.root().join("restored").exists(), "{}", world.report(&run));
  assert!(run.stdout.contains("switch back: the delegation check failed"), "{}", world.report(&run));
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
fn test_switch_switches_back_when_the_first_tool_result_is_a_failed_command() {
  // A command that errored is not a pass, even though the run goes on. The check fails at its wait's end.
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
fn test_switch_switches_back_when_the_first_tool_result_is_not_a_command() {
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
fn test_switch_switches_back_when_the_run_makes_no_tool_call_and_names_its_backend() {
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

/// One gateway failure as a run's error event names it: the class the release decision keeps without a switch back.
const GATEWAY_ERROR: &str = "litellm.BadGatewayError: Bad Gateway while calling the model endpoint";

/// One agent failure before its model call, as the slice's refusal names it: the server's own class.
const SPAWN_ERROR: &str =
  "master_cc_crashed: session cgroup slice charliebot-agents.slice cannot enable cpu for its children";

#[test]
#[ignore = "integration"]
fn test_switch_keeps_the_new_release_and_exits_6_when_the_model_fails_the_delegation_twice() {
  // The delegation run ends failed with a gateway error: a model-side failure. The check is retried once, the retry
  // fails the same way, and the new release keeps serving with the exit status of its own.
  let mut world = World::new();
  let run = switch_to_new_release(
    &mut world,
    &[("task_state", json!("failed")), ("worktree", json!("missing")), ("task_error", json!(GATEWAY_ERROR))],
  );

  assert_eq!(run.code, 6, "{}", world.report(&run));
  let report = world.report(&run);
  assert!(
    report.contains(&format!(
      "SWITCH KEPT, MODEL CHECK FAILED: delegation_check failed on both attempts ({GATEWAY_ERROR}); rust {} serves \
       port {}",
      world.new_sha, world.port
    )),
    "{report}"
  );
  assert!(report.contains("delegation_check: model-side error on attempt 1"), "{report}");
  assert_eq!(release_calls(&world, &world.new_sha).iter().filter(|call| **call == DELEGATE).count(), 2, "{report}");
  assert!(world.serves_release(&world.new_sha), "{report}");
  assert_eq!(fs::read_link(world.releases.join("current")).expect("current"), PathBuf::from(&world.new_sha));
  assert_eq!(world.git(&world.clone, &["rev-parse", "HEAD"]), world.new_sha, "{report}");
  assert_eq!(task_left_behind(&world), Vec::<String>::new(), "{report}");
}

#[test]
#[ignore = "integration"]
fn test_switch_switches_back_when_the_delegation_run_fails_before_its_model_call() {
  // The same shape of failure with the agent's own error text: the server's class, so the switch switches back.
  let mut world = World::new();
  let run = switch_to_new_release(
    &mut world,
    &[("task_state", json!("failed")), ("worktree", json!("missing")), ("task_error", json!(SPAWN_ERROR))],
  );

  assert_eq!(run.code, 2, "{}", world.report(&run));
  let report = world.report(&run);
  assert!(report.contains("switch back: the delegation check failed"), "{report}");
  assert!(report.contains(&format!("switched back: rust {} serves port {}", world.old_sha, world.port)), "{report}");
  world.assert_untouched();
  assert_eq!(release_calls(&world, &world.new_sha).iter().filter(|call| **call == DELEGATE).count(), 1, "{report}");
}

#[test]
#[ignore = "integration"]
fn test_switch_keeps_the_new_release_and_exits_6_when_the_probe_fails_on_the_model_side_twice() {
  // The probe run ends failed with a rate limit: a model-side failure, retried once and then kept and reported.
  let mut world = World::new();
  let run = switch_to_new_release(
    &mut world,
    &[
      ("run_state", json!("failed")),
      ("output", json!("none")),
      ("probe_error", json!("Error code: 429: rate limited")),
    ],
  );

  assert_eq!(run.code, 6, "{}", world.report(&run));
  let report = world.report(&run);
  assert!(report.contains("SWITCH KEPT, MODEL CHECK FAILED: probe_turn failed on both attempts"), "{report}");
  assert!(world.serves_release(&world.new_sha), "{report}");
  assert_eq!(
    release_calls(&world, &world.new_sha).iter().filter(|call| **call == "POST /api/sessions/").count(),
    2,
    "{report}"
  );
}

#[test]
#[ignore = "integration"]
fn test_switch_switches_back_when_the_probe_run_fails_on_the_server_side() {
  // The probe run ends failed with no error text, and then with a spawn error. No model service is named, so the
  // switch treats the failure as the server's and switches back on the first attempt.
  let mut world = World::new();
  let run = switch_to_new_release(
    &mut world,
    &[("run_state", json!("failed")), ("output", json!("none")), ("probe_error", json!(SPAWN_ERROR))],
  );

  assert_eq!(run.code, 2, "{}", world.report(&run));
  assert!(world.report(&run).contains("switch back: the probe turn failed"), "{}", world.report(&run));
  world.assert_untouched();
  assert_eq!(
    release_calls(&world, &world.new_sha).iter().filter(|call| **call == "POST /api/sessions/").count(),
    1,
    "{}",
    world.report(&run)
  );
}

#[test]
#[ignore = "integration"]
fn test_switch_reads_with_the_current_release_when_the_target_lacks_a_read() {
  // A release built between the switch-helper group and run-first-command still works as a switch's target. Its
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
