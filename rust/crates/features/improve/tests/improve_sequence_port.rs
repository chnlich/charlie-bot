#[path = "../../../runtime/tasks/tests/process_guard.rs"]
#[allow(dead_code)]
mod process_guard;
#[path = "../../../runtime/tasks/tests/recovery_support.rs"]
mod recovery_support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use charliebot_agents::backend_pool::{self, BackendPool};
use charliebot_core::event_types;
use charliebot_core::repo_root::RepoRoot;
use charliebot_improve::{command, landing, register, sequence};
use charliebot_registry::Registry;
use charliebot_registry::backend::{
  BackendLifecycle, BackendTraits, Launch, LaunchContext, LaunchRefused, ResumeStyle,
};
use charliebot_registry::sequence::SequenceTree;
use charliebot_store::StoreResult;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunOutcomeKind, SequenceRef};
use charliebot_tasks::dispatch::DispatchExecutor;
use charliebot_tasks::spawner_backends::{self, SubagentRole};
use futures_util::future::BoxFuture;
use recovery_support::{Scripts, Stack, SyntheticRepo};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};
use tokio::task::{AbortHandle, JoinHandle};

const SESSION_REQUEST: &str = "improve-port-manager";
const TEST_BACKEND_TYPE: &str = "improve_port_script";
const QUOTA_RESET: &str = "2999-01-01T00:00:00Z";

static QUOTA_REJECTED_HOMES: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static GIT_OVERRIDE_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

#[derive(Default)]
struct ParentWakes(Arc<Mutex<Vec<String>>>);

impl DispatchExecutor for ParentWakes {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    _pending_inputs: &'a [charliebot_store::ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    let session_id = session_id.to_string();
    let wakes = Arc::clone(&self.0);
    Box::pin(async move {
      wakes.lock().expect("wake list is not poisoned").push(session_id);
      Ok(None)
    })
  }
}

struct QuotaOnceLifecycle;

impl BackendLifecycle for QuotaOnceLifecycle {
  fn place<'a>(&'a self, context: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      if context.option.id == "quota"
        && QUOTA_REJECTED_HOMES
          .lock()
          .expect("quota rejection list is not poisoned")
          .insert(context.cfg.charliebot_home.clone())
      {
        return Err(LaunchRefused {
          message: "Claude account pool exhausted; earliest reset 2999-01-01 00:00 UTC".to_string(),
          quota_exhausted: true,
          resets_at: Some(UtcDateTime::parse(QUOTA_RESET).expect("the fixed reset time parses")),
        });
      }
      Ok(Launch {
        backend_kwargs: Map::new(),
        resume_id: context.held_native_id.clone(),
        prompt: None,
        account_label: None,
        private: None,
      })
    })
  }
}

fn register_improve_port(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.backend_type(recovery_support::scripted_type(
    TEST_BACKEND_TYPE,
    BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    Some(Arc::new(QuotaOnceLifecycle)),
  ))?;
  register(registry)
}

struct ControllerTask {
  handle: JoinHandle<()>,
  _abort_on_drop: AbortOnDrop,
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
  fn drop(&mut self) {
    self.0.abort();
  }
}

struct LoopHarness {
  stack: Stack,
  _repo: SyntheticRepo,
  manager: SessionMetadata,
  child: SessionMetadata,
  state: command::ImproveState,
  scripts: Scripts,
  wakes: Arc<Mutex<Vec<String>>>,
  process_guard: process_guard::ProcessGroupGuard,
  _git_override_guard: AsyncMutexGuard<'static, ()>,
}

impl LoopHarness {
  async fn new(pool_ids: &[&str], extra_ids: &[&str], scripts: Vec<String>) -> anyhow::Result<LoopHarness> {
    let git_override_guard = GIT_OVERRIDE_LOCK.lock().await;
    let scripts = Scripts::new(scripts);
    let mut option_ids = if pool_ids.is_empty() { vec!["fake"] } else { pool_ids.to_vec() };
    for id in extra_ids {
      if !option_ids.contains(id) {
        option_ids.push(id);
      }
    }
    let options = option_ids.iter().map(|id| scripts.option(id, TEST_BACKEND_TYPE)).collect();
    let pool = pool_ids.iter().map(|id| id.to_string()).collect();
    let stack = Stack::start_with_package(options, pool, ("test.improve_port", register_improve_port)).await?;
    stack.write_access_key()?;
    let repo = SyntheticRepo::create()?;
    let manager = stack
      .node(
        SESSION_REQUEST,
        None,
        TaskProfile::Manager,
        Some(TaskSpec { goal: "synthetic manager".to_string(), ..TaskSpec::default() }),
      )
      .await?;
    stack
      .store()
      .append_chat_event(&manager.id, json!({"type": "user", "content": "Take off the improve loop."}))
      .await?;
    let pool_loop = !pool_ids.is_empty();
    let resolved_backend = if pool_loop { "" } else { "fake" };
    let resolved_model = if pool_loop { "" } else { "fake-model" };
    let state = command::reserve_loop_state(
      &stack.cfg,
      &manager.id,
      &command::Reservation {
        goal: "initial goal",
        work_branch: "improve/port-test",
        repo_path: repo.repo.to_str().expect("synthetic repo path is UTF-8"),
        plan: None,
        base_branch: Some("main"),
        merge_back: false,
        resolved_backend,
        resolved_model,
        backend_from_pool: pool_loop,
      },
    )?;
    let child = sequence::create_improve_child(
      stack.store(),
      &manager.id,
      state.loop_id,
      &state.goal,
      Some(repo.repo.to_str().expect("synthetic repo path is UTF-8")),
      Some("main"),
      state.backend.as_deref(),
    )
    .await?;
    let wakes = Arc::new(Mutex::new(Vec::new()));
    stack.dispatch.set_executor(Some(Arc::new(ParentWakes(Arc::clone(&wakes)))));
    Ok(LoopHarness {
      stack,
      _repo: repo,
      manager,
      child,
      state,
      scripts,
      wakes,
      process_guard: process_guard::ProcessGroupGuard::new(),
      _git_override_guard: git_override_guard,
    })
  }

  fn start_loop(&self, iterations: i64) -> ControllerTask {
    let cfg = Arc::clone(&self.stack.cfg);
    let tree = charliebot_tasks::recovery::sequence_tree(&self.stack.features, &self.stack.adapter)
      .expect("the sequence tree is installed");
    let repo_root = self.stack.features.startup.require::<RepoRoot>().expect("the repo root is installed");
    let pool = backend_pool::backend_pool(&self.stack.features).expect("the backend pool is installed");
    let adapter = self.stack.adapter.clone();
    let session_id = self.manager.id.clone();
    let child_id = self.child.id.clone();
    let goal = self.state.goal.clone();
    let loop_id = self.state.loop_id;
    let handle = tokio::spawn(async move {
      let spec = sequence::LoopSpec { session_id: &session_id, loop_id, iterations, child_id: &child_id, goal: &goal };
      sequence::run_improve_sequence(&spec, &cfg, &tree, &repo_root, &pool, &adapter).await;
    });
    ControllerTask { _abort_on_drop: AbortOnDrop(handle.abort_handle()), handle }
  }

  async fn finish_loop(&self, task: ControllerTask) {
    let ControllerTask { handle, _abort_on_drop } = task;
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
      while !handle.is_finished() {
        self.track_processes();
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
      handle.await.expect("the loop controller joins");
    })
    .await;
    drop(_abort_on_drop);
    completed.expect("the loop controller settles within thirty seconds");
    self.track_processes();
  }

  fn track_processes(&self) {
    for run in self.stack.run_records(&self.child.id).expect("the child runs read") {
      if let Some(pid) = run.pid {
        self.process_guard.track(pid);
      }
    }
  }

  fn events(&self, session_id: &str) -> Vec<charliebot_store::ChatEvent> {
    self.stack.run_events(session_id).expect("session facts read")
  }

  fn child_reports(&self) -> Vec<Value> {
    self
      .events(&self.manager.id)
      .into_iter()
      .filter(|event| {
        event["type"].as_str() == Some(event_types::CHILD_REPORT)
          && event["child_session_id"].as_str() == Some(self.child.id.as_str())
      })
      .map(|event| event.to_value())
      .collect()
  }

  fn iteration_reports(&self) -> Vec<Value> {
    self
      .child_reports()
      .into_iter()
      .filter(|event| {
        event["summary"].as_str().is_some_and(|summary| summary.starts_with("[Improve loop 1 · iteration "))
      })
      .collect()
  }

  fn final_reports(&self) -> Vec<Value> {
    self
      .child_reports()
      .into_iter()
      .filter(|event| event["summary"].as_str().is_some_and(|summary| summary.starts_with("[Improve loop 1]")))
      .collect()
  }

  fn wake_count(&self) -> usize {
    self.wakes.lock().expect("wake list is not poisoned").len()
  }
}

fn event_script(event: &Value, exit_code: i32) -> String {
  format!("printf '%s\\n' '{}'; exit {exit_code}", event)
}

fn child_close_facts(events: &[charliebot_store::ChatEvent]) -> Vec<Value> {
  events
    .iter()
    .filter(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
    .map(|event| event.to_value())
    .collect()
}

fn run_events_of(harness: &LoopHarness, run_id: &str) -> anyhow::Result<Vec<Value>> {
  let path = harness.stack.store().run_dir(&harness.child.id, run_id).join(run_records::RUN_EVENTS_NAME);
  std::fs::read_to_string(path)?
    .lines()
    .filter(|line| !line.is_empty())
    .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
    .collect()
}

#[tokio::test]
async fn two_iterations_use_one_child_and_keep_their_run_order() -> anyhow::Result<()> {
  let harness = LoopHarness::new(
    &[],
    &[],
    vec![recovery_support::result_script("first words"), recovery_support::result_script("second words")],
  )
  .await?;

  harness.finish_loop(harness.start_loop(2)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  assert_eq!(runs.len(), 2);
  assert!(runs.iter().all(|run| run.session_id == harness.child.id && run.kind == RunKind::Iteration));
  assert_eq!(
    runs.iter().filter_map(|run| run.sequence_ref.as_ref().map(|seq| seq.position)).collect::<Vec<_>>(),
    [1, 2]
  );
  assert_eq!(harness.child.id, sequence::improve_child_id(&harness.manager.id, harness.state.loop_id));
  assert_eq!(harness.iteration_reports().len(), 2);
  assert_eq!(harness.final_reports().len(), 1);
  assert_eq!(child_close_facts(&harness.events(&harness.child.id)).len(), 1);
  Ok(())
}

#[tokio::test]
async fn the_second_iteration_reads_the_live_goal_file() -> anyhow::Result<()> {
  let home = tempfile::tempdir()?;
  let gate = home.path().join("release-first-iteration");
  let first_script = format!(
    "while [ ! -f '{}' ]; do sleep 0.02; done; {}",
    gate.display(),
    recovery_support::result_script("first words")
  );
  let harness = LoopHarness::new(&[], &[], vec![first_script, recovery_support::result_script("second words")]).await?;
  let controller = harness.start_loop(2);
  let wait_for_first_run = tokio::time::timeout(Duration::from_secs(10), async {
    loop {
      harness.track_processes();
      if harness.stack.run_records(&harness.child.id).expect("child runs read").iter().any(|run| run.pid.is_some()) {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await;
  if let Err(timeout_error) = wait_for_first_run {
    let state = command::require_loop_state(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)?;
    let mut run_diagnostics = Vec::new();
    for run in harness.stack.run_records(&harness.child.id)? {
      let errors: Vec<Value> =
        run_events_of(&harness, &run.id)?.into_iter().filter(|event| event["type"] == event_types::ERROR).collect();
      run_diagnostics.push(format!(
        "run {} (pid={:?}, backend={:?}, worktree={:?}) launch error and error events: {errors:#?}",
        run.id, run.pid, run.backend, run.worktree_path
      ));
    }
    panic!(
      "the first worker process starts: {timeout_error:?}; improve state status={:?} end_error={:?}; iteration runs:\n{}",
      state.status,
      state.end_error,
      if run_diagnostics.is_empty() { "<none>".to_string() } else { run_diagnostics.join("\n") }
    );
  }
  std::fs::write(
    command::loop_goal_path(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id),
    "revised live goal",
  )?;
  std::fs::write(gate, "release")?;
  harness.finish_loop(controller).await;

  let builds = harness.scripts.builds();
  assert_eq!(builds.len(), 2);
  assert!(builds[0].prompt_contains("initial goal"));
  assert!(builds[1].prompt_contains("revised live goal"));
  Ok(())
}

#[tokio::test]
async fn each_pooled_iteration_run_records_the_selected_backend() -> anyhow::Result<()> {
  let harness = LoopHarness::new(
    &["account-main"],
    &[],
    vec![recovery_support::result_script("first words"), recovery_support::result_script("second words")],
  )
  .await?;

  harness.finish_loop(harness.start_loop(2)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  assert_eq!(
    runs.iter().map(|run| run.backend.as_deref()).collect::<Vec<_>>(),
    [Some("account-main"), Some("account-main")]
  );
  assert!(runs.iter().all(|run| run.model.as_deref() == Some("account-main-model")));
  assert_eq!(harness.scripts.builds().len(), 2);
  Ok(())
}

#[tokio::test]
async fn a_launch_quota_failure_cools_the_only_member_with_its_reset() -> anyhow::Result<()> {
  let harness = LoopHarness::new(&["quota"], &[], Vec::new()).await?;

  harness.finish_loop(harness.start_loop(3)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  assert_eq!(runs.len(), 1);
  assert!(runs[0].pid.is_none());
  let run_events = run_events_of(&harness, &runs[0].id)?;
  assert_eq!(run_events.iter().filter(|event| event["type"] == "error" && event["quota_exhausted"] == true).count(), 1);
  assert!(run_events.iter().any(|event| {
    event["message"].as_str().is_some_and(|text| text.contains("earliest reset") && text.contains("UTC"))
  }));
  let state = command::require_loop_state(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)?;
  assert_eq!(state.status, command::LoopStatus::Failed);
  assert_eq!(state.blocked_iteration, Some(1));
  assert!(
    state.blocked_reason.as_deref().is_some_and(|reason| reason.starts_with("launch refused with quota exhausted:"))
  );
  assert_eq!(harness.iteration_reports().len(), 0);
  assert_eq!(harness.final_reports().len(), 1);
  assert_eq!(harness.wake_count(), 1);
  assert!(harness.final_reports()[0]["summary"].as_str().unwrap().contains("launch refused with quota exhausted:"));
  let pool = backend_pool::backend_pool(&harness.stack.features)?;
  assert!(pool.recovery_report(&harness.stack.cfg).contains("quota cooling until"));
  Ok(())
}

#[tokio::test]
async fn a_cooling_pool_member_is_followed_by_the_next_member() -> anyhow::Result<()> {
  let harness =
    LoopHarness::new(&["quota", "next"], &[], vec![recovery_support::result_script("second words")]).await?;

  harness.finish_loop(harness.start_loop(2)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  assert_eq!(runs.iter().map(|run| run.backend.as_deref()).collect::<Vec<_>>(), [Some("quota"), Some("next")]);
  assert_eq!(
    run_records::terminal_outcome(&harness.events(&harness.child.id), &runs[0].id)?,
    Some(RunOutcomeKind::Failed)
  );
  assert_eq!(
    run_records::terminal_outcome(&harness.events(&harness.child.id), &runs[1].id)?,
    Some(RunOutcomeKind::Success)
  );
  assert_eq!(
    harness.iteration_reports().iter().map(|event| event["outcome"].as_str()).collect::<Vec<_>>(),
    [Some("failed"), Some("success")]
  );
  let pool = backend_pool::backend_pool(&harness.stack.features)?;
  let recovery = pool.recovery_report(&harness.stack.cfg);
  assert!(recovery.contains("quota cooling until"));
  assert!(recovery.contains("next not cooling"));
  assert_eq!(
    command::require_loop_state(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)?.status,
    command::LoopStatus::Blocked
  );
  Ok(())
}

#[tokio::test]
async fn a_textual_quota_error_blocks_when_the_next_selection_finds_no_member() -> anyhow::Result<()> {
  let quota_error = json!({"type": "error", "message": "Budget has been exceeded!"});
  let harness = LoopHarness::new(&["fake"], &[], vec![event_script(&quota_error, 1)]).await?;

  harness.finish_loop(harness.start_loop(3)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  assert_eq!(runs.len(), 1);
  let state = command::require_loop_state(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)?;
  assert_eq!(state.status, command::LoopStatus::Failed);
  let final_report = harness.final_reports().remove(0);
  let summary = final_report["summary"].as_str().expect("final report summary is text");
  assert!(summary.contains("no backends.pool member is available"), "final report summary: {summary}");
  assert!(summary.contains("fake cooling until"), "final report summary: {summary}");
  Ok(())
}

#[tokio::test]
async fn three_iterations_deliver_reports_and_wake_the_parent_four_times() -> anyhow::Result<()> {
  let harness = LoopHarness::new(
    &[],
    &[],
    vec![
      recovery_support::result_script("first words"),
      recovery_support::result_script("second words"),
      recovery_support::result_script("third words"),
    ],
  )
  .await?;

  harness.finish_loop(harness.start_loop(3)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  let reports = harness.iteration_reports();
  assert_eq!(runs.len(), 3);
  assert_eq!(reports.len(), 3);
  assert_eq!(harness.final_reports().len(), 1);
  assert_eq!(harness.wake_count(), 4);
  assert_eq!(reports.iter().map(|event| event["outcome"].as_str()).collect::<Vec<_>>(), [Some("success"); 3]);
  let reported_run_refs =
    reports.iter().map(|event| event["result_refs"][1].as_str().expect("run result ref is text")).collect::<Vec<_>>();
  let expected_run_refs = runs.iter().map(|run| format!("run:{}", run.id)).collect::<Vec<_>>();
  assert_eq!(reported_run_refs, expected_run_refs.iter().map(String::as_str).collect::<Vec<_>>());
  for iteration in 1..=3 {
    let report = command::loop_dir(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)
      .join(format!("iter_{iteration:04}.md"));
    assert!(report.is_file());
  }
  let child_events = harness.events(&harness.child.id);
  let closes = child_close_facts(&child_events);
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["request_id"], "improve:1:close");
  Ok(())
}

#[tokio::test]
async fn an_iteration_report_carries_the_mechanical_header_and_fallback_file() -> anyhow::Result<()> {
  let harness = LoopHarness::new(&[], &[], vec![recovery_support::result_script("worker words")]).await?;

  harness.finish_loop(harness.start_loop(1)).await;

  let report = harness.iteration_reports().remove(0);
  let summary = report["summary"].as_str().expect("the report summary is text");
  assert!(summary.starts_with("[Improve loop 1 · iteration 1/1] report_valid=false [invalid: no report file] "));
  assert!(summary.contains(" commits_added=0 "));
  assert!(summary.contains(" report="));
  assert!(summary.ends_with("\n\nworker words"));
  let path = command::loop_dir(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id).join("iter_0001.md");
  let fallback = std::fs::read_to_string(path)?;
  assert!(fallback.starts_with("<!-- runner fallback: worker wrote no report -->\n"));
  assert!(fallback.contains("worker words"));
  Ok(())
}

#[tokio::test]
async fn a_non_quota_failed_iteration_is_reported_and_the_loop_continues() -> anyhow::Result<()> {
  let failed = json!({"type": "result", "subtype": "success", "result": "the build broke", "is_error": true});
  let harness =
    LoopHarness::new(&[], &[], vec![event_script(&failed, 1), recovery_support::result_script("recovered words")])
      .await?;

  harness.finish_loop(harness.start_loop(2)).await;

  let runs = harness.stack.run_records(&harness.child.id)?;
  let reports = harness.iteration_reports();
  assert_eq!(runs.len(), 2);
  assert_eq!(
    reports.iter().map(|event| event["outcome"].as_str()).collect::<Vec<_>>(),
    [Some("failed"), Some("success")]
  );
  assert!(reports[0]["summary"].as_str().unwrap().ends_with("\n\nthe build broke"));
  assert_eq!(
    command::require_loop_state(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id)?.status,
    command::LoopStatus::Blocked
  );
  Ok(())
}

#[tokio::test]
async fn replaying_an_iteration_report_is_idempotent() -> anyhow::Result<()> {
  let harness = LoopHarness::new(&[], &[], vec![recovery_support::result_script("first words")]).await?;

  harness.finish_loop(harness.start_loop(1)).await;

  let reports = harness.iteration_reports();
  assert_eq!(reports.len(), 1);
  let original = reports[0].clone();
  let wake_count = harness.wake_count();
  let event_count = harness.events(&harness.manager.id).len();
  let refs = original["result_refs"]
    .as_array()
    .expect("result refs are an array")
    .iter()
    .map(|value| value.as_str().expect("result ref is text").to_string())
    .collect::<Vec<_>>();
  let (replayed, created) = harness
    .stack
    .dispatch
    .deliver_child_report(
      &harness.child.id,
      json!({"id": original["child_event_id"]}).into(),
      original["outcome"].as_str().expect("outcome is text"),
      original["summary"].as_str().expect("summary is text"),
      Some(&refs),
      Some(&harness.manager.id),
    )
    .await?;

  assert!(!created);
  assert_eq!(replayed["id"], charliebot_store::compact::CompactValue::from(original["id"].clone()));
  assert_eq!(harness.events(&harness.manager.id).len(), event_count);
  assert_eq!(harness.wake_count(), wake_count);
  Ok(())
}

#[tokio::test]
async fn improve_controller_keeps_base_registry_lookups_and_skips_run_replay() -> anyhow::Result<()> {
  let harness = LoopHarness::new(&[], &[], Vec::new()).await?;
  let controller = harness
    .stack
    .features
    .registry
    .sequence_controllers()
    .find(|controller| controller.sequence_kind() == sequence::SEQUENCE_KIND)
    .expect("the improve controller is registered");
  assert_eq!(controller.sequence_kind(), "improve");
  assert!(controller.binding(&harness.manager.id).is_none());
  assert!(!controller.owns_session(&harness.manager));
  assert!(controller.listing_fields(std::slice::from_ref(&harness.manager.id), UtcDateTime::now()).is_empty());
  assert!(harness.stack.features.registry.binding_for(&harness.manager.id)?.is_none());

  let controller_ids = [harness.manager.id.clone()];
  let mut base_listing = HashMap::from([(harness.manager.id.clone(), Map::new())]);
  for base in harness.stack.features.registry.sequence_controllers().filter(|base| base.sequence_kind() != "improve") {
    for (session_id, fields) in base.listing_fields(&controller_ids, UtcDateTime::now()) {
      base_listing.entry(session_id).or_default().extend(fields);
    }
  }
  assert_eq!(
    harness.stack.features.registry.sequence_listing_fields(&controller_ids, UtcDateTime::now()),
    base_listing
  );

  let run = recovery_support::synthetic_run(&harness.child.id, "synthetic-improve-run", RunKind::Iteration);
  let mut run = run;
  run.sequence_ref = Some(SequenceRef {
    kind: sequence::SEQUENCE_KIND.to_string(),
    owner_ref: sequence::loop_owner_ref(&harness.stack.cfg, &harness.manager.id, harness.state.loop_id),
    position: 1,
  });
  let run = harness.stack.register(run).await?;
  let tree = charliebot_tasks::recovery::sequence_tree(&harness.stack.features, &harness.stack.adapter)?;
  assert!(!controller.recover_run(&harness.child.id, &run, None, &tree).await?);
  assert!(harness.stack.store().task_state(&harness.child.id)? == "open");
  assert!(child_close_facts(&harness.events(&harness.child.id)).is_empty());
  let stored = harness.stack.store().read_run_sync(&harness.child.id, &run.id)?.expect("the run is registered");
  assert!(stored.pid.is_none());
  assert!(tree.terminal_outcome_of(&harness.child.id, &run.id).await?.is_none());
  Ok(())
}

#[tokio::test]
async fn improve_children_keep_the_requested_or_session_backend() -> anyhow::Result<()> {
  let explicit = LoopHarness::new(&["fake"], &["other"], Vec::new()).await?;
  let (backend, model, from_pool) = spawner_backends::resolve_requested_subagent_backend_model(
    &explicit.stack.cfg,
    explicit.stack.store(),
    &explicit.manager.id,
    Some("other"),
    SubagentRole::Improve,
    &BackendPool::default(),
  )
  .await?;
  assert_eq!(backend, "other");
  assert_eq!(model.as_deref(), Some("other-model"));
  assert!(!from_pool);
  let explicit_child = sequence::create_improve_child(
    explicit.stack.store(),
    &explicit.manager.id,
    explicit.state.loop_id + 1,
    "another goal",
    None,
    None,
    Some(&backend),
  )
  .await?;
  assert_eq!(explicit_child.backend, "other");
  drop(explicit);

  let inherited = LoopHarness::new(&[], &[], Vec::new()).await?;
  let (backend, model, from_pool) = spawner_backends::resolve_requested_subagent_backend_model(
    &inherited.stack.cfg,
    inherited.stack.store(),
    &inherited.manager.id,
    None,
    SubagentRole::Improve,
    &BackendPool::default(),
  )
  .await?;
  assert_eq!(backend, "fake");
  assert_eq!(model.as_deref(), Some("fake-model"));
  assert!(!from_pool);
  let inherited_child = sequence::create_improve_child(
    inherited.stack.store(),
    &inherited.manager.id,
    inherited.state.loop_id + 1,
    "another goal",
    None,
    None,
    Some(&backend),
  )
  .await?;
  assert_eq!(inherited_child.backend, "fake");
  Ok(())
}

#[tokio::test]
async fn recovery_loads_a_legacy_failed_state_without_end_error() -> anyhow::Result<()> {
  let harness = LoopHarness::new(&[], &[], Vec::new()).await?;
  let loop_id = harness.state.loop_id;
  let mut state = harness.state.clone();
  state.status = command::LoopStatus::Failed;
  state.server_pid = Some(0);
  state.end_error = None;
  command::save_loop_state(&harness.stack.cfg, &harness.manager.id, &state)?;
  command::clear_active_loop_lock(&harness.stack.cfg, &harness.manager.id)?;
  let state_path = command::loop_dir(&harness.stack.cfg, &harness.manager.id, loop_id).join(command::STATE_FILE);
  let mut legacy = serde_json::to_value(&state)?;
  legacy.as_object_mut().expect("state serializes as an object").remove("end_error");
  std::fs::write(&state_path, serde_json::to_vec(&legacy)?)?;
  assert!(command::load_loop_state(&harness.stack.cfg, &harness.manager.id, loop_id)?.unwrap().end_error.is_none());

  let child_dir = command::loop_dir(&harness.stack.cfg, &harness.manager.id, loop_id);
  for (position, text) in [(1, "first report"), (2, "second report")] {
    let mut run =
      recovery_support::synthetic_run(&harness.child.id, &format!("legacy-iteration-{position}"), RunKind::Iteration);
    run.sequence_ref = Some(SequenceRef {
      kind: sequence::SEQUENCE_KIND.to_string(),
      owner_ref: sequence::loop_owner_ref(&harness.stack.cfg, &harness.manager.id, loop_id),
      position,
    });
    let run = harness.stack.register(run).await?;
    harness.stack.record_terminal(&harness.child.id, &run.id, RunOutcomeKind::Success).await?;
    std::fs::write(child_dir.join(format!("iter_{position:04}.md")), format!("## Iter {position}\n\n{text}\n"))?;
  }

  harness.stack.boot().await?;

  assert_eq!(harness.stack.store().task_state(&harness.child.id)?, "cancelled");
  assert_eq!(child_close_facts(&harness.events(&harness.child.id)).len(), 1);
  let report = harness.final_reports().remove(0);
  assert_eq!(report["outcome"], "failed");
  assert!(
    report["summary"].as_str().unwrap().starts_with("[Improve loop 1] Improve loop failed.\n\nIteration summaries:")
  );
  assert!(report["summary"].as_str().unwrap().contains("first report"));
  assert!(report["summary"].as_str().unwrap().contains("second report"));
  Ok(())
}

struct GitOverrideGuard;

impl Drop for GitOverrideGuard {
  fn drop(&mut self) {
    charliebot_tasks::git::clear_git_program_override();
  }
}

fn install_git_script(script: &str) -> GitOverrideGuard {
  charliebot_tasks::git::set_git_program_override("/bin/sh", &["-c", script]);
  GitOverrideGuard
}

#[tokio::test]
async fn a_rejected_fast_forward_names_the_branch_that_was_pushed() {
  let _lock = GIT_OVERRIDE_LOCK.lock().await;
  let _override = install_git_script(
    "case \"$2\" in *:*) printf '%s\\n' 'rejected: non-fast-forward' >&2; exit 1 ;; *) exit 0 ;; esac",
  );
  let repo = tempfile::tempdir().expect("temporary repo path");

  let landing =
    landing::land_work_branch_after_loop(repo.path(), "improve/test", "main", true, false, 1, "session").await;

  assert_eq!(
    landing.end_error.as_deref(),
    Some(
      "The fast-forward landing onto main failed (rejected: non-fast-forward); the work branch was pushed to origin."
    )
  );
  assert_eq!(landing.merge_result.as_ref().unwrap()["merged"], false);
  assert_eq!(landing.merge_result.as_ref().unwrap()["work_branch"], "improve/test");
}

#[tokio::test]
async fn two_rejected_landing_pushes_name_the_branch_push_error() {
  let _lock = GIT_OVERRIDE_LOCK.lock().await;
  let script = "case \"$2\" in *:*) printf '%s\\n' 'rejected: non-fast-forward' >&2 ;; *) printf '%s\\n' 'auth expired' >&2 ;; esac; exit 1";
  let _override = install_git_script(script);
  let repo = tempfile::tempdir().expect("temporary repo path");

  let landing =
    landing::land_work_branch_after_loop(repo.path(), "improve/test", "main", true, false, 1, "session").await;

  assert_eq!(
    landing.end_error.as_deref(),
    Some(
      "The fast-forward landing onto main failed (rejected: non-fast-forward); pushing the work branch to origin also failed (auth expired)."
    )
  );
  assert_eq!(landing.merge_result.as_ref().unwrap()["merged"], false);
}

#[tokio::test]
async fn a_best_effort_branch_push_failure_is_reported_without_merge_back() {
  let _lock = GIT_OVERRIDE_LOCK.lock().await;
  let _override = install_git_script("printf '%s\\n' 'auth expired' >&2; exit 1");
  let repo = tempfile::tempdir().expect("temporary repo path");

  let landing =
    landing::land_work_branch_after_loop(repo.path(), "improve/test", "main", false, false, 1, "session").await;

  assert_eq!(landing.end_error.as_deref(), Some("Pushing the work branch to origin failed (auth expired)."));
  assert!(landing.merge_result.is_none());
}
