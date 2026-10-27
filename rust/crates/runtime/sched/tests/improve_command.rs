//! The improve loop's durable state and report-reading judgments, ported from `tests/test_improve_command.py`.

use std::path::Path;

use charliebot_core::config::{CREDENTIALS_FILENAME, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_sched::improve::command::{self, ImproveLoopAlreadyRunningError, Reservation};
use charliebot_sched::improve::judgment;
use serde_json::{Value, json};

const SESSION: &str = "synthetic-improve-session";

fn config(home: &Path) -> CharlieBotConfig {
  std::fs::write(home.join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
  Registry::new().load_config(home).expect("default test config")
}

fn reservation<'a>(goal: &'a str, work_branch: &'a str, plan: Option<&'a str>) -> Reservation<'a> {
  Reservation {
    goal,
    work_branch,
    repo_path: "/tmp/repo",
    plan,
    base_branch: Some("main"),
    merge_back: false,
    resolved_backend: "",
    resolved_model: "",
    backend_from_pool: false,
  }
}

fn assistant_text(text: &str) -> Value {
  json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
}

#[test]
// Python: tests/test_improve_command.py::test_reserve_loop_state_raises_when_session_already_has_running_loop
fn a_second_reservation_names_the_running_loop() {
  let home = tempfile::tempdir().expect("temporary home");
  let cfg = config(home.path());
  let first = command::reserve_loop_state(&cfg, SESSION, &reservation("optimize", "improve/test", None))
    .expect("the first reservation succeeds");

  let error = command::reserve_loop_state(&cfg, SESSION, &reservation("optimize", "improve/other", None))
    .expect_err("a second reservation is refused while the first runs");

  let running = error.downcast_ref::<ImproveLoopAlreadyRunningError>().expect("the refusal is typed");
  assert_eq!(running.loop_id, Some(first.loop_id));
  assert!(
    error.to_string().starts_with(&format!("Loop {} is already running", first.loop_id)),
    "the refusal names the running loop: {error}"
  );
}

#[test]
// Python: tests/test_cli_improve.py::test_session_takes_a_new_loop_after_stop
fn a_stopped_loop_releases_its_lock_for_a_new_loop_id() {
  let home = tempfile::tempdir().expect("temporary home");
  let cfg = config(home.path());
  let first = command::reserve_loop_state(&cfg, SESSION, &reservation("make it faster", "improve/first", None))
    .expect("the first loop reserves");

  assert!(command::stop_improve_loop(&cfg, SESSION).expect("the loop stops"));
  command::clear_active_loop_lock(&cfg, SESSION).expect("the controller releases the active lock");
  let second = command::reserve_loop_state(&cfg, SESSION, &reservation("make it clearer", "improve/second", None))
    .expect("the session reserves another loop");

  assert_ne!(second.loop_id, first.loop_id);
  assert_eq!(
    command::load_loop_state(&cfg, SESSION, first.loop_id).expect("the first state reads").unwrap().status,
    "stopped"
  );
  assert_eq!(second.status, "running");
}

#[test]
// Python: tests/test_improve_command.py::test_quota_blocker_reason_ignores_allowed_rate_limit_event
fn an_allowed_rate_limit_event_is_no_blocker() {
  let events =
    [json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "overageStatus": "allowed"}})];

  let (blocker, _) = judgment::failed_iteration_judgments(&events, 1, "failed").expect("the judgments read");

  assert_eq!(blocker, None);
}

#[test]
// Python: tests/test_improve_command.py::test_quota_blocker_reason_scans_newest_first
fn the_newest_quota_match_names_the_blocker() {
  // Newest first: the error event is the newer of the two, so its match wins over the older rejection.
  let events = [
    json!({"type": "error", "content": "Provider rejected the request: insufficient quota."}),
    json!({
      "type": "rate_limit_event",
      "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "overageStatus": "allowed"}
    }),
  ];

  let (blocker, _) = judgment::failed_iteration_judgments(&events, 1, "failed").expect("the judgments read");

  assert_eq!(blocker.as_deref(), Some("provider quota/token/rate-limit rejection (insufficient quota)"));
}

#[test]
// Python: tests/test_improve_command.py::test_quota_blocker_match_reads_the_launch_error_events_quota_flag
fn a_refused_launch_names_its_quota_cause_by_flag() {
  let event = json!({"type": "error", "message": "no login can take this run", "quota_exhausted": true});

  let blocker = judgment::quota_blocker_match(&event).expect("the match reads");

  assert_eq!(blocker.as_deref(), Some("launch refused with quota exhausted: no login can take this run"));
}

#[test]
// Python: tests/test_improve_command.py::test_quota_blocker_match_leaves_a_false_flag_to_the_text_patterns
fn a_false_quota_flag_leaves_the_relay_limit_refusal_unjudged() {
  let event = json!({
    "type": "error",
    "message": "Claude account relay limit reached (3 relays in one run); this run did not complete.",
    "quota_exhausted": false,
  });

  let blocker = judgment::quota_blocker_match(&event).expect("the match reads");

  assert_eq!(blocker, None);
}

#[test]
// Python: tests/test_improve_command.py::test_quota_blocker_match_judges_an_error_event_without_the_flag_by_text
fn an_unflagged_error_event_is_judged_by_its_text() {
  let event = json!({"type": "error", "message": "Provider rejected the request: insufficient quota."});

  let blocker = judgment::quota_blocker_match(&event).expect("the match reads");

  assert_eq!(blocker.as_deref(), Some("provider quota/token/rate-limit rejection (insufficient quota)"));
}

#[test]
// Python: tests/test_improve_command.py::test_extract_iteration_summary_prefers_newest_result_or_assistant
fn the_newest_summary_wins_whichever_kind_carries_it() {
  // Newest first: the assistant text is newer than the result, so its words are the summary.
  let events = [assistant_text("newest words"), json!({"type": "result", "result": "older result text"})];

  let summary = judgment::extract_iteration_summary(&events, 3, "failed");

  assert_eq!(summary, "newest words");
}

#[test]
// Python: tests/test_improve_command.py::test_newest_first_events_prefiltered_parity
fn the_candidate_prefilter_keeps_the_judgments_inputs() {
  // Over a log with both writer shapes, a giant tool_result line, a type-less line, blank and malformed lines, the
  // newest-first walk keeps the candidate types and the type-less line, in newest-first order.
  let home = tempfile::tempdir().expect("temporary home");
  let path = home.path().join("events.jsonl");
  let adopt_signal = json!({"session_id": "abc", "timestamp": "t"});
  let rate_limit = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected"}});
  let giant = json!({"type": "tool_result", "tool_name": "Bash", "content": "x".repeat(512 * 1024 + 11)});
  let noise = json!({"type": "tool_result", "content": "more noise"});
  let assistant = assistant_text("iter words");
  let lines = [
    adopt_signal.to_string(),
    rate_limit.to_string(),
    String::new(),
    "{not json".to_string(),
    giant.to_string(),
    noise.to_string(),
    assistant.to_string(),
  ];
  std::fs::write(&path, lines.join("\n") + "\n").expect("the event log");

  let events = judgment::newest_first_events(&path).expect("the events read");

  assert_eq!(events, vec![assistant.clone(), rate_limit.clone(), adopt_signal]);
  let (blocker, summary) = judgment::failed_iteration_judgments(&events, 1, "failed").expect("the judgments read");
  assert!(blocker.is_some(), "the rejected rate limit is the blocker");
  assert_eq!(summary, "iter words");
}

#[test]
// Python: tests/test_improve_command.py::test_reserve_loop_state_writes_goal_file
fn reservation_writes_the_live_goal_and_keeps_the_snapshot_in_state() {
  let home = tempfile::tempdir().expect("temporary home");
  let cfg = config(home.path());
  let state = command::reserve_loop_state(&cfg, SESSION, &reservation("make it faster", "improve/test", None))
    .expect("the reservation succeeds");

  let goal_path = command::loop_goal_path(&cfg, SESSION, state.loop_id);
  let expected = cfg.sessions_dir().join(SESSION).join("loops").join(state.loop_id.to_string()).join("goal.md");
  assert_eq!(goal_path, expected);
  assert_eq!(std::fs::read_to_string(&goal_path).expect("the goal file"), "make it faster");
  let loaded = command::load_loop_state(&cfg, SESSION, state.loop_id).expect("the state reads");
  assert_eq!(loaded.map(|loaded| loaded.goal).as_deref(), Some("make it faster"));
}

#[test]
// Python: tests/test_improve_command.py::test_reserve_loop_state_writes_optional_plan_file
fn reservation_writes_the_plan_only_when_given() {
  let home = tempfile::tempdir().expect("temporary home");
  let cfg = config(home.path());
  let state = command::reserve_loop_state(
    &cfg,
    SESSION,
    &reservation("make it faster", "improve/test", Some("1. measure bottleneck")),
  )
  .expect("the reservation succeeds");

  let plan_path = command::loop_plan_path(&cfg, SESSION, state.loop_id);
  let expected = cfg.sessions_dir().join(SESSION).join("loops").join(state.loop_id.to_string()).join("plan.md");
  assert_eq!(plan_path, expected);
  assert_eq!(std::fs::read_to_string(&plan_path).expect("the plan file"), "1. measure bottleneck");
}

#[test]
// Python: tests/test_improve_command.py::test_read_loop_goal_raises_when_missing
fn a_missing_goal_file_fails_without_a_fallback() {
  let home = tempfile::tempdir().expect("temporary home");
  let loop_dir = home.path().join("loops").join("1");
  std::fs::create_dir_all(&loop_dir).expect("the loop directory");

  let error = command::read_loop_goal(&loop_dir).expect_err("a missing goal file is a hard failure");

  assert!(error.to_string().contains("goal file missing"), "the error names the missing file: {error}");
}

#[test]
// Python: tests/test_improve_command.py::test_read_loop_plan_returns_none_when_missing
fn a_missing_plan_file_reads_as_none() {
  let home = tempfile::tempdir().expect("temporary home");
  let loop_dir = home.path().join("loops").join("1");
  std::fs::create_dir_all(&loop_dir).expect("the loop directory");

  let plan = command::read_loop_plan(&loop_dir).expect("the plan reads");

  assert_eq!(plan, None);
}
