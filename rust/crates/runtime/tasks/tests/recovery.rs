//! Startup recovery: the node pass and the boot pass over crash windows, ported from the Python startup-recovery tests.
//!
//! A replay step whose owning task has not landed fails the node pass with the named error of `README.md`'s table, and
//! the boot pass logs it as `task_recovery_node_failed`. Each ported test names its Python case; tests without a
//! store-level write-fault seam exercise the corresponding boot retry at the completion boundary.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use charliebot_agents::transport::identity;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::sequence::{SequenceCompletion, SequenceExecutor};
use charliebot_store::ChatEvent;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RAW_LOG_NAME, RunKind, RunOutcome, RunOutcomeKind, RunRecord, SequenceRef};
use charliebot_store::write_faults::{self, Hit};
use charliebot_store::{CreateTask, PatchTask, TaskCaller};
use charliebot_tasks::execution::is_out_of_space_error;
use charliebot_tasks::recovery::{self, PassCounters};
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, LOOP_KIND, LogCapture, LoopClose, LoopMarker, PROBE_KIND, Scripts, Stack, SyntheticRepo,
  assert_logged_node_failure, cold_read_spec, implement_spec, last_child_report, script_option, synthetic_run,
};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

/// The stop request a node that closed before its queued run settles it with (`closed_task_stop_request_id`).
fn closed_task_stop(run_id: &str) -> String {
  format!("closed-task:{run_id}")
}

fn script_run_spec() -> TaskSpec {
  TaskSpec { goal: "synthetic worker task".to_string(), task_type: Some(TaskType::ScriptRun), ..TaskSpec::default() }
}

#[test]
#[ignore = "subprocess entry point for durable capture recovery"]
fn final_stdout_supervisor() {
  charliebot_agents::transport::final_stdout::supervise(&[std::env::var_os("TEST_CAPTURE_DIR").unwrap()]).unwrap();
}

async fn captured_run(
  stack: &Stack,
  node: &SessionMetadata,
  kind: RunKind,
  guard: &ProcessGroupGuard,
) -> anyhow::Result<std::process::Child> {
  use charliebot_agents::transport::{cgroup, final_stdout, spawn};
  let run_id = "captured";
  stack
    .register(RunRecord {
      backend: Some("capture".into()),
      model: Some("capture-model".into()),
      ..synthetic_run(&node.id, run_id, kind)
    })
    .await?;
  let directory = stack.store().run_dir(&node.id, run_id);
  let response = json!([
    {"type": "session_attached", "session_id": "captured-native-id"},
    {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "captured answer"}]}},
    {"type": "result", "subtype": "success", "result": "captured answer", "usage": {
      "input_tokens": 10, "output_tokens": 23, "cache_read_input_tokens": 3
    }}
  ]);
  let spec = spawn::SpawnSpec::new(
    "/bin/sh",
    vec![
      "-c".into(),
      format!("printf '%s' $$ > started; while [ ! -e release ]; do sleep 0.01; done; printf '%s' '{}'", response,)
        .into(),
    ],
    Some(directory.clone()),
    std::collections::HashMap::new(),
    None,
    cgroup::SessionCgroupConfig::disabled(),
  );
  final_stdout::prepare(&spec, &directory, &LaunchArgs::default(), None)?;
  let child = Command::new(std::env::current_exe()?)
    .args(["--exact", "final_stdout_supervisor", "--ignored"])
    .env("TEST_CAPTURE_DIR", &directory)
    .current_dir(&directory)
    .process_group(0)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::inherit())
    .spawn()?;
  guard.track(child.id());
  let pin = identity::pin_process_identity(child.id());
  stack
    .launch(&node.id, run_id, pin.pid, pin.pid_start.as_deref().context("capture supervisor has an identity")?)
    .await?;
  settle_until(|| Ok(directory.join("started").exists())).await.context("capture command starts")?;
  Ok(child)
}

#[tokio::test]
async fn final_stdout_recovery_delivers_live_and_dead_runs_and_parent_reports() -> anyhow::Result<()> {
  for dead in [false, true] {
    for profile in [TaskProfile::Worker, TaskProfile::Manager] {
      let log = LogCapture::default();
      let _capture = log.install();
      let guard = ProcessGroupGuard::new();
      let mut option = script_option("capture", "exit 0");
      option.type_fields.insert("final_stdout".into(), true.into());
      let stack = Stack::start_with(
        vec![script_option(BACKEND_ID, &recovery_support::result_script("report noted")), option],
        vec![BACKEND_ID.to_string()],
      )
      .await?;
      stack.write_access_key()?;
      let parent = stack.node("parent", None, TaskProfile::Manager, None).await?;
      let node = stack
        .node("captured", Some(&parent.id), profile, (profile == TaskProfile::Worker).then(script_run_spec))
        .await?;
      let kind = if profile == TaskProfile::Worker { RunKind::Work } else { RunKind::ManagerTurn };
      let mut child = captured_run(&stack, &node, kind, &guard).await?;
      let directory = stack.store().run_dir(&node.id, "captured");
      let mut completed = None;
      if dead {
        std::fs::write(directory.join("release"), "")?;
        settle_until(|| Ok(child.try_wait()?.is_some())).await?;
        completed = charliebot_agents::transport::recovery::raw_completion_time(&directory.join(RAW_LOG_NAME));
      }
      let counters = stack.node_pass(&node.id).await?;
      assert_eq!((counters.resumed, counters.drained), if dead { (0, 1) } else { (1, 0) });
      if !dead {
        assert!(child.try_wait()?.is_none());
        assert!(run_records::terminal_outcome(&stack.run_events(&node.id)?, "captured")?.is_none());
        std::fs::write(directory.join("release"), "")?;
      }
      assert_eq!(
        stack
          .wait_terminal(&node.id, "captured")
          .await
          .with_context(|| format!("profile={profile:?} dead={dead}: {}", log.text()))?,
        RunOutcomeKind::Success
      );
      child.wait()?;
      let run = stack.store().read_run_sync(&node.id, "captured")?.unwrap();
      assert_eq!(run.exit_code, Some(0));
      if let Some(completed) = completed {
        assert_eq!(
          run.ended_at.unwrap().into_inner().unix_timestamp_nanos() / 1_000,
          completed.unix_timestamp_nanos() / 1_000
        );
      }
      let events: Vec<Value> = if profile == TaskProfile::Worker {
        std::fs::read_to_string(directory.join(run_records::RUN_EVENTS_NAME))?
          .lines()
          .map(serde_json::from_str)
          .collect::<Result<_, _>>()?
      } else {
        stack.run_event_values(&node.id)?
      };
      assert!(events.iter().any(|event| event.to_string().contains("captured answer")), "{profile:?}: {events:?}");
      assert!(events.iter().any(|event| event["usage"]["output_tokens"] == 23));
      if profile == TaskProfile::Worker {
        settle_until(|| Ok(!stack.child_reports(&parent.id)?.is_empty())).await?;
        let report = stack.child_reports(&parent.id)?;
        assert_eq!(report[0]["outcome"], "completed");
        assert_eq!(report[0]["result_refs"], json!(["run:captured"]));
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn final_stdout_stop_of_an_adopted_run_ends_its_process() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let mut option = script_option("capture", "exit 0");
  option.type_fields.insert("final_stdout".into(), true.into());
  let stack =
    Stack::start_with(vec![script_option(BACKEND_ID, "exit 0"), option], vec![BACKEND_ID.to_string()]).await?;
  let node = stack.node("captured", None, TaskProfile::Worker, Some(script_run_spec())).await?;
  let mut child = captured_run(&stack, &node, RunKind::Work, &guard).await?;
  let agent_pid = std::fs::read_to_string(stack.store().run_dir(&node.id, "captured").join("started"))?.parse()?;
  assert_eq!(stack.node_pass(&node.id).await?.resumed, 1);
  settle_until(|| Ok(stack.adapter.resume_follows().contains(&(node.id.clone(), "captured".into())))).await?;
  stack.store().request_stop(&node.id, "captured", "operator-stop").await?;
  settle_until(|| Ok(child.try_wait()?.is_some())).await?;
  settle_until(|| Ok(identity::read_pid_stat(agent_pid).is_none_or(|stat| stat.state == "Z"))).await?;
  assert_eq!(stack.wait_terminal(&node.id, "captured").await?, RunOutcomeKind::Interrupted);
  Ok(())
}

/// One JSON request to the delegated-worker route, authenticated as the synthetic operator.
async fn post_delegate(router: &axum::Router, body: Value) -> anyhow::Result<(StatusCode, Value)> {
  let response = router
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/api/internal/delegate")
        .header(header::AUTHORIZATION, "Bearer synthetic-access-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body)?))?,
    )
    .await?;
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await?;
  Ok((status, serde_json::from_slice(&bytes)?))
}

/// The `run_stop_requested` facts of one run, as their request ids.
fn stop_request_ids(events: &[ChatEvent], run_id: &str) -> Vec<String> {
  events
    .iter()
    .filter(|event| event["type"].as_str() == Some("run_stop_requested") && event["run_id"].as_str() == Some(run_id))
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
      .filter(|event| event["type"].as_str() == Some(event_types::PROMPT_CHANGED))
      .map(|event| event.to_value())
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

/// A delegated repo-less script run creates its worker once, and a lost-response replay returns that same child and
/// Run while persisting the task card in the manager's chat.
#[tokio::test]
async fn delegate_creates_one_worker_and_replays_the_same_run() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_delegate_creates_one_child_and_replays_are_stable.
  let guard = ProcessGroupGuard::new();
  let stack =
    Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"Synthetic worker finished"}'"#)
      .await?;
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let parent = stack.node("delegate-parent", None, TaskProfile::Manager, None).await?;

  // The user message authorizes takeoff but must not launch a manager turn in this route-focused test.
  stack.dispatch.set_executor(None);
  stack.dispatch.admit_input(&parent.id, event_types::USER, "Start the synthetic task.", "user", json!({})).await?;
  stack.dispatch.set_executor(Some(Arc::new(stack.adapter.clone())));

  let router = charliebot_tasks::routes::routes(&stack.features);
  let body = json!({
    "session_id": parent.id,
    "description": "Synthetic repo-less task",
    "task_type": "script-run",
    "request_id": "synthetic-delegate-request",
    "backend": "fake",
  });
  let (first_status, first) = post_delegate(&router, body.clone()).await?;
  assert_eq!(first_status, StatusCode::OK, "{first}");
  let (replay_status, replay) = post_delegate(&router, body).await?;
  assert_eq!(replay_status, StatusCode::OK, "{replay}");
  assert_eq!(replay["session_id"], first["session_id"]);
  assert_eq!(replay["run_id"], first["run_id"]);
  assert_eq!(first["thread_id"], first["run_id"]);

  let child_id = first["session_id"].as_str().expect("delegate returns a child session id").to_string();
  let run_id = first["run_id"].as_str().expect("delegate returns the first run id").to_string();
  let mut tracked_pid = None;
  settle_until(|| {
    let run = stack.store().read_run_sync(&child_id, &run_id)?;
    if let Some(pid) = run.as_ref().and_then(|record| record.pid)
      && tracked_pid != Some(pid)
    {
      guard.track(pid);
      tracked_pid = Some(pid);
    }
    Ok(run_records::terminal_outcome(&stack.run_events(&child_id)?, &run_id)?.is_some())
  })
  .await?;

  assert_eq!(run_records::terminal_outcome(&stack.run_events(&child_id)?, &run_id)?, Some(RunOutcomeKind::Success));
  assert_eq!(stack.store().child_ids(&parent.id)?, vec![child_id.clone()]);
  assert_eq!(stack.run_records(&child_id)?.iter().filter(|record| record.kind == RunKind::Work).count(), 1);
  let card = stack
    .store()
    .load_chat_events_sync(&parent.id)?
    .into_iter()
    .find(|event| {
      event["type"].as_str() == Some(event_types::TASK_DELEGATED)
        && event["child_session_id"].as_str() == Some(child_id.as_str())
    })
    .expect("the parent's chat holds the delegated-task card");
  assert_eq!(card["thread_id"].as_str(), Some(run_id.as_str()));

  // The completed child report schedules manager turns in the background. Wait for the report to be consumed and
  // track each turn's process so a test exit cannot leave one stopped at its launch gate.
  let mut tracked_parent_runs = HashSet::new();
  settle_until(|| {
    let events = stack.run_events(&parent.id)?;
    let runs = stack.run_records(&parent.id)?;
    let mut all_runs_terminal = true;
    for run in &runs {
      if let Some(pid) = run.pid
        && tracked_parent_runs.insert(run.id.clone())
      {
        guard.track(pid);
      }
      if run_records::terminal_outcome(&events, &run.id)?.is_none() {
        all_runs_terminal = false;
      }
    }
    let has_manager_turn = runs.iter().any(|run| run.kind == RunKind::ManagerTurn);
    Ok(
      has_manager_turn
        && all_runs_terminal
        && stack.dispatch.pending_inputs(&parent.id)?.is_empty()
        && !stack.child_reports(&parent.id)?.is_empty(),
    )
  })
  .await?;
  Ok(())
}

/// A failed worker Run reports its durable outcome and evidence once, then archives the stopped worker.
#[tokio::test]
async fn failed_worker_run_reports_to_parent_and_archives_worker() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_a_failed_worker_run_reports_once_and_archives_the_worker.
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 17").await?;
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let parent = stack.node("failure-parent", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("failing-worker", Some(&parent.id), TaskProfile::Worker, Some(script_run_spec())).await?;
  stack.queue_run(&worker.id, "run-failed-worker", RunKind::Work).await?;
  // Keep this test focused on report persistence; the parent's later turn has its own execution tests.
  stack.dispatch.set_executor(None);

  let settlement = stack.adapter.execute_run(&worker.id, "run-failed-worker").await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-failed-worker")?.and_then(|run| run.pid) {
    guard.track(pid);
  }

  assert_eq!(settlement.outcome.as_deref(), Some("failed"));
  let reports = child_reports(&stack, &parent.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!(["run:run-failed-worker"]));
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
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
/// re-enters the launch checks at boot and finishes through the worker adapter.
#[tokio::test]
async fn restart_before_launch_requeues_through_the_same_launch_checks() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_restart_before_launch_requeues_through_the_same_launch_checks.
  let guard = ProcessGroupGuard::new();
  let stack =
    Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"Synthetic worker finished"}'"#)
      .await?;
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(script_run_spec())).await?;
  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  stack.queue_run(&worker.id, "run-queued", RunKind::Work).await?;
  stack.claim(&worker.id, "run-queued").await?;

  stack.boot().await?;
  settle_until(|| Ok(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-queued")?.is_some())).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-queued")?.expect("the queued run is recorded");
  assert!(run.pid.is_some(), "the worker launch did not persist its process identity: {run:?}");
  guard.track(run.pid.expect("checked above"));
  assert_eq!(
    run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-queued")?,
    Some(RunOutcomeKind::Success)
  );
  Ok(())
}

/// `test_task_recovery.py::test_restart_after_launch_reattaches_live_process`: a recorded live process is followed,
/// not relaunched, and its result settles the original Run.
#[tokio::test]
async fn restart_after_launch_reattaches_live_process() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_restart_after_launch_reattaches_live_process.
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(script_run_spec())).await?;
  stack.queue_run(&worker.id, "run-live", RunKind::Work).await?;
  stage_raw_result(&stack.store().run_dir(&worker.id, "run-live"), Duration::from_secs(1))?;

  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let identity = identity::pin_process_identity(live.id());
  let pid_start = identity.pid_start.expect("the live process has a stat line");
  stack.launch(&worker.id, "run-live", live.id(), &pid_start).await?;

  stack.node_pass(&worker.id).await?;
  let key = (worker.id.clone(), "run-live".to_string());
  settle_until(|| Ok(stack.adapter.resume_follows().contains(&key))).await?;
  assert!(live.try_wait()?.is_none(), "the startup node pass returns while the worker process is alive");
  live.kill()?;
  live.wait()?;
  settle_until(|| Ok(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-live")?.is_some())).await?;
  settle_until(|| Ok(!stack.adapter.resume_follows().contains(&key))).await?;

  let run = stack.store().read_run_sync(&worker.id, "run-live")?.expect("the launched run is recorded");
  assert_eq!(run.pid, Some(live.id()));
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-live")?, Some(RunOutcomeKind::Success));
  Ok(())
}

/// A one-time stop of the thread that polls a launch, at a point inside one poll of the launch task. It stands in for
/// the synchronous work a launch can be doing when the server stops.
struct Hold {
  state: Mutex<HoldState>,
  changed: Condvar,
}

struct HoldState {
  entered: bool,
  released: bool,
}

impl Hold {
  const fn new() -> Self {
    Hold { state: Mutex::new(HoldState { entered: false, released: false }), changed: Condvar::new() }
  }

  /// Block the first caller until `release` and return true; every later call returns false at once.
  fn hold(&self) -> bool {
    let mut state = self.state.lock().expect("hold state is not poisoned");
    if state.entered {
      return false;
    }
    state.entered = true;
    while !state.released {
      state = self.changed.wait(state).expect("hold state is not poisoned");
    }
    true
  }

  fn entered(&self) -> bool {
    self.state.lock().expect("hold state is not poisoned").entered
  }

  fn release(&self) {
    self.state.lock().expect("hold state is not poisoned").released = true;
    self.changed.notify_all();
  }
}

/// The launch's backend build, which runs just before `worker_starting`.
static BUILD_HOLD: Hold = Hold::new();
/// The translation of the agent's first raw line, while the agent runs.
static TRANSLATE_HOLD: Hold = Hold::new();
const HELD_BUILD_TYPE: &str = "held_build";
const HELD_TRANSLATE_TYPE: &str = "held_translate";

/// Where a held backend stops at its hold.
#[derive(Clone, Copy)]
enum HoldPoint {
  Build,
  Translate,
}

/// A `/bin/sh` backend over its option's script, whose factory or translator stops at `hold`.
struct HeldFactory {
  hold: &'static Hold,
  point: HoldPoint,
}

impl BackendFactory for HeldFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("a held backend requires an option")?;
    let script = option.type_fields.get("script").and_then(serde_yaml::Value::as_str).context("no script")?;
    let translate_hold = match self.point {
      HoldPoint::Build => {
        self.hold.hold();
        None
      }
      HoldPoint::Translate => Some(self.hold),
    };
    Ok(Box::new(HeldBackend { script: script.to_string(), translate_hold }))
  }
}

struct HeldBackend {
  script: String,
  translate_hold: Option<&'static Hold>,
}

impl AgentBackend for HeldBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(HeldTranslate(self.translate_hold))
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

/// Translates each raw line to itself, except the line that waited at the hold: that one yields no event, so the
/// follow's next await after it is the raw log's metadata read, a blocking-pool call.
struct HeldTranslate(Option<&'static Hold>);

impl Translate for HeldTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    if self.0.is_some_and(Hold::hold) {
      return Ok(Vec::new());
    }
    Ok(vec![raw])
  }
}

fn held_type(type_name: &'static str, factory: HeldFactory) -> BackendTypeRegistration {
  BackendTypeRegistration {
    type_name,
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(factory),
    lifecycle: None,
    translate_fallback: false,
  }
}

fn register_held_types(registry: &mut charliebot_registry::Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.backend_type(held_type(HELD_BUILD_TYPE, HeldFactory { hold: &BUILD_HOLD, point: HoldPoint::Build }))?;
  registry
    .backend_type(held_type(HELD_TRANSLATE_TYPE, HeldFactory { hold: &TRANSLATE_HOLD, point: HoldPoint::Translate }))
}

/// The server's runtime (`charliebot-server` `run_server`).
fn server_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
  Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

/// Release `hold` once the runtime's blocking pool refuses work, which `Runtime::drop` starts after the let-go returns,
/// or after `limit`, whichever comes first. A launch that is still in its poll at the refusal sees each blocking-pool
/// call fail with `background task failed`.
fn release_at_teardown(hold: &'static Hold, runtime: tokio::runtime::Handle, limit: Duration) -> JoinHandle<()> {
  std::thread::spawn(move || {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
      let (sender, receiver) = mpsc::channel();
      drop(runtime.spawn_blocking(move || sender.send(())));
      // A refused closure is dropped unrun, which closes the channel.
      if receiver.recv().is_err() {
        break;
      }
      std::thread::sleep(Duration::from_millis(5));
    }
    hold.release();
  })
}

/// A worker under a manager whose input the stack dispatched, as a trigger that the boot recovers dispatches it. The
/// run's launch is in flight, or refused when the dispatch came after the let-go.
struct DispatchedWorker {
  stack: Stack,
  manager_id: String,
  worker_id: String,
  run_id: String,
}

/// Dispatch a worker's first input on a stack whose one option is `backend_type` running `script`. With `after_let_go`
/// the stack's let-go step runs first, as a dispatch that comes once the server's stop has begun.
async fn dispatched_worker(backend_type: &str, script: &str, after_let_go: bool) -> anyhow::Result<DispatchedWorker> {
  let option = BackendOption { backend_type: backend_type.to_string(), ..script_option(BACKEND_ID, script) };
  let stack =
    Stack::start_with_package(vec![option], vec![BACKEND_ID.to_string()], ("test.held_backends", register_held_types))
      .await?;
  test_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(script_run_spec())).await?;
  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  if after_let_go {
    recovery::let_go_step(Arc::clone(&stack.features)).await?;
  }
  let decision = stack.dispatch.dispatch_pending(&worker.id).await?;
  let run_id = decision["run_id"].as_str().with_context(|| format!("the dispatch names no run: {decision}"))?;
  Ok(DispatchedWorker { stack, manager_id: manager.id, worker_id: worker.id, run_id: run_id.to_string() })
}

/// Stop the server the way SIGINT does after its services stop: the let-go step, then the runtime's drop. The hold is
/// released at the drop, or after a bound when the let-go waits on the held launch.
fn stop_server(runtime: tokio::runtime::Runtime, stack: &Stack, hold: &'static Hold) -> anyhow::Result<()> {
  let releaser = release_at_teardown(hold, runtime.handle().clone(), Duration::from_millis(300));
  runtime.block_on(recovery::let_go_step(Arc::clone(&stack.features)))?;
  drop(runtime);
  releaser.join().expect("the hold releaser does not panic");
  Ok(())
}

/// The durable facts a cut launch leaves, which Python's next boot reads: no terminal fact, no error event in the run's
/// events log, no report to the parent, no close, and an open task.
fn assert_cut_launch_facts(cut: &DispatchedWorker) -> anyhow::Result<()> {
  let DispatchedWorker { stack, manager_id, worker_id, run_id } = cut;
  let facts = stack.run_events(worker_id)?;
  assert_eq!(run_records::terminal_outcome(&facts, run_id)?, None, "the cut run has no terminal fact");
  assert_eq!(child_reports(stack, manager_id)?.len(), 0, "the cut run reports nothing");
  assert_eq!(closes_of(stack, worker_id)?, 0, "the cut run closes nothing");
  assert_eq!(stack.store().task_state(worker_id)?, "open");
  let events_log = stack.store().run_dir(worker_id, run_id).join(run_records::RUN_EVENTS_NAME);
  let error_events = std::fs::read_to_string(&events_log)
    .unwrap_or_default()
    .lines()
    .filter(|line| serde_json::from_str::<Value>(line).is_ok_and(|event| event["type"] == "error"))
    .count();
  assert_eq!(error_events, 0, "the cut run's events log records no failure");
  Ok(())
}

/// The next server boot on the cut stack's home, with `scripts` behind its one option: the boot pass, then the cut run's
/// end and the parent's turn on the run's report.
async fn boot_after_cut(
  cut: &DispatchedWorker,
  scripts: &Scripts,
  guard: &ProcessGroupGuard,
  before_end: impl FnOnce(&PassCounters) -> anyhow::Result<()>,
) -> anyhow::Result<Stack> {
  let next = Stack::start_on_home(
    Arc::clone(&cut.stack.home),
    vec![scripts.option(BACKEND_ID, BACKEND_TYPE)],
    vec![BACKEND_ID.to_string()],
  )
  .await?;
  let counters = next.boot().await?;
  before_end(&counters)?;
  assert_eq!(wait_for_run(&next, guard, &cut.worker_id, &cut.run_id).await?, RunOutcomeKind::Success);
  settle_until(|| Ok(!run_ids_of_kind(&next, &cut.manager_id, RunKind::ManagerTurn)?.is_empty())).await?;
  settle_manager(&next, &cut.manager_id).await?;
  let reports = child_reports(&next, &cut.manager_id)?;
  assert_eq!(reports.len(), 1, "{reports:?}");
  assert_eq!(reports[0]["outcome"], "completed");
  assert_eq!(next.run_records(&cut.worker_id)?.len(), 1, "the next boot registers no second run");
  Ok(next)
}

/// A launch that the server's stop cuts before its agent starts, while the launch is still inside one poll. Python's
/// cancel passes the launch's `except Exception` handlers, so the run keeps no process, no end and no terminal fact, and
/// its task stays open (`task_execution.py:719`, `:861`, `:1488`). The next boot's dispatch launches the same run
/// (`_reconcile_node` step 4, `session_dispatch.py:455-497`).
#[test]
fn a_launch_the_stop_cuts_before_its_agent_starts_runs_at_the_next_boot() -> anyhow::Result<()> {
  let runtime = server_runtime()?;
  let cut = runtime.block_on(async {
    let cut = dispatched_worker(HELD_BUILD_TYPE, &result_script("never runs"), false).await?;
    settle_until(|| Ok(BUILD_HOLD.entered())).await?;
    anyhow::Ok(cut)
  })?;
  stop_server(runtime, &cut.stack, &BUILD_HOLD)?;

  let run = cut.stack.store().read_run_sync(&cut.worker_id, &cut.run_id)?.context("the cut run is recorded")?;
  assert_eq!((run.pid, run.started_at, run.ended_at, run.exit_code), (None, None, None, None), "{run:?}");
  assert_cut_launch_facts(&cut)?;

  server_runtime()?.block_on(async {
    let guard = ProcessGroupGuard::new();
    let scripts = Scripts::new(vec![result_script("relaunched"), result_script("report noted")]);
    let next = boot_after_cut(&cut, &scripts, &guard, |_| Ok(())).await?;
    let run = next.store().read_run_sync(&cut.worker_id, &cut.run_id)?.context("the run is recorded")?;
    assert!(run.pid.is_some(), "the next boot launched the cut run: {run:?}");
    anyhow::Ok(())
  })
}

/// A launch requested after the let-go began starts no task. Its run keeps the state that Python's launch task leaves
/// when `asyncio.run` cancels it before its first step (`task_execution.py:522`, `:731`; `asyncio/runners.py:72`): the
/// run registered with its input claimed, no process, no terminal fact, and an open task. The next boot's dispatch
/// launches it once.
#[test]
fn a_launch_requested_after_the_let_go_runs_once_at_the_next_boot() -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _capture = log.install();
  let runtime = server_runtime()?;
  let refused = runtime.block_on(dispatched_worker(BACKEND_TYPE, &result_script("never runs"), true))?;
  drop(runtime);
  let logged = log.text();
  assert!(logged.contains("run_launch_after_let_go"), "the launch after the let-go was not refused: {logged}");

  let run =
    refused.stack.store().read_run_sync(&refused.worker_id, &refused.run_id)?.context("the refused run is recorded")?;
  assert_eq!((run.pid, run.started_at, run.ended_at, run.exit_code), (None, None, None, None), "{run:?}");
  assert!(!run.input_event_ids.is_empty(), "the refused run keeps its claimed input: {run:?}");
  assert_cut_launch_facts(&refused)?;

  server_runtime()?.block_on(async {
    let guard = ProcessGroupGuard::new();
    let scripts = Scripts::new(vec![result_script("launched at the next boot"), result_script("report noted")]);
    boot_after_cut(&refused, &scripts, &guard, |_| Ok(())).await?;
    anyhow::Ok(())
  })
}

/// A launch whose agent runs when the server stops. Python's cancel leaves the agent running, with its process
/// identity recorded and no terminal fact (`agent_process/base.py:994-1012`). The next boot re-attaches the run, and
/// the agent's own result ends it (`_reconcile_node` step 2).
#[test]
fn a_launch_the_stop_cuts_while_its_agent_runs_is_reattached_at_the_next_boot() -> anyhow::Result<()> {
  let release = tempfile::tempdir()?;
  let release_file = release.path().join("release");
  let script = format!(
    "printf '%s\\n' '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; {}",
    json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "working"}]}}),
    release_file.display(),
    result_script("finished after the restart"),
  );
  let runtime = server_runtime()?;
  let cut = runtime.block_on(async {
    let cut = dispatched_worker(HELD_TRANSLATE_TYPE, &script, false).await?;
    settle_until(|| Ok(TRANSLATE_HOLD.entered())).await?;
    anyhow::Ok(cut)
  })?;
  let guard = ProcessGroupGuard::new();
  let run = cut.stack.store().read_run_sync(&cut.worker_id, &cut.run_id)?.context("the cut run is recorded")?;
  let pid = run.pid.context("the agent's identity is recorded before its first line")?;
  guard.track(pid);
  stop_server(runtime, &cut.stack, &TRANSLATE_HOLD)?;

  let run = cut.stack.store().read_run_sync(&cut.worker_id, &cut.run_id)?.context("the cut run is recorded")?;
  assert_eq!(run.pid, Some(pid));
  assert!(run.started_at.is_some() && run.ended_at.is_none() && run.exit_code.is_none(), "{run:?}");
  let pid_start = run.pid_start.clone().context("the cut run records its start tick")?;
  let stat = identity::read_pid_stat(pid).filter(|stat| stat.pid_start == pid_start);
  assert!(stat.is_some_and(|stat| stat.state != "Z"), "the agent keeps running after the stop");
  assert_cut_launch_facts(&cut)?;

  server_runtime()?.block_on(async {
    let scripts = Scripts::new(vec![result_script("report noted")]);
    let next = boot_after_cut(&cut, &scripts, &guard, |counters| {
      assert_eq!(counters.resumed, 1, "the live run is re-attached: {counters:?}");
      std::fs::write(&release_file, "")?;
      Ok(())
    })
    .await?;
    let run = next.store().read_run_sync(&cut.worker_id, &cut.run_id)?.context("the run is recorded")?;
    assert_eq!(run.pid, Some(pid), "the re-attached run keeps its agent");
    anyhow::Ok(())
  })
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
/// a launched worker run whose process died unseen drains from its raw log and lands the durable result.
#[tokio::test]
async fn a_drained_worker_run_lands_its_result_and_raw_end_time() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_drain_ended_at_is_raw_log_last_write_live_exit_keeps_write_time.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(script_run_spec())).await?;
  stack.queue_run(&worker.id, "run-drained", RunKind::Work).await?;
  stack.launch(&worker.id, "run-drained", 424_901, "1-424000").await?;
  let raw_mtime = stage_raw_result(&stack.store().run_dir(&worker.id, "run-drained"), Duration::from_secs(3600))?;

  stack.node_pass(&worker.id).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-drained")?.expect("the drained run is recorded");
  let ended_at = run.ended_at.expect("the drain lands an end time").into_inner();
  assert!((ended_at - time::OffsetDateTime::from(raw_mtime)).abs() < time::Duration::seconds(1));
  assert_eq!(
    run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-drained")?,
    Some(RunOutcomeKind::Success)
  );
  Ok(())
}

const REPO: &str = "/synthetic/repo";
/// A successful work run with real repo, branch and worktree provenance.
async fn finished_work_run(
  stack: &Stack,
  session_id: &str,
  run_id: &str,
  repo: &SyntheticRepo,
  branch: &str,
  worktree: &Path,
  started_at: Option<&str>,
) -> anyhow::Result<()> {
  let record = RunRecord {
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some(branch.to_string()),
    worktree_path: Some(worktree.to_string_lossy().into_owned()),
    started_at: started_at.map(UtcDateTime::parse).transpose()?,
    ..queued_work(session_id, run_id)
  };
  stack.register_with_task_spec(record, "synthetic implement task").await?;
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
  backend: &str,
) -> anyhow::Result<()> {
  let record = RunRecord {
    backend: Some(backend.to_string()),
    model: Some(format!("{backend}-model")),
    review_of_run_id: Some(work_run_id.to_string()),
    ..queued_run(session_id, review_id, RunKind::Review)
  };
  stack.register(record).await?;
  stack.finish(session_id, review_id, outcome).await?;
  Ok(())
}

fn test_access_key(stack: &Stack) -> anyhow::Result<()> {
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  Ok(())
}

fn worktree(repo: &SyntheticRepo, stack: &Stack, _session_id: &str, run_id: &str) -> anyhow::Result<(String, PathBuf)> {
  let branch = format!("task/{run_id}");
  let path = Path::new(&stack.cfg.paths.worktree_dir).join(charliebot_tasks::git::git_worktree_dir_name(&branch));
  repo.add_worktree(&path, &branch)?;
  Ok((branch, path))
}

fn child_reports(stack: &Stack, manager_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .run_events(manager_id)?
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_types::CHILD_REPORT))
      .map(|event| event.to_value())
      .collect(),
  )
}

fn repo_commit(repo: &SyntheticRepo, branch: &str) -> anyhow::Result<String> {
  let output = Command::new("git").arg("-C").arg(&repo.repo).args(["rev-parse", branch]).output()?;
  anyhow::ensure!(output.status.success(), "git rev-parse {branch} failed");
  Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

async fn wait_for_run(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  session_id: &str,
  run_id: &str,
) -> anyhow::Result<RunOutcomeKind> {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  let mut tracked = false;
  loop {
    let run = stack.store().read_run_sync(session_id, run_id)?.context("run record exists while waiting")?;
    if !tracked && let Some(pid) = run.pid {
      guard.track(pid);
      tracked = true;
    }
    if let Some(outcome) = run_records::terminal_outcome(&stack.run_events(session_id)?, run_id)? {
      return Ok(outcome);
    }
    anyhow::ensure!(std::time::Instant::now() < deadline, "run {run_id} did not finish within ten seconds");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
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
  Ok(stack.run_events(session_id)?.iter().filter(|event| event["type"].as_str() == Some("task_closed")).count())
}

/// A result event the scripted test backend translates as a successful terminal event.
fn result_script(summary: &str) -> String {
  let event = json!({"type": "result", "subtype": "success", "result": summary});
  format!("printf '%s\\n' '{}'", serde_json::to_string(&event).expect("JSON event serializes"))
}

/// `test_task_recovery.py::test_recovery_never_rereviews_a_successfully_reviewed_work_run`.
#[tokio::test]
async fn recovery_never_rereviews_a_successfully_reviewed_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_never_rereviews_a_successfully_reviewed_work_run
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-reviewed")?;
  finished_work_run(&stack, &worker.id, "run-reviewed", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-done", "run-reviewed", RunOutcomeKind::Success, "fake2").await?;

  for _round in 0..2 {
    stack.node_pass(&worker.id).await?;
    assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-done"]);
  }
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task`.
#[tokio::test]
async fn recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  stack.dispatch.set_executor(None);
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work-late")?;
  finished_work_run(&stack, &worker.id, "run-work-early", &repo, &branch, &worktree, Some("2026-10-18T09:00:00Z"))
    .await?;
  finished_work_run(&stack, &worker.id, "run-work-late", &repo, &branch, &worktree, Some("2026-10-18T09:30:00Z"))
    .await?;
  review_of(&stack, &worker.id, "review-done", "run-work-late", RunOutcomeKind::Success, "fake2").await?;

  for _round in 0..2 {
    stack.node_pass(&worker.id).await?;
  }
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?, ["review-done"]);
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "completed");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task`.
#[tokio::test]
async fn recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  test_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (early_branch, early_worktree) = worktree(&repo, &stack, &worker.id, "run-work-early")?;
  let (late_branch, late_worktree) = worktree(&repo, &stack, &worker.id, "run-work-late")?;
  finished_work_run(
    &stack,
    &worker.id,
    "run-work-early",
    &repo,
    &early_branch,
    &early_worktree,
    Some("2026-10-18T09:00:00Z"),
  )
  .await?;
  finished_work_run(
    &stack,
    &worker.id,
    "run-work-late",
    &repo,
    &late_branch,
    &late_worktree,
    Some("2026-10-18T09:30:00Z"),
  )
  .await?;
  let guard = ProcessGroupGuard::new();

  stack.node_pass(&worker.id).await?;
  let reviews = run_ids_of_kind(&stack, &worker.id, RunKind::Review)?;
  assert_eq!(reviews.len(), 1);
  let review = stack.store().read_run_sync(&worker.id, &reviews[0])?.expect("replayed review is recorded");
  assert_eq!(review.review_of_run_id.as_deref(), Some("run-work-late"));
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review.id).await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.len(), 1);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reports_blocked_when_the_pool_has_no_reviewer_member`.
#[tokio::test]
async fn recovery_reports_blocked_when_the_pool_has_no_reviewer_member() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reports_blocked_when_the_pool_has_no_reviewer_member
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  stack.dispatch.set_executor(None);
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;

  stack.node_pass(&worker.id).await?;
  assert!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.is_empty());
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("found no available reviewer backend"));
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_spawns_no_review_behind_a_registered_unstarted_work_run`.
#[tokio::test]
async fn recovery_spawns_no_review_behind_a_registered_unstarted_work_run() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_spawns_no_review_behind_a_registered_unstarted_work_run
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with_min_free_disk_gib(
    vec![script_option(BACKEND_ID, "exit 0")],
    vec![BACKEND_ID.to_string()],
    i64::MAX,
  )
  .await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work-early")?;
  finished_work_run(&stack, &worker.id, "run-work-early", &repo, &branch, &worktree, None).await?;
  stack.queue_run(&worker.id, "run-work-queued", RunKind::Work).await?;

  stack.boot().await?;
  settle_until(|| {
    Ok(stack.run_events(&worker.id)?.iter().any(|event| {
      event["type"].as_str() == Some(event_types::RUN_LAUNCH_WITHHELD)
        && event["run_id"].as_str() == Some("run-work-queued")
    }))
  })
  .await?;
  assert!(run_ids_of_kind(&stack, &worker.id, RunKind::Review)?.is_empty());
  let queued = stack.store().read_run_sync(&worker.id, "run-work-queued")?.expect("the queued run is recorded");
  assert_eq!(queued.pid, None);
  let events = stack.run_events(&worker.id)?;
  assert_eq!(run_records::terminal_outcome(&events, "run-work-queued")?, None);
  assert!(!events.iter().any(|event| event["type"].as_str() == Some("run_stop_requested")));
  Ok(())
}

/// `test_task_recovery.py::test_recovery_closed_task_skips_landing`.
#[tokio::test]
async fn recovery_closed_task_skips_landing() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_closed_task_skips_landing
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success, "fake2").await?;
  stack.close(&worker.id, "close-1").await?;

  let facts = stack.run_events(&worker.id)?.len();
  stack.node_pass(&worker.id).await?;
  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  assert_eq!(stack.run_events(&worker.id)?.len(), facts);
  assert!(!worktree.exists(), "closed completed work is cleaned up");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_reopened_task_reproves_landing`.
#[tokio::test]
async fn recovery_reopened_task_reproves_landing() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_reopened_task_reproves_landing
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  repo.publish_worktree(&worktree)?;
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success, "fake2").await?;
  stack.close(&worker.id, "close-1").await?;
  stack
    .completion
    .restore_task(&worker.id, "reopen-1", "recheck the delivery", &CallerIdentity::Operator { session_id: None })
    .await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");

  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_open_task_landing_unchanged`.
#[tokio::test]
async fn recovery_open_task_landing_unchanged() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_open_task_landing_unchanged
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  stack.dispatch.set_executor(None);
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  repo.publish_worktree(&worktree)?;
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-done", "run-work", RunOutcomeKind::Success, "fake2").await?;

  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "completed");
  let landed_ref = format!("landed:origin/main@{}", repo_commit(&repo, &branch)?);
  assert!(reports[0]["result_refs"].as_array().unwrap().iter().any(|value| value == &landed_ref));
  Ok(())
}

/// `test_task_recovery.py::test_recovery_after_a_failed_review_picks_the_next_preference_backend`.
#[tokio::test]
async fn recovery_after_a_failed_review_picks_the_next_preference_backend() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_after_a_failed_review_picks_the_next_preference_backend
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", "exit 0"),
      script_option("fake2", "exit 0"),
      script_option("fake3", &result_script("review ok")),
    ],
    vec!["fake2".to_string(), "fake3".to_string()],
  )
  .await?;
  test_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-failed", "run-work", RunOutcomeKind::Failed, "fake2").await?;
  let guard = ProcessGroupGuard::new();

  stack.node_pass(&worker.id).await?;
  let reviews = run_ids_of_kind(&stack, &worker.id, RunKind::Review)?;
  assert_eq!(reviews.len(), 2);
  let retry_id = reviews.iter().find(|run_id| run_id.as_str() != "review-failed").context("retry is registered")?;
  assert_eq!(retry_id, &charliebot_store::control_events::stable_run_id(&worker.id, "review:run-work:2"));
  let retry = stack.store().read_run_sync(&worker.id, retry_id)?.expect("retry record is readable");
  assert_eq!(retry.backend.as_deref(), Some("fake3"));
  assert_eq!(retry.review_of_run_id.as_deref(), Some("run-work"));
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &retry.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// Append one run's events log, as the worker writes it: one JSON object per line.
fn write_run_events(stack: &Stack, session_id: &str, run_id: &str, events: &[Value]) -> anyhow::Result<()> {
  let path = stack.store().run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME);
  std::fs::create_dir_all(path.parent().context("a run events log has a run directory")?)?;
  let text: String = events.iter().map(|event| format!("{event}\n")).collect();
  std::fs::write(path, text)?;
  Ok(())
}

/// A cold-read child of `manager` over a page file in `dir`, with the page's sha256 as its report states it.
async fn cold_read_child(
  stack: &Stack,
  manager: &SessionMetadata,
  dir: &Path,
) -> anyhow::Result<(SessionMetadata, PathBuf, String)> {
  let page = dir.join("page.html");
  let text = "<html><body><p>sitrep body</p></body></html>";
  std::fs::write(&page, text)?;
  let worker = stack.node("cold-read", Some(&manager.id), TaskProfile::Worker, Some(cold_read_spec(&page))).await?;
  Ok((worker, page, charliebot_store::control_events::sha256_hex(text)))
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_delivery`: a cold-read run that succeeded and whose
/// delivery a crash cut is replayed: the parent gets the report body and the child closes. A repeated pass lands nothing
/// twice.
#[tokio::test]
async fn recovery_replays_a_lost_cold_read_delivery() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_delivery.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let dir = tempfile::tempdir()?;
  let (worker, page, digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold", RunKind::Work).await?;
  write_run_events(&stack, &worker.id, "run-cold", &[json!({"type": "result", "result": "(1) answered."})])?;
  stack.finish(&worker.id, "run-cold", RunOutcomeKind::Success).await?;

  stack.boot().await?;
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(report["outcome"], "completed");
  assert_eq!(
    report["summary"],
    format!("cold read of {}\npage sha256 {digest}\nbackend fake\n(1) answered.", page.display())
  );
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");

  // A repeated pass lands nothing twice.
  stack.boot().await?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(stack.run_records(&worker.id)?.len(), 1);
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

/// A parent chat log holding a native transcript user event must not fail the child's recovery pass.
///
/// The native event carries `message.content` as a plain string. Delivering the child's report primes the
/// parent's aggregator; a failed prime leaves the child unrecovered and the boot pass without its marker.
#[tokio::test]
async fn recovery_accepts_native_user_events_with_string_content() -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _guard = log.install();
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  stack
    .store()
    .append_chat_event(
      &manager.id,
      json!({
        "type": "user",
        "message": {"role": "user", "content": "synthetic native prompt"},
        "parentUuid": null,
        "sessionId": manager.id,
        "uuid": "synthetic-native-event",
        "timestamp": "2026-01-01T00:00:00Z",
      }),
    )
    .await?;
  let dir = tempfile::tempdir()?;
  let (worker, _page, _digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold", RunKind::Work).await?;
  write_run_events(&stack, &worker.id, "run-cold", &[json!({"type": "result", "result": "(1) answered."})])?;
  stack.finish(&worker.id, "run-cold", RunOutcomeKind::Success).await?;

  stack.boot().await?;

  assert!(!log.text().contains("task_recovery_node_failed"), "{}", log.text());
  assert!(boot_marker_path(&stack).exists(), "a clean pass writes the boot marker");
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_failure_chain`: a failed cold-read run whose chain a
/// crash cut with one backend left. The replay records the attempt, starts the retry on the next pool backend, and the
/// retry's answer delivers the report.
#[tokio::test]
async fn recovery_replays_a_lost_cold_read_failure_chain() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_failure_chain.
  let stack = Stack::start_with(
    vec![script_option(BACKEND_ID, "exit 0"), script_option("fake2", &result_script("(1) answered."))],
    vec!["fake2".to_string()],
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let dir = tempfile::tempdir()?;
  let (worker, page, digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&worker.id, "run-cold-1", RunOutcomeKind::Failed).await?;

  stack.boot().await?;
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(report["outcome"], "completed");
  assert_eq!(
    report["summary"],
    format!(
      "cold read of {}\npage sha256 {digest}\nattempt fake failed: run run-cold-1 failed without a reportable output\n\
       backend fake2\n(1) answered.",
      page.display()
    )
  );
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");

  // A repeated pass lands nothing twice.
  stack.boot().await?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Work)?.len(), 2);
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_all_failed_report`: with no backend left, the replay
/// reports the parent with the attempt line and the all-failed trailer, and the report's delivery archives the child. A
/// repeated pass lands nothing twice.
#[tokio::test]
async fn recovery_replays_a_lost_cold_read_all_failed_report() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_all_failed_report.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let dir = tempfile::tempdir()?;
  let (worker, page, digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&worker.id, "run-cold-1", RunOutcomeKind::Failed).await?;

  stack.boot().await?;
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(report["outcome"], "failed");
  assert_eq!(
    report["summary"],
    format!(
      "cold read of {}\npage sha256 {digest}\nattempt fake failed: run run-cold-1 failed without a reportable output\n\
       every model failed (1 tried)",
      page.display()
    )
  );
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  let closes = stack.closes(&worker.id)?;
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["outcome"], "archived");
  assert_eq!(closes[0]["actor"], "system");
  assert_eq!(closes[0]["request_id"], "unsuccessful:run-cold-1");

  // A repeated pass lands nothing twice.
  stack.boot().await?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(stack.run_records(&worker.id)?.len(), 1);
  assert_eq!(stack.closes(&worker.id)?.len(), 1);
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_a_lost_cold_read_interrupted_report`: an interrupted cold-read run
/// reports at replay exactly as the live chain's fall-through does, never through the next-backend retry. The report's
/// delivery archives the child. A repeated pass lands nothing twice.
#[tokio::test]
async fn recovery_replays_a_lost_cold_read_interrupted_report() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_replays_a_lost_cold_read_interrupted_report.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let dir = tempfile::tempdir()?;
  let (worker, _page, _digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&worker.id, "run-cold-1", RunOutcomeKind::Interrupted).await?;

  stack.boot().await?;
  let outcomes = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect::<Vec<_>>();
  assert_eq!(outcomes, [json!("interrupted")]);
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Work)?, ["run-cold-1"], "no retry run follows");
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  let closes = stack.closes(&worker.id)?;
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["actor"], "system");
  assert_eq!(closes[0]["request_id"], "unsuccessful:run-cold-1");
  assert_eq!(closes[0]["summary"], "run run-cold-1 ended interrupted; archived by the server");

  // A repeated pass lands nothing twice.
  stack.boot().await?;
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  assert_eq!(stack.closes(&worker.id)?.len(), 1);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_cold_read_failure_with_a_registered_retry_never_double_retries`: a failed
/// cold-read run whose retry a crash left registered but unlaunched. The replay launches the queued retry and never
/// re-drives the failed attempt's chain, so there is one retry run and one report.
#[tokio::test]
async fn recovery_cold_read_failure_with_a_registered_retry_never_double_retries() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_recovery_cold_read_failure_with_a_registered_retry_never_double_retries.
  let stack = Stack::start_with(
    vec![script_option(BACKEND_ID, "exit 0"), script_option("fake2", &result_script("(1) answered."))],
    vec!["fake2".to_string()],
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let dir = tempfile::tempdir()?;
  let (worker, page, digest) = cold_read_child(&stack, &manager, dir.path()).await?;
  stack.queue_run(&worker.id, "run-cold-1", RunKind::Work).await?;
  stack.finish(&worker.id, "run-cold-1", RunOutcomeKind::Failed).await?;
  let retry = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake2-model".to_string()),
    ..synthetic_run(&worker.id, "run-cold-2", RunKind::Work)
  };
  stack.register(retry).await?;

  stack.boot().await?;
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(report["outcome"], "completed");
  assert_eq!(
    report["summary"],
    format!("cold read of {}\npage sha256 {digest}\nbackend fake2\n(1) answered.", page.display())
  );
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  assert_eq!(run_ids_of_kind(&stack, &worker.id, RunKind::Work)?, ["run-cold-1", "run-cold-2"]);
  Ok(())
}

/// `test_task_recovery.py::test_recovery_replays_an_earlier_failed_run_without_archiving_a_node_a_later_run_finished`:
/// a failed earlier run's report replay is idempotent, and a later successful run still owns the review follow-up.
#[tokio::test]
async fn recovery_replays_failed_earlier_run_without_archiving_worker() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::
  //   test_recovery_replays_an_earlier_failed_run_without_archiving_a_node_a_later_run_finished.
  let stack = Stack::start("exit 0").await?;
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_spec(Path::new(REPO)))).await?;
  stack.queue_run(&worker.id, "run-a", RunKind::Work).await?;
  stack.finish(&worker.id, "run-a", RunOutcomeKind::Failed).await?;
  stack.queue_run(&worker.id, "run-b", RunKind::Work).await?;
  stack.finish(&worker.id, "run-b", RunOutcomeKind::Success).await?;

  stack.boot().await?;
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.iter().map(|report| report["outcome"].as_str()).collect::<Vec<_>>(), [Some("failed")]);
  assert_eq!(reports[0]["result_refs"], json!(["run:run-a"]));
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert_eq!(closes_of(&stack, &worker.id)?, 0);
  settle_manager(&stack, &manager.id).await?;
  Ok(())
}

/// `test_task_recovery.py::test_recovery_archives_an_open_worker_whose_failure_report_the_parent_already_holds`: an
/// existing parent report is not delivered or dispatched again, and recovery archives the worker exactly once.
#[tokio::test]
async fn recovery_archives_worker_after_deduplicating_its_existing_failure_report() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::
  //   test_recovery_archives_an_open_worker_whose_failure_report_the_parent_already_holds.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-failed", RunKind::Work).await?;
  stack.record_terminal(&worker.id, "run-failed", RunOutcomeKind::Failed).await?;
  let source = stack
    .run_events(&worker.id)?
    .into_iter()
    .rev()
    .find(|event| {
      event["type"].as_str() == Some(event_types::RUN_FINISHED) && event["run_id"].as_str() == Some("run-failed")
    })
    .context("the failed worker run has its terminal event")?;
  stack.dispatch.set_executor(None);
  let result_refs = vec!["run:run-failed".to_string()];
  let (_report, created) = stack
    .dispatch
    .deliver_child_report(&worker.id, source, "failed", "run run-failed failed", Some(&result_refs), Some(&manager.id))
    .await?;
  assert!(created, "the setup delivery creates the parent's report");
  assert_eq!(child_reports(&stack, &manager.id)?.len(), 1);

  stack.node_pass(&worker.id).await?;
  assert_eq!(child_reports(&stack, &manager.id)?.len(), 1, "replay does not append a second report");
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  assert_eq!(closes_of(&stack, &worker.id)?, 1);

  stack.node_pass(&worker.id).await?;
  assert_eq!(child_reports(&stack, &manager.id)?.len(), 1, "a repeated pass does not append another report");
  assert_eq!(closes_of(&stack, &worker.id)?, 1, "a repeated pass does not archive twice");
  Ok(())
}

/// `test_task_recovery.py::test_reconcile_replays_an_already_delivered_blocked_report_without_duplication`.
#[tokio::test]
async fn reconcile_replays_an_already_delivered_blocked_report_without_duplication() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_reconcile_replays_an_already_delivered_blocked_report_without_duplication
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_spec(&repo.repo))).await?;
  stack.dispatch.set_executor(None);
  let (branch, worktree) = worktree(&repo, &stack, &worker.id, "run-work")?;
  let commit = Command::new("git")
    .arg("-C")
    .arg(&worktree)
    .args(["commit", "--allow-empty", "-m", "unlanded synthetic change"])
    .output()?;
  anyhow::ensure!(commit.status.success(), "the synthetic worktree commit succeeds");
  finished_work_run(&stack, &worker.id, "run-work", &repo, &branch, &worktree, None).await?;
  review_of(&stack, &worker.id, "review-replay-1", "run-work", RunOutcomeKind::Success, "fake2").await?;
  review_of(&stack, &worker.id, "review-replay-2", "run-work", RunOutcomeKind::Success, "fake3").await?;

  stack.node_pass(&worker.id).await?;
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("ended without landing work run"));
  let worker_facts = stack.run_events(&worker.id)?.len();
  let manager_facts = stack.run_events(&manager.id)?.len();

  stack.node_pass(&worker.id).await?;
  assert_eq!(child_reports(&stack, &manager.id)?.len(), 1);
  assert_eq!(stack.run_events(&worker.id)?.len(), worker_facts);
  assert_eq!(stack.run_events(&manager.id)?.len(), manager_facts);
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  Ok(())
}

/// The loop child's close fact and report counts, read from the store.
fn closes(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .run_events(session_id)?
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
      .map(|event| event.to_value())
      .collect(),
  )
}

fn stop_requests(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<usize> {
  Ok(
    stack
      .run_events(session_id)?
      .iter()
      .filter(|event| {
        event["type"].as_str() == Some(event_types::RUN_STOP_REQUESTED) && event["run_id"].as_str() == Some(run_id)
      })
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
    kind: RunKind::Iteration,
    sequence_ref: Some(SequenceRef { kind: LOOP_KIND.to_string(), owner_ref: manager_id.to_string(), position }),
    ..queued_work(child_id, run_id)
  };
  stack.register(record).await
}

/// One scheduled step of the probe controller: a queued run of the child that carries the probe's sequence reference.
fn probe_step(session_id: &str, run_id: &str) -> RunRecord {
  RunRecord {
    kind: RunKind::ScheduledStep,
    sequence_ref: Some(SequenceRef {
      kind: PROBE_KIND.to_string(),
      owner_ref: "cron:nightly@2026-10-08T00:00Z".to_string(),
      position: 0,
    }),
    ..queued_run(session_id, run_id, RunKind::ScheduledStep)
  }
}

/// `test_sequence_controller_runs.py::test_a_finished_sequence_run_asks_its_owning_controller_after_run`: a durable
/// finish asks the owning controller's after-run duty. A cron step's finish re-drives its firing, and a running loop's
/// iteration leaves its child open. No owned run logs the no-controller warning.
#[tokio::test]
async fn a_finished_sequence_run_asks_its_owning_controller_after_run() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_a_finished_sequence_run_asks_its_owning_controller_after_run.
  let stack = Stack::start("exit 0").await?;
  // A running loop owes no close yet, so its close step returns and the child stays open.
  let (manager, child) = loop_child(&stack, marker("running", None)).await?;
  stack.register(probe_step(&child, "step-0")).await?;
  iteration(&stack, &child, &manager, "iter-1", 1).await?;

  stack.adapter.after_worker_run_for_test(&child, "step-0", "success").await?;
  assert_eq!(stack.probe_calls(&child)?, ["after_run step-0"]);

  let log = LogCapture::default();
  let _guard = log.install();
  stack.adapter.after_worker_run_for_test(&child, "iter-1", "success").await?;
  assert!(!log.text().contains("sequence_run_without_controller"), "an owned run logs no warning: {}", log.text());
  assert_eq!(stack.store().task_state(&child)?, "open");
  Ok(())
}

/// `test_sequence_controller_runs.py::test_recovery_asks_the_owning_controller_and_counts_one_follow_up`: recovery
/// replays a terminal step and an unlaunched one, one follow-up each, and leaves a launched, non-terminal step to the
/// earlier pass.
#[tokio::test]
async fn recovery_asks_the_owning_controller_and_counts_one_follow_up() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_recovery_asks_the_owning_controller_and_counts_one_follow_up.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let child = stack.node("leaf", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.register(probe_step(&child.id, "step-fin")).await?;
  stack.register(probe_step(&child.id, "step-queued")).await?;
  stack.register(probe_step(&child.id, "step-live")).await?;
  stack.record_terminal(&child.id, "step-fin", RunOutcomeKind::Success).await?;
  stack.launch(&child.id, "step-live", 999_999, "1-424000").await?;

  let mut counters = PassCounters::default();
  recovery::replay_followups(&stack.features, &stack.adapter, &child.id, &mut counters).await?;

  assert_eq!(counters.followups, 2);
  assert_eq!(stack.probe_calls(&child.id)?, ["recover_run step-fin success", "recover_run step-queued none"]);
  let events = stack.run_events(&child.id)?;
  assert!(run_records::terminal_outcome(&events, "step-live")?.is_none(), "the launched step keeps no terminal fact");
  Ok(())
}

/// The owning package of an orphan Run is gone: no registered controller claims its kind, and the Run it left behind
/// outlives it.
const ORPHAN_KIND: &str = "deleted_feature";
const ORPHAN_OWNER: &str = "deleted:loop-1";

/// A queued Run whose sequence kind no registered controller claims.
fn orphan_run(session_id: &str, run_id: &str) -> RunRecord {
  RunRecord {
    kind: RunKind::Iteration,
    sequence_ref: Some(SequenceRef { kind: ORPHAN_KIND.to_string(), owner_ref: ORPHAN_OWNER.to_string(), position: 1 }),
    ..queued_run(session_id, run_id, RunKind::Iteration)
  }
}

/// Assert that the log holds exactly one `sequence_run_without_controller` warning, naming the session, the run and the
/// owner_ref.
fn assert_one_no_controller_warning(log: &LogCapture, session_id: &str, run_id: &str) {
  let logged = log.text();
  let warnings: Vec<&str> = logged.lines().filter(|line| line.contains("sequence_run_without_controller")).collect();
  assert_eq!(warnings.len(), 1, "one no-controller warning: {logged}");
  assert!(warnings[0].contains(session_id), "the warning names the session: {}", warnings[0]);
  assert!(warnings[0].contains(&format!("run_id={run_id}")), "the warning names the run: {}", warnings[0]);
  assert!(warnings[0].contains(&format!("owner_ref={ORPHAN_OWNER}")), "the warning names the owner: {}", warnings[0]);
}

/// `test_sequence_controller_runs.py::test_a_finish_without_a_controller_warns_and_does_nothing_else`: a Run whose kind
/// no controller owns logs the warning with the session, run and owner_ref, and its finish lands nothing.
#[tokio::test]
async fn a_finish_without_a_controller_warns_and_does_nothing_else() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_a_finish_without_a_controller_warns_and_does_nothing_else.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let leaf = stack.node("leaf", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.register(orphan_run(&leaf.id, "orphan")).await?;
  let log = LogCapture::default();
  let _guard = log.install();

  stack.adapter.after_worker_run_for_test(&leaf.id, "orphan", "success").await?;

  assert_one_no_controller_warning(&log, &leaf.id, "orphan");
  let stored = stack.store().read_run_sync(&leaf.id, "orphan")?.expect("the queued run stays registered");
  assert_eq!(stored.pid, None, "nothing started");
  assert!(run_records::terminal_outcome(&stack.run_events(&leaf.id)?, "orphan")?.is_none(), "nothing landed");
  Ok(())
}

/// One replay pass over a node, returning the log lines the pass wrote.
async fn replay_pass(stack: &Stack, session_id: &str, counters: &mut PassCounters) -> anyhow::Result<LogCapture> {
  let log = LogCapture::default();
  let _guard = log.install();
  recovery::replay_followups(&stack.features, &stack.adapter, session_id, counters).await?;
  Ok(log)
}

/// The `run_finished` facts a node holds for one run.
fn run_finished_count(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<usize> {
  Ok(
    stack
      .run_events(session_id)?
      .iter()
      .filter(|event| {
        event["type"].as_str() == Some(event_types::RUN_FINISHED) && event["run_id"].as_str() == Some(run_id)
      })
      .count(),
  )
}

/// `test_sequence_controller_runs.py::test_recovery_without_a_controller_lands_interrupted_then_only_logs`: recovery of a
/// registered, unlaunched Run whose controller is gone lands its interrupted finish once and logs the warning. A second
/// pass finds the terminal fact and only logs.
#[tokio::test]
async fn recovery_without_a_controller_lands_interrupted_then_only_logs() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_recovery_without_a_controller_lands_interrupted_then_only_logs.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let leaf = stack.node("leaf", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.register(orphan_run(&leaf.id, "orphan")).await?;
  let mut counters = PassCounters::default();

  let first = replay_pass(&stack, &leaf.id, &mut counters).await?;

  assert_eq!(counters.followups, 0);
  assert_one_no_controller_warning(&first, &leaf.id, "orphan");
  let stored = stack.store().read_run_sync(&leaf.id, "orphan")?.expect("the orphan run stays registered");
  assert!(stored.ended_at.is_some(), "the interrupted finish writes the run's end");
  let events = stack.run_events(&leaf.id)?;
  assert_eq!(run_records::terminal_outcome(&events, "orphan")?, Some(RunOutcomeKind::Interrupted));
  assert_eq!(run_finished_count(&stack, &leaf.id, "orphan")?, 1);

  let second = replay_pass(&stack, &leaf.id, &mut counters).await?;

  assert_eq!(counters.followups, 0, "a terminal Run only logs");
  assert_one_no_controller_warning(&second, &leaf.id, "orphan");
  assert_eq!(run_finished_count(&stack, &leaf.id, "orphan")?, 1, "the second pass lands nothing twice");
  Ok(())
}

/// `test_sequence_controller_runs.py::test_recovery_follows_a_live_sequence_run_without_its_controller`: a launched,
/// non-terminal Run without its controller is followed like any other live Run, with no interrupted finish and no
/// warning.
#[tokio::test]
async fn recovery_follows_a_live_sequence_run_without_its_controller() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_recovery_follows_a_live_sequence_run_without_its_controller.
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let leaf = stack.node("leaf", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.register(orphan_run(&leaf.id, "orphan")).await?;
  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let pid_start = identity::pin_process_identity(live.id()).pid_start.expect("the live process has a stat line");
  stack.launch(&leaf.id, "orphan", live.id(), &pid_start).await?;
  let log = LogCapture::default();
  let _log_guard = log.install();

  let counters = stack.node_pass(&leaf.id).await?;

  let key = (leaf.id.clone(), "orphan".to_string());
  assert!(stack.adapter.resume_follows().contains(&key), "the live run is followed in the background");
  assert_eq!(counters.resumed, 1);
  assert_eq!(counters.followups, 0);
  assert!(!log.text().contains("sequence_run_without_controller"), "a live run logs no warning: {}", log.text());
  assert!(run_records::terminal_outcome(&stack.run_events(&leaf.id)?, "orphan")?.is_none());
  assert!(live.try_wait()?.is_none(), "the node pass leaves the live run running");
  live.kill()?;
  live.wait()?;
  settle_until(|| Ok(!stack.adapter.resume_follows().contains(&key))).await?;
  Ok(())
}

/// `test_sequence_controller_runs.py::test_a_launch_without_a_controller_fails_loudly_naming_the_owner_ref`: a launch
/// that reaches a controller-less sequence Run fails before any process starts. The error names the owner_ref, rides the
/// run's own events log, and lands the failed finish.
#[tokio::test]
async fn a_launch_without_a_controller_fails_loudly_naming_the_owner_ref() -> anyhow::Result<()> {
  // Python: tests/test_sequence_controller_runs.py::test_a_launch_without_a_controller_fails_loudly_naming_the_owner_ref.
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let leaf = stack.node("leaf", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.register(orphan_run(&leaf.id, "orphan")).await?;

  let observation = SequenceExecutor::launch_and_settle(&stack.adapter, &leaf.id, "orphan", None).await?;

  assert_eq!(observation.withheld, None);
  assert_eq!(observation.outcome.as_deref(), Some("failed"));
  let stored = stack.store().read_run_sync(&leaf.id, "orphan")?.expect("the orphan run stays registered");
  assert_eq!(stored.pid, None, "no process ever started");
  let events_log = stack.store().run_dir(&leaf.id, "orphan").join(run_records::RUN_EVENTS_NAME);
  let error_events =
    std::fs::read_to_string(&events_log)?.lines().map(serde_json::from_str::<Value>).collect::<Result<Vec<_>, _>>()?;
  assert_eq!(error_events.len(), 1, "the launch's one durable evidence: {error_events:?}");
  assert_eq!(error_events[0]["type"], event_types::ERROR);
  let message = error_events[0]["message"].as_str().unwrap_or_default();
  assert_eq!(
    message,
    format!(
      "TaskInvalidError: sequence run orphan carries owner_ref {} and no registered controller owns its kind",
      charliebot_core::pyfmt::str_repr(ORPHAN_OWNER)
    ),
    "the error matches Python's class and message"
  );
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&leaf.id)?, "orphan")?, Some(RunOutcomeKind::Failed));
  Ok(())
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
  stage_raw_result(&stack.store().run_dir(&child, "iter-run-1"), Duration::from_secs(1))?;
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

/// A dead iteration drains its result after the sequence controller marks the loop interrupted and closes its child.
#[tokio::test]
async fn startup_interrupts_a_loop_whose_iteration_process_died_and_names_the_drain() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_recovery_closes_an_interrupted_loop_whose_process_died.
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("running", close_owed("failed", "[Synthetic loop 1] failed"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stack.launch(&child, "iter-run-1", 424_902, "1-424000").await?;
  stage_raw_result(&stack.store().run_dir(&child, "iter-run-1"), Duration::from_secs(3600))?;

  stack.boot().await?;

  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  assert_eq!(closes(&stack, &child)?.len(), 1);
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&child)?, "iter-run-1")?, Some(RunOutcomeKind::Success));
  Ok(())
}

/// `test_task_sequences.py::test_startup_recovery_leaves_a_live_iterations_child_open_until_its_finish`:
/// a live iteration keeps its child open. The boot pass marks the loop interrupted and leaves the process running, and
/// its re-attach runs in the background until the process finishes.
#[tokio::test]
async fn startup_leaves_a_live_iterations_child_open_until_its_finish() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_startup_recovery_leaves_a_live_iterations_child_open_until_its_finish.
  let guard = ProcessGroupGuard::new();
  let stack = Stack::start("exit 0").await?;
  let (manager, child) =
    loop_child(&stack, marker("running", close_owed("failed", "[Synthetic loop 1] failed"))).await?;
  iteration(&stack, &child, &manager, "iter-run-1", 1).await?;
  stage_raw_result(&stack.store().run_dir(&child, "iter-run-1"), Duration::from_secs(1))?;
  let mut live = Command::new("/bin/sleep").arg("30").process_group(0).spawn()?;
  guard.track(live.id());
  let pid_start = identity::pin_process_identity(live.id()).pid_start.expect("the live process has a stat line");
  stack.launch(&child, "iter-run-1", live.id(), &pid_start).await?;

  stack.boot().await?;
  let key = (child.clone(), "iter-run-1".to_string());
  settle_until(|| Ok(stack.adapter.resume_follows().contains(&key))).await?;

  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  assert_eq!(stack.store().task_state(&child)?, "open");
  assert!(closes(&stack, &child)?.is_empty());
  assert!(live.try_wait()?.is_none(), "the startup pass leaves the live iteration running");
  live.kill()?;
  live.wait()?;
  settle_until(|| Ok(!stack.adapter.resume_follows().contains(&key))).await?;
  assert_eq!(stack.store().task_state(&child)?, "cancelled");
  assert_eq!(closes(&stack, &child)?.len(), 1);
  Ok(())
}

/// The text an out-of-space error carries, as the store flattens it.
fn enospc_text() -> String {
  std::io::Error::from_raw_os_error(libc::ENOSPC).to_string()
}

/// Poll until `condition` holds, failing after ten seconds.
async fn settle_until(mut condition: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
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

/// `test_task_execution.py::test_manager_turn_master_done_enospc_retries_through_master_queue`: the manager turn's
/// MASTER_DONE write fails out of space, so the turn lands no terminal fact. The retry's node pass re-attaches the dead
/// turn through the master queue, and the resumed follow lands the end record with the raw log's last write time.
#[tokio::test]
async fn manager_turn_master_done_enospc_retries_through_master_queue() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_manager_turn_master_done_enospc_retries_through_master_queue.
  let stack = Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"turn result"}'"#).await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  std::fs::write(stack.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
  let guard = ProcessGroupGuard::new();
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::MASTER_DONE,
    &[manager.id.as_str()],
    1,
    libc::ENOSPC,
  );
  stack.dispatch.admit_input(&manager.id, event_types::USER, "Take off.", "user", json!({})).await?;
  let decision = stack.dispatch.dispatch_pending(&manager.id).await?;
  let run_id = decision["run_id"].as_str().expect("the manager dispatch names its run").to_string();

  settle_until(|| Ok(stack.store().read_run_sync(&manager.id, &run_id)?.is_some_and(|run| run.pid.is_some()))).await?;
  let pid =
    stack.store().read_run_sync(&manager.id, &run_id)?.and_then(|run| run.pid).expect("the turn records its pid");
  guard.track(pid);
  settle_until(|| Ok(run_records::terminal_outcome(&stack.run_events(&manager.id)?, &run_id)?.is_some())).await?;

  assert_eq!(fault.hits(), [Hit::Failed, Hit::Written], "the live MASTER_DONE failed and the retry's write landed");
  let run = stack.store().read_run_sync(&manager.id, &run_id)?.expect("the run is recorded");
  let outcome = run_records::terminal_outcome(&stack.run_events(&manager.id)?, &run_id)?;
  assert_eq!(outcome, Some(RunOutcomeKind::Success));
  let raw_log = stack.store().run_dir(&manager.id, &run_id).join(RAW_LOG_NAME);
  let raw_mtime = std::fs::metadata(&raw_log)?.modified()?;
  let ended_at = run.ended_at.expect("the retry lands the end time").into_inner();
  let raw_written = time::OffsetDateTime::from(raw_mtime);
  let delta = ended_at - raw_written;
  assert!(delta.abs() < time::Duration::seconds(1), "ended_at {ended_at} is not the raw log's write {raw_written}");

  let follow = (manager.id.clone(), run_id.clone());
  settle_until(|| Ok(!stack.adapter.resume_follows().contains(&follow))).await?;
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  Ok(())
}

#[tokio::test]
async fn recovery_stops_a_launched_run_on_a_closed_worker() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_startup_recovery_stops_a_launched_run_on_a_closed_worker.
  let stack = Stack::start("exit 0").await?;
  let worker = stack.node("worker", None, TaskProfile::Worker, None).await?;
  let process_guard = ProcessGroupGuard::new();
  let mut command = Command::new("/bin/sleep");
  command.arg("60").process_group(0);
  let mut process = command.spawn()?;
  let pid = process.id();
  process_guard.track(pid);
  let pid_start = identity::read_pid_stat(pid).context("reading the synthetic worker pid")?.pid_start;
  stack.queue_run(&worker.id, "run-live-closed", RunKind::Work).await?;
  stack.launch(&worker.id, "run-live-closed", pid, &pid_start).await?;

  let guard = stack.store().control_lock().await;
  let request_id = "cancel-before-stop-fact";
  let close_event = charliebot_store::control_events::build_control_event(
    event_types::TASK_CLOSED,
    "user",
    &worker.id,
    Some(&charliebot_store::control_events::stable_close_event_id(&worker.id, request_id)),
    None,
    [
      ("request_id".to_string(), json!(request_id)),
      ("outcome".to_string(), json!("cancelled")),
      ("summary".to_string(), json!("stop")),
      ("result_refs".to_string(), json!([])),
      ("run_ids".to_string(), json!([])),
      ("report_to".to_string(), Value::Null),
    ],
  );
  stack.store().append_control_fact(&guard, &worker.id, close_event).await?;
  drop(guard);

  stack.node_pass(&worker.id).await?;

  let events = stack.run_events(&worker.id)?;
  let stop_ids = stop_request_ids(&events, "run-live-closed");
  assert_eq!(stop_ids, [closed_task_stop("run-live-closed")]);
  assert_eq!(run_records::terminal_outcome(&events, "run-live-closed")?, Some(RunOutcomeKind::Interrupted));
  assert!(process.try_wait()?.is_some());
  Ok(())
}

/// `test_task_recovery.py::test_retry_round_skips_runs_this_process_already_drives`:
/// a node pass that a retry runs skips worker and manager runs this process already drives, so no second follower or
/// review starts.
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
  register_cron: bool,
) -> anyhow::Result<()> {
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack =
    if register_cron { Stack::start_with_cron_controller("exit 0").await? } else { Stack::start("exit 0").await? };
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

  let counters = stack.boot().await.expect("the boot pass returns Ok");

  assert_eq!(counters.nodes, 2, "the boot pass visits both nodes");
  assert_eq!(counters.drained, 1, "the manager node drains its dead turn");
  settle_manager(&stack, &manager.id).await?;
  let manager_events = stack.run_events(&manager.id)?;
  assert!(
    run_records::terminal_outcome(&manager_events, "run-manager-dead")?.is_some(),
    "the manager node recovers its dead turn while the sequence node is visited"
  );
  let refused = stack.store().read_run_sync(&sequence.id, "run-sequence")?.expect("the queued run is recorded");
  assert_eq!(refused.pid, None, "the queued sequence run never launches during recovery");
  let sequence_events = stack.run_events(&sequence.id)?;
  assert!(
    run_records::terminal_outcome(&sequence_events, "run-sequence")?.is_none(),
    "the queued sequence run keeps no terminal fact"
  );
  if register_cron {
    assert_eq!(counters.followups, 1, "the registered cron controller recovers the queued sequence run");
    assert!(
      !log.text().contains("task_recovery_node_failed"),
      "the registered cron node is not refused: {}",
      log.text()
    );
  } else {
    assert_logged_node_failure(&log, sequence_kind);
  }
  Ok(())
}

/// A loop child that a prior process left running beside its manager's dead turn: the registered controller marks the
/// loop interrupted, and the boot pass drains the manager's turn past it.
#[tokio::test]
async fn a_running_loop_beside_a_dead_manager_turn_leaves_the_turn_to_recover() -> anyhow::Result<()> {
  // Python: tests/test_task_sequences.py::test_restart_recovery_marks_improve_loop_through_registered_controller. The
  // loop kind is the synthetic controller's; the improve controller itself is registered by charliebot-improve.
  let stack = Stack::start("exit 0").await?;
  // The drain builds the manager run's child environment, which signs a run token with `charliebot.access_key`.
  stack.write_access_key()?;
  let (manager, child) = loop_child(&stack, marker("running", None)).await?;
  stack.queue_run(&manager, "run-manager-dead", RunKind::ManagerTurn).await?;
  stack.launch(&manager, "run-manager-dead", 424_821, "1-424000").await?;

  let counters = stack.boot().await?;

  assert_eq!(counters.drained, 1, "the manager node drains its dead turn");
  assert_eq!(stack.read_marker(&child)?.status, "interrupted");
  settle_manager(&stack, &manager).await?;
  let manager_events = stack.run_events(&manager)?;
  assert!(
    run_records::terminal_outcome(&manager_events, "run-manager-dead")?.is_some(),
    "the manager node recovers its dead turn beside the loop"
  );
  Ok(())
}

/// A refused `improve` run stops only its own node: the boot pass logs the named error and the manager node still
/// recovers its dead turn, with the improve node first and with it second. There is no Python case, since Python runs
/// the improve controller; this covers the interim refusal of `UNLANDED_SEQUENCE_KINDS`.
#[tokio::test]
async fn a_refused_improve_node_leaves_the_manager_node_to_recover() -> anyhow::Result<()> {
  for sequence_first in [true, false] {
    boot_past_a_refused_sequence_node("improve", RunKind::Iteration, sequence_first, false)
      .await
      .with_context(|| format!("with the sequence node first: {sequence_first}"))?;
  }
  Ok(())
}

/// The same recovery for a `cron` run, in both node orders: the registered cron controller recovers the queued run
/// without launching it, while the manager node still drains its dead turn.
#[tokio::test]
async fn a_refused_cron_node_leaves_the_manager_node_to_recover() -> anyhow::Result<()> {
  // Python: tests/test_cron_sequence.py::test_recovery_redrives_a_mid_chain_firing_from_durable_facts
  for sequence_first in [true, false] {
    boot_past_a_refused_sequence_node("cron", RunKind::ScheduledStep, sequence_first, true)
      .await
      .with_context(|| format!("with the sequence node first: {sequence_first}"))?;
  }
  Ok(())
}

/// fills ended_at from the raw log's last write and exit_code from its result line, and writes no second end record.
#[tokio::test]
async fn retry_repairs_a_half_written_end_record_from_the_raw_log() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_retry_and_boot_reconcile_repair_half_written_end_metadata.
  let stack = Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"work done"}'"#).await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  stack.write_access_key()?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let task = TaskSpec { goal: "do the work".to_string(), ..TaskSpec::default() };
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(task)).await?;
  let fault = write_faults::inject_run_record_write_fault(stack.store(), true, 1, libc::ENOSPC);

  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let decision = stack.dispatch.dispatch_pending(&worker.id).await?;
  let run_id = decision["run_id"].as_str().expect("the dispatch names its run").to_string();
  assert_eq!(stack.wait_terminal(&worker.id, &run_id).await?, RunOutcomeKind::Success);

  let hits = fault.hits();
  assert_eq!(hits[..2], [Hit::Failed, Hit::Written], "the end write failed once; the retry's write landed");
  let raw_log = stack.store().run_dir(&worker.id, &run_id).join(RAW_LOG_NAME);
  let raw_written = time::OffsetDateTime::from(std::fs::metadata(&raw_log)?.modified()?);
  let run = stack.store().read_run_sync(&worker.id, &run_id)?.expect("the run is recorded");
  let ended_at = run.ended_at.expect("the retry lands the end time").into_inner();
  let delta = ended_at - raw_written;
  assert!(delta.abs() < time::Duration::seconds(1), "ended_at {ended_at} is not the raw log's write {raw_written}");
  assert_eq!(run.exit_code, Some(0), "the raw log's successful result event");
  let finished = stack
    .run_events(&worker.id)?
    .iter()
    .filter(|event| {
      event["type"].as_str() == Some(event_types::RUN_FINISHED) && event["run_id"].as_str() == Some(run_id.as_str())
    })
    .count();
  assert_eq!(finished, 1, "the retry never wrote a second end record");
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  // The success reports to the manager; wait for the turn it launches before the stack drops.
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  settle_until(|| Ok(!stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.is_empty())).await?;
  settle_manager(&stack, &manager.id).await?;
  assert!(stack.dispatch.pending_inputs(&manager.id)?.is_empty());
  Ok(())
}
/// delivers the failed report exactly once.
#[tokio::test]
async fn boot_node_out_of_space_hands_node_to_retry() -> anyhow::Result<()> {
  // Python: tests/test_task_recovery.py::test_boot_node_out_of_space_hands_node_to_retry.
  let stack = Stack::start("exit 0").await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("worker", Some(&manager.id), TaskProfile::Worker, None).await?;
  stack.queue_run(&worker.id, "run-failed", RunKind::Work).await?;
  stack.finish(&worker.id, "run-failed", RunOutcomeKind::Failed).await?;
  assert!(child_reports(&stack, &manager.id)?.is_empty());

  let fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::CHILD_REPORT,
    &[manager.id.as_str()],
    1,
    libc::ENOSPC,
  );
  stack.boot().await?;

  settle_until(|| Ok(child_reports(&stack, &manager.id)?.len() == 1)).await?;
  assert!(fault.hits().contains(&Hit::Failed), "the boot pass's delivery write failed out of space");
  assert_eq!(child_reports(&stack, &manager.id)?[0]["outcome"], "failed");
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  assert_eq!(child_reports(&stack, &manager.id)?.len(), 1, "exactly one report");
  Ok(())
}

fn boot_marker_path(stack: &Stack) -> PathBuf {
  stack.cfg.charliebot_home.join("state").join("boot_recovery.json")
}

fn write_python_boot_marker(stack: &Stack, pass_started_at: &str) -> anyhow::Result<()> {
  let path = boot_marker_path(stack);
  std::fs::create_dir_all(path.parent().expect("boot marker has a parent directory"))?;
  std::fs::write(&path, format!("{{\"pass_started_at\": \"{pass_started_at}\"}}"))?;
  Ok(())
}

/// Backdates a session's two durable files to `before` ahead of the boot marker, as Python's `_stamp_durable_files`
/// does: a skip needs both files past the mtime slack before the marker.
fn backdate_durable_files(stack: &Stack, session_id: &str, before: Duration) -> anyhow::Result<()> {
  let marker: Value = serde_json::from_slice(&std::fs::read(boot_marker_path(stack))?)?;
  let pass_started_at =
    UtcDateTime::parse(marker["pass_started_at"].as_str().context("the marker has no pass start")?)?;
  let stamp = SystemTime::from(pass_started_at.into_inner()) - before;
  for path in [stack.store().chat_events_path(session_id), stack.store().metadata_path(session_id)] {
    std::fs::File::open(path)?.set_modified(stamp)?;
  }
  Ok(())
}

#[derive(Default)]
struct RecoveryWorkerOptions {
  task: Option<TaskSpec>,
  outcome: Option<RunOutcomeKind>,
  sequence_ref: Option<SequenceRef>,
  worktree_path: Option<String>,
  close: bool,
}

async fn add_recovery_worker(
  stack: &Stack,
  request_id: &str,
  run_id: &str,
  options: RecoveryWorkerOptions,
) -> anyhow::Result<SessionMetadata> {
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack.node(request_id, Some(&manager.id), TaskProfile::Worker, options.task).await?;
  let mut run = synthetic_run(&worker.id, run_id, RunKind::Work);
  run.backend = Some(BACKEND_ID.to_string());
  run.model = Some(format!("{BACKEND_ID}-model"));
  run.sequence_ref = options.sequence_ref;
  run.worktree_path = options.worktree_path;
  stack.register(run).await?;
  if let Some(outcome) = options.outcome {
    stack.record_terminal(&worker.id, run_id, outcome).await?;
  }
  if options.close {
    stack.close(&worker.id, "close-worker").await?;
  }
  Ok(worker)
}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .ancestors()
    .nth(4)
    .expect("the tasks crate has a repository root ancestor")
    .to_path_buf()
}

fn python_marker_instant(path: &Path) -> anyhow::Result<String> {
  let repo_root = repo_root();
  let helper = repo_root.join("rust/crates/runtime/tasks/tests/boot_recovery_marker_reference.py");
  let output = Command::new("uv").args(["run", "python"]).arg(helper).arg(path).current_dir(repo_root).output()?;
  anyhow::ensure!(
    output.status.success(),
    "uv run python failed:\nstdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(String::from_utf8(output.stdout)?.trim_end().to_string())
}

fn path_lexists(path: &Path) -> anyhow::Result<bool> {
  match path.symlink_metadata() {
    Ok(_) => Ok(true),
    Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(false),
    Err(error) => Err(error.into()),
  }
}

#[tokio::test]
async fn clean_boot_writes_marker_and_the_next_boot_skips_a_closed_worker() -> anyhow::Result<()> {
  // Python: tests/test_boot_recovery_skip.py:95.
  let stack = Stack::start("exit 0").await?;
  let worker = add_recovery_worker(
    &stack,
    "worker",
    "run-success",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;
  let marker_path = boot_marker_path(&stack);
  assert!(!marker_path.exists(), "a new home has no boot-recovery marker");

  let first = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(first.nodes, 2);
  assert_eq!(first.full, 2);
  assert_eq!(first.skipped, 0);
  let first_marker: Value = serde_json::from_slice(&std::fs::read(&marker_path)?)?;
  let first_time = first_marker["pass_started_at"].as_str().expect("marker stores its pass start time").to_string();
  assert!(!marker_path.with_extension("json.tmp").exists(), "the atomic rename consumes the temp file");

  // The first pass's files sit within the mtime slack of its marker, so they are backdated past it before pass 2.
  backdate_durable_files(&stack, &worker.id, Duration::from_secs(2))?;
  let second = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(second.nodes, 2);
  assert_eq!(second.full, 1);
  assert_eq!(second.skipped, 1);
  assert_eq!(second.marker.as_deref(), Some(first_time.as_str()));
  assert_eq!(second.keep_reasons.get("state_open"), Some(&1));
  assert_eq!(stack.store().run_backend(&worker.id).as_deref(), Some(BACKEND_ID));
  let second_marker: Value = serde_json::from_slice(&std::fs::read(&marker_path)?)?;
  assert!(second_marker["pass_started_at"].as_str().expect("marker stores its pass start time") > first_time.as_str());
  Ok(())
}

/// Writes `content` as the boot marker, then runs one boot pass over a closed worker and its open manager. The unreadable
/// marker reads as absent: both nodes get a full check, one `boot_recovery_marker_unreadable` line names the byte size
/// and `error_type`, and the clean pass rewrites the marker with the start time it ran under.
async fn assert_unreadable_boot_marker_reads_as_absent(content: &[u8], error_type: &str) -> anyhow::Result<()> {
  // Python: tests/test_boot_recovery_skip.py::test_unreadable_marker_checks_every_node_then_is_rewritten.
  let stack = Stack::start("exit 0").await?;
  add_recovery_worker(
    &stack,
    "unreadable-worker",
    "run-unreadable",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;
  let marker_path = boot_marker_path(&stack);
  std::fs::create_dir_all(marker_path.parent().expect("boot marker has a parent directory"))?;
  std::fs::write(&marker_path, content)?;
  let log = LogCapture::default();
  let _subscriber = log.install();
  let before = UtcDateTime::now();
  let outcome = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await;
  let after = UtcDateTime::now();
  assert!(outcome.is_ok(), "{error_type}: the unreadable marker must not stop the pass: {outcome:?}");
  let counters = outcome?;
  assert_eq!((counters.full, counters.skipped), (2, 0), "{error_type}: every node gets a full check");

  let logged = log.text();
  let unreadable: Vec<&str> = logged.lines().filter(|line| line.contains("boot_recovery_marker_unreadable")).collect();
  assert_eq!(unreadable.len(), 1, "{error_type}: one unreadable-marker line: {logged}");
  let fields: Vec<&str> = unreadable[0].split_whitespace().collect();
  let size = format!("size={}", content.len());
  assert!(fields.contains(&size.as_str()), "{error_type}: the line carries the byte size: {}", unreadable[0]);
  let error = format!("error_type=\"{error_type}\"");
  assert!(fields.contains(&error.as_str()), "{error_type}: the line names the exception: {}", unreadable[0]);

  let rewritten: Value = serde_json::from_slice(&std::fs::read(&marker_path)?)?;
  let instant = UtcDateTime::parse(rewritten["pass_started_at"].as_str().expect("the clean pass rewrites the marker"))?;
  assert!(
    before <= instant && instant <= after,
    "{error_type}: the rewritten marker holds the pass start: {rewritten}"
  );
  Ok(())
}

#[tokio::test]
async fn an_empty_boot_marker_reads_as_absent() -> anyhow::Result<()> {
  assert_unreadable_boot_marker_reads_as_absent(b"", "JSONDecodeError").await
}

#[tokio::test]
async fn a_boot_marker_that_is_not_json_reads_as_absent() -> anyhow::Result<()> {
  assert_unreadable_boot_marker_reads_as_absent(b"not json", "JSONDecodeError").await
}

#[tokio::test]
async fn a_boot_marker_that_is_not_an_object_reads_as_absent() -> anyhow::Result<()> {
  assert_unreadable_boot_marker_reads_as_absent(b"[]", "TypeError").await
}

#[tokio::test]
async fn a_boot_marker_without_pass_started_at_reads_as_absent() -> anyhow::Result<()> {
  assert_unreadable_boot_marker_reads_as_absent(b"{}", "KeyError").await
}

#[tokio::test]
async fn a_boot_marker_with_a_bad_timestamp_reads_as_absent() -> anyhow::Result<()> {
  assert_unreadable_boot_marker_reads_as_absent(br#"{"pass_started_at": "x"}"#, "ValueError").await
}

#[tokio::test]
async fn a_boot_marker_with_a_non_string_timestamp_reads_as_absent() -> anyhow::Result<()> {
  // No Python test case: the TypeError that src/runtime/task_recovery.py:58-74 raises for a non-string timestamp.
  assert_unreadable_boot_marker_reads_as_absent(br#"{"pass_started_at": 5}"#, "TypeError").await
}

#[tokio::test]
async fn a_boot_marker_that_is_not_utf8_reads_as_absent() -> anyhow::Result<()> {
  // No Python test case: the UnicodeDecodeError that src/runtime/task_recovery.py:58-74 raises before parsing.
  assert_unreadable_boot_marker_reads_as_absent(b"\xff\xfe{}", "UnicodeDecodeError").await
}

#[tokio::test]
async fn failed_boot_keeps_the_previous_marker_and_out_of_space_still_retries() -> anyhow::Result<()> {
  // Python: tests/test_boot_recovery_skip.py:114.
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  let _settled_worker = add_recovery_worker(
    &stack,
    "settled-worker",
    "run-settled",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;
  stack.boot().await?;
  let marker_path = boot_marker_path(&stack);
  let before = std::fs::read(&marker_path)?;

  let root_manager = stack.node("report-manager", None, TaskProfile::Manager, None).await?;
  let failed_worker = stack.node("report-worker", Some(&root_manager.id), TaskProfile::Worker, None).await?;
  stack.queue_run(&failed_worker.id, "run-failed", RunKind::Work).await?;
  stack.record_terminal(&failed_worker.id, "run-failed", RunOutcomeKind::Failed).await?;
  let fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::CHILD_REPORT,
    &[root_manager.id.as_str()],
    1,
    libc::ENOSPC,
  );

  stack.boot().await?;
  assert!(fault.hits().contains(&Hit::Failed), "the out-of-space report failed during the boot pass");
  assert_eq!(std::fs::read(&marker_path)?, before, "the failed pass leaves the previous marker unchanged");
  settle_until(|| Ok(!stack.child_reports(&root_manager.id)?.is_empty())).await?;
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  assert_eq!(std::fs::read(&marker_path)?, before, "the end-landing retry does not advance the boot marker");
  assert!(log.text().contains("task_recovery_node_failed"));
  Ok(())
}

#[tokio::test]
async fn breaking_each_skip_condition_alone_keeps_the_worker_in_full_recovery() -> anyhow::Result<()> {
  // Python: tests/test_boot_recovery_skip.py:144.
  for condition in
    ["state_open", "file_after_marker", "run_unended", "sequence_ref", "worktree_exists", "cold_read_failed"]
  {
    let stack = Stack::start("exit 0").await?;
    let task = if condition == "cold_read_failed" {
      let page = stack.home.path().join("synthetic-page.html");
      std::fs::write(&page, "<html><body>synthetic</body></html>")?;
      Some(cold_read_spec(&page))
    } else {
      None
    };
    let worktree = (condition == "worktree_exists").then(|| stack.home.path().join("live-worktree"));
    if let Some(path) = &worktree {
      std::fs::create_dir(path)?;
    }
    let sequence_ref = (condition == "sequence_ref").then(|| SequenceRef {
      kind: "retry".to_string(),
      owner_ref: "synthetic-owner".to_string(),
      position: 0,
    });
    let worker = add_recovery_worker(
      &stack,
      "condition-worker",
      "run-condition",
      RecoveryWorkerOptions {
        task,
        outcome: (condition != "run_unended").then_some(if condition == "cold_read_failed" {
          RunOutcomeKind::Failed
        } else {
          RunOutcomeKind::Success
        }),
        sequence_ref,
        worktree_path: worktree.map(|path| path.display().to_string()),
        close: condition != "state_open",
      },
    )
    .await?;
    write_python_boot_marker(&stack, "2099-01-01T00:00:00+00:00")?;
    if condition == "file_after_marker" {
      let metadata = std::fs::File::open(stack.store().metadata_path(&worker.id))?;
      metadata.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(4_102_444_800))?;
    }

    let counters = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
    assert_eq!(counters.skipped, 0, "{condition} must prevent this worker from being skipped");
    assert_eq!(counters.full, 2, "the open manager and the kept worker receive full checks: {condition}");
    let expected_reason_count = if condition == "state_open" { 2 } else { 1 };
    assert_eq!(counters.keep_reasons.get(condition), Some(&expected_reason_count), "first blocker for {condition}");
  }
  Ok(())
}

#[tokio::test]
async fn dangling_worktree_symlink_keeps_a_closed_worker_in_full_recovery() -> anyhow::Result<()> {
  // No Python test case: os.path.lexists in src/runtime/task_recovery.py:119 (23de57ca8).
  use std::os::unix::fs::symlink;

  let stack = Stack::start("exit 0").await?;
  let dangling = stack.home.path().join("missing-worktree");
  symlink(stack.home.path().join("absent-target"), &dangling)?;
  let worker = add_recovery_worker(
    &stack,
    "symlink-worker",
    "run-symlink",
    RecoveryWorkerOptions {
      outcome: Some(RunOutcomeKind::Success),
      worktree_path: Some(dangling.display().to_string()),
      close: true,
      ..RecoveryWorkerOptions::default()
    },
  )
  .await?;
  write_python_boot_marker(&stack, "2099-01-01T00:00:00+00:00")?;

  let counters = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(counters.full, 2);
  assert_eq!(counters.skipped, 0);
  assert_eq!(counters.keep_reasons.get("worktree_exists"), Some(&1));
  assert_eq!(stack.store().task_state(&worker.id)?, "completed");
  Ok(())
}

#[tokio::test]
async fn worktree_path_under_a_regular_file_reads_as_absent_and_the_boot_pass_continues() -> anyhow::Result<()> {
  // Python: src/runtime/task_recovery.py:119.
  let stack = Stack::start("exit 0").await?;
  let blocker = stack.home.path().join("regular-file");
  std::fs::write(&blocker, "synthetic")?;
  let under_file = blocker.join("worktree");
  assert!(!path_lexists(&under_file)?, "a path under a regular file is absent");
  let worker = add_recovery_worker(
    &stack,
    "enotdir-worker",
    "run-enotdir",
    RecoveryWorkerOptions {
      outcome: Some(RunOutcomeKind::Success),
      worktree_path: Some(under_file.display().to_string()),
      close: true,
      ..RecoveryWorkerOptions::default()
    },
  )
  .await?;
  write_python_boot_marker(&stack, "2099-01-01T00:00:00+00:00")?;
  let log = LogCapture::default();
  let _guard = log.install();

  let counters = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(counters.full, 1, "only the open manager gets a full check");
  assert_eq!(counters.skipped, 1, "the closed worker is skipped, as Python skips it");
  let logged = log.text();
  let warnings: Vec<&str> = logged.lines().filter(|line| line.contains("task_recovery_worktree_stat_failed")).collect();
  assert_eq!(warnings.len(), 1, "one warning for the unreadable path: {logged}");
  assert!(warnings[0].contains(&worker.id), "the warning names the session: {}", warnings[0]);
  assert!(warnings[0].contains(&under_file.display().to_string()), "the warning names the path: {}", warnings[0]);
  Ok(())
}

#[tokio::test]
async fn end_landing_retry_runs_the_full_node_pass_after_boot_skip() -> anyhow::Result<()> {
  // Python: tests/test_boot_recovery_skip.py:191.
  let stack = Stack::start("exit 0").await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(20));
  let worker = add_recovery_worker(
    &stack,
    "retry-worker",
    "run-retry",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;
  write_python_boot_marker(&stack, "2099-01-01T00:00:00+00:00")?;
  let before = stack.adapter.reconcile_node_calls();
  let boot = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(boot.skipped, 1);
  assert_eq!(stack.adapter.reconcile_node_calls(), before + 1, "the boot pass checks only the open manager");

  stack.adapter.start_landing_retry_for_test(&worker.id);
  settle_until(|| Ok(stack.adapter.reconcile_node_calls() == before + 2)).await?;
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  assert_eq!(stack.adapter.reconcile_node_calls(), before + 2, "the retry runs one full worker check");
  Ok(())
}

#[tokio::test]
async fn rust_reads_python_isoformat_marker_with_and_without_microseconds() -> anyhow::Result<()> {
  // No Python test case: the marker text that src/runtime/task_recovery.py:58-74 reads and writes.
  for timestamp in ["2026-01-02T03:04:05+00:00", "2026-01-02T03:04:05.123456+00:00"] {
    let stack = Stack::start("exit 0").await?;
    std::fs::create_dir_all(stack.cfg.sessions_dir())?;
    write_python_boot_marker(&stack, timestamp)?;

    let counters = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
    assert_eq!(counters.marker.as_deref(), Some(timestamp));
  }
  Ok(())
}

#[tokio::test]
async fn python_fromisoformat_reads_the_rust_written_marker_instant() -> anyhow::Result<()> {
  // No Python test case: the marker text that src/runtime/task_recovery.py:58-74 reads and writes.
  let stack = Stack::start("exit 0").await?;
  std::fs::create_dir_all(stack.cfg.sessions_dir())?;
  recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  let marker_path = boot_marker_path(&stack);
  let marker: Value = serde_json::from_slice(&std::fs::read(&marker_path)?)?;
  let rust_instant = UtcDateTime::parse(marker["pass_started_at"].as_str().expect("marker has a pass start"))?;
  assert_eq!(python_marker_instant(&marker_path)?, rust_instant.to_python_isoformat());
  Ok(())
}

#[tokio::test]
async fn recovery_done_logs_one_line_with_marker_and_skip_counters() -> anyhow::Result<()> {
  // No Python test case: one done line per pass (src/runtime/task_recovery.py:189-190, server.py:214-223).
  let log = LogCapture::default();
  let _subscriber = log.install();
  let stack = Stack::start("exit 0").await?;
  let worker = add_recovery_worker(
    &stack,
    "logged-worker",
    "run-logged",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;

  recovery::recovery_step(Arc::clone(&stack.features)).await?;
  backdate_durable_files(&stack, &worker.id, Duration::from_secs(2))?;
  recovery::recovery_step(Arc::clone(&stack.features)).await?;
  let text = log.text();
  assert_eq!(text.matches("task_tree_recovery_done").count(), 2, "one done line per pass: {text}");
  assert!(text.contains("full=2"), "first pass counts the full checks: {text}");
  assert!(text.contains("skipped=0"), "first pass has no skips: {text}");
  assert!(text.contains("full=1"), "second pass checks the open manager: {text}");
  assert!(text.contains("skipped=1"), "second pass skips the closed worker: {text}");
  assert!(text.contains("marker="), "the log carries the marker used by the pass: {text}");
  assert!(text.contains("keep_reasons="), "the log carries each first blocker: {text}");
  Ok(())
}

#[tokio::test]
async fn skipped_worker_warms_the_display_backend_without_a_full_check() -> anyhow::Result<()> {
  // No Python test case: the display warm-up of src/runtime/task_recovery.py:166-168.
  let stack = Stack::start("exit 0").await?;
  let worker = add_recovery_worker(
    &stack,
    "display-worker",
    "run-display",
    RecoveryWorkerOptions { outcome: Some(RunOutcomeKind::Success), close: true, ..RecoveryWorkerOptions::default() },
  )
  .await?;
  assert_eq!(stack.store().run_backend(&worker.id), None);
  write_python_boot_marker(&stack, "2099-01-01T00:00:00+00:00")?;

  let counters = recovery::reconcile_task_tree(&stack.features, &stack.adapter).await?;
  assert_eq!(counters.skipped, 1);
  assert_eq!(counters.full, 1, "the open manager still receives a full check");
  assert_eq!(stack.store().run_backend(&worker.id).as_deref(), Some(BACKEND_ID));
  Ok(())
}
