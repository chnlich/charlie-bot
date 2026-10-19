//! Startup recovery: the node pass and the boot pass over crash windows, ported from the Python startup-recovery tests.
//!
//! A replay step whose owning task has not landed fails the node pass with the named error of `README.md`'s table, and
//! the boot pass logs it as `task_recovery_node_failed`. Each ported test names its Python case; tests without a
//! store-level write-fault seam exercise the corresponding boot retry at the completion boundary.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use charliebot_agents::transport::identity;
use charliebot_core::event_types;
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::sequence::SequenceCompletion;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{self, RAW_LOG_NAME, RunKind, RunOutcome, RunOutcomeKind, RunRecord, SequenceRef};
use charliebot_store::{CreateTask, PatchTask, TaskCaller};
use charliebot_tasks::execution::is_out_of_space_error;
use charliebot_tasks::recovery::{self, PassCounters};
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, LOOP_KIND, LogCapture, LoopClose, LoopMarker, Stack, assert_logged_node_failure, assert_named_error,
  cold_read_spec, implement_spec, script_option, synthetic_run,
};
use serde_json::{Map, Value, json};

/// The stop request a node that closed before its queued run settles it with (`closed_task_stop_request_id`).
fn closed_task_stop(run_id: &str) -> String {
  format!("closed-task:{run_id}")
}

/// The `run_stop_requested` facts of one run, as their request ids.
fn stop_request_ids(events: &[Value], run_id: &str) -> Vec<String> {
  events
    .iter()
    .filter(|event| event["type"] == "run_stop_requested" && event["run_id"] == run_id)
    .map(|event| event["request_id"].as_str().unwrap_or_default().to_string())
    .collect()
}

/// Rewrite a run's end metadata with empty `ended_at` and `exit_code`: the half-written end record a crash leaves.
fn half_write_end_record(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<()> {
  let path = stack.store().run_metadata_path(session_id, run_id);
  let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
  record["ended_at"] = Value::Null;
  record["exit_code"] = Value::Null;
  std::fs::write(&path, serde_json::to_string(&record)?)?;
  Ok(())
}

/// Swap a node's subtree prompt reference on disk without appending its fact. A crash between a prompt edit's two
/// durable writes leaves this state; the store has no write-fault seam, so the test writes it directly.
fn crash_after_subtree_swap(stack: &Stack, session_id: &str, body: &str) -> anyhow::Result<String> {
  let prompt_ref = stack.store().store_prompt_body(body)?;
  let path = stack.store().metadata_path(session_id);
  let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
  meta["subtree_prompt_ref"] = Value::String(prompt_ref.clone());
  std::fs::write(&path, serde_json::to_string(&meta)?)?;
  Ok(prompt_ref)
}

/// The `prompt_changed` facts of one task, in the order they landed.
fn prompt_facts(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .store()
      .load_chat_events_sync(session_id)?
      .into_iter()
      .filter(|event| event["type"] == event_types::PROMPT_CHANGED)
      .collect(),
  )
}

/// Stage a raw log whose last write is `age` before now, carrying one successful result line.
fn stage_raw_result(run_dir: &std::path::Path, age: Duration) -> anyhow::Result<SystemTime> {
  std::fs::create_dir_all(run_dir)?;
  let path = run_dir.join(RAW_LOG_NAME);
  std::fs::write(&path, "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"staged\"}\n")?;
  let stamped = SystemTime::now() - age;
  std::fs::OpenOptions::new().write(true).open(&path)?.set_modified(stamped)?;
  Ok(stamped)
}

/// `test_task_recovery.py::test_stop_request_wins_over_launch_and_recovery`: a queued run with a durable stop request
/// never launches and claims nothing, and a repeated pass changes nothing.
#[tokio::test]
async fn stop_request_wins_over_launch_and_recovery() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_stop_request_wins_over_launch_and_recovery.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-stopped", RunKind::Work).await?;
  stack.claim(&worker.id, "run-stopped").await?;
  stack.store().request_stop(&worker.id, "run-stopped", "stop-1").await?;

  stack.boot().await?;

  let events = stack.run_events(&worker.id)?;
  assert!(run_records::stop_requested_in_events(&events, "run-stopped", None));
  let run = stack.store().read_run_sync(&worker.id, "run-stopped")?.expect("the queued run is recorded");
  assert_eq!(run.pid, None, "a stop request keeps the queued run from launching");
  assert!(run_records::terminal_outcome(&events, "run-stopped")?.is_none());

  let facts = events.len();
  stack.boot().await?;
  assert_eq!(stack.run_events(&worker.id)?.len(), facts, "a repeated pass records nothing new");
  Ok(())
}

/// `test_task_recovery.py::test_reconcile_settles_a_queued_run_on_a_completed_node_once`: a completed node holding a
/// queued run gets one closed-task stop request, and a repeated pass records nothing more.
#[tokio::test]
async fn reconcile_settles_a_queued_run_on_a_completed_node_once() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_reconcile_settles_a_queued_run_on_a_completed_node_once.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-done", RunKind::Work).await?;
  stack.finish(&worker.id, "run-done", RunOutcomeKind::Success).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  stack.queue_run(&worker.id, "run-queued", RunKind::Work).await?;

  stack.boot().await?;

  let ids = stop_request_ids(&stack.run_events(&worker.id)?, "run-queued");
  assert_eq!(ids, vec![closed_task_stop("run-queued")]);
  let facts = stack.run_events(&worker.id)?.len();
  stack.boot().await?;
  assert_eq!(stop_request_ids(&stack.run_events(&worker.id)?, "run-queued").len(), 1);
  assert_eq!(stack.run_events(&worker.id)?.len(), facts);
  Ok(())
}

/// `test_task_recovery.py::test_restart_before_launch_requeues_through_the_same_launch_checks`: a claimed queued run
/// re-enters the launch checks at boot. A worker run's launch belongs to the worker-run task, so the node pass refuses
/// it with that owner's name, and the run stays queued.
#[tokio::test]
async fn restart_before_launch_requeues_through_the_same_launch_checks() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_restart_before_launch_requeues_through_the_same_launch_checks.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  stack.queue_run(&worker.id, "run-queued", RunKind::Work).await?;
  stack.claim(&worker.id, "run-queued").await?;

  let error = stack.node_pass(&worker.id).await.expect_err("the worker launch is refused");
  let text = format!("{error:#}");
  assert!(text.contains("owned by the worker-run task"), "{text}");
  // The startup pass logs the same failure and carries on.
  stack.boot().await?;
  assert!(log.text().contains("task_recovery_node_failed"), "{}", log.text());
  let run = stack.store().read_run_sync(&worker.id, "run-queued")?.expect("the queued run is recorded");
  assert_eq!(run.pid, None, "a refused launch starts no process");
  assert!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-queued")?.is_none());
  Ok(())
}

/// `test_task_recovery.py::test_restart_after_launch_reattaches_live_process`: a recorded live process is followed, not
/// relaunched. The follow of a worker run belongs to the worker-run task, so the node pass names that owner, and the
/// live process is left running for the owner to follow.
#[tokio::test]
async fn restart_after_launch_reattaches_live_process() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_restart_after_launch_reattaches_live_process.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-live", RunKind::Work).await?;

  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let identity = identity::pin_process_identity(live.id());
  let pid_start = identity.pid_start.expect("the live process has a stat line");
  stack.launch(&worker.id, "run-live", live.id(), &pid_start).await?;

  let error = stack.node_pass(&worker.id).await.expect_err("the worker re-attach belongs to the worker-run task");
  assert_named_error(&error, "worker-run");
  stack.boot().await?;
  assert_logged_node_failure(&log, "worker-run");
  let run = stack.store().read_run_sync(&worker.id, "run-live")?.expect("the launched run is recorded");
  assert_eq!(run.pid, Some(live.id()));
  assert!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-live")?.is_none());
  live.kill()?;
  live.wait()?;
  Ok(())
}

/// `test_task_recovery.py::test_retry_and_boot_reconcile_repair_half_written_end_metadata` (the boot half): a terminal
/// run whose end metadata is empty is repaired from its raw log's last write, once.
#[tokio::test]
async fn boot_reconcile_repairs_a_half_written_end_record_from_the_raw_log() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_retry_and_boot_reconcile_repair_half_written_end_metadata.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-half-written", RunKind::Work).await?;
  stack.finish(&worker.id, "run-half-written", RunOutcomeKind::Success).await?;
  half_write_end_record(&stack, &worker.id, "run-half-written")?;
  let raw_mtime = stage_raw_result(&stack.store().run_dir(&worker.id, "run-half-written"), Duration::from_secs(1800))?;

  stack.boot().await?;

  let run = stack.store().read_run_sync(&worker.id, "run-half-written")?.expect("the run is recorded");
  let ended_at = run.ended_at.expect("the boot pass fills the end time");
  let expected = time::OffsetDateTime::from(raw_mtime);
  let delta = ended_at.into_inner() - expected;
  assert!(delta.abs() < time::Duration::seconds(1), "ended_at {ended_at:?} is not the raw log's last write {expected}");
  assert_eq!(run.exit_code, Some(0), "the raw log's successful result event");
  Ok(())
}

/// `test_task_execution.py::test_repeat_finish_fills_only_empty_end_metadata`: a repeated finish fills the empty end
/// fields of a terminal run and moves no value that is already written.
#[tokio::test]
async fn repeat_finish_fills_only_empty_end_metadata() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_repeat_finish_fills_only_empty_end_metadata.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-half", RunKind::Work).await?;
  stack.finish(&worker.id, "run-half", RunOutcomeKind::Success).await?;
  half_write_end_record(&stack, &worker.id, "run-half")?;

  let ended = UtcDateTime::parse("2026-10-18T10:00:00Z")?;
  let first_repeat = RunOutcome {
    outcome: RunOutcomeKind::Failed,
    input_event_ids: Vec::new(),
    exit_code: Some(-1),
    ended_at: Some(ended),
  };
  stack.dispatch.finish_run(&worker.id, "run-half", first_repeat).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-half")?.expect("the run is recorded");
  assert_eq!(run.ended_at, Some(ended));
  assert_eq!(run.exit_code, Some(-1));
  let outcome = run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-half")?;
  assert_eq!(outcome, Some(RunOutcomeKind::Success), "the first outcome never moves");

  let later = UtcDateTime::parse("2026-10-18T10:01:00Z")?;
  let second_repeat = RunOutcome {
    outcome: RunOutcomeKind::Failed,
    input_event_ids: Vec::new(),
    exit_code: Some(-2),
    ended_at: Some(later),
  };
  stack.dispatch.finish_run(&worker.id, "run-half", second_repeat).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-half")?.expect("the run is recorded");
  assert_eq!(run.ended_at, Some(ended), "a complete end record changes nothing");
  assert_eq!(run.exit_code, Some(-1));
  Ok(())
}

/// `test_task_execution.py::test_is_out_of_space_error_walks_cause_and_context_chain`: only ENOSPC and EDQUOT enter the
/// retry, wherever they sit on the error's chain. Rust errors have a source chain and no context chain, and they cannot
/// form a cycle, so the Python cycle case has no counterpart here.
#[test]
fn is_out_of_space_error_walks_the_cause_chain() {
  // Python: tests/test_task_execution.py::test_is_out_of_space_error_walks_cause_and_context_chain.
  let enospc = std::io::Error::from_raw_os_error(libc::ENOSPC);
  let edquot = std::io::Error::from_raw_os_error(libc::EDQUOT);
  assert!(is_out_of_space_error(&enospc));
  assert!(is_out_of_space_error(&edquot));
  assert!(!is_out_of_space_error(&std::io::Error::from_raw_os_error(libc::EACCES)));
  let wrapped = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC)).context("the landing failed");
  assert!(is_out_of_space_error(wrapped.as_ref()));
  let deep = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EDQUOT))
    .context("the inner wrap")
    .context("the outer wrap");
  assert!(is_out_of_space_error(deep.as_ref()));
  let plain = anyhow::anyhow!("landing failed");
  assert!(!is_out_of_space_error(plain.as_ref()));
}

/// `test_task_execution.py::test_drain_ended_at_is_raw_log_last_write_live_exit_keeps_write_time`:
/// a launched worker run whose process died unseen drains through the worker-run task. The node pass names that owner
/// and records no end time.
#[tokio::test]
async fn a_drained_worker_run_belongs_to_the_worker_run_task() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_drain_ended_at_is_raw_log_last_write_live_exit_keeps_write_time.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-drained", RunKind::Work).await?;
  stack.launch(&worker.id, "run-drained", 424_901, "1-424000").await?;
  stage_raw_result(&stack.store().run_dir(&worker.id, "run-drained"), Duration::from_secs(3600))?;

  let error = stack.node_pass(&worker.id).await.expect_err("the drain of a worker run belongs to the worker-run task");
  assert_named_error(&error, "worker-run");
  stack.boot().await?;
  assert_logged_node_failure(&log, "worker-run");
  let run = stack.store().read_run_sync(&worker.id, "run-drained")?.expect("the drained run is recorded");
  assert_eq!(run.ended_at, None, "no drain end time is written by the refused follow");
  Ok(())
}

const REPO: &str = "/synthetic/repo";
const PAGE: &str = "/synthetic/page.html";

/// Run the node pass, then the startup pass: the node pass refuses with the named error of `owner`, and the startup
/// pass logs that node's failure. Returns the node pass's error text.
async fn assert_refused_by(stack: &Stack, log: &LogCapture, session_id: &str, owner: &str) -> anyhow::Result<String> {
  let error = stack.node_pass(session_id).await.expect_err("the replay step belongs to a later task");
  assert_named_error(&error, owner);
  stack.boot().await?;
  assert_logged_node_failure(log, owner);
  Ok(format!("{error:#}"))
}

/// A queued worker run's launch runs through the worker-run task, so the node pass refuses it with that owner's name.
async fn assert_queued_worker_launch_refused(stack: &Stack, log: &LogCapture, session_id: &str) -> anyhow::Result<()> {
  let error = stack.node_pass(session_id).await.expect_err("a worker run launches through the worker-run task");
  let text = format!("{error:#}");
  assert!(text.contains("owned by the worker-run task"), "{text}");
  stack.boot().await?;
  assert!(log.text().contains(&text), "the startup pass logs the refusal: {}", log.text());
  Ok(())
}

/// A work run that finished success on an implement node, launched at `started_at` when given.
async fn finished_work_run(
  stack: &Stack,
  session_id: &str,
  run_id: &str,
  started_at: Option<&str>,
) -> anyhow::Result<()> {
  let record = RunRecord {
    repo_path: Some(REPO.to_string()),
    base_branch: Some("main".to_string()),
    branch_name: Some("task/work".to_string()),
    started_at: started_at.map(UtcDateTime::parse).transpose()?,
    ..queued_work(session_id, run_id)
  };
  stack.register(record).await?;
  stack.finish(session_id, run_id, RunOutcomeKind::Success).await?;
  Ok(())
}

/// A review of `work_run_id` that ended with `outcome`.
async fn review_of(
  stack: &Stack,
  session_id: &str,
  review_id: &str,
  work_run_id: &str,
  outcome: RunOutcomeKind,
) -> anyhow::Result<()> {
  let record =
    RunRecord { review_of_run_id: Some(work_run_id.to_string()), ..queued_run(session_id, review_id, RunKind::Review) };
  stack.register(record).await?;
  stack.finish(session_id, review_id, outcome).await?;
  Ok(())
}

fn queued_work(session_id: &str, run_id: &str) -> RunRecord {
  queued_run(session_id, run_id, RunKind::Work)
}

fn queued_run(session_id: &str, run_id: &str, kind: RunKind) -> RunRecord {
  RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    ..synthetic_run(session_id, run_id, kind)
  }
}

fn run_ids_of_kind(stack: &Stack, session_id: &str, kind: RunKind) -> anyhow::Result<Vec<String>> {
  Ok(stack.run_records(session_id)?.into_iter().filter(|run| run.kind == kind).map(|run| run.id).collect())
}

fn closes_of(stack: &Stack, session_id: &str) -> anyhow::Result<usize> {
  Ok(stack.run_events(session_id)?.iter().filter(|event| event["type"] == "task_closed").count())
}

/// `test_task_recovery.py::test_recovery_never_rereviews_a_successfully_reviewed_work_run`: a work run whose review
/// already succeeded gets no second review at any restart. The replay of the review's own follow-up belongs to the
/// review-chain task, so the node pass names that owner and registers no review.
#[tokio::test]
async fn recovery_never_rereviews_a_successfully_reviewed_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_never_rereviews_a_successfully_reviewed_work_run.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-reviewed", None).await?;
  review_of(&stack, &worker.id, "review-done", "run-reviewed", RunOutcomeKind::Success).await?;

  for _round in 0..2 {
    assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
    assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-done"]);
  }
  Ok(())
}

/// `test_task_recovery.py::test_recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task`: an earlier work
/// run that a later one superseded never gets a review. The replay refuses the later run's review, so the earlier run
/// is never named.
#[tokio::test]
async fn recovery_spawns_no_review_for_a_superseded_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work-early", Some("2026-10-18T09:00:00Z")).await?;
  finished_work_run(&stack, &worker.id, "run-work-late", Some("2026-10-18T09:30:00Z")).await?;
  review_of(&stack, &worker.id, "review-done", "run-work-late", RunOutcomeKind::Success).await?;

  let text = assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert!(!text.contains("run-work-early"), "the superseded run owes no review: {text}");
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-done"]);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task`: the newest work
/// run that has no review is the one the live path would have reviewed, so the replay names its review spawn and never
/// the earlier run's.
#[tokio::test]
async fn recovery_reviews_only_the_newest_unreviewed_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work-early", Some("2026-10-18T09:00:00Z")).await?;
  finished_work_run(&stack, &worker.id, "run-work-late", Some("2026-10-18T09:30:00Z")).await?;

  let text = assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert!(text.contains("the review spawn of work run run-work-late"), "{text}");
  assert!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.is_empty(), "the refused spawn registers no review");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reports_blocked_when_the_pool_has_no_reviewer_member`: with a single pool
/// member no reviewer can be staffed. The replay's review spawn is the review-chain's, so the node pass names that
/// owner, and no review is registered.
#[tokio::test]
async fn recovery_refers_a_single_member_pool_review_to_the_review_chain() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reports_blocked_when_the_pool_has_no_reviewer_member.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", Some("2026-10-18T09:00:00Z")).await?;

  assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.is_empty());
  Ok(())
}

/// `test_task_recovery.py::test_recovery_spawns_no_review_behind_a_registered_unstarted_work_run`:
/// a registered run that never started makes the earlier successful run's review unowed. The queued run's own launch is
/// refused with the worker-run owner's name, and the review is never spawned.
#[tokio::test]
async fn recovery_spawns_no_review_behind_a_registered_unstarted_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_spawns_no_review_behind_a_registered_unstarted_work_run.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work-early", Some("2026-10-18T09:00:00Z")).await?;
  stack.queue_run(&worker.id, "run-work-queued", RunKind::Work).await?;

  assert_queued_worker_launch_refused(&stack, &log, &worker.id).await?;
  assert!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.is_empty(), "no review behind the unstarted run");
  let queued = stack.store().read_run_sync(&worker.id, "run-work-queued")?.expect("the queued run is recorded");
  assert_eq!(queued.pid, None);
  let stops = stack.run_events(&worker.id)?.iter().filter(|event| event["type"] == "run_stop_requested").count();
  assert_eq!(stops, 0, "an open node's refused launch records no stop request");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_closed_task_skips_landing`: a closed task owes no review spawn, so the replay
/// reaches the review run's own follow-up, which belongs to the review-chain task. Repeated passes append no fact.
#[tokio::test]
async fn recovery_closed_task_skips_the_review_spawn() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_closed_task_skips_landing.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success).await?;
  stack.close(&worker.id, "close-1").await?;

  let text = assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert!(text.contains("the review follow-up of run review-done"), "{text}");
  let facts = stack.run_events(&worker.id)?.len();
  stack.node_pass(&worker.id).await.expect_err("the review follow-up is still unlanded");
  assert_eq!(stack.run_events(&worker.id)?.len(), facts, "a repeated pass appends no fact");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reopened_task_reproves_landing`: a restored task is open again, so its work
/// run owes a review spawn and the replay names the review-chain task.
#[tokio::test]
async fn recovery_reopened_task_refers_the_review_to_the_review_chain() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reopened_task_reproves_landing.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success).await?;
  stack.close(&worker.id, "close-1").await?;
  stack
    .completion
    .restore_task(&worker.id, "reopen-1", "recheck the delivery", &CallerIdentity::Operator { session_id: None })
    .await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");

  assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  Ok(())
}

/// `test_task_recovery.py::test_recovery_open_task_landing_unchanged`: an open task's replayed review follow-up is the
/// review-chain's, so the node pass names that owner and leaves the task open.
#[tokio::test]
async fn recovery_open_task_review_follow_up_refers_to_the_review_chain() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_open_task_landing_unchanged.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success).await?;

  assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_after_a_failed_review_picks_the_next_preference_backend`: a failed review is a
/// used attempt. The replay's review spawn belongs to the review-chain task, so the node pass names it and registers no
/// retry.
#[tokio::test]
async fn recovery_failed_review_refers_the_retry_to_the_review_chain() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_after_a_failed_review_picks_the_next_preference_backend.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0"), script_option("fake3", "exit 0")],
    vec!["fake2".to_string(), "fake3".to_string()],
  )
  .await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", None).await?;
  review_of(&stack, &worker.id, "review-failed", "run-work", RunOutcomeKind::Failed).await?;

  assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-failed"]);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_delivery`: a cold-read run that succeeded and whose
/// delivery a crash cut belongs to the cold-read task, so the node pass names that owner.
#[tokio::test]
async fn recovery_cold_read_delivery_refers_to_the_cold_read_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_delivery.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let reader = stack.node("cold-read", None, TaskProfile::Worker, Some(cold_read_spec(Path::new(PAGE)))).await?;
  stack.queue_run(&reader.id, "run-cold", RunKind::Work).await?;
  stack.finish(&reader.id, "run-cold", RunOutcomeKind::Success).await?;

  let text = assert_refused_by(&stack, &log, &reader.id, "cold-read").await?;
  assert!(text.contains("the cold-read delivery of run run-cold"), "{text}");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_failure_chain`: a failed cold-read run with no later
/// run has its failure chain owned by the cold-read task, so the node pass names that owner.
#[tokio::test]
async fn recovery_cold_read_failure_chain_refers_to_the_cold_read_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_failure_chain.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  let reader = stack.node("cold-read", None, TaskProfile::Worker, Some(cold_read_spec(Path::new(PAGE)))).await?;
  stack.queue_run(&reader.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&reader.id, "run-cold-1", RunOutcomeKind::Failed).await?;

  let text = assert_refused_by(&stack, &log, &reader.id, "cold-read").await?;
  assert!(text.contains("the cold-read failure chain of run run-cold-1"), "{text}");
  assert_eq!(closes_of(&stack, &reader.id)?, 0, "the refused chain archives nothing");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_all_failed_report`: with no backend left the failure
/// chain is still the cold-read task's, so the node pass names the same owner and archives nothing.
#[tokio::test]
async fn recovery_cold_read_all_failed_report_refers_to_the_cold_read_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_all_failed_report.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let reader = stack.node("cold-read", None, TaskProfile::Worker, Some(cold_read_spec(Path::new(PAGE)))).await?;
  stack.queue_run(&reader.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&reader.id, "run-cold-1", RunOutcomeKind::Failed).await?;

  let text = assert_refused_by(&stack, &log, &reader.id, "cold-read").await?;
  assert!(text.contains("the cold-read failure chain of run run-cold-1"), "{text}");
  assert_eq!(closes_of(&stack, &reader.id)?, 0);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_interrupted_report`: an interrupted cold-read run's
/// report belongs to the cold-read task, and no retry run follows it.
#[tokio::test]
async fn recovery_cold_read_interrupted_report_refers_to_the_cold_read_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_interrupted_report.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let reader = stack.node("cold-read", None, TaskProfile::Worker, Some(cold_read_spec(Path::new(PAGE)))).await?;
  stack.queue_run(&reader.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&reader.id, "run-cold-1", RunOutcomeKind::Interrupted).await?;

  let text = assert_refused_by(&stack, &log, &reader.id, "cold-read").await?;
  assert!(text.contains("the cold-read interrupted report of run run-cold-1"), "{text}");
  assert_eq!(run_ids_of_kind(&stack, &reader.id, RunKind::Work)?, ["run-cold-1"], "no retry run follows");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_cold_read_failure_with_a_registered_retry_never_double_retries`: a failed
/// cold-read run with a registered retry drives nothing itself. The retry's launch is the worker-run task's, and the
/// node pass refuses it, so the failed attempt is never re-driven and no second retry appears.
#[tokio::test]
async fn recovery_cold_read_failure_with_a_registered_retry_never_double_retries() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_cold_read_failure_with_a_registered_retry_never_double_retries.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  let reader = stack.node("cold-read", None, TaskProfile::Worker, Some(cold_read_spec(Path::new(PAGE)))).await?;
  stack.queue_run(&reader.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&reader.id, "run-cold-1", RunOutcomeKind::Failed).await?;
  stack.queue_run(&reader.id, "run-cold-2", RunKind::Work).await?;

  assert_queued_worker_launch_refused(&stack, &log, &reader.id).await?;
  assert_eq!(run_ids_of_kind(&stack, &reader.id, RunKind::Work)?, ["run-cold-1", "run-cold-2"]);
  assert_eq!(closes_of(&stack, &reader.id)?, 0, "the failed attempt is never re-driven into an archive");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_an_earlier_failed_run_without_archiving_a_node_a_later_run_finished`:
/// the failed earlier run's report belongs to the worker-run task, so the node pass names it. The node stays open, as
/// the later successful run ended last.
#[tokio::test]
async fn recovery_failed_earlier_run_refers_its_report_to_the_worker_run_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::
  //   test_recovery_replays_an_earlier_failed_run_without_archiving_a_node_a_later_run_finished.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  stack.queue_run(&worker.id, "run-a", RunKind::Work).await?;
  stack.finish(&worker.id, "run-a", RunOutcomeKind::Failed).await?;
  stack.queue_run(&worker.id, "run-b", RunKind::Work).await?;
  stack.finish(&worker.id, "run-b", RunOutcomeKind::Success).await?;

  let text = assert_refused_by(&stack, &log, &worker.id, "worker-run").await?;
  assert!(text.contains("the failure report of work run run-a to its parent"), "{text}");
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert_eq!(closes_of(&stack, &worker.id)?, 0);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_archives_an_open_worker_whose_failure_report_the_parent_already_holds`: the
/// failure report of a failed open worker belongs to the worker-run task, so the node pass names that owner and
/// archives nothing.
#[tokio::test]
async fn recovery_failed_worker_report_refers_to_the_worker_run_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::
  //   test_recovery_archives_an_open_worker_whose_failure_report_the_parent_already_holds.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-failed", RunKind::Work).await?;
  stack.finish(&worker.id, "run-failed", RunOutcomeKind::Failed).await?;

  let text = assert_refused_by(&stack, &log, &worker.id, "worker-run").await?;
  assert!(text.contains("the failure report of work run run-failed to its parent"), "{text}");
  assert_eq!(closes_of(&stack, &worker.id)?, 0);
  Ok(())
}

/// `test_task_recovery.py::test_reconcile_replays_an_already_delivered_blocked_report_without_duplication`: the replay
/// of a successful work run's review, and of a second review of it, belongs to the review-chain task. The node pass
/// names that owner and appends nothing to the worker.
#[tokio::test]
async fn recovery_replayed_review_chain_appends_nothing_before_its_owner_lands() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::
  //   test_reconcile_replays_an_already_delivered_blocked_report_without_duplication.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  finished_work_run(&stack, &worker.id, "run-work", None).await?;
  review_of(&stack, &worker.id, "review-replay-1", "run-work", RunOutcomeKind::Success).await?;
  review_of(&stack, &worker.id, "review-replay-2", "run-work", RunOutcomeKind::Success).await?;
  let facts = stack.run_events(&worker.id)?.len();

  assert_refused_by(&stack, &log, &worker.id, "review-chain").await?;
  assert_eq!(stack.run_events(&worker.id)?.len(), facts, "the refused replay appends no fact");
  Ok(())
}

/// The loop child's close fact and report counts, read from the store.
fn closes(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(stack.run_events(session_id)?.into_iter().filter(|event| event["type"] == event_types::TASK_CLOSED).collect())
}

fn child_reports(stack: &Stack, manager_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(stack.run_events(manager_id)?.into_iter().filter(|event| event["type"] == event_types::CHILD_REPORT).collect())
}

fn stop_requests(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<usize> {
  Ok(
    stack
      .run_events(session_id)?
      .iter()
      .filter(|event| event["type"] == event_types::RUN_STOP_REQUESTED && event["run_id"] == run_id)
      .count(),
  )
}

/// Wait until every run of the manager has a terminal fact: the turns that the child reports woke have ended, so the
/// test's stack drops with no process of its own in flight.
async fn settle_manager(stack: &Stack, manager_id: &str) -> anyhow::Result<()> {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  loop {
    let events = stack.run_events(manager_id)?;
    let mut open = false;
    for run in stack.run_records(manager_id)? {
      if run_records::terminal_outcome(&events, &run.id)?.is_none() {
        open = true;
      }
    }
    if !open {
      return Ok(());
    }
    anyhow::ensure!(std::time::Instant::now() < deadline, "the manager's turns did not settle");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

/// A manager with one loop child under it. The child's marker is the loop's durable state on disk.
async fn loop_child(stack: &Stack, marker: LoopMarker) -> anyhow::Result<(String, String)> {
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let child = stack.node("loop-child", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.write_marker(&child.id, &marker)?;
  Ok((manager.id, child.id))
}

fn close_owed(outcome: &str, summary: &str) -> Option<LoopClose> {
  Some(LoopClose {
    outcome: outcome.to_string(),
    summary: summary.to_string(),
    result_refs: vec!["loop:1".to_string()],
  })
}

fn marker(status: &str, close: Option<LoopClose>) -> LoopMarker {
  LoopMarker { status: status.to_string(), close }
}

/// One iteration of the loop: a run of the child that carries the loop's sequence reference.
async fn iteration(
  stack: &Stack,
  child_id: &str,
  manager_id: &str,
  run_id: &str,
  position: i64,
) -> anyhow::Result<RunRecord> {
  let record = RunRecord {
    sequence_ref: Some(SequenceRef { kind: LOOP_KIND.to_string(), owner_ref: manager_id.to_string(), position }),
    ..queued_work(child_id, run_id)
  };
  stack.register(record).await
}

/// `test_task_sequences.py::test_restart_recovery_marks_improve_loop_through_registered_controller`: the registered
/// controller marks a loop that a prior process left running as interrupted, during the boot pass.
#[tokio::test]
async fn restart_recovery_marks_a_running_loop_through_the_registered_controller() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_restart_recovery_marks_improve_loop_through_registered_controller.
  let stack = Stack::start("exit 0").await?;
  let (_manager, child) = loop_child(&stack, marker("running", None)).await?;

  stack.boot().await?;
  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  stack.boot().await?;
  assert_eq!(stack.read_marker(&child)?.status, "interrupted", "a repeated pass marks nothing new");
  Ok(())
}

/// `test_task_sequences.py::test_a_reopened_child_stays_open_across_restarts`: the close step stopped the queued
/// iteration and closed the child. After the operator reopens it, the boot pass leaves it open, adds no stop request,
/// and writes no second close.
#[tokio::test]
async fn a_reopened_child_stays_open_across_restarts() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_a_reopened_child_stays_open_across_restarts.
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("blocked", close_owed("blocked", "[Synthetic loop 1] blocked"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.record_terminal(&child, "iter-run-1", RunOutcomeKind::Success).await?;
  iteration(&stack, &child, &manager, "iter-run-2", 2).await?;
  stack.store().request_stop(&child, "iter-run-2", "loop-close:iter-run-2").await?;
  let evidence = json!({"summary": "[Synthetic loop 1] blocked", "result_refs": ["loop:1"], "run_ids": []});
  SequenceCompletion::close_sequence_child(stack.completion.as_ref(), &child, "loop-close", "blocked", &evidence)
    .await?;
  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  stack
    .completion
    .restore_task(&child, "reopen-1", "operator retry", &CallerIdentity::Operator { session_id: None })
    .await?;
  assert_eq!(stack.store().task_state(&child)?, "open");

  stack.boot().await?;

  assert_eq!(stack.store().task_state(&child)?, "open");
  assert_eq!(stop_requests(&stack, &child, "iter-run-2")?, 1, "no new stop request");
  assert_eq!(closes(&stack, &child)?.len(), 1);
  settle_manager(&stack, &manager).await?;
  assert_eq!(child_reports(&stack, &manager)?.len(), 1);
  Ok(())
}

/// `test_task_sequences.py::test_startup_recovers_a_stop_requested_launched_iteration_and_closes`: a launched iteration
/// with a durable stop request ends at startup, and its after-run closes the child with the loop's failed outcome.
#[tokio::test]
async fn startup_closes_a_loop_whose_stop_requested_iteration_ended() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_recovers_a_stop_requested_launched_iteration_and_closes.
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("running", close_owed("failed", "[Synthetic loop 1] failed"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let pid_start = identity::pin_process_identity(live.id()).pid_start.expect("the live process has a stat line");
  stack.launch(&child, "iter-run-1", live.id(), &pid_start).await?;
  stack.store().request_stop(&child, "iter-run-1", "boot-stop").await?;

  stack.boot().await?;

  wait_for_exit(&mut live).await?;
  let events = stack.run_events(&child)?;
  assert_eq!(run_records::terminal_outcome(&events, "iter-run-1")?, Some(RunOutcomeKind::Interrupted));
  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  let closed = closes(&stack, &child)?;
  assert_eq!(closed.len(), 1);
  assert_eq!(closed[0]["sequence_outcome"], "failed");
  settle_manager(&stack, &manager).await?;
  let reports = child_reports(&stack, &manager)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "failed");
  Ok(())
}

/// Wait until a child process exits and reap it. The stop's signal lands during the boot pass, so the exit follows.
async fn wait_for_exit(child: &mut std::process::Child) -> anyhow::Result<()> {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  while child.try_wait()?.is_none() {
    anyhow::ensure!(std::time::Instant::now() < deadline, "the stopped process did not exit");
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  Ok(())
}

/// `test_task_sequences.py::test_startup_closes_a_loop_that_crashed_between_final_state_and_close`: the loop's final
/// state is on disk and its close is not. The boot pass closes the child with the loop's outcome and delivers one final
/// report to the parent.
#[tokio::test]
async fn startup_closes_a_loop_that_crashed_between_final_state_and_close() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_closes_a_loop_that_crashed_between_final_state_and_close.
  let stack = Stack::start("exit 0").await?;
  let summary = "[Synthetic loop 1] Loop ran 2 iteration(s)\n\niter one report\n\niter two report";
  let (manager, child) = loop_child(&stack, marker("blocked", close_owed("blocked", summary))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.record_terminal(&child, "iter-run-1", RunOutcomeKind::Success).await?;
  iteration(&stack, &child, &manager, "iter-run-2", 2).await?;
  stack.record_terminal(&child, "iter-run-2", RunOutcomeKind::Success).await?;

  stack.boot().await?;

  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  let closed = closes(&stack, &child)?;
  assert_eq!(closed.len(), 1);
  assert_eq!(closed[0]["sequence_outcome"], "blocked");
  settle_manager(&stack, &manager).await?;
  let reports = child_reports(&stack, &manager)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert_eq!(reports[0]["summary"], summary);
  assert_eq!(reports[0]["result_refs"], json!(["loop:1"]));
  Ok(())
}

/// `test_task_sequences.py::test_startup_closes_a_failed_loop_and_reports_its_end_error`: a loop whose final failed
/// state carries its end error and whose close a crash cut. The boot pass closes the child failed, and the parent's one
/// final report carries the end error text.
#[tokio::test]
async fn startup_closes_a_failed_loop_and_reports_its_end_error() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_closes_a_failed_loop_and_reports_its_end_error.
  let stack = Stack::start("exit 0").await?;
  let summary = "[Synthetic loop 1] end error: synthetic failure";
  let (manager, child) = loop_child(&stack, marker("failed", close_owed("failed", summary))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.record_terminal(&child, "iter-run-1", RunOutcomeKind::Success).await?;

  stack.boot().await?;

  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  assert_eq!(closes(&stack, &child)?[0]["sequence_outcome"], "failed");
  settle_manager(&stack, &manager).await?;
  let reports = child_reports(&stack, &manager)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "failed");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("end error: synthetic failure"));
  Ok(())
}

/// `test_task_sequences.py::test_startup_closes_a_loop_that_crashed_with_an_iteration_registered_unlaunched`: a queued
/// iteration never launches. The close step stops it first, so the child closes and the queued run stays unlaunched.
#[tokio::test]
async fn startup_closes_a_loop_with_an_unlaunched_iteration_stopped_first() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::
  //   test_startup_closes_a_loop_that_crashed_with_an_iteration_registered_unlaunched.
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("blocked", close_owed("blocked", "[Synthetic loop 1] blocked"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.record_terminal(&child, "iter-run-1", RunOutcomeKind::Success).await?;
  iteration(&stack, &child, &manager, "iter-run-2", 2).await?;

  stack.boot().await?;

  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  assert_eq!(closes(&stack, &child)?.len(), 1);
  let queued = stack.store().read_run_sync(&child, "iter-run-2")?.expect("the queued iteration is recorded");
  assert_eq!(queued.pid, None);
  let events = stack.run_events(&child)?;
  assert!(run_records::terminal_outcome(&events, "iter-run-2")?.is_none());
  assert_eq!(stop_requests(&stack, &child, "iter-run-2")?, 1);
  settle_manager(&stack, &manager).await?;
  assert_eq!(child_reports(&stack, &manager)?.len(), 1);
  Ok(())
}

/// `test_task_sequences.py::test_startup_closes_a_loop_that_crashed_between_run_finished_and_after_run`:
/// the iteration's finish landed and its after-run did not. The boot pass closes the child cancelled, and the parent
/// gets one report that carries the iteration's text.
#[tokio::test]
async fn startup_closes_a_loop_whose_iteration_finished_before_its_after_run() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_closes_a_loop_that_crashed_between_run_finished_and_after_run.
  let stack = Stack::start("exit 0").await?;
  let summary = "[Synthetic loop 1] cancelled\n\niter one report";
  let (manager, child) = loop_child(&stack, marker("stopped", close_owed("cancelled", summary))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.record_terminal(&child, "iter-run-1", RunOutcomeKind::Success).await?;

  stack.boot().await?;

  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  assert_eq!(closes(&stack, &child)?[0]["sequence_outcome"], "cancelled");
  settle_manager(&stack, &manager).await?;
  let reports = child_reports(&stack, &manager)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "cancelled");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("iter one report"));
  Ok(())
}

/// A dead iteration's drain belongs to the worker-run task. The boot pass marks the loop interrupted, and the node pass
/// refuses the drain with that owner's name, so the child stays open and nothing closes.
#[tokio::test]
async fn startup_interrupts_a_loop_whose_iteration_process_died_and_names_the_drain() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_recovery_closes_an_interrupted_loop_whose_process_died.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("running", close_owed("failed", "[Synthetic loop 1] failed"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.launch(&child, "iter-run-1", 424_902, "1-424000").await?;
  stage_raw_result(&stack.store().run_dir(&child, "iter-run-1"), Duration::from_secs(3600))?;

  let error = stack.node_pass(&child).await.expect_err("the iteration drain belongs to the worker-run task");
  assert_named_error(&error, "worker-run");

  stack.boot().await?;

  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  assert_eq!(stack.store().task_state(&child)?, "open", "the refused drain closes nothing");
  assert!(closes(&stack, &child)?.is_empty());
  assert_logged_node_failure(&log, "worker-run");
  Ok(())
}

/// `test_task_sequences.py::test_startup_recovery_leaves_a_live_iterations_child_open_until_its_finish`:
/// a live iteration keeps its child open. The boot pass marks the loop interrupted and leaves the process running, and
/// its re-attach belongs to the worker-run task.
#[tokio::test]
async fn startup_leaves_a_live_iterations_child_open_until_its_finish() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_recovery_leaves_a_live_iterations_child_open_until_its_finish.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("running", close_owed("failed", "[Synthetic loop 1] failed"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let pid_start = identity::pin_process_identity(live.id()).pid_start.expect("the live process has a stat line");
  stack.launch(&child, "iter-run-1", live.id(), &pid_start).await?;

  let error = stack.node_pass(&child).await.expect_err("the iteration re-attach belongs to the worker-run task");
  assert_named_error(&error, "worker-run");

  stack.boot().await?;

  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  assert_eq!(stack.store().task_state(&child)?, "open");
  assert!(closes(&stack, &child)?.is_empty());
  assert!(live.try_wait()?.is_none(), "the startup pass leaves the live iteration running");
  assert_logged_node_failure(&log, "worker-run");
  live.kill()?;
  live.wait()?;
  Ok(())
}

/// The text an out-of-space error carries, as the store flattens it.
fn enospc_text() -> String {
  std::io::Error::from_raw_os_error(libc::ENOSPC).to_string()
}

/// Poll until `condition` holds, failing after ten seconds.
async fn settle_until(condition: impl Fn() -> anyhow::Result<bool>) -> anyhow::Result<()> {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  while !condition()? {
    anyhow::ensure!(std::time::Instant::now() < deadline, "the condition did not hold within ten seconds");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  Ok(())
}

/// A boot replay whose post-finish follow-up fails out of space keeps its end record. The retry delivers the
/// success-based report exactly once.
#[tokio::test]
async fn boot_retry_delivers_the_parent_report_once_after_a_follow_up_failure() -> anyhow::Result<()> {
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-finished", RunKind::Work).await?;
  stack.fail_follow_up(1, &enospc_text());
  let _ = stack.finish(&worker.id, "run-finished", RunOutcomeKind::Success).await;
  let ended = stack.store().read_run_sync(&worker.id, "run-finished")?.and_then(|run| run.ended_at);
  assert!(ended.is_some(), "the end record landed before the follow-up failed");
  assert!(child_reports(&stack, &manager.id)?.is_empty());

  stack.boot().await?;

  settle_manager(&stack, &manager.id).await?;
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "completed");
  let fresh = stack.store().read_run_sync(&worker.id, "run-finished")?.and_then(|run| run.ended_at);
  assert_eq!(fresh, ended, "the replay keeps the live end time");
  Ok(())
}

/// The end record is empty and a boot follow-up fails out of space. The node is handed to the end-landing retry, whose
/// first round completes the follow-up, and the end time stays the raw log's last write.
#[tokio::test]
async fn boot_retry_repairs_a_half_written_end_record_after_a_follow_up_failure() -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-half-written", RunKind::Work).await?;
  stack.fail_follow_up(1, &enospc_text());
  stack.finish(&worker.id, "run-half-written", RunOutcomeKind::Success).await.expect_err("the follow-up fails");
  half_write_end_record(&stack, &worker.id, "run-half-written")?;
  let raw_mtime = stage_raw_result(&stack.store().run_dir(&worker.id, "run-half-written"), Duration::from_secs(1800))?;
  stack.fail_follow_up(1, &enospc_text());
  assert_eq!(stack.store().task_state(&worker.id)?, "open");

  stack.boot().await?;

  assert!(log.text().contains("No space left on device"), "the boot pass logs the out-of-space node: {}", log.text());
  settle_until(|| Ok(stack.store().task_state(&worker.id)? == "completed")).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-half-written")?.expect("the run is recorded");
  let ended_at = run.ended_at.expect("the repair fills the end time");
  let delta = ended_at.into_inner() - time::OffsetDateTime::from(raw_mtime);
  assert!(delta.abs() < time::Duration::seconds(1), "ended_at is the raw log's last write");
  assert_eq!(run.exit_code, Some(0));
  Ok(())
}

/// A boot follow-up that fails for a reason other than space starts no retry. The node stays open until a later pass
/// completes it.
#[tokio::test]
async fn boot_non_space_follow_up_failure_starts_no_retry() -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  let eacces = std::io::Error::from_raw_os_error(libc::EACCES).to_string();
  stack.queue_run(&worker.id, "run-finished", RunKind::Work).await?;
  stack.fail_follow_up(1, &eacces);
  stack.finish(&worker.id, "run-finished", RunOutcomeKind::Success).await.expect_err("the follow-up fails");
  half_write_end_record(&stack, &worker.id, "run-finished")?;
  stack.fail_follow_up(1, &eacces);

  stack.boot().await?;

  assert!(log.text().contains("Permission denied"), "the boot pass logs the failed node: {}", log.text());
  tokio::time::sleep(Duration::from_millis(200)).await;
  assert_eq!(stack.store().task_state(&worker.id)?, "open", "no retry completed the follow-up");
  stack.boot().await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "completed", "the next pass completes it");
  Ok(())
}

/// `test_task_recovery.py::test_retry_round_skips_runs_this_process_already_drives`:
/// a node pass that a retry runs skips the worker and manager runs its process already drives, so no second follower
/// or review starts. Without the driver, the same pass follows the launched worker run, which belongs to worker-run.
#[tokio::test]
async fn a_retry_round_skips_the_runs_this_process_already_drives() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_retry_round_skips_runs_this_process_already_drives.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  for (run_id, pid) in [("run-inflight", 424_811), ("run-followed", 424_812)] {
    stack.queue_run(&worker.id, run_id, RunKind::Work).await?;
    stack.launch(&worker.id, run_id, pid, "1-424000").await?;
  }
  stack.queue_run(&worker.id, "run-work", RunKind::Work).await?;
  stack.finish(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  let pending = RunRecord {
    review_of_run_id: Some("run-work".to_string()),
    ..queued_run(&worker.id, "review-pending", RunKind::Review)
  };
  stack.register(pending).await?;

  let driven = |run: &RunRecord| run.id == "run-inflight" || run.id == "run-followed";
  let driven_filter: &(dyn Fn(&RunRecord) -> bool + Sync) = &driven;
  let mut counters = PassCounters::default();
  recovery::reconcile_node(&stack.features, &stack.adapter, &worker.id, &mut counters, Some(driven_filter)).await?;

  for run_id in ["run-inflight", "run-followed"] {
    let run = stack.store().read_run_sync(&worker.id, run_id)?.expect("the driven run is recorded");
    assert!(run.pid.is_some(), "the driven run keeps its process");
    assert!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, run_id)?.is_none());
  }
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-pending"], "no second review");

  stack.queue_run(&manager.id, "run-manager-followed", RunKind::ManagerTurn).await?;
  stack.launch(&manager.id, "run-manager-followed", 424_813, "1-424000").await?;
  let manager_driven = |run: &RunRecord| run.id == "run-manager-followed";
  let manager_filter: &(dyn Fn(&RunRecord) -> bool + Sync) = &manager_driven;
  let mut counters = PassCounters::default();
  recovery::reconcile_node(&stack.features, &stack.adapter, &manager.id, &mut counters, Some(manager_filter)).await?;
  let manager_run =
    stack.store().read_run_sync(&manager.id, "run-manager-followed")?.expect("the queued manager follow is recorded");
  assert!(manager_run.pid.is_some());
  assert!(run_records::terminal_outcome(&stack.run_events(&manager.id)?, &manager_run.id)?.is_none());

  let undriven: &(dyn Fn(&RunRecord) -> bool + Sync) = &|_: &RunRecord| false;
  let mut counters = PassCounters::default();
  let error = recovery::reconcile_node(&stack.features, &stack.adapter, &worker.id, &mut counters, Some(undriven))
    .await
    .expect_err("without a driver the launched run belongs to the worker-run task");
  assert_named_error(&error, "worker-run");
  Ok(())
}

/// `test_task_prompt_change_durable.py::test_two_scope_patch_crash_between_scopes_repairs_cleanly`:
/// a PATCH that crashed after its subtree scope swapped the metadata. The retry lands the interrupted subtree fact
/// before it applies the node scope, so each scope has exactly one fact, in order.
#[tokio::test]
async fn two_scope_prompt_patch_retry_repairs_a_crash_between_scopes() -> anyhow::Result<()> {
  // Python: tests/test_task_prompt_change_durable.py::test_two_scope_patch_crash_between_scopes_repairs_cleanly.
  let stack = Stack::start("exit 0").await?;
  let leaf = stack.node("leaf", None, TaskProfile::Worker, None).await?;
  let subtree_ref = crash_after_subtree_swap(&stack, &leaf.id, "subtree rule\n")?;
  let node_ref = stack.store().store_prompt_body("node rule\n")?;
  let retry = PatchTask {
    subtree_prompt: Some(Some("subtree rule\n".to_string())),
    node_prompt: Some(Some("node rule\n".to_string())),
    ..Default::default()
  };

  let patched = stack.store().patch_task(&leaf.id, retry, TaskCaller::Operator, &*stack.dispatch).await?;

  assert_eq!(patched.subtree_prompt_ref.as_deref(), Some(subtree_ref.as_str()));
  assert_eq!(patched.node_prompt_ref.as_deref(), Some(node_ref.as_str()));
  let facts = prompt_facts(&stack, &leaf.id)?;
  let landed: Vec<(&str, &str)> = facts
    .iter()
    .map(|fact| (fact["scope"].as_str().unwrap_or_default(), fact["new_ref"].as_str().unwrap_or_default()))
    .collect();
  assert_eq!(landed, [("subtree", subtree_ref.as_str()), ("node", node_ref.as_str())]);
  Ok(())
}

/// `test_task_prompt_change_durable.py::test_recovery_sweep_is_idempotent_and_does_not_duplicate`:
/// the boot sweep lands the fact of an interrupted prompt edit once. Later passes find the fact in place and append
/// nothing.
#[tokio::test]
async fn recovery_sweep_lands_an_interrupted_prompt_fact_once() -> anyhow::Result<()> {
  // Python: tests/test_task_prompt_change_durable.py::test_recovery_sweep_is_idempotent_and_does_not_duplicate.
  let stack = Stack::start("exit 0").await?;
  let leaf = stack.node("leaf", None, TaskProfile::Worker, None).await?;
  let subtree_ref = crash_after_subtree_swap(&stack, &leaf.id, "rule\n")?;
  assert!(prompt_facts(&stack, &leaf.id)?.is_empty(), "the crash left the fact unappended");

  stack.boot().await?;
  let first = prompt_facts(&stack, &leaf.id)?;
  assert_eq!(first.len(), 1, "the first pass lands the fact");
  assert_eq!(first[0]["new_ref"], subtree_ref);
  stack.boot().await?;
  stack.boot().await?;
  assert_eq!(prompt_facts(&stack, &leaf.id)?, first, "the later passes append nothing");
  Ok(())
}

/// A root node under a fixed session id. The boot pass visits the session directories in sorted order, so the id
/// decides which node the pass reaches first.
async fn node_with_id(stack: &Stack, session_id: &str, profile: TaskProfile) -> anyhow::Result<SessionMetadata> {
  let request = CreateTask {
    request_id: format!("create-{session_id}"),
    task_parent_id: None,
    profile,
    task: None,
    name: Some(format!("Synthetic {session_id}")),
    backend: Some(BACKEND_ID.to_string()),
    group: None,
    session_id: Some(session_id.to_string()),
  };
  Ok(stack.store().create_task(request, TaskCaller::Operator, Map::new()).await?)
}

/// Boot a home holding a sequence node with one queued run of `sequence_kind` and a manager node with one dead turn.
/// `sequence_first` gives the sequence node the id the boot pass reaches first, or the one it reaches second.
async fn boot_past_a_refused_sequence_node(
  sequence_kind: &str,
  run_kind: RunKind,
  sequence_first: bool,
) -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  // The drain builds the manager run's child environment, which signs a run token with `charliebot.access_key`.
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let (sequence_id, manager_id) =
    if sequence_first { ("synthetic-node-a", "synthetic-node-b") } else { ("synthetic-node-b", "synthetic-node-a") };
  let sequence = node_with_id(&stack, sequence_id, TaskProfile::Worker).await?;
  let manager = node_with_id(&stack, manager_id, TaskProfile::Manager).await?;
  let sequence_ref =
    SequenceRef { kind: sequence_kind.to_string(), owner_ref: "synthetic-owner".to_string(), position: 0 };
  let queued = RunRecord { sequence_ref: Some(sequence_ref), ..queued_run(&sequence.id, "run-sequence", run_kind) };
  stack.register(queued).await?;
  stack.queue_run(&manager.id, "run-manager-dead", RunKind::ManagerTurn).await?;
  stack.launch(&manager.id, "run-manager-dead", 424_821, "1-424000").await?;

  let counters = stack.boot().await.expect("the boot pass returns Ok when one node is refused");

  assert_eq!(counters.nodes, 2, "the boot pass visits both nodes");
  assert_eq!(counters.drained, 1, "the manager node drains its dead turn");
  settle_manager(&stack, &manager.id).await?;
  let manager_events = stack.run_events(&manager.id)?;
  assert!(
    run_records::terminal_outcome(&manager_events, "run-manager-dead")?.is_some(),
    "the manager node recovers its dead turn past the refused node"
  );
  let refused = stack.store().read_run_sync(&sequence.id, "run-sequence")?.expect("the queued run is recorded");
  assert_eq!(refused.pid, None, "the refused run never launches");
  let sequence_events = stack.run_events(&sequence.id)?;
  assert!(
    run_records::terminal_outcome(&sequence_events, "run-sequence")?.is_none(),
    "the refused run keeps no terminal fact"
  );
  assert_logged_node_failure(&log, sequence_kind);
  Ok(())
}

/// A refused `improve` run stops only its own node: the boot pass logs the named error and the manager node still
/// recovers its dead turn, with the improve node first and with it second. There is no Python case, since Python runs
/// the improve controller; this covers the interim refusal of `UNLANDED_SEQUENCE_KINDS`.
#[tokio::test]
async fn a_refused_improve_node_leaves_the_manager_node_to_recover() -> anyhow::Result<()> {
  for sequence_first in [true, false] {
    boot_past_a_refused_sequence_node("improve", RunKind::Iteration, sequence_first)
      .await
      .with_context(|| format!("with the sequence node first: {sequence_first}"))?;
  }
  Ok(())
}

/// The same refusal for a `cron` run, in both node orders: the cron controller has not landed either.
#[tokio::test]
async fn a_refused_cron_node_leaves_the_manager_node_to_recover() -> anyhow::Result<()> {
  for sequence_first in [true, false] {
    boot_past_a_refused_sequence_node("cron", RunKind::ScheduledStep, sequence_first)
      .await
      .with_context(|| format!("with the sequence node first: {sequence_first}"))?;
  }
  Ok(())
}
