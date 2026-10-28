//! The switch's probe turn (probe_turn of scripts/switch-server.sh), run for real against a stand-in server that
//! answers with the behavior each test gives. A test starts its own server, because the turn runs once per server:
//! a second turn would find the session of the first. Both probe waits are one second.

use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::json;

use super::world::World;
use charliebot_shell_harness::Run;

const PROBE_BOOTSTRAP: &str = "GET /api/sessions/probe-session/bootstrap";
const PROBE_CANCEL: &str = "POST /api/sessions/probe-session/runs/probe-run/cancel";
const PROBE_ARCHIVE: &str = "POST /api/sessions/probe-session/archive";
const NUDGE_CANCEL: &str = "POST /api/sessions/probe-session/runs/nudge-run/cancel";
const SLOW_LISTING: &str = "GET /api/sessions/probe-session/runs?limit=500";

#[test]
fn test_probe_passes_on_first_output_then_stops_the_run_and_archives() {
  let world = World::new();
  let (run, calls) = world.probe_turn(&[
    ("run_state", json!("running")),
    ("output", json!("message")),
    ("output_after_polls", json!(1)),
    ("cancel", json!("settles")),
  ]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(calls.contains(&PROBE_CANCEL.to_string()), "{calls:?}");
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert_stage_lines(&run);
}

#[test]
fn test_probe_passes_without_a_cancel_when_the_run_ends_success() {
  let world = World::new();
  let (run, calls) = world.probe_turn(&[("run_state", json!("success")), ("output", json!("none"))]);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(!calls.iter().any(|call| call.ends_with("/cancel")), "{calls:?}");
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert_stage_lines(&run);
}

#[test]
fn test_probe_fails_at_once_when_the_run_fails_before_any_output() {
  let world = World::new();
  let (run, calls) = world.probe_turn(&[("run_state", json!("failed")), ("output", json!("none"))]);
  assert_eq!(run.code, 1, "{}", run.describe());
  // The failed run ends the wait on its first read.
  assert_eq!(calls.iter().filter(|call| **call == PROBE_BOOTSTRAP).count(), 1, "{calls:?}");
  assert!(!calls.iter().any(|call| call.ends_with("/cancel")), "{calls:?}");
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert_stage_lines(&run);
}

#[test]
fn test_probe_fails_and_stops_the_run_when_no_output_comes_within_the_wait() {
  let world = World::new();
  let (run, calls) = world.probe_turn(&[("run_state", json!("running")), ("output", json!("none"))]);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(calls.contains(&PROBE_CANCEL.to_string()), "{calls:?}");
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert_stage_lines(&run);
}

#[test]
fn test_probe_fails_when_the_stop_does_not_settle_within_its_wait() {
  let world = World::new();
  let (run, calls) =
    world.probe_turn(&[("run_state", json!("running")), ("output", json!("tool")), ("cancel", json!("hangs"))]);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(calls.contains(&PROBE_CANCEL.to_string()), "{calls:?}");
  assert_stage_lines(&run);
}

#[test]
fn test_probe_archive_stops_the_follow_up_run_that_refuses_the_archive() {
  // The probe run ends success, then the server starts a follow-up run. The archive's 409 makes the probe stop it.
  let world = World::new();
  let (run, calls) = world.probe_turn(&[
    ("run_state", json!("success")),
    ("output", json!("none")),
    ("followup", json!("nudge")),
    ("cancel", json!("settles")),
  ]);
  assert_eq!(run.code, 0, "{}", run.describe());
  // The first archive answers 409.
  assert_eq!(calls.iter().filter(|call| **call == PROBE_ARCHIVE).count(), 2, "{calls:?}");
  let archive = calls.iter().position(|call| call == PROBE_ARCHIVE).expect("the first archive");
  let nudge = calls.iter().position(|call| call == NUDGE_CANCEL).expect("the follow-up stop");
  assert!(nudge > archive, "{calls:?}");
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert!(run.stdout.contains("probe: archive answered 409"), "{}", run.describe());
  assert!(
    stage_line(&run, r"probe: stop requested session=probe-session run=nudge-run elapsed=\d+s"),
    "{}",
    run.describe()
  );
  assert_stage_lines(&run);
}

#[test]
fn test_probe_archive_fails_naming_the_follow_up_run_that_does_not_settle() {
  // A follow-up run that the stop cannot end holds the archive: the probe fails and logs the run it waited for.
  let world = World::new();
  let (run, calls) = world.probe_turn(&[
    ("run_state", json!("success")),
    ("output", json!("none")),
    ("followup", json!("nudge")),
    ("cancel", json!("hangs")),
  ]);
  assert_eq!(run.code, 1, "{}", run.describe());
  // Refused once; the stop timed out, so there is no second archive.
  assert_eq!(calls.iter().filter(|call| **call == PROBE_ARCHIVE).count(), 1, "{calls:?}");
  assert!(
    stage_line(&run, r"probe: run did not settle within 1s session=probe-session run=nudge-run elapsed=\d+s"),
    "{}",
    run.describe()
  );
  assert_stage_lines(&run);
}

#[test]
fn test_probe_archive_does_not_cancel_an_empty_run_id() {
  // A refused message leaves no run to stop, but the probe still archives the empty session.
  let world = World::new();
  let (run, calls) = world.probe_turn(&[("message_status", json!(500))]);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(calls.last(), Some(&PROBE_ARCHIVE.to_string()), "{calls:?}");
  assert!(!calls.iter().any(|call| call.ends_with("/cancel")), "{calls:?}");
  assert!(run.stdout.contains("probe: archived session=probe-session"), "{}", run.describe());
  assert_stage_lines(&run);
}

#[test]
fn test_probe_archive_bounds_a_slow_run_list_by_its_wait() {
  // A run listing that answers only after END reports the wait timeout, not the listing's timed-out status.
  let world = World::new();
  let start = Instant::now();
  let (run, calls) =
    world.probe_turn(&[("run_state", json!("success")), ("output", json!("none")), ("archive_runs_delay", json!(2))]);
  let elapsed = start.elapsed();
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(elapsed < Duration::from_millis(1800), "the listing was not bounded: {elapsed:?}");
  assert!(calls.iter().any(|call| call.starts_with(SLOW_LISTING)), "{calls:?}");
  assert!(
    stage_line(&run, r"probe: archive did not finish within 1s session=probe-session run=- elapsed=\d+s"),
    "{}",
    run.describe()
  );
  assert_stage_lines(&run);
}

#[test]
fn test_probe_archive_names_the_run_when_its_listing_answers_after_the_wait() {
  // The wait ends while the run listing that follows the stop request is in flight: the probe reports the settle
  // timeout with the run it waited for, as the clock check at the top of a pass does.
  let world = World::new();
  let (run, calls) = world.probe_turn(&[
    ("run_state", json!("running")),
    ("output", json!("message")),
    ("cancel", json!("hangs")),
    ("archive_runs_delay", json!(2)),
    ("archive_runs_delay_after", json!(1)),
  ]);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(calls.contains(&PROBE_CANCEL.to_string()), "{calls:?}");
  assert_eq!(calls.iter().filter(|call| **call == SLOW_LISTING).count(), 2, "{calls:?}");
  assert!(
    stage_line(&run, r"probe: run did not settle within 1s session=probe-session run=probe-run elapsed=\d+s"),
    "{}",
    run.describe()
  );
  assert_stage_lines(&run);
}

/// Whether the run's stdout holds a line matching `pattern`.
fn stage_line(run: &Run, pattern: &str) -> bool {
  Regex::new(pattern).expect("the stage pattern").is_match(&run.stdout)
}

/// Each probe stage logs one line that names its session and its run and gives the elapsed seconds.
fn assert_stage_lines(run: &Run) {
  let stage_lines: Vec<&str> =
    run.stdout.lines().filter(|line| line.contains("switch: probe: ") && line.contains(" session=")).collect();
  assert!(!stage_lines.is_empty(), "{}", run.describe());
  let pattern = Regex::new(r"probe: .* session=probe-session run=\S+ elapsed=\d+s$").expect("the stage pattern");
  for line in stage_lines {
    assert!(pattern.is_match(line), "{line}\n{}", run.describe());
  }
}
