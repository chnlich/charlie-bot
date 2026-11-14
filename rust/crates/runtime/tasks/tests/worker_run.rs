//! The worker-run cases over the task-execution stack: a worker Run launches through
//! the adapter, its process is a `/bin/sh` script, and its delivery chain runs on the real store.
//!
//! The review spawn and follow-up of an implement Run run for real. The work and verify
//! launches are asserted here; the review launch itself, iteration and scheduled-step launches stay with their
//! owners' test files.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use charliebot_agents::backend_availability;
use charliebot_agents::backend_pool;
use charliebot_agents::cli_base::CliHost;
use charliebot_agents::model_pool;
use charliebot_core::config::{
  BackendOption, CallClass, CharlieBotConfig, PoolBilling, PoolConfig, PoolMember, PoolRule, RoutingConfig,
};
use charliebot_core::event_types;
use charliebot_registry::Registry;
use charliebot_registry::caller::CallerIdentity;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::write_faults::{self, Hit};
use charliebot_store::{ChatEvent, CreateTask, PatchTask, TaskCaller};
use charliebot_tasks::prompts::{LAUNCH_TEXT_FILENAME, SNAPSHOT_FILENAME};

use charliebot_tasks::{cold_read, handoff, routes, spawner_backends, task_sessions};
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, CLI_BINARY_KEY, ROTATION_POOL_ID, Scripts, Stack, TOKEN_FIELD, cold_read_body,
  init_repo_with_origin, last_child_report, post_delegate, result_script, scripted_type, settle_until, synthetic_git,
  synthetic_run, write_sitrep,
};
use serde_json::{Map, Value, json};
use serde_yaml::Mapping;
use serde_yaml::Value as YamlValue;
use time::{Duration as TimeDuration, OffsetDateTime};

/// The id of the priority pool that the Auto sessions of the failover tests run on.
const AUTO_POOL_ID: &str = "auto";

/// The bound every wait in this file uses. The integration runner shares its CPUs with the build and the other
/// selected tests, so a state that takes milliseconds locally can take seconds there; a wait reaches this bound only
/// when the state never arrives, and a wait that reaches it names the state it awaited.
const TEST_DEADLINE: Duration = Duration::from_secs(120);

/// The members of the pool `pool_id`, in config order.
fn member_ids<'a>(cfg: &'a charliebot_core::config::CharlieBotConfig, pool_id: &str) -> Vec<&'a str> {
  cfg
    .backends
    .pool(pool_id)
    .expect("the pool is configured")
    .members
    .iter()
    .map(|member| member.backend.as_str())
    .collect()
}

fn backend_availability(
  features: &Arc<charliebot_registry::context::FeatureContext>,
) -> anyhow::Result<Arc<backend_availability::BackendAvailability>> {
  backend_availability::backend_availability(features)
}

/// Tracks every run process in a test stack and kills its process group when the test scope ends.
struct StackProcessGuard<'a> {
  stack: &'a Stack,
  groups: ProcessGroupGuard,
}

impl<'a> StackProcessGuard<'a> {
  fn new(stack: &'a Stack) -> Self {
    StackProcessGuard { stack, groups: ProcessGroupGuard::new() }
  }

  fn track_all(&self) -> anyhow::Result<()> {
    for (_, run) in self.stack.store().list_all_run_records_sync()? {
      if let (Some(pid), Some(pid_start)) = (run.pid, run.pid_start.as_deref()) {
        self.groups.track_if_start(pid, pid_start);
      }
    }
    Ok(())
  }

  async fn track_run(&self, session_id: &str, run_id: &str) -> anyhow::Result<()> {
    settle_until(|| {
      let run = self.stack.store().read_run_sync(session_id, run_id)?;
      if let Some(record) = run {
        if let (Some(pid), Some(pid_start)) = (record.pid, record.pid_start.as_deref()) {
          self.groups.track_if_start(pid, pid_start);
          Ok(true)
        } else {
          Ok(false)
        }
      } else {
        Ok(false)
      }
    })
    .await?;
    Ok(())
  }
}

impl Drop for StackProcessGuard<'_> {
  fn drop(&mut self) {
    if let Err(error) = self.track_all() {
      eprintln!("test process cleanup could not read run records: {error:#}");
    }
  }
}

/// The task spec shape: a goal and a task type, no repository.
fn spec(goal: &str, task_type: Option<TaskType>) -> TaskSpec {
  TaskSpec { goal: goal.to_string(), task_type, ..TaskSpec::default() }
}

/// The implement task of the repository cases: a goal and the repository's base branch.
fn implement_task(repo: &Path, goal: &str) -> TaskSpec {
  TaskSpec {
    goal: goal.to_string(),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    ..TaskSpec::default()
  }
}

/// A stack whose one backend option runs `scripts` in order, one per build, with the operator's access key on disk.
async fn scripted_stack(scripts: &Scripts) -> anyhow::Result<Stack> {
  let stack = Stack::start_with(vec![scripts.option(BACKEND_ID, BACKEND_TYPE)], vec![BACKEND_ID.to_string()]).await?;
  stack.write_access_key()?;
  Ok(stack)
}

/// A worker run record on the synthetic backend, with a repository and base branch when given.
fn work_run(session_id: &str, run_id: &str, repo: Option<&Path>) -> RunRecord {
  RunRecord {
    repo_path: repo.map(|path| path.display().to_string()),
    base_branch: repo.map(|_| "main".to_string()),
    backend: Some(BACKEND_ID.to_string()),
    model: Some(format!("{BACKEND_ID}-model")),
    ..synthetic_run(session_id, run_id, RunKind::Work)
  }
}

/// Persist a run record with its task spec text, as a reservation does (`register_run(record, task_spec_text=...)`).
async fn register_run(stack: &Stack, record: RunRecord, task_spec_text: &str) -> anyhow::Result<RunRecord> {
  let session_id = record.session_id.clone();
  let guard = stack.store().control_lock().await;
  Ok(stack.store().register_run_locked(&guard, &session_id, record, Some(task_spec_text)).await?)
}

/// The error text of a run's events log: the first `error` event (`_read_error_event`).
fn error_text(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<String> {
  let path = stack.store().run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME);
  if !path.is_file() {
    return Ok(String::new());
  }
  for line in std::fs::read_to_string(path)?.lines().filter(|line| !line.trim().is_empty()) {
    let event: Value = serde_json::from_str(line)?;
    if event["type"] == "error" {
      return Ok(event["message"].as_str().or_else(|| event["content"].as_str()).unwrap_or_default().to_string());
    }
  }
  Ok(String::new())
}

/// Wait until a node has no run without a terminal fact (`_settle_parent`, over the durable views).
/// Waits until every run of `session_id` is terminal and no input is pending on it, so no launch of the test's
/// making sits at the gate when the stack drops.
///
/// A delivered child report schedules the parent's manager turn in the background. Its run record may not exist yet
/// when the state above already holds, so the node also waits for one manager turn per delivered report.
async fn settle_node(stack: &Stack, session_id: &str) -> anyhow::Result<()> {
  settle_until(|| {
    let events = stack.run_events(session_id)?;
    for run in stack.run_records(session_id)? {
      if run_records::terminal_outcome(&events, &run.id)?.is_none() {
        return Ok(false);
      }
    }
    if !stack.dispatch.pending_inputs(session_id)?.is_empty() {
      return Ok(false);
    }
    if stack.run_ids_of_kind(session_id, RunKind::ManagerTurn)?.len() < stack.child_reports(session_id)?.len() {
      return Ok(false);
    }
    Ok(true)
  })
  .await
}

/// A repository whose local base branch holds a commit origin lacks fails the base check before any process starts. The
/// Run lands the durable failed fact with the error as its evidence, and the parent receives the failure report.
#[tokio::test]
#[ignore = "integration"]
async fn bare_branch_base_with_unpushed_local_commit_launch_fails() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let scripts = Scripts::new(vec![result_script("never runs")]);
  let stack = scripted_stack(&scripts).await?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("w", Some(&manager.id), TaskProfile::Worker, Some(implement_task(&repo, "## Goal\n\nadd a marker file\n")))
    .await?;

  // One unpushed commit on the local main: origin does not have it, so the base check fails closed.
  std::fs::write(repo.join("local_only.txt"), "local work\n")?;
  synthetic_git(&repo, &["add", "-A"])?;
  synthetic_git(&repo, &["commit", "-q", "-m", "unpushed local commit"])?;
  let local_tip = synthetic_git(&repo, &["rev-parse", "main"])?;

  register_run(&stack, work_run(&worker.id, "run-work", Some(&repo)), "## Goal\n\nadd a marker file\n").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Failed);
  let run = stack.store().read_run_sync(&worker.id, "run-work")?.expect("the run is recorded");
  assert!(run.started_at.is_none(), "the Run never spawned");
  // The failed launch's report wakes the parent's manager turn, which may take a build from the same script queue.
  // Only a build in the worker's own run directory shows that the worker's backend started.
  let worker_run_dir = stack.store().run_dir(&worker.id, "run-work");
  assert!(
    !scripts.builds().iter().any(|build| build.launch.log_dir.as_deref() == Some(worker_run_dir.as_path())),
    "the worker's backend was never invoked"
  );
  let error = error_text(&stack, &worker.id, "run-work")?;
  assert!(error.contains("BaseBranchResolutionError"), "{error}");
  assert!(error.contains("differs from origin/main"), "{error}");
  assert!(error.contains(&local_tip[..12]), "{error}");
  assert!(!error.contains("fast-forward"), "{error}");
  // The error text is the run's evidence, never an ordinary chat row.
  let rows = stack.store().load_chat_events_sync(&worker.id)?;
  assert!(rows.iter().all(|row| !row.to_value().to_string().contains(&error)), "the error text is in no chat row");
  assert!(!stack.store().run_dir(&worker.id, "run-work").join("launch_prompt.md").exists(), "no launch prompt");

  // The failed launch reports to the parent through the after-run path, which runs after the terminal fact.
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  let report = stack.child_reports(&manager.id)?.pop().expect("checked above");
  assert_eq!(report["outcome"], "failed");
  assert!(report["summary"].as_str().unwrap_or_default().contains("differs from origin/main"), "{report}");
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// Advance origin's main from a second clone, so the fixture's local main falls behind it
/// (`_advance_origin_from_second_clone`).
fn advance_origin(dir: &Path, origin: &Path, file: &str) -> anyhow::Result<String> {
  let second = dir.join("second");
  synthetic_git(dir, &["clone", "-q", &origin.display().to_string(), &second.display().to_string()])?;
  synthetic_git(&second, &["config", "user.email", "t@example.com"])?;
  synthetic_git(&second, &["config", "user.name", "t"])?;
  std::fs::write(second.join(file), "ahead\n")?;
  synthetic_git(&second, &["add", "-A"])?;
  synthetic_git(&second, &["commit", "-q", "-m", "advance origin"])?;
  synthetic_git(&second, &["push", "-q", "origin", "main"])?;
  synthetic_git(&second, &["rev-parse", "HEAD"])
}

/// The bare base form when the shared checkout's local main is only behind origin. The Run launches anyway, its work
/// branch starts at the origin tip, and the local main ref is untouched. The review that lands the work runs on the
/// review chain, so the case asserts the work Run's launch and success.
#[tokio::test]
#[ignore = "integration"]
async fn bare_branch_base_behind_starts_from_origin_tip() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let marker = dir.path().join("cwd.txt");
  let scripts = Scripts::new(vec![format!("pwd > '{}'; {}", marker.display(), result_script("implemented"))]);
  let stack = scripted_stack(&scripts).await?;
  let (repo, origin) = init_repo_with_origin(dir.path())?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("w", Some(&manager.id), TaskProfile::Worker, Some(implement_task(&repo, "## Goal\n\nadd a marker file\n")))
    .await?;

  // Origin gains a commit while the fixture's local main stays put: local main is strictly behind origin/main.
  let origin_tip = advance_origin(dir.path(), &origin, "ahead.txt")?;
  let local_main = synthetic_git(&repo, &["rev-parse", "main"])?;
  assert_ne!(local_main, origin_tip);

  register_run(&stack, work_run(&worker.id, "run-work", Some(&repo)), "## Goal\n\nadd a marker file\n").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  let work = stack.store().read_run_sync(&worker.id, "run-work")?.expect("the run is recorded");
  let worktree = work.worktree_path.clone().expect("a repository work Run has a worktree");
  // The process starts in its worktree root, where the CLI finds the repo guide.
  assert_eq!(std::fs::read_to_string(&marker)?.trim(), worktree);
  // The work branch started at the new origin tip, never at the stale local main.
  assert_eq!(synthetic_git(Path::new(&worktree), &["rev-parse", "HEAD"])?, origin_tip);
  assert_eq!(synthetic_git(&repo, &["rev-parse", "main"])?, local_main);
  // The success wakes the parent in the background; wait for the turn it launches before the stack drops.
  settle_until(|| Ok(!stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.is_empty())).await?;
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// A repository work run on a launcher backend runs where the launcher decides: the server prepares no worktree for
/// it, starts no branch, and its brief names no path under this host's worktree directory, because such a path would
/// be a place that is absent where the run executes. A run on this host's own backend keeps its worktree
/// (`bare_branch_base_behind_starts_from_origin_tip`, and the cron fires' tests in `features/cron`).
#[tokio::test]
#[ignore = "integration"]
async fn a_repo_work_run_on_a_launcher_backend_prepares_no_worktree_and_names_none() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("worker done")]);
  let mut option = scripts.option(BACKEND_ID, BACKEND_TYPE);
  option
    .type_fields
    .insert(YamlValue::String(CLI_BINARY_KEY.to_string()), YamlValue::String("/path/to/launcher".to_string()));
  let stack = Stack::start_with(vec![option], vec![BACKEND_ID.to_string()]).await?;
  stack.write_access_key()?;
  let process_guard = StackProcessGuard::new(&stack);
  let dir = tempfile::tempdir()?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(implement_task(&repo, "ship it"))).await?;

  stack.dispatch.admit_input(&worker.id, event_types::USER, "do the thing", "user", json!({})).await?;
  let run = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  process_guard.track_run(&worker.id, &run).await?;
  assert_eq!(stack.wait_terminal(&worker.id, &run).await?, RunOutcomeKind::Success);
  process_guard.track_all()?;

  let record = stack.store().read_run_sync(&worker.id, &run)?.expect("the run is recorded");
  assert_eq!(record.worktree_path, None, "a launcher-backend run prepares no worktree");
  assert_eq!(record.branch_name, None, "a launcher-backend run starts no branch");
  let launch = std::fs::read_to_string(stack.store().run_dir(&worker.id, &run).join(LAUNCH_TEXT_FILENAME))?;
  assert!(launch.contains("ship it"), "{launch}");
  let worktree_dir = stack.cfg.paths.worktree_dir.clone();
  assert!(!launch.contains(&worktree_dir), "{launch}");
  assert!(!launch.contains("charliebot/task-"), "{launch}");
  Ok(())
}

/// An interrupted run reports on the spot through the failure entry, and the report's delivery archives the worker.
#[tokio::test]
#[ignore = "integration"]
async fn an_interrupted_worker_run_reports_once_and_archives_the_worker() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("stop noted")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("say the phrase", Some(TaskType::QuickEdit))))
    .await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "say the phrase").await?;
  stack
    .dispatch
    .finish_run(
      &worker.id,
      "run-work",
      RunOutcome { exit_code: Some(-1), ..RunOutcome::new(RunOutcomeKind::Interrupted) },
    )
    .await?;

  // The durable interrupted fact drives exactly the finish chain the live finalize runs.
  stack.adapter.after_worker_run_for_test(&worker.id, "run-work", "interrupted").await?;

  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("interrupted")]);
  let closes = stack.closes(&worker.id)?;
  assert_eq!(closes.len(), 1);
  let fact = &closes[0];
  assert_eq!(fact["outcome"], "archived");
  assert_eq!(fact["actor"], "system");
  assert_eq!(fact["request_id"], "unsuccessful:run-work");
  assert_eq!(fact["run_ids"], json!(["run-work"]));
  assert_eq!(fact["summary"], "run run-work ended interrupted; archived by the server");
  settle_node(&stack, &manager.id).await?;
  assert_eq!(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(), 1, "the report woke the parent once");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn a_late_failure_report_is_skipped_after_worker_cancellation() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("manager noted the close")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "do the work").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Failed).await?;
  stack
    .completion
    .cancel_task(&worker.id, "cancel-before-report", "stop", &CallerIdentity::Operator { session_id: None })
    .await?;
  let close_report_count = stack.child_reports(&manager.id)?.len();

  stack.adapter.after_worker_run_for_test(&worker.id, "run-work", "failed").await?;

  assert_eq!(close_report_count, 1);
  assert_eq!(stack.child_reports(&manager.id)?.len(), close_report_count);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn successful_work_on_a_closed_worker_does_not_register_a_review() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("manager noted the close"); 4]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", Some(TaskType::Implement))))
    .await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "do the work").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  stack
    .completion
    .cancel_task(&worker.id, "cancel-before-review", "stop", &CallerIdentity::Operator { session_id: None })
    .await?;

  stack.adapter.after_worker_run_for_test(&worker.id, "run-work", "success").await?;

  assert_eq!(stack.run_records(&worker.id)?.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(), ["run-work"]);
  assert_eq!(stack.child_reports(&manager.id)?.len(), 1);
  Ok(())
}

/// A repo-less quick edit closes completed when its work Run succeeds, spawns no review, and reports completed to the
/// parent, which takes one turn.
#[tokio::test]
#[ignore = "integration"]
async fn repo_less_quick_edit_closes_without_review() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("modified /etc/cron.d/sweep"), result_script("manager noted it")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node(
      "w",
      Some(&manager.id),
      TaskProfile::Worker,
      Some(spec("bump the host cron schedule line", Some(TaskType::QuickEdit))),
    )
    .await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "bump the host cron schedule line").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  settle_until(|| Ok(stack.store().task_state(&worker.id)? == "completed")).await?;
  assert!(stack.run_ids_of_kind(&worker.id, RunKind::Review)?.is_empty(), "a quick edit has no review Run");
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  assert_eq!(stack.child_reports(&manager.id)?.last().expect("checked above")["outcome"], "completed");
  settle_node(&stack, &manager.id).await?;
  assert_eq!(
    stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(),
    1,
    "the delivered report woke the parent"
  );
  Ok(())
}

/// An upload admitted while a worker run holds its slot waits for that run's end. The continuation resumes the run's
/// native conversation, and its prompt names the upload's path.
#[tokio::test]
#[ignore = "integration"]
async fn a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let started = dir.path().join("started");
  let gate = dir.path().join("gate");
  // The first process pins its native conversation in its raw log, then holds its slot until the gate file exists.
  let init = json!({"type": "system", "subtype": "init", "session_id": "native-1"});
  let held = format!(
    "touch '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; printf '%s\\n' '{init}'; {}",
    started.display(),
    gate.display(),
    result_script("did the work")
  );
  let scripts = Scripts::new(vec![held, result_script("looked at the screenshot")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node(
      "w",
      Some(&manager.id),
      TaskProfile::Worker,
      Some(spec("bump the host cron schedule line", Some(TaskType::QuickEdit))),
    )
    .await?;
  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the work.", "user", json!({})).await?;
  let run1 = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  settle_until(|| Ok(started.is_file())).await?;

  // Admitted while the first run holds its slot: pending at that run's end.
  let upload_path = dir.path().join("uploads").join("screenshot.png").display().to_string();
  let upload = json!({"filename": "screenshot.png", "path": upload_path});
  let admitted = stack
    .dispatch
    .admit_input(&worker.id, event_types::USER, "see attached", "user", json!({"uploaded_files": [upload]}))
    .await?;
  std::fs::write(&gate, "")?;
  assert_eq!(stack.wait_terminal(&worker.id, &run1).await?, RunOutcomeKind::Success);

  settle_until(|| Ok(stack.run_ids_of_kind(&worker.id, RunKind::Work)?.len() == 2)).await?;
  let run2 =
    stack.run_ids_of_kind(&worker.id, RunKind::Work)?.into_iter().find(|id| *id != run1).expect("checked above");
  assert_eq!(stack.wait_terminal(&worker.id, &run2).await?, RunOutcomeKind::Success);
  let record = stack.store().read_run_sync(&worker.id, &run2)?.expect("the continuation is recorded");
  let admitted_id = admitted["id"].as_str().expect("an admitted input has an id");
  assert_eq!(record.input_event_ids, vec![admitted_id.to_string()]);
  let builds = scripts.builds();
  // The scripted type resumes by CLI flag, so the held native id reaches the continuation as `--resume <id>`
  // (`route_resume_session_style`); the case reads the same id from its `resume_session_id` keyword.
  assert_eq!(builds[1].launch.extra_flags, ["--resume", "native-1"]);
  assert!(builds[1].prompt_contains(&upload_path), "the continuation prompt names the upload");
  Ok(())
}

/// A run's persisted prompt snapshot (`_snapshot_of`).
fn snapshot_of(record: &RunRecord) -> anyhow::Result<Value> {
  let path =
    record.prompt_snapshot_ref.as_deref().ok_or_else(|| anyhow::anyhow!("run {} has no snapshot", record.id))?;
  Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// The `source_ref` of every source in a snapshot's blocks.
fn snapshot_source_refs(snapshot: &Value) -> Vec<String> {
  snapshot["blocks"]
    .as_array()
    .into_iter()
    .flatten()
    .flat_map(|block| block["sources"].as_array().into_iter().flatten())
    .filter_map(|source| source["source_ref"].as_str().map(str::to_string))
    .collect()
}

/// A script-run work launch and a verify launch ride the same assembler with their own contracts. The work launch
/// carries the worker contract, the goal and the input in its launch text, and a repository sandbox; the verify launch
/// carries the verify contract only.
#[tokio::test]
#[ignore = "integration"]
async fn worker_run_kinds_deliver_snapshot_bytes_and_task_context() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![
    result_script("worker done"),
    result_script("report noted"),
    result_script("verified"),
    result_script("report noted"),
  ]);
  let stack = scripted_stack(&scripts).await?;
  let process_guard = StackProcessGuard::new(&stack);
  let dir = tempfile::tempdir()?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let script_task = TaskSpec {
    goal: "ship it".to_string(),
    repo_path: Some(repo.display().to_string()),
    task_type: Some(TaskType::ScriptRun),
    ..TaskSpec::default()
  };
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(script_task)).await?;

  stack.dispatch.admit_input(&worker.id, event_types::USER, "do the thing", "user", json!({})).await?;
  let run = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  process_guard.track_run(&worker.id, &run).await?;
  assert_eq!(stack.wait_terminal(&worker.id, &run).await?, RunOutcomeKind::Success);
  process_guard.track_all()?;
  let record = stack.store().read_run_sync(&worker.id, &run)?.expect("the run is recorded");
  let snapshot = snapshot_of(&record)?;
  let joined = snapshot["blocks"]
    .as_array()
    .into_iter()
    .flatten()
    .filter_map(|block| block["text"].as_str())
    .collect::<Vec<_>>()
    .join("\n\n");
  let builds = scripts.builds();
  assert_eq!(
    builds[0].launch.instructions_content.as_deref(),
    Some(joined.as_str()),
    "the launch gets the snapshot bytes"
  );
  let refs = snapshot_source_refs(&snapshot);
  assert!(refs.iter().any(|source| source == "prompts/worker.md"), "{refs:?}");
  assert!(!refs.iter().any(|source| source == "prompts/task_manager.md"), "{refs:?}");
  let launch_text = std::fs::read_to_string(stack.store().run_dir(&worker.id, &run).join("launch_prompt.md"))?;
  assert!(launch_text.contains("ship it"), "the goal is in the launch text");
  assert!(launch_text.contains("do the thing"), "the input is in the launch text");
  assert!(launch_text.contains("isolated sandbox"), "the script-run bindings context is in the launch text");
  assert!(!launch_text.contains("This is a script-run task"), "that rule is managed, not launch text");
  // A repository script-run task gets its sandbox worktree, recorded on the Run.
  assert!(record.worktree_path.is_some(), "the repository sandbox is recorded");
  assert!(record.branch_name.is_some(), "the sandbox branch is recorded");

  // The verify contract: the verify task's managed block, and no worker contract.
  let verify_task =
    TaskSpec { goal: "verify it".to_string(), task_type: Some(TaskType::Verify), ..TaskSpec::default() };
  let verify = stack.node("v", Some(&manager.id), TaskProfile::Worker, Some(verify_task)).await?;
  stack.dispatch.admit_input(&verify.id, event_types::USER, "verify the result", "user", json!({})).await?;
  let verify_run = launched_run_id(&stack.dispatch.dispatch_pending(&verify.id).await?)?;
  process_guard.track_run(&verify.id, &verify_run).await?;
  assert_eq!(stack.wait_terminal(&verify.id, &verify_run).await?, RunOutcomeKind::Success);
  process_guard.track_all()?;
  let verify_record = stack.store().read_run_sync(&verify.id, &verify_run)?.expect("the verify run is recorded");
  let verify_refs = snapshot_source_refs(&snapshot_of(&verify_record)?);
  assert!(verify_refs.iter().any(|source| source == "prompts/verify.md"), "{verify_refs:?}");
  assert!(!verify_refs.iter().any(|source| source == "prompts/worker.md"), "{verify_refs:?}");
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 2)).await?;
  settle_node(&stack, &manager.id).await?;
  process_guard.track_all()?;
  Ok(())
}

/// A repository whose local base is ahead of origin fails worktree preparation after the Run was admitted. The Run
/// lands its durable failed fact with the actual error as evidence in its own events log, and the parent receives a
/// failure report naming it. The report's delivery archives the worker.
#[tokio::test]
#[ignore = "integration"]
async fn worktree_preparation_failure_lands_failed_run_and_reports_to_parent() -> anyhow::Result<()> {
  let stack = Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"failure noted"}'"#).await?;
  stack.write_access_key()?;
  let dir = tempfile::tempdir()?;
  let root = dir.path().join("prep-fail");
  std::fs::create_dir_all(&root)?;
  let (repo, _origin) = init_repo_with_origin(&root)?;
  // The failure's exact shape: the local base branch is ahead of origin.
  std::fs::write(repo.join("local.txt"), "local work\n")?;
  synthetic_git(&repo, &["add", "."])?;
  synthetic_git(&repo, &["commit", "-q", "-m", "local-only"])?;

  let parent = stack.node("delegate-parent", None, TaskProfile::Manager, None).await?;
  // The user message authorizes takeoff without launching a manager turn: the turn waits for the delegation's report.
  stack.dispatch.set_executor(None);
  stack
    .dispatch
    .admit_input(&parent.id, event_types::USER, "Take off and delegate the phrase task.", "user", json!({}))
    .await?;
  stack.dispatch.set_executor(Some(Arc::new(stack.adapter.clone())));

  let router = charliebot_tasks::routes::routes(&stack.features);
  let body = json!({
    "session_id": parent.id,
    "description": "## Goal\n\nsay the phrase\n",
    "task_type": "quick-edit",
    "keep_worktree": false,
    "repo_path": repo.display().to_string(),
    "base_branch": "main",
  });
  let (status, delegated) = post_delegate(&router, body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = delegated["session_id"].as_str().expect("delegate returns a child session id").to_string();
  let run_id = delegated["run_id"].as_str().expect("delegate returns the first run id").to_string();

  assert_eq!(stack.wait_terminal(&child_id, &run_id).await?, RunOutcomeKind::Failed);
  let run = stack.store().read_run_sync(&child_id, &run_id)?.expect("the run is recorded");
  assert!(run.pid.is_none(), "the process never started; the failure is a launch failure");
  assert!(
    run.events_ref.as_deref().is_some_and(|path| path.ends_with("events.jsonl")),
    "the evidence ref is set: {run:?}"
  );
  let error = error_text(&stack, &child_id, &run_id)?;
  assert!(error.contains("differs from origin/main"), "the evidence names the actual error: {error}");

  settle_until(|| Ok(!stack.child_reports(&parent.id)?.is_empty())).await?;
  let report = stack.child_reports(&parent.id)?.pop().expect("checked above");
  assert_eq!(report["outcome"], "failed");
  assert!(report["summary"].as_str().unwrap_or_default().contains("differs from origin/main"), "{report}");
  settle_until(|| Ok(stack.store().task_state(&child_id)? == "archived")).await?;
  assert_eq!(stack.child_reports(&parent.id)?.len(), 1, "the failed-before-start report is the only one");
  let closes = stack.closes(&child_id)?;
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["outcome"], "archived");
  assert_eq!(closes[0]["actor"], "system");
  assert_eq!(closes[0]["request_id"], format!("unsuccessful:{run_id}"));
  assert_eq!(closes[0]["run_ids"], json!([run_id]));
  settle_node(&stack, &parent.id).await?;
  Ok(())
}

/// A manager turn's stored snapshot reads on after its rule body is deleted, and an explicit retry is a new Run that
/// assembles from the current sources.
#[tokio::test]
#[ignore = "integration"]
async fn recovery_after_rule_deletion_uses_the_original_snapshot() -> anyhow::Result<()> {
  let stack = Stack::start(r#"printf '%s\n' '{"type":"result","subtype":"success","result":"turn"}'"#).await?;
  stack.write_access_key()?;
  let guard = ProcessGroupGuard::new();
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let rule = PatchTask { node_prompt: Some(Some("original rule\n".to_string())), ..PatchTask::default() };
  stack.store().patch_task(&manager.id, rule, TaskCaller::Operator, &*stack.dispatch).await?;
  stack.dispatch.admit_input(&manager.id, event_types::USER, "go", "user", json!({})).await?;
  let run = launched_run_id(&stack.dispatch.dispatch_pending(&manager.id).await?)?;
  settle_until(|| Ok(stack.store().read_run_sync(&manager.id, &run)?.is_some_and(|run| run.pid.is_some()))).await?;
  let pid = stack.store().read_run_sync(&manager.id, &run)?.and_then(|run| run.pid).expect("the turn records its pid");
  guard.track(pid);
  assert_eq!(stack.wait_terminal(&manager.id, &run).await?, RunOutcomeKind::Success);

  let record = stack.store().read_run_sync(&manager.id, &run)?.expect("the run is recorded");
  let stored = snapshot_of(&record)?;
  let texts: Vec<&str> =
    stored["blocks"].as_array().into_iter().flatten().filter_map(|block| block["text"].as_str()).collect();
  assert!(texts.iter().any(|text| text.contains("original rule")), "the snapshot carries the rule body");

  // The rule body is deleted afterwards; the Run's stored snapshot still reads.
  let node_ref = stack.store().load_task_meta(&manager.id).await?.node_prompt_ref.expect("the node rule is stored");
  let body = stack.cfg.charliebot_home.join("prompt_bodies").join(format!("{node_ref}.md"));
  std::fs::remove_file(&body)?;
  assert_eq!(snapshot_of(&record)?, stored);

  // An explicit retry is a new Run with the current sources: the body is rewritten first, then the retry assembles.
  std::fs::write(&body, "rewritten rule\n")?;
  let retry = task_sessions::create_retry(stack.store(), &stack.cfg, &manager.id, "retry-1", &run, None, None).await?;
  assert_ne!(retry.id, run);
  Ok(())
}

/// A worker Run whose backend cannot be resolved fails at launch, and no manager-turn launch notice is written for it.
#[tokio::test]
#[ignore = "integration"]
async fn a_worker_launch_failure_writes_no_turn_launch_error() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node(
      "w",
      Some(&manager.id),
      TaskProfile::Worker,
      Some(spec("bump the host cron schedule line", Some(TaskType::QuickEdit))),
    )
    .await?;
  let run = RunRecord { backend: Some("not-configured".to_string()), ..work_run(&worker.id, "run-work", None) };
  register_run(&stack, run, "bump the host cron schedule line").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Failed);
  let notices: Vec<Value> = stack
    .store()
    .load_chat_events_sync(&worker.id)?
    .into_iter()
    .chain(stack.run_events(&worker.id)?)
    .filter(|event| event["source"].as_str() == Some("turn_launch"))
    .map(|event| event.to_value())
    .collect();
  assert!(notices.is_empty(), "a worker launch writes no turn launch notice: {notices:?}");
  Ok(())
}

/// A worker run whose error result names a rejection and whose stderr carries only a model warning: the run's failure
/// row carries the result text, not the warning.
#[tokio::test]
#[ignore = "integration"]
async fn a_failed_run_reports_its_error_result_instead_of_the_stderr_tail() -> anyhow::Result<()> {
  let result = json!({
    "type": event_types::RESULT,
    "subtype": "success",
    "is_error": true,
    "api_error_status": 402,
    "result": "API Error: 402 the upstream rejected the request",
  });
  let warning = "[synthetic:model_warning] {\"model\":\"synthetic-model\"}";
  let script = script_lines(&[result], &format!("printf '%s\\n' '{warning}' >&2; exit 1"));
  let scripts = Scripts::new(vec![script, result_script("report consumed")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "do the work").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Failed);
  let log = run_log(&stack, &worker.id, "run-work")?;
  let failure_rows: Vec<&str> = log
    .iter()
    .filter(|event| event["type"] == event_types::ERROR)
    .filter_map(|event| event["content"].as_str())
    .collect();
  assert_eq!(failure_rows, ["API Error: 402 the upstream rejected the request"], "one failure row carries the result");
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// The first `run_id` of a dispatch decision that launched.
fn launched_run_id(decision: &Value) -> anyhow::Result<String> {
  Ok(decision["run_id"].as_str().ok_or_else(|| anyhow::anyhow!("the dispatch names no run: {decision}"))?.to_string())
}

/// The `run_finished` write of a successful worker run fails out of space. The end-landing retry drains the run from
/// its raw log and lands the end record, the parent gets exactly one completed report, and no second process starts.
#[tokio::test]
#[ignore = "integration"]
async fn worker_run_finished_enospc_retries_and_lands_without_restart() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("work done"), result_script("report consumed")]);
  let stack = scripted_stack(&scripts).await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;
  let fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::RUN_FINISHED,
    &[worker.id.as_str()],
    1,
    libc::ENOSPC,
  );

  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let run_id = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  assert_eq!(stack.wait_terminal(&worker.id, &run_id).await?, RunOutcomeKind::Success);
  assert_eq!(fault.hits(), [Hit::Failed, Hit::Written], "one failed write; the retry's write landed");
  assert_eq!(stack.store().busy_since(&worker.id), None, "the dead process's busy timer is closed");

  // The task's type is unset, so the worker takes the non-implement branch: no review, and the completion follow-up
  // closes it with one completed report.
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("completed")], "success-based, once");
  assert!(stack.adapter.landing_retry_nodes().is_empty(), "a clean round ended the retry task");
  let work_prompts = scripts.builds().iter().filter(|build| build.prompt_contains("Start the task.")).count();
  assert_eq!(work_prompts, 1, "one process: the retry drained, never relaunched");
  Ok(())
}

/// A worker whose task record leaves the task type unset takes the non-implement branch: its success spawns no review,
/// and the completion follow-up closes it with one completed report.
#[tokio::test]
#[ignore = "integration"]
async fn unset_task_type_success_takes_the_non_implement_branch() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("work done")]);
  let stack = scripted_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;

  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let run_id = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  assert_eq!(stack.wait_terminal(&worker.id, &run_id).await?, RunOutcomeKind::Success);
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("completed")]);
  assert!(stack.run_ids_of_kind(&worker.id, RunKind::Review)?.is_empty(), "no review run is spawned");
  Ok(())
}

/// A successful run's delivery write fails out of space. The end record is already on disk, the retry delivers the
/// success-based report exactly once, and the run's live end time survives the retry.
#[tokio::test]
#[ignore = "integration"]
async fn parent_report_enospc_retry_delivers_report_once() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("work done"), result_script("report consumed")]);
  let stack = scripted_stack(&scripts).await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;
  let fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::CHILD_REPORT,
    &[manager.id.as_str()],
    1,
    libc::ENOSPC,
  );

  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let run_id = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  assert_eq!(stack.wait_terminal(&worker.id, &run_id).await?, RunOutcomeKind::Success);
  // The retry's re-delivery is asynchronous with the live finish: wait for the one report.
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  assert_eq!(fault.hits(), [Hit::Failed, Hit::Written], "the live delivery failed; the retry's landed");
  let ended_at = stack.store().read_run_sync(&worker.id, &run_id)?.and_then(|run| run.ended_at);
  assert!(ended_at.is_some());
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("completed")], "success-based, once");
  let fresh = stack.store().read_run_sync(&worker.id, &run_id)?.and_then(|run| run.ended_at);
  assert_eq!(fresh, ended_at, "the retry kept the live end time");
  assert!(stack.adapter.landing_retry_nodes().is_empty());
  Ok(())
}

/// Below the disk threshold a worker-class run stays queued with one blocked report, while a manager turn still
/// launches. A zero threshold skips the check and the queued worker launches.
#[tokio::test]
#[ignore = "integration"]
async fn disk_headroom_precheck_withholds_worker_run_but_not_manager_turn() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("work done"), result_script("report consumed")]);
  let stack = Stack::start_with_min_free_disk_gib(
    vec![scripts.option(BACKEND_ID, BACKEND_TYPE)],
    vec![BACKEND_ID.to_string()],
    1_000_000_000,
  )
  .await?;
  let process_guard = StackProcessGuard::new(&stack);
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;

  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let run_id = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;

  // The launch is fire-and-forget: poll for the withheld verdict's durable event, then re-read the queued run.
  let withheld = withheld_reason(&stack, &worker.id)?;
  let withheld = match withheld {
    Some(reason) => reason,
    None => {
      settle_until(|| Ok(withheld_reason(&stack, &worker.id)?.is_some())).await?;
      withheld_reason(&stack, &worker.id)?.expect("checked above")
    }
  };
  assert!(withheld.contains("disk free") && withheld.contains("below 1000000000 GiB"), "{withheld}");
  assert!(withheld.contains(&stack.home.path().display().to_string()), "{withheld}");
  let run = stack.store().read_run_sync(&worker.id, &run_id)?.expect("the queued run is recorded");
  assert!(run.pid.is_none(), "no process, stays queued");
  assert!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, &run_id)?.is_none());
  let worker_run_dir = stack.store().run_dir(&worker.id, &run_id);
  assert!(
    !scripts.builds().iter().any(|build| build.launch.log_dir.as_deref() == Some(worker_run_dir.as_path())),
    "the worker never reached its backend"
  );
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  process_guard.track_all()?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("blocked")]);

  // A manager turn launches under the same threshold: the blocked report's own wake started one.
  settle_node(&stack, &manager.id).await?;
  process_guard.track_all()?;
  let turns = stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?;
  assert_eq!(turns.len(), 1);
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&manager.id)?, &turns[0])?, Some(RunOutcomeKind::Success));

  // Threshold 0 skips the check, so the queued worker launches.
  stack.set_min_free_disk_gib(0)?;
  let decision = stack.dispatch.dispatch_pending(&worker.id).await?;
  let run_id = launched_run_id(&decision)?;
  process_guard.track_run(&worker.id, &run_id).await?;
  assert_eq!(stack.wait_terminal(&worker.id, &run_id).await?, RunOutcomeKind::Success);
  process_guard.track_all()?;
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// The reason of the worker's `run_launch_withheld` fact, when one landed.
fn withheld_reason(stack: &Stack, session_id: &str) -> anyhow::Result<Option<String>> {
  Ok(
    stack
      .run_events(session_id)?
      .into_iter()
      .find(|event| event["type"] == event_types::RUN_LAUNCH_WITHHELD)
      .map(|event| event["reason"].as_str().unwrap_or_default().to_string()),
  )
}

/// A non-space failure in the end path lands the run at once and starts no retry. A success fact whose delivery hits
/// EIO sends no failed report.
#[tokio::test]
#[ignore = "integration"]
async fn non_space_end_failure_lands_immediately_and_starts_no_retry() -> anyhow::Result<()> {
  let scripts =
    Scripts::new(vec![result_script("work done"), result_script("more work done"), result_script("report consumed")]);
  let stack = scripted_stack(&scripts).await?;
  stack.adapter.set_landing_retry_interval(Duration::from_millis(50));
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack.node("w", Some(&manager.id), TaskProfile::Worker, Some(spec("do the work", None))).await?;

  // First half: the run_finished write raises EACCES, not space.
  let _first_fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::RUN_FINISHED,
    &[worker.id.as_str()],
    1,
    libc::EACCES,
  );
  stack.dispatch.admit_input(&worker.id, event_types::USER, "Start the task.", "user", json!({})).await?;
  let first = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  assert_eq!(
    stack.wait_terminal(&worker.id, &first).await?,
    RunOutcomeKind::Failed,
    "landed immediately by the existing path"
  );
  settle_until(|| Ok(!stack.child_reports(&manager.id)?.is_empty())).await?;
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("failed")], "the failed landing reports once");
  assert!(stack.adapter.landing_retry_nodes().is_empty(), "no retry for a non-space error");

  // Second half: a success fact whose delivery hits EIO. The durable outcome governs, so no failed report follows.
  let _second_fault = write_faults::inject_chat_append_fault(
    stack.store(),
    event_types::CHILD_REPORT,
    &[manager.id.as_str()],
    50,
    libc::EIO,
  );
  stack.dispatch.admit_input(&worker.id, event_types::USER, "More work.", "user", json!({})).await?;
  let second = launched_run_id(&stack.dispatch.dispatch_pending(&worker.id).await?)?;
  let second_outcome = stack.wait_terminal(&worker.id, &second).await?;
  assert_eq!(second_outcome, RunOutcomeKind::Success);
  settle_until(|| Ok(stack.adapter.landing_retry_nodes().is_empty())).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert!(!outcomes[1..].contains(&json!("failed")), "{outcomes:?}");
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// A `/bin/sh` script that prints each event as one JSON line and then runs `tail`.
fn script_lines(events: &[Value], tail: &str) -> String {
  let mut commands = vec![":".to_string()];
  for event in events {
    let line = serde_json::to_string(event).expect("a JSON value serializes");
    assert!(!line.contains('\''), "a scripted event line must not contain a single quote");
    commands.push(format!("printf '%s\\n' '{line}'"));
  }
  commands.push(tail.to_string());
  commands.join("; ")
}

/// The events of a run's events log, one per line.
fn run_log(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<Vec<Value>> {
  let path = stack.store().run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME);
  std::fs::read_to_string(path)?
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(|line| Ok(serde_json::from_str(line)?))
    .collect()
}

/// Over a non-Claude backend: the launch carries no `claude_account` either.
#[tokio::test]
#[ignore = "integration"]
async fn unpooled_fresh_worker_launch_carries_no_account_on_a_non_claude_backend() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("done")]);
  let stack = scripted_stack(&scripts).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  assert!(scripts.builds()[0].launch.backend_kwargs.get("claude_account").is_none());
  Ok(())
}

/// A launched worker run holds its busy interval from its recorded start while its process is alive, and the terminal
/// fact closes the interval. The sidebar paints that interval as the row's spinner.
#[tokio::test]
#[ignore = "integration"]
async fn a_launched_worker_run_holds_its_busy_interval_until_it_finishes() -> anyhow::Result<()> {
  let gate_dir = tempfile::tempdir()?;
  let gate = gate_dir.path().join("release");
  let script = format!("while [ ! -e '{}' ]; do sleep 0.05; done; {}", gate.display(), result_script("done"));
  let scripts = Scripts::new(vec![script]);
  let stack = scripted_stack(&scripts).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "ship it").await?;
  assert_eq!(stack.store().busy_since(&worker.id), None, "no interval opens before the launch");

  stack.adapter.launch(&worker.id, "run-work")?;
  // The process waits at the gate, so the run stays alive for every assertion until the gate opens.
  settle_until(|| Ok(stack.store().busy_since(&worker.id).is_some())).await?;
  let run = stack.store().read_run_sync(&worker.id, "run-work")?.expect("the launched run is registered");
  assert!(run.pid.is_some() && run.ended_at.is_none(), "the process is launched and the run has not ended");
  assert_eq!(stack.store().busy_since(&worker.id), run.started_at, "the interval opens at the recorded start");

  std::fs::write(&gate, "")?;
  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  assert_eq!(stack.store().busy_since(&worker.id), None, "the finish closes the interval");
  Ok(())
}

// ---------------------------------------------------------------------------
// Quota failover over the backend pool
// ---------------------------------------------------------------------------

/// The review feedback an interrupted run answered, bound to its revision batch.
const REVIEW_FEEDBACK: &str = "Review: the rename missed the call in foo.py";

/// A stack with three options over one scripted type, `fake`, `fake2` and `fake3`, and the pool `fake2` then `fake3`.
async fn failover_stack(scripts: &Scripts) -> anyhow::Result<Stack> {
  let options = ["fake", "fake2", "fake3"]
    .map(|id| BackendOption { model: "fake-model".to_string(), ..scripts.option(id, BACKEND_TYPE) })
    .to_vec();
  let stack = Stack::start_with(options, vec!["fake2".to_string(), "fake3".to_string()]).await?;
  stack.write_access_key()?;
  Ok(stack)
}

/// Install a synthetic shell router in the stack's hot-reloaded configuration.
fn install_router(stack: &Stack, directory: &Path, body: &str, arguments: &[String]) -> anyhow::Result<()> {
  install_router_on(stack, stack.cfg.as_ref().clone(), directory, body, arguments)
}

/// Install a synthetic shell router in `cfg`, and serve `cfg` as the stack's hot-reloaded configuration.
fn install_router_on(
  stack: &Stack,
  mut cfg: CharlieBotConfig,
  directory: &Path,
  body: &str,
  arguments: &[String],
) -> anyhow::Result<()> {
  let path = directory.join("fake-router.sh");
  std::fs::write(&path, format!("set -eu\n{body}\n"))?;
  let mut command = vec!["/bin/sh".to_string(), path.display().to_string()];
  command.extend(arguments.iter().cloned());
  cfg.backends.routing = Some(RoutingConfig { command });
  stack.replace_config(cfg)
}

/// A router body that prints `answer` as its one JSON object. It drains its request first, as a router reads its stdin.
fn router_answer_body(answer: Value) -> String {
  format!("cat >/dev/null\nprintf '%s' '{answer}'")
}

/// A router body that routes to `backends` in that order.
fn router_order_body(backends: &[&str]) -> String {
  router_answer_body(json!({"routed": true, "backends": backends, "reason": "design p=0.9"}))
}

/// A router body that fails: it writes `router boom` to stderr and exits 3.
const FAILING_ROUTER: &str = "cat >/dev/null\nprintf '%s' 'router boom' >&2\nexit 3";

/// A manager over the scripted options `fake` and `fake2` with the pool `pool`, holding a takeoff message. The
/// dispatcher has no executor, so the admitted message stays pending and launches nothing.
async fn delegating_manager(scripts: &Scripts, pool: &[&str]) -> anyhow::Result<(Stack, String)> {
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let stack = Stack::start_with(options, pool.iter().map(|id| id.to_string()).collect()).await?;
  stack.write_access_key()?;
  let manager = stack.node("router-parent", None, TaskProfile::Manager, None).await?;
  stack.dispatch.set_executor(None);
  stack.dispatch.admit_input(&manager.id, event_types::USER, "Take off and delegate this.", "user", json!({})).await?;
  Ok((stack, manager.id))
}

/// The body of a repo-less quick-edit delegation under `manager_id`.
fn quick_edit_body(manager_id: &str) -> Value {
  json!({"session_id": manager_id, "description": "run the quick edit", "task_type": "quick-edit"})
}

/// A route that names the pool: the child keeps the pool id, and the node records the route with its pool flag.
#[tokio::test]
#[ignore = "integration"]
async fn a_route_naming_the_pool_keeps_the_pool_id_on_the_child() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&[ROTATION_POOL_ID]), &[])?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id and its first turn selects the member");
  assert_eq!(meta.extra("route").and_then(|route| route.get("backends")), Some(&json!([ROTATION_POOL_ID])));
  Ok(())
}

/// The `backends.options` entries of the scripted `options`, each with the token its builds read.
fn option_entries(options: &[BackendOption]) -> String {
  options
    .iter()
    .map(|option| {
      let token = option.type_fields.get(TOKEN_FIELD).and_then(|value| value.as_str()).expect("a scripted option has its token");
      format!(
        "    - id: {id}\n      label: {id}\n      type: {BACKEND_TYPE}\n      model: {model}\n      {TOKEN_FIELD}: {token}\n",
        id = option.id,
        model = option.model,
      )
    })
    .collect()
}

/// The `server` section the harness runs with (`Stack::config`), written out because a loaded config otherwise takes the
/// host defaults: a nonzero session memory cap starts a session cgroup, which a test host without the agents slice fails,
/// and the headroom and work-guard switches make a launch depend on the host too.
const HOME_SERVER_SECTION: &str = concat!(
  "server:\n",
  "  host: 127.0.0.1\n",
  "  port: 18650\n",
  "  subprocess_buffer_limit_mb: 1\n",
  "  session_memory_max_mb: 0\n",
  "  session_swap_max_mb: 0\n",
  "  session_tasks_max: 0\n",
  "  min_free_disk_gib: 0\n",
  "  enable_session_cpu_weight: true\n",
  "  enable_session_io_weight: true\n",
  "  enable_local_work_guard: false\n",
);

/// The config a home loads when its `backends` section is `backends` (YAML under the key, indented two spaces) over the
/// scripted `options`. It goes through the config loader, so a `backends.pools` entry declares its pool as a host home
/// does.
fn home_config(stack: &Stack, options: &[BackendOption], backends: &str) -> anyhow::Result<CharlieBotConfig> {
  let home = stack.home.path();
  let yaml = format!(
    "{HOME_SERVER_SECTION}paths:\n  workspace_dirs: []\n  worktree_dir: {}\n  scratch_dir: {}\nbackends:\n{backends}  options:\n{}",
    home.join("worktrees").display(),
    home.join("scripts").display(),
    option_entries(options),
  );
  std::fs::write(home.join("config.yaml"), yaml)?;
  Ok(stack.features.registry.load_config(home)?)
}

/// Delegate `body` through the served configuration and wait for the work run it registers. The run must name one of
/// `members` as its backend, record that member's model, and end successfully: a run that cannot resolve its backend
/// fails here, at the launch the live delegation failed at.
async fn delegate_and_run(stack: &Stack, body: Value, members: &[&str]) -> anyhow::Result<(String, RunRecord)> {
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let run_id = text(&delegated, "run_id")?;
  let outcome = stack.wait_terminal(&child_id, &run_id).await?;
  let error = error_text(stack, &child_id, &run_id)?;
  assert_eq!(outcome, RunOutcomeKind::Success, "the work run ended {outcome:?}: {error}");
  let run = stack.store().read_run_sync(&child_id, &run_id)?.expect("the delegation registers its work run");
  let member = run.backend.clone().unwrap_or_default();
  assert!(members.contains(&member.as_str()), "the work run names backend {member:?}, want a member of {members:?}");
  assert_eq!(run.model.as_deref(), Some(format!("{member}-model").as_str()), "the run records its member's model");
  Ok((child_id, run))
}

/// The child keeps the pool id, and its `pool_member` names one of the pool's `members`.
async fn assert_child_keeps_pool_and_member(
  stack: &Stack,
  child_id: &str,
  pool: &str,
  members: &[&str],
) -> anyhow::Result<()> {
  let meta = stack.store().load_task_meta(child_id).await?;
  assert_eq!(meta.backend, pool, "the child keeps the pool id");
  let member = meta.extra(model_pool::POOL_MEMBER_FIELD).and_then(Value::as_str).unwrap_or_default();
  assert!(members.contains(&member), "the child records pool_member {member:?}, want a member of {members:?}");
  Ok(())
}

/// The `backends` of a home that names the rotate pool `light` over `fake` and `fake2`, with its `calls`.
const LIGHT_POOL_BACKENDS: &str = "  pools:\n    - {id: light, label: Light, rule: rotate, members: [fake, fake2]}\n  calls:\n    worker: light\n    review: light\n    verify: light\n";

/// The `backends` of a home whose rotate pool `auto` holds `fake`, `fake2` and `fake3`, with `calls.verify` naming it.
/// On a rotate pool the position order makes a member exclusion visible: the first selection starts at `fake`.
const AUTO_VERIFY_BACKENDS: &str =
  "  pools:\n    - {id: auto, label: Auto, rule: rotate, members: [fake, fake2, fake3]}\n  calls:\n    verify: auto\n";

/// The `backends` of a home with a work pool (`fake`, `fake2`) and a disjoint review pool (`fake3`, `fake4`), and
/// `calls.review` naming the review pool.
const SPLIT_POOL_BACKENDS: &str = "  pools:\n    - {id: workpool, label: Work, rule: rotate, members: [fake, fake2]}\n    - {id: reviewpool, label: Review, rule: rotate, members: [fake3, fake4]}\n  calls:\n    review: reviewpool\n";

/// The `backends` of a home whose review pool holds `fake`, the first work-pool member, beside `fake3`.
const OVERLAPPING_POOL_BACKENDS: &str = "  pools:\n    - {id: workpool, label: Work, rule: rotate, members: [fake, fake2]}\n    - {id: reviewpool, label: Review, rule: rotate, members: [fake, fake3]}\n  calls:\n    review: reviewpool\n";

/// The `backends` of a home with one work pool and no `calls.review` key.
const ROUTE_ONLY_BACKENDS: &str = "  pools:\n    - {id: workpool, label: Work, rule: rotate, members: [fake, fake2]}\n";

/// A router whose `run_on: light` answers the pool `light`: the routine delegation's work run launches on one member
/// of `light`.
#[tokio::test]
#[ignore = "integration"]
async fn a_routine_delegation_launches_on_a_light_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("light work done")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, LIGHT_POOL_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["light"]), &[])?;

  let (child_id, _) = delegate_and_run(&stack, quick_edit_body(&manager), &["fake", "fake2"]).await?;
  assert_child_keeps_pool_and_member(&stack, &child_id, "light", &["fake", "fake2"]).await
}

/// With routing unset: the `calls.worker` pool target selects the member at delegation, and the work run launches on it.
#[tokio::test]
#[ignore = "integration"]
async fn a_calls_pool_worker_delegation_without_routing_launches_on_a_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("calls work done")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  stack.replace_config(home_config(&stack, &options, LIGHT_POOL_BACKENDS)?)?;

  let (child_id, _) = delegate_and_run(&stack, quick_edit_body(&manager), &["fake", "fake2"]).await?;
  assert_child_keeps_pool_and_member(&stack, &child_id, "light", &["fake", "fake2"]).await?;
  Ok(())
}

/// An explicit `--backend light` names the pool: the child keeps `light`, and the work run launches on one member of it.
#[tokio::test]
#[ignore = "integration"]
async fn a_named_pool_backend_launches_on_a_member_and_keeps_the_pool_id() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("named work done")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  stack.replace_config(home_config(&stack, &options, LIGHT_POOL_BACKENDS)?)?;
  let mut body = quick_edit_body(&manager);
  body["backend"] = json!("light");

  let (child_id, _) = delegate_and_run(&stack, body, &["fake", "fake2"]).await?;
  assert_child_keeps_pool_and_member(&stack, &child_id, "light", &["fake", "fake2"]).await
}

/// A routed verify: the router names `light` for the verify, and its run launches on one member of `light`.
#[tokio::test]
#[ignore = "integration"]
async fn a_routed_verify_delegation_launches_on_a_light_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, LIGHT_POOL_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["light"]), &[])?;
  let body = json!({"session_id": manager, "description": "confirm the change", "task_type": "verify"});

  let (child_id, _) = delegate_and_run(&stack, body, &["fake", "fake2"]).await?;
  assert_child_keeps_pool_and_member(&stack, &child_id, "light", &["fake", "fake2"]).await
}

/// The review of a routed work run: the route names the pool, so the review takes a member of it that the work run did
/// not run on.
#[tokio::test]
#[ignore = "integration"]
async fn a_routed_pool_work_run_reviews_on_another_member() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let scripts = Scripts::new(vec![result_script("implemented"), result_script("review ok")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, LIGHT_POOL_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["light"]), &[])?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;
  let body = json!({
    "session_id": manager,
    "description": "add a marker file",
    "task_type": "implement",
    "repo_path": repo.display().to_string(),
    "base_branch": "main",
  });

  let (child_id, work) = delegate_and_run(&stack, body, &["fake", "fake2"]).await?;
  let review = wait_for_review_of(&stack, &child_id, &work.id).await?;
  let work_member = work.backend.clone().unwrap_or_default();
  let review_member = review.backend.clone().unwrap_or_default();
  assert!(
    ["fake", "fake2"].contains(&review_member.as_str()) && review_member != work_member,
    "the review runs on {review_member:?}, want the pool member other than the work run's {work_member:?}"
  );
  assert_eq!(stack.wait_terminal(&child_id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// The review run that work run `work_run_id` registers, once it registers.
async fn wait_for_review_of(stack: &Stack, session_id: &str, work_run_id: &str) -> anyhow::Result<RunRecord> {
  let deadline = std::time::Instant::now() + TEST_DEADLINE;
  loop {
    let review = stack
      .run_records(session_id)?
      .into_iter()
      .find(|run| run.kind == RunKind::Review && run.review_of_run_id.as_deref() == Some(work_run_id));
    if let Some(review) = review {
      return Ok(review);
    }
    anyhow::ensure!(std::time::Instant::now() < deadline, "work run {work_run_id} did not register its review");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

/// The body of a repo-backed implement delegation under `manager_id`.
fn implement_body(manager_id: &str, repo: &Path) -> Value {
  json!({
    "session_id": manager_id,
    "description": "add a marker file",
    "task_type": "implement",
    "repo_path": repo.display().to_string(),
    "base_branch": "main",
  })
}

/// A routed work run whose route names the work pool while `calls.review` names a disjoint review pool: the review
/// takes a member of the review pool, never the route's pool.
#[tokio::test]
#[ignore = "integration"]
async fn a_routed_work_run_reviews_on_the_calls_review_pool() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let scripts = Scripts::new(vec![result_script("implemented"), result_script("review ok")]);
  let options = ["fake", "fake2", "fake3", "fake4"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, SPLIT_POOL_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["workpool"]), &[])?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;

  let (child_id, work) = delegate_and_run(&stack, implement_body(&manager, &repo), &["fake", "fake2"]).await?;
  let review = wait_for_review_of(&stack, &child_id, &work.id).await?;
  let review_member = review.backend.clone().unwrap_or_default();
  assert!(
    ["fake3", "fake4"].contains(&review_member.as_str()),
    "the review runs on {review_member:?}, want a member of the calls.review pool"
  );
  assert_eq!(stack.wait_terminal(&child_id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// `calls.review` names a pool that holds the member the work run used: the reviewer is another member of that pool.
#[tokio::test]
#[ignore = "integration"]
async fn a_review_pool_holding_the_work_member_reviews_on_its_other_member() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let scripts = Scripts::new(vec![result_script("implemented"), result_script("review ok")]);
  let options = ["fake", "fake2", "fake3"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, OVERLAPPING_POOL_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["workpool"]), &[])?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;

  let (child_id, work) = delegate_and_run(&stack, implement_body(&manager, &repo), &["fake", "fake2"]).await?;
  let review = wait_for_review_of(&stack, &child_id, &work.id).await?;
  assert_eq!(review.backend.as_deref(), Some("fake3"), "the review skips the review pool's copy of the work member");
  assert_eq!(stack.wait_terminal(&child_id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// With `calls.review` unset, a routed work run's review keeps the route's pool: another member of it.
#[tokio::test]
#[ignore = "integration"]
async fn an_unset_calls_review_reviews_on_the_routes_pool() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let scripts = Scripts::new(vec![result_script("implemented"), result_script("review ok")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let cfg = home_config(&stack, &options, ROUTE_ONLY_BACKENDS)?;
  let router_dir = tempfile::tempdir()?;
  install_router_on(&stack, cfg, router_dir.path(), &router_order_body(&["workpool"]), &[])?;
  let (repo, _origin) = init_repo_with_origin(dir.path())?;

  let (child_id, work) = delegate_and_run(&stack, implement_body(&manager, &repo), &["fake", "fake2"]).await?;
  let review = wait_for_review_of(&stack, &child_id, &work.id).await?;
  let review_member = review.backend.clone().unwrap_or_default();
  let work_member = work.backend.clone().unwrap_or_default();
  assert!(
    ["fake", "fake2"].contains(&review_member.as_str()) && review_member != work_member,
    "the review runs on {review_member:?}, want the route pool's member other than the work run's {work_member:?}"
  );
  assert_eq!(stack.wait_terminal(&child_id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// Cooling route: the routed option is cooling, so the default path runs the work on the pool and the delegation still
/// answers 200.
#[tokio::test]
#[ignore = "integration"]
async fn a_cooling_routed_option_falls_back_to_the_pool() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&["fake2"]), &[])?;
  backend_availability(&stack.features)?.record(
    "fake2",
    None,
    backend_availability::REASON_QUOTA_ERROR,
    "synthetic quota error",
  )?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")), "the pool's first member");
  Ok(())
}

/// No member available: every pool member is cooling, so the delegation answers 400 and the
/// detail names each member's recovery time.
#[tokio::test]
#[ignore = "integration"]
async fn every_backend_unavailable_returns_400_with_recovery_times() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("unused")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&[ROTATION_POOL_ID]), &[])?;
  backend_availability(&stack.features)?.record(
    "fake",
    None,
    backend_availability::REASON_QUOTA_ERROR,
    "synthetic quota error",
  )?;
  backend_availability(&stack.features)?.record(
    "fake2",
    None,
    backend_availability::REASON_QUOTA_ERROR,
    "synthetic quota error",
  )?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  let detail = response["detail"].as_str().unwrap_or_default();
  assert!(detail.contains("pool 'pool' has no selectable member") && detail.contains("recovers at"), "{response}");
  Ok(())
}

/// No routing section: with `backends.routing` unset, no router starts and the pool decides.
#[tokio::test]
#[ignore = "integration"]
async fn without_a_routing_section_no_router_starts_and_the_pool_decides() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), FAILING_ROUTER, &[])?;
  let mut cfg = stack.cfg.as_ref().clone();
  cfg.backends.routing = None; // the command would fail loudly if the code ever ran it
  stack.replace_config(cfg)?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert!(meta.extra("route").is_none_or(Value::is_null), "the node carries no route: {:?}", meta.extra("route"));
  Ok(())
}

/// Pinned delegation: a delegation that names its backend never asks the router.
#[tokio::test]
#[ignore = "integration"]
async fn a_pinned_delegation_never_calls_the_router() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pinned work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), FAILING_ROUTER, &[])?;
  let mut body = quick_edit_body(&manager);
  body["backend"] = json!("fake2");

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, "fake2");
  assert!(meta.extra("route").is_none_or(Value::is_null), "the node carries no route: {:?}", meta.extra("route"));
  Ok(())
}

/// Cold read: a cold-read delegation never asks the router, and its node carries no route.
#[tokio::test]
#[ignore = "integration"]
async fn a_cold_read_delegation_never_calls_the_router() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![answer_script("(1) answered.")]);
  let stack = cold_read_stack(&scripts, &["fake", "fake2"], &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), FAILING_ROUTER, &[])?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;

  let (child_id, _) = delegate_cold_read(&stack, &manager.id, &page).await?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert!(meta.extra("route").is_none_or(Value::is_null), "the node carries no route: {:?}", meta.extra("route"));
  Ok(())
}

/// Delegation cases: one routed verify request carries the chosen backend and route through
/// both the worker node metadata and its delegation card, and the router sees the manager's backend excluded.
#[tokio::test]
#[ignore = "integration"]
async fn routed_verify_delegation_persists_the_route_and_excludes_its_session_backend() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let stack = Stack::start_with(options, vec!["fake".to_string()]).await?;
  stack.write_access_key()?;
  let router_dir = tempfile::tempdir()?;
  let capture = router_dir.path().join("request.json");
  let calls = router_dir.path().join("calls.txt");
  install_router(
    &stack,
    router_dir.path(),
    "cat > \"$1\"\nprintf '%s\\n' started >> \"$2\"\nprintf '%s' '{\"routed\":true,\"backends\":[\"fake2\"],\"reason\":\"verify route\"}'",
    &[capture.display().to_string(), calls.display().to_string()],
  )?;
  let manager = stack.node("router-parent", None, TaskProfile::Manager, None).await?;
  stack.dispatch.set_executor(None);
  stack
    .dispatch
    .admit_input(&manager.id, event_types::USER, "Take off and verify this work.", "user", json!({}))
    .await?;

  let body = json!({
    "session_id": manager.id,
    "description": "confirm the change",
    "task_type": "verify",
  });
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = delegated["session_id"].as_str().expect("the response names the worker");
  let run_id = delegated["run_id"].as_str().expect("the response names the work run");
  let meta = stack.store().load_task_meta(child_id).await?;
  assert_eq!(meta.backend, "fake2");
  assert_eq!(meta.extra("route"), Some(&json!({"backends": ["fake2"], "reason": "verify route"})));
  let events = stack.store().load_chat_events_sync(&manager.id)?;
  let card =
    events.iter().find(|event| event["type"] == event_types::TASK_DELEGATED).expect("the delegation card exists");
  assert_eq!(card["route"].to_value(), json!({"backends": ["fake2"], "reason": "verify route"}));

  let request: Value = serde_json::from_slice(&std::fs::read(capture)?)?;
  assert_eq!(request["role"], "verify");
  assert_eq!(request["text"], "confirm the change");
  assert_eq!(request["session_backend"], "fake", "the manager's backend, which the router sees");
  assert_eq!(
    request["candidates"],
    json!([{"id": "fake2", "cooling_until": null}, {"id": ROTATION_POOL_ID, "cooling_until": null}])
  );
  assert_eq!(std::fs::read_to_string(calls)?, "started\n");
  assert_eq!(stack.wait_terminal(child_id, run_id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// A router failure does not fail the delegation: the implement takes the pool, and its node's route names the failure.
#[tokio::test]
#[ignore = "integration"]
async fn a_router_failure_takes_the_pool_and_records_the_failure() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), FAILING_ROUTER, &[])?;

  let body = json!({"session_id": manager, "description": "implement the change", "task_type": "implement"});
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")), "the pool's first member");
  let route = meta.extra("route").expect("the node records its route");
  assert_eq!(route["backends"], json!([]));
  let reason = route["reason"].as_str().unwrap_or_default();
  assert!(reason.starts_with("router failed: ") && reason.contains("router boom"), "{route}");
  Ok(())
}

/// A verify whose router fails keeps the verify selection: the reviewer is the pool member the session does not run on.
#[tokio::test]
#[ignore = "integration"]
async fn a_verify_router_failure_takes_the_verify_selection() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), FAILING_ROUTER, &[])?;

  let body = json!({"session_id": manager, "description": "confirm the change", "task_type": "verify"});
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(
    meta.extra(model_pool::POOL_MEMBER_FIELD),
    Some(&json!("fake2")),
    "the pool member that is not the session's backend"
  );
  let reason = meta.extra("route").and_then(|route| route["reason"].as_str()).unwrap_or_default();
  assert!(reason.starts_with("router failed: "), "{reason}");
  Ok(())
}

/// A quick-edit the router does not list takes the pool, and its node's route says the router did not route it.
#[tokio::test]
#[ignore = "integration"]
async fn an_unrouted_quick_edit_takes_the_pool_and_records_why() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(
    &stack,
    router_dir.path(),
    &router_answer_body(json!({"routed": false, "backends": [], "reason": "quick-edit not routed"})),
    &[],
  )?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")), "the pool's first member");
  assert_eq!(meta.extra("route"), Some(&json!({"backends": [], "reason": "quick-edit not routed"})));
  Ok(())
}

/// A verify the router does not list keeps the verify selection, and its node's route says why.
#[tokio::test]
#[ignore = "integration"]
async fn an_unrouted_verify_takes_the_verify_selection_and_records_why() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(
    &stack,
    router_dir.path(),
    &router_answer_body(json!({"routed": false, "backends": [], "reason": "verify not routed"})),
    &[],
  )?;

  let body = json!({"session_id": manager, "description": "confirm the change", "task_type": "verify"});
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(
    meta.extra(model_pool::POOL_MEMBER_FIELD),
    Some(&json!("fake2")),
    "the pool member that is not the session's backend"
  );
  assert_eq!(meta.extra("route"), Some(&json!({"backends": [], "reason": "verify not routed"})));
  Ok(())
}

/// A routed option that is cooling runs on the default path, which is the pool, and the route says it was cooling.
#[tokio::test]
#[ignore = "integration"]
async fn a_cooling_routed_option_runs_on_the_default_path_and_says_so() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&["fake2"]), &[])?;
  backend_availability(&stack.features)?.record(
    "fake2",
    None,
    backend_availability::REASON_QUOTA_ERROR,
    "synthetic quota error",
  )?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")), "the pool's first member");
  assert_eq!(
    meta.extra("route"),
    Some(&json!({"backends": ["fake2"], "reason": "design p=0.9; fake2 cooling; ran on the default path"}))
  );
  Ok(())
}

/// Which form a synthetic worker takes: a concrete backend the request named, or a pool-id session whose work run
/// names the member.
#[derive(Clone, Copy)]
enum WorkerForm {
  Direct,
  Pool,
}

/// A worker whose run ended on quota exhaustion, with its revision batch bound (`_interrupted_worker_run`). A pool-form
/// worker keeps the pool id on its session and records the member its run launches on. Returns the worker's id and the
/// run's id.
async fn interrupted_worker_run(
  stack: &Stack,
  parent: &str,
  task_type: TaskType,
  form: WorkerForm,
) -> anyhow::Result<(String, String)> {
  let (session_backend, member) = match form {
    WorkerForm::Direct => ("fake2", None),
    WorkerForm::Pool => (ROTATION_POOL_ID, Some("fake2")),
  };
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "worker".to_string(),
        task_parent_id: Some(parent.to_string()),
        profile: TaskProfile::Worker,
        task: Some(spec("do the work", Some(task_type))),
        name: Some("Synthetic worker".to_string()),
        backend: Some(session_backend.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  if let Some(member) = member {
    stack
      .store()
      .update_slot_fields(
        &worker.id,
        model_pool::OWNER,
        Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!(member))]),
      )
      .await?;
  }
  let run = revision_run_on_quota(stack, &worker.id, "fake2").await?;
  Ok((worker.id, run))
}

/// Admit the review feedback to `worker` and finish its revision run `run-revision` as failed on `backend`. Returns the
/// run's id.
async fn revision_run_on_quota(stack: &Stack, worker: &str, backend: &str) -> anyhow::Result<String> {
  let feedback = stack.dispatch.admit_input(worker, event_types::USER, REVIEW_FEEDBACK, "user", json!({})).await?;
  let input = feedback["id"].as_str().expect("an admitted input has an id").to_string();
  let record = RunRecord {
    backend: Some(backend.to_string()),
    model: Some("fake-model".to_string()),
    input_event_ids: vec![input],
    ..synthetic_run(worker, "run-revision", RunKind::Work)
  };
  register_run(stack, record, "do the work").await?;
  let failed = RunOutcome { exit_code: Some(1), ..RunOutcome::new(RunOutcomeKind::Failed) };
  stack.dispatch.finish_run(worker, "run-revision", failed).await?;
  Ok("run-revision".to_string())
}

/// The failover of a pool-chosen worker whose run ended on quota exhaustion: the member cools, the worker's own
/// `pool_member` moves to the next member, and a retry run on it binds the interrupted run's input batch and launches
/// with the feedback it answers.
async fn quota_failure_fails_over_to_the_next_member(task_type: TaskType) -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("retry done"), result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, task_type, WorkerForm::Pool).await?;
  stack.adapter.record_quota_failure_for_test(&worker, &run, None);
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  let interrupted =
    stack.run_records(&worker)?.into_iter().find(|record| record.id == run).expect("the run is recorded");
  let retry = stack
    .run_records(&worker)?
    .into_iter()
    .find(|record| record.retry_of_run_id.as_deref() == Some(run.as_str()))
    .expect("the failover registers a retry run");
  assert_eq!(retry.backend.as_deref(), Some("fake3"));
  assert_eq!(retry.model.as_deref(), Some("fake-model"));
  assert_eq!(retry.input_event_ids, interrupted.input_event_ids);
  assert_eq!(stack.wait_terminal(&worker, &retry.id).await?, RunOutcomeKind::Success);
  settle_node(&stack, &worker).await?;
  let first = scripts.builds().into_iter().next().expect("the retry launched a process");
  assert!(first.prompt_contains(REVIEW_FEEDBACK), "the retry's launch names the feedback it answers");
  let meta = stack.store().load_task_meta(&worker).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the session keeps its pool");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake3")), "the session's own member moved");
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  let failures: Vec<Value> =
    stack.child_reports(&manager.id)?.into_iter().filter(|report| report["outcome"] == "failed").collect();
  assert!(failures.is_empty(), "a failover sends no failure report: {failures:?}");
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  assert!(backend_availability(&stack.features)?.out_until("fake2").is_some(), "the failed member cooled");
  // The retry's success wakes the parent in the background; wait for the turn it launches before the stack drops.
  settle_until(|| Ok(!stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.is_empty())).await?;
  settle_node(&stack, &worker).await?;
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// A quota failure on an implement task's pool node fails over to the next member.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failure_on_an_implement_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  quota_failure_fails_over_to_the_next_member(TaskType::Implement).await
}

/// A quota failure on a quick edit's pool node fails over to the next member.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failure_on_a_quick_edit_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  quota_failure_fails_over_to_the_next_member(TaskType::QuickEdit).await
}

/// A quota failure on a script run's pool node fails over to the next member.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failure_on_a_script_run_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  quota_failure_fails_over_to_the_next_member(TaskType::ScriptRun).await
}

/// A quota failure on a verify task's pool node fails over to the next member.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failure_on_a_verify_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  quota_failure_fails_over_to_the_next_member(TaskType::Verify).await
}

/// A stack whose config holds the rotate pool `auto` over `fake`, `fake2` and `fake3`, with `calls.verify: auto`.
async fn auto_verify_stack(scripts: &Scripts) -> anyhow::Result<Stack> {
  let options = ["fake", "fake2", "fake3"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let stack = Stack::start_with(options.clone(), Vec::new()).await?;
  stack.write_access_key()?;
  stack.replace_config(home_config(&stack, &options, AUTO_VERIFY_BACKENDS)?)?;
  Ok(stack)
}

/// A manager session on the pool `auto` whose recorded member is `member`.
async fn auto_manager_on_member(stack: &Stack, member: &str) -> anyhow::Result<SessionMetadata> {
  let manager = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "auto-verify-parent".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Auto verify parent".to_string()),
        backend: Some("auto".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  stack
    .store()
    .update_slot_fields(
      &manager.id,
      model_pool::OWNER,
      Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!(member))]),
    )
    .await?;
  Ok(manager)
}

/// Acceptance ④, the first selection: an Auto session on `fake2` sends verify to `auto`; the child keeps the pool id
/// and its first run lands on another member.
#[tokio::test]
#[ignore = "integration"]
async fn a_verify_from_an_auto_session_selects_another_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified")]);
  let stack = auto_verify_stack(&scripts).await?;
  let manager = auto_manager_on_member(&stack, "fake2").await?;
  stack.dispatch.set_executor(None);
  stack
    .dispatch
    .admit_input(&manager.id, event_types::USER, "Take off and verify this work.", "user", json!({}))
    .await?;
  let body = json!({"session_id": manager.id, "description": "confirm the change", "task_type": "verify"});

  let (child_id, run) = delegate_and_run(&stack, body, &["fake", "fake2", "fake3"]).await?;

  assert_eq!(run.backend.as_deref(), Some("fake"), "the verify leaves the session's member fake2 out");
  let child = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(child.backend, "auto", "the child keeps the pool id");
  assert_eq!(child.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")));
  Ok(())
}

/// Acceptance ④, the quota switch: a verify child on `fake` ends on quota, and the switch also leaves the initiating
/// session's member `fake2` out, so the retry lands on `fake3`.
#[tokio::test]
#[ignore = "integration"]
async fn a_verify_quota_switch_leaves_the_initiating_sessions_member_out() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verified again")]);
  let stack = auto_verify_stack(&scripts).await?;
  let manager = auto_manager_on_member(&stack, "fake2").await?;
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "verify-worker".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(spec("confirm the change", Some(TaskType::Verify))),
        name: Some("Synthetic verify worker".to_string()),
        backend: Some("auto".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  stack
    .store()
    .update_slot_fields(
      &worker.id,
      model_pool::OWNER,
      Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!("fake"))]),
    )
    .await?;
  let run = revision_run_on_quota(&stack, &worker.id, "fake").await?;
  stack.adapter.record_quota_failure_for_test(&worker.id, &run, None);
  stack.adapter.after_worker_run_for_test(&worker.id, &run, "failed").await?;

  let retry = stack
    .run_records(&worker.id)?
    .into_iter()
    .find(|record| record.retry_of_run_id.as_deref() == Some(run.as_str()))
    .expect("the quota failover registers a retry");
  assert_eq!(retry.backend.as_deref(), Some("fake3"), "the switch skips the session's member fake2");
  assert_eq!(stack.wait_terminal(&worker.id, &retry.id).await?, RunOutcomeKind::Success);
  let child = stack.store().load_task_meta(&worker.id).await?;
  assert_eq!(child.backend, "auto", "the child keeps the pool id");
  assert_eq!(child.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake3")));
  let parent = stack.store().load_task_meta(&manager.id).await?;
  assert_eq!(
    parent.extra(model_pool::POOL_MEMBER_FIELD),
    Some(&json!("fake2")),
    "the child's switch leaves the parent's member alone"
  );
  Ok(())
}

/// Acceptance ④'s load order: a priority pool's delegation picks the member with the fewest sessions, so a quick edit
/// delegated from a session on `fake` takes `fake2`.
#[tokio::test]
#[ignore = "integration"]
async fn a_priority_pool_delegation_picks_the_least_loaded_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("distributed")]);
  let options = ["fake", "fake2"]
    .map(|id| BackendOption { billing: Some(PoolBilling::Subscription), ..scripts.option(id, BACKEND_TYPE) })
    .to_vec();
  let members = ["fake", "fake2"].map(|backend| PoolMember { backend: backend.to_string() }).to_vec();
  let auto = PoolConfig {
    id: AUTO_POOL_ID.to_string(),
    label: "Auto".to_string(),
    rule: PoolRule::Priority,
    members,
    visible: true,
  };
  let stack = Stack::start_with_auto(options, Vec::new(), auto).await?;
  stack.write_access_key()?;
  let parent = auto_manager_on_member(&stack, "fake").await?;
  stack.dispatch.set_executor(None);
  stack.dispatch.admit_input(&parent.id, event_types::USER, "Take off and delegate this.", "user", json!({})).await?;

  let (child_id, run) = delegate_and_run(&stack, quick_edit_body(&parent.id), &["fake", "fake2"]).await?;

  assert_eq!(
    run.backend.as_deref(),
    Some("fake2"),
    "the parent holds fake, so the child takes the least loaded member"
  );
  let child = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(child.backend, AUTO_POOL_ID, "the child keeps the pool id");
  assert_eq!(child.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake2")));
  Ok(())
}

/// A pool-id session switches in its own pool and never consults the route it kept: the retry runs on the pool's next
/// member, and one switch event is persisted.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failover_of_a_pool_session_ignores_the_route_and_writes_one_system_event() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("route retry"), result_script("parent noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("route-failover-parent", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::ScriptRun, WorkerForm::Pool).await?;
  let route = backend_pool::BackendRoute {
    backends: vec!["fake".to_string(), "fake3".to_string()],
    reason: "route before pool".to_string(),
  };
  let guard = stack.store().control_lock().await;
  stack
    .store()
    .update_task_slot_fields_locked(
      &guard,
      &worker,
      backend_pool::METADATA_OWNER,
      Map::from_iter([("route".to_string(), json!(route))]),
    )
    .await?;
  drop(guard);
  stack.dispatch.set_executor(None);
  let resets_at = UtcDateTime::parse("2030-01-02T03:04:00+00:00").expect("a fixed instant parses");
  stack.adapter.record_quota_failure_for_test(&worker, &run, Some(resets_at));
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  let retry = stack
    .run_records(&worker)?
    .into_iter()
    .find(|record| record.retry_of_run_id.as_deref() == Some(run.as_str()))
    .expect("the quota failover registered a retry");
  assert_eq!(retry.backend.as_deref(), Some("fake3"), "the session's own pool picks the next member");
  let meta = stack.store().load_task_meta(&worker).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID);
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake3")));
  let switches = stack
    .store()
    .load_chat_events_sync(&worker)?
    .into_iter()
    .filter(|event| event["type"] == event_types::QUOTA_FAILOVER)
    .collect::<Vec<_>>();
  assert_eq!(switches.len(), 1, "one quota failover event is persisted: {switches:?}");
  assert_eq!(switches[0]["from"], "fake2");
  assert_eq!(switches[0]["to"], "fake3");
  assert_eq!(switches[0]["set"], ROTATION_POOL_ID);
  assert_eq!(switches[0]["recovers_at"], "2030-01-02T03:04+00:00");
  assert_eq!(stack.wait_terminal(&worker, &retry.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// Spent route: the route names only the member that ran out, so the failover takes the pool's
/// first free member, and the retry runs there.
#[tokio::test]
#[ignore = "integration"]
async fn quota_failover_uses_the_pool_once_the_route_is_spent() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("retry done"), result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "worker".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(spec("do the work", Some(TaskType::Implement))),
        name: Some("Synthetic worker".to_string()),
        backend: Some("fake3".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let route = backend_pool::BackendRoute { backends: vec!["fake3".to_string()], reason: "design p=0.9".to_string() };
  let guard = stack.store().control_lock().await;
  stack
    .store()
    .update_task_slot_fields_locked(
      &guard,
      &worker.id,
      backend_pool::METADATA_OWNER,
      Map::from_iter([("route".to_string(), json!(route))]),
    )
    .await?;
  drop(guard);
  let run = RunRecord {
    backend: Some("fake3".to_string()),
    model: Some("fake-model".to_string()),
    ..synthetic_run(&worker.id, "run-work", RunKind::Work)
  };
  register_run(&stack, run, "do the work").await?;
  let failed = RunOutcome { exit_code: Some(1), ..RunOutcome::new(RunOutcomeKind::Failed) };
  stack.dispatch.finish_run(&worker.id, "run-work", failed).await?;
  stack.adapter.record_quota_failure_for_test(&worker.id, "run-work", None);
  stack.adapter.after_worker_run_for_test(&worker.id, "run-work", "failed").await?;

  let _retry = stack
    .run_records(&worker.id)?
    .into_iter()
    .find(|record| record.retry_of_run_id.as_deref() == Some("run-work"))
    .expect("the quota failover registers a retry");
  assert_eq!(stack.store().load_task_meta(&worker.id).await?.backend, "fake2");
  Ok(())
}

/// Write the revision run's events log: a read with its result, then a bash call still in progress. The handoff note
/// summarizes it.
fn write_run_events(stack: &Stack, worker: &str, run: &str) -> anyhow::Result<()> {
  let events = [
    json!({"type":"assistant","message":{"content":[
      {"type":"text","text":"I read the entry point."},
      {"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"/synthetic/src/main.rs"}}
    ]}}),
    json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"fn main() {}"}]}}),
    json!({"type":"assistant","message":{"content":[
      {"type":"tool_use","id":"tool-2","name":"Bash","input":{"command":"cargo test -p cron"}}
    ]}}),
  ];
  let dir = stack.store().run_dir(worker, run);
  std::fs::create_dir_all(&dir)?;
  let lines = events.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
  std::fs::write(dir.join(run_records::RUN_EVENTS_NAME), format!("{lines}\n"))?;
  Ok(())
}

/// Test ③: a worker that inherits an Auto session's pool id fails over within Auto when its run ends on quota. The
/// delegation orders Auto by session load, so the worker takes a member other than the parent's; that member cools, the
/// node moves to the next Auto member and not to the rotation pool, and the switch record sits in the worker's own chat
/// with the handoff summary from the run's events log.
#[tokio::test]
#[ignore = "integration"]
async fn an_inherited_worker_fails_over_to_the_next_auto_member() -> anyhow::Result<()> {
  // The delegation launches the worker's first run, which takes the first script; the retry takes the second.
  let scripts = Scripts::new(vec![result_script("retry done"), result_script("retry done")]);
  let options = ["fake", "fake2", "fake3"]
    .map(|id| BackendOption {
      model: "fake-model".to_string(),
      billing: Some(PoolBilling::Subscription),
      ..scripts.option(id, BACKEND_TYPE)
    })
    .to_vec();
  let members = ["fake", "fake2", "fake3"].map(|backend| PoolMember { backend: backend.to_string() }).to_vec();
  let auto = PoolConfig {
    id: AUTO_POOL_ID.to_string(),
    label: "Auto".to_string(),
    rule: PoolRule::Priority,
    members,
    visible: true,
  };
  let stack = Stack::start_with_auto(options, Vec::new(), auto).await?;
  stack.write_access_key()?;
  let parent = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "auto-parent".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Auto parent".to_string()),
        backend: Some(AUTO_POOL_ID.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  stack
    .store()
    .update_slot_fields(
      &parent.id,
      model_pool::OWNER,
      Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!("fake"))]),
    )
    .await?;
  stack.dispatch.set_executor(None);
  stack.dispatch.admit_input(&parent.id, event_types::USER, "Take off and delegate this.", "user", json!({})).await?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&parent.id)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let worker = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&worker).await?;
  assert_eq!(meta.backend, AUTO_POOL_ID, "the worker takes the pool id and picks its own member");
  assert_eq!(
    meta.extra(model_pool::POOL_MEMBER_FIELD),
    Some(&json!("fake2")),
    "the parent holds fake, so the load order gives the worker fake2"
  );

  // The delegated first run finishes before the review feedback arrives, so the revision starts from a settled worker.
  settle_node(&stack, &worker).await?;
  let run = revision_run_on_quota(&stack, &worker, "fake2").await?;
  write_run_events(&stack, &worker, &run)?;
  stack.adapter.record_quota_failure_for_test(&worker, &run, None);
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  let interrupted =
    stack.run_records(&worker)?.into_iter().find(|record| record.id == run).expect("the run is recorded");
  let retry = stack
    .run_records(&worker)?
    .into_iter()
    .find(|record| record.retry_of_run_id.as_deref() == Some(run.as_str()))
    .expect("the failover registers a retry run");
  assert_eq!(retry.backend.as_deref(), Some("fake3"), "the next Auto member runs the retry");
  assert_eq!(retry.input_event_ids, interrupted.input_event_ids);
  let outcome = stack.wait_terminal(&worker, &retry.id).await?;
  assert_eq!(outcome, RunOutcomeKind::Success, "{}", error_text(&stack, &worker, &retry.id)?);
  settle_node(&stack, &worker).await?;
  let worker_meta = stack.store().load_task_meta(&worker).await?;
  assert_eq!(worker_meta.backend, AUTO_POOL_ID, "the worker keeps its pool");
  assert_eq!(worker_meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake3")), "the worker's own member moved");
  let parent_meta = stack.store().load_task_meta(&parent.id).await?;
  assert_eq!(
    parent_meta.extra(model_pool::POOL_MEMBER_FIELD),
    Some(&json!("fake")),
    "the child's switch leaves the parent's member alone"
  );
  assert!(backend_availability(&stack.features)?.out_until("fake2").is_some(), "the spent member is recorded out");

  let switches: Vec<Value> = quota_failover_events(&stack, &worker)?.iter().map(|event| event.to_value()).collect();
  assert_eq!(switches.len(), 1, "one switch record in the worker's own chat: {switches:?}");
  assert_eq!(switches[0]["from"], "fake2");
  assert_eq!(switches[0]["to"], "fake3");
  assert_eq!(switches[0]["set"], AUTO_POOL_ID);
  assert_eq!(switches[0]["summary"], handoff::SUMMARY_OK);

  let launches = scripts.builds();
  assert!(launches[0].prompt_contains("run the quick edit"), "the delegated first run takes the first script");
  let retry_launch = launches
    .into_iter()
    .find(|build| build.prompt_contains(REVIEW_FEEDBACK))
    .expect("the retry launched with the feedback it answers");
  assert!(
    retry_launch.prompt_contains("this task switched from backend fake2 to fake3 after fake2 spent its quota"),
    "the retry's launch carries the switch note"
  );
  let failed_dir = stack.store().run_dir(&worker, &run).display().to_string();
  assert!(retry_launch.prompt_contains(&failed_dir), "the note names the failed run's directory: {failed_dir}");
  assert!(retry_launch.prompt_contains("files: /synthetic/src/main.rs"), "the summary comes from the run's events log");
  Ok(())
}

/// Acceptance 1: a delegation that names a pool id is accepted. The child session keeps the pool id and records the
/// member the pool selects, and the work run launches on that member.
#[tokio::test]
#[ignore = "integration"]
async fn a_delegation_naming_a_pool_keeps_the_pool_and_runs_on_its_member() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let body = json!({
    "session_id": manager,
    "description": "run the quick edit",
    "task_type": "quick-edit",
    "backend": ROTATION_POOL_ID,
  });
  let (status, delegated) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let run_id = text(&delegated, "run_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, ROTATION_POOL_ID, "the child keeps the pool id the request named");
  assert_eq!(meta.extra(model_pool::POOL_MEMBER_FIELD), Some(&json!("fake")), "the pool's first member runs it");
  let outcome = stack.wait_terminal(&child_id, &run_id).await?;
  assert_eq!(outcome, RunOutcomeKind::Success, "{}", error_text(&stack, &child_id, &run_id)?);
  let run = stack.store().read_run_sync(&child_id, &run_id)?.expect("the delegation registers its work run");
  assert_eq!(run.backend.as_deref(), Some("fake"), "the run names the member, never the pool");
  assert_eq!(run.model.as_deref(), Some("fake-model"));
  assert_eq!(scripts.builds().len(), 1, "the run launched");
  Ok(())
}

/// A node whose delegation named its backend records the member out and reports the quota error: no retry starts and
/// no other member is consulted. Acceptance 3.
#[tokio::test]
#[ignore = "integration"]
async fn an_explicitly_named_worker_records_it_out_and_is_not_failed_over() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::Implement, WorkerForm::Direct).await?;
  stack.adapter.record_quota_failure_for_test(&worker, &run, None);
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  assert!(stack.run_records(&worker)?.iter().all(|record| record.retry_of_run_id.is_none()), "no retry starts");
  let reports = stack.child_reports(&manager.id)?;
  assert_eq!(reports.len(), 1, "the failure is reported once: {reports:?}");
  assert_eq!(reports[0]["outcome"], "failed");
  assert!(
    backend_availability(&stack.features)?.out_until("fake2").is_some(),
    "an explicitly named run records its member out"
  );
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  assert_eq!(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(), 1);
  Ok(())
}

/// With the next member cooling too, no retry starts, and the parent's report names each member's recovery time.
#[tokio::test]
#[ignore = "integration"]
async fn failover_with_every_member_cooling_reports_each_recovery_time() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::Implement, WorkerForm::Pool).await?;
  let fake3_reset = UtcDateTime::parse("2030-01-02T03:04:00+00:00").expect("a fixed instant parses");
  backend_availability(&stack.features)?.record(
    "fake3",
    Some(fake3_reset),
    backend_availability::REASON_QUOTA_ERROR,
    "synthetic quota error",
  )?;
  let fake2_reset = UtcDateTime::parse("2030-01-02T02:00:00+00:00").expect("a fixed instant parses");
  stack.adapter.record_quota_failure_for_test(&worker, &run, Some(fake2_reset));
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  assert!(stack.run_records(&worker)?.iter().all(|record| record.retry_of_run_id.is_none()), "no retry starts");
  let reports = stack.child_reports(&manager.id)?;
  assert_eq!(reports.len(), 1, "the failure is reported once: {reports:?}");
  let summary = reports[0]["summary"].as_str().expect("the report names the recovery times");
  assert!(
    summary.contains("pool 'pool' has no selectable member:\n- fake2: quota_error, recovers at 2030-01-02T02:00")
      && summary.contains("- fake3: quota_error, recovers at 2030-01-02T03:04"),
    "{summary}"
  );
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  assert_eq!(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(), 1);
  Ok(())
}

// ---------------------------------------------------------------------------
// Temporary backend failures over the backend pool (the worker-path cases)
// ---------------------------------------------------------------------------

/// The capacity message that a backend's `error` event carries, and the message of the named-backend cases.
const CAPACITY_MESSAGE: &str = "Selected model is at capacity. Please try a different model.";

/// A worker whose first run ended on scripted events, as `_launch_worker_error_event` leaves it. `_root` holds the
/// run's repository and worktree directories, which the retry starts in.
struct ErrorEventRun {
  stack: Stack,
  manager_id: String,
  worker_id: String,
  first_backend: String,
  failed: RunRecord,
  outcome: RunOutcomeKind,
  _root: tempfile::TempDir,
}

/// A script that prints each event as one JSON line, then exits with `code`, as a worker process does.
fn events_script(events: &[Value], code: i32) -> String {
  let mut lines: Vec<String> =
    events.iter().map(|event| format!("printf '%s\\n' '{}'", event.to_string().replace('\'', r"'\''"))).collect();
  lines.push(format!("exit {code}"));
  lines.join("\n")
}

/// An `error` event as a backend writes it: a message and its content.
fn error_event(message: &str, content: &str) -> Value {
  json!({"type": event_types::ERROR, "message": message, "content": content})
}

/// A Claude Code `result` event that ends in an error, with its `api_error_status` (a number, or null when absent).
fn claude_error_result(api_error_status: Value, result: &str) -> Value {
  json!({"type": event_types::RESULT, "is_error": true, "api_error_status": api_error_status, "result": result})
}

/// Where an error-event worker's first run is chosen: a named backend, the rotate pool `ROTATION_POOL_ID`, or the
/// priority pool `AUTO_POOL_ID`. Each pool holds `fake` and `fake2`, in that order.
#[derive(Clone, Copy)]
enum ErrorEventPool {
  Named,
  Rotate,
  Priority,
}

/// Launch a rotate-pool or named worker's first run, as `launch_error_event_worker_on` does.
async fn launch_error_event_worker(first_events: &[Value], pool_chosen: bool) -> anyhow::Result<ErrorEventRun> {
  let pool = if pool_chosen { ErrorEventPool::Rotate } else { ErrorEventPool::Named };
  launch_error_event_worker_on(first_events, pool).await
}

/// Launch an implement worker's first run, which prints `first_events` and exits 1, and wait for it to end. A
/// pool-chosen worker takes the pool's first member; a named worker takes `fake`. The retry on the next member prints
/// one assistant line and exits 1, and the manager's turn after the failure report prints one result.
async fn launch_error_event_worker_on(first_events: &[Value], pool: ErrorEventPool) -> anyhow::Result<ErrorEventRun> {
  let retry_events =
    [json!({"type": event_types::ASSISTANT, "message": {"content": [{"type": "text", "text": "retry stopped"}]}})];
  let scripts =
    Scripts::new(vec![events_script(first_events, 1), events_script(&retry_events, 1), result_script("manager noted")]);
  let options = ["fake", "fake2"]
    .map(|id| BackendOption { model: "fake-model".to_string(), ..scripts.option(id, BACKEND_TYPE) })
    .to_vec();
  let stack = match pool {
    ErrorEventPool::Named | ErrorEventPool::Rotate => {
      Stack::start_with(options, vec!["fake".to_string(), "fake2".to_string()]).await?
    }
    ErrorEventPool::Priority => {
      let options = options
        .into_iter()
        .map(|option| BackendOption { billing: Some(PoolBilling::Subscription), ..option })
        .collect();
      let members = ["fake", "fake2"].map(|backend| PoolMember { backend: backend.to_string() }).to_vec();
      let auto = PoolConfig {
        id: AUTO_POOL_ID.to_string(),
        label: "Auto".to_string(),
        rule: PoolRule::Priority,
        members,
        visible: true,
      };
      Stack::start_with_auto(options, Vec::new(), auto).await?
    }
  };
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let pool_id = match pool {
    ErrorEventPool::Named => None,
    ErrorEventPool::Rotate => Some(ROTATION_POOL_ID),
    ErrorEventPool::Priority => Some(AUTO_POOL_ID),
  };
  let (session_backend, first_backend) = match pool_id {
    Some(pool_id) => {
      let backends = backend_pool::backend_pool(&stack.features)?;
      let member = backends.selector(&stack.cfg).select(pool_id, &[])?.expect("the pool selects a first member").id;
      (pool_id.to_string(), member)
    }
    None => ("fake".to_string(), "fake".to_string()),
  };

  let root = tempfile::tempdir()?;
  let (repo, worktree) = (root.path().join("repo"), root.path().join("worktree"));
  std::fs::create_dir(&repo)?;
  std::fs::create_dir(&worktree)?;
  let task = TaskSpec {
    goal: "implement the requested change".to_string(),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    keep_worktree: true,
    ..TaskSpec::default()
  };
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "worker".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(task),
        name: None,
        backend: Some(session_backend.clone()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  if pool_id.is_some() {
    stack
      .store()
      .update_slot_fields(
        &worker.id,
        model_pool::OWNER,
        Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!(first_backend))]),
      )
      .await?;
  }
  let admitted =
    stack.dispatch.admit_input(&worker.id, event_types::USER, "implement this input batch", "user", json!({})).await?;
  let record = RunRecord {
    backend: Some(first_backend.clone()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    branch_name: Some("charliebot/task-test".to_string()),
    worktree_path: Some(worktree.display().to_string()),
    input_event_ids: vec![admitted["id"].as_str().expect("an admitted input has an id").to_string()],
    ..synthetic_run(&worker.id, "run-initial", RunKind::Work)
  };
  register_run(&stack, record, "implement the requested change").await?;
  stack.adapter.launch(&worker.id, "run-initial")?;
  let outcome = stack.wait_terminal(&worker.id, "run-initial").await?;
  let failed =
    stack.run_records(&worker.id)?.into_iter().find(|run| run.id == "run-initial").expect("the first run is recorded");
  Ok(ErrorEventRun { stack, manager_id: manager.id, worker_id: worker.id, first_backend, failed, outcome, _root: root })
}

/// The retry that a failover registers for `run_id`, or `None` when none starts within ten seconds.
async fn wait_for_retry(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<Option<RunRecord>> {
  let deadline = std::time::Instant::now() + Duration::from_secs(10);
  loop {
    let retry = stack.run_records(session_id)?.into_iter().find(|run| run.retry_of_run_id.as_deref() == Some(run_id));
    if retry.is_some() || std::time::Instant::now() >= deadline {
      return Ok(retry);
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn quota_failover_events(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<ChatEvent>> {
  Ok(stack.run_events(session_id)?.into_iter().filter(|event| event["type"] == event_types::QUOTA_FAILOVER).collect())
}

/// The failover of a pool-chosen worker whose first run ended on a temporary error or a spent quota: the member cools,
/// the node moves to `fake2`, the retry binds the failed run's inputs and worktree, and the parent hears only the
/// retry's failure.
async fn assert_failover_to_next_member(error: &ErrorEventRun) -> anyhow::Result<()> {
  let stack = &error.stack;
  let pool = backend_pool::backend_pool(&stack.features)?;
  assert_eq!(error.outcome, RunOutcomeKind::Failed);
  let retry = wait_for_retry(stack, &error.worker_id, &error.failed.id).await?;
  assert!(retry.is_some(), "the temporary error fails the run over to the next member");
  let retry = retry.expect("checked above");
  assert_eq!(stack.wait_terminal(&error.worker_id, &retry.id).await?, RunOutcomeKind::Failed);
  settle_until(|| Ok(stack.child_reports(&error.manager_id)?.len() == 1)).await?;

  let first = error.first_backend.as_str();
  assert!(pool.availability().out_until(first).is_some(), "the failed member cools");
  assert!(pool.availability().out_until("fake2").is_none(), "the next member stays selectable");
  let remaining = pool.availability().out_until(first).expect("checked above") - OffsetDateTime::now_utc();
  assert!(
    remaining > TimeDuration::minutes(29) && remaining <= backend_availability::DEFAULT_COOLDOWN,
    "{remaining:?}"
  );
  assert_eq!(retry.backend.as_deref(), Some("fake2"));
  assert_eq!(retry.input_event_ids, error.failed.input_event_ids);
  assert_eq!(retry.worktree_path, error.failed.worktree_path);
  let switches = quota_failover_events(stack, &error.worker_id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["from"].as_str(), Some(first));
  assert_eq!(switches[0]["to"], "fake2");
  assert_eq!(switches[0]["recovers_at"].as_str(), pool.availability().out_until_minutes(first).as_deref());
  let reports = stack.child_reports(&error.manager_id)?;
  assert_eq!(reports.len(), 1, "{reports:?}");
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!([format!("run:{}", retry.id)]));
  let pool_id = switches[0][event_types::QUOTA_FAILOVER_SET].as_str().unwrap_or_default();
  assert_eq!(member_ids(&stack.cfg, pool_id), ["fake", "fake2"], "the switch names a pool of both members");
  settle_node(stack, &error.worker_id).await?;
  settle_node(stack, &error.manager_id).await
}

/// The no-failover case: no retry starts, and the parent hears the first run's failure once.
async fn assert_no_failover(error: &ErrorEventRun) -> anyhow::Result<()> {
  let stack = &error.stack;
  assert_eq!(error.outcome, RunOutcomeKind::Failed);
  settle_until(|| Ok(stack.child_reports(&error.manager_id)?.len() == 1)).await?;
  assert_eq!(stack.run_records(&error.worker_id)?.len(), 1, "no retry run starts");
  assert!(quota_failover_events(stack, &error.worker_id)?.is_empty(), "no failover event is written");
  let reports = stack.child_reports(&error.manager_id)?;
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!([format!("run:{}", error.failed.id)]));
  settle_node(stack, &error.manager_id).await
}

/// No pool member cooled: the run's failure was neither a spent quota nor a temporary failure.
fn assert_no_member_cooled(error: &ErrorEventRun) -> anyhow::Result<()> {
  let pool = backend_pool::backend_pool(&error.stack.features)?;
  assert!(
    error
      .stack
      .cfg
      .backends
      .pool(ROTATION_POOL_ID)
      .expect("the rotation pool")
      .members
      .iter()
      .all(|member| pool.availability().out_until(&member.backend).is_none()),
    "no member cools"
  );
  Ok(())
}

/// The worker whose backend the delegation named: no pool serves it, and it has no route.
async fn assert_named_worker(error: &ErrorEventRun) -> anyhow::Result<()> {
  let meta = error.stack.store().load_task_meta(&error.worker_id).await?;
  assert_eq!(meta.backend, "fake");
  assert!(meta.extra("route").is_none_or(Value::is_null), "a named worker has no route");
  assert_eq!(error.first_backend, "fake");
  Ok(())
}

/// The named backend's member recorded out: a spent quota or a temporary failure records every run's member out,
/// whether a pool chose it or the request named it.
fn assert_named_member_recorded_out(error: &ErrorEventRun) -> anyhow::Result<()> {
  let pool = backend_pool::backend_pool(&error.stack.features)?;
  assert!(
    pool.availability().out_until(&error.first_backend).is_some(),
    "the named backend '{}' is recorded out",
    error.first_backend
  );
  Ok(())
}

/// The run's events log holds the `result` event that the test scripted.
fn assert_result_logged(error: &ErrorEventRun, result: &str) -> anyhow::Result<()> {
  let path = error.stack.store().run_dir(&error.worker_id, &error.failed.id).join(run_records::RUN_EVENTS_NAME);
  let logged =
    std::fs::read_to_string(path)?.lines().map(serde_json::from_str::<Value>).collect::<Result<Vec<_>, _>>()?;
  assert!(logged.iter().any(|event| event["type"] == event_types::RESULT && event["result"] == result), "{logged:?}");
  Ok(())
}

// A capacity error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn at_capacity_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event(CAPACITY_MESSAGE, "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A run with no output fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn no_output_stall_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event("model produced no output for 1200s", "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A bad gateway error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn bad_gateway_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A connection error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn internal_server_error_with_connection_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "litellm.InternalServerError: InternalServerError: OpenAIException - Connection error.";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A 500 error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn internal_server_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event("Internal Server Error", "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// An SSE stall fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn opencode_session_stall_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "OpenCode backend failed: OpenCode SSE stream carried no session progress for 3600.0 s \
    (heartbeats during silence: 359)";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A closed peer fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn opencode_closed_peer_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "OpenCode backend failed: peer closed connection without sending complete message body \
    (incomplete chunked read)";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_failover_to_next_member(&error).await
}

// A stderr tail and request id do not trigger failover: the step case.
#[tokio::test]
#[ignore = "integration"]
async fn step_limit_error_with_bad_gateway_content_does_not_fail_over() -> anyhow::Result<()> {
  let error = launch_error_event_worker(
    &[error_event("Step limit (1000) exceeded without task completion.", "Bad Gateway")],
    true,
  )
  .await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

// A stderr tail and request id do not trigger failover: the 400 case.
#[tokio::test]
#[ignore = "integration"]
async fn request_id_with_status_digits_does_not_fail_over() -> anyhow::Result<()> {
  let error =
    launch_error_event_worker(&[error_event("Error code: 400 - invalid request (request id req_502ab1)", "")], true)
      .await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

// The review run's model-access refusal: the member cools with the refusal reason, and the run continues on the next
// member with the same input and a note that names the refusal.
#[tokio::test]
#[ignore = "integration"]
async fn an_access_refused_run_fails_over_to_the_next_member() -> anyhow::Result<()> {
  let refused = "Failed to authenticate. API Error: 401 Not allowed to access model due to tags configuration. \
                 Passed model=deepseek-flash and tags=['deepseek']";
  let error = launch_error_event_worker(&[claude_error_result(json!(401), refused)], true).await?;
  let stack = &error.stack;
  assert_eq!(error.outcome, RunOutcomeKind::Failed);
  let retry = wait_for_retry(stack, &error.worker_id, &error.failed.id)
    .await?
    .context("the refusal fails the run over to the next member")?;
  assert_eq!(stack.wait_terminal(&error.worker_id, &retry.id).await?, RunOutcomeKind::Failed);
  settle_until(|| Ok(stack.child_reports(&error.manager_id)?.len() == 1)).await?;

  let first = error.first_backend.as_str();
  let pool = backend_pool::backend_pool(&stack.features)?;
  let row = pool.availability().row(first).context("the refused member is recorded out")?;
  assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  let remaining = row.out_until.into_inner() - OffsetDateTime::now_utc();
  assert!(
    remaining > TimeDuration::minutes(29) && remaining <= backend_availability::DEFAULT_COOLDOWN,
    "{remaining:?}"
  );
  assert_eq!(retry.backend.as_deref(), Some("fake2"), "the next member runs the retry");
  assert_eq!(retry.input_event_ids, error.failed.input_event_ids, "the retry reuses the failed run's input");
  let switches = quota_failover_events(stack, &error.worker_id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["reason"], event_types::QUOTA_REASON_ACCESS_REFUSED);
  assert_eq!(switches[0]["from"].as_str(), Some(first));
  assert_eq!(switches[0]["to"], "fake2");
  let launch = std::fs::read_to_string(stack.store().run_dir(&error.worker_id, &retry.id).join(LAUNCH_TEXT_FILENAME))?;
  assert!(
    launch.contains(&format!("this task switched from backend {first} to fake2 after {first} refused access")),
    "{launch}"
  );
  settle_node(stack, &error.worker_id).await?;
  settle_node(stack, &error.manager_id).await?;
  // Only the retry's own failure can end the task: the refusal left it open for the retry.
  let closes = stack
    .run_events(&error.worker_id)?
    .iter()
    .map(|event| event.to_value())
    .filter(|event| event["type"] == event_types::TASK_CLOSED)
    .collect::<Vec<_>>();
  for event in &closes {
    assert_eq!(event["run_ids"], json!([retry.id.clone()]), "the refusal itself did not end the task: {event}");
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// A launch refused by the Claude login layer
// ---------------------------------------------------------------------------

/// The recovery time the refused launch of these tests states.
const REFUSED_RECOVERY: &str = "2999-01-02T03:04:05Z";

/// A backend type whose every launch refuses, as the Claude login layer does when no login is usable.
struct RefusedLaunchLifecycle {
  failure: charliebot_registry::backend::MemberFailure,
  resets_at: Option<UtcDateTime>,
}

impl charliebot_registry::backend::BackendLifecycle for RefusedLaunchLifecycle {
  fn place<'a>(
    &'a self,
    _ctx: &'a charliebot_registry::backend::LaunchContext,
  ) -> futures_util::future::BoxFuture<
    'a,
    anyhow::Result<charliebot_registry::backend::Launch, charliebot_registry::backend::LaunchRefused>,
  > {
    Box::pin(async move {
      Err(charliebot_registry::backend::LaunchRefused {
        message: "Claude account pool has no available account (main: auth-failure cooldown until 10:15 UTC); this \
                  run did not complete."
          .to_string(),
        failure: self.failure,
        resets_at: self.resets_at,
      })
    })
  }
}

fn refused_traits() -> charliebot_registry::backend::BackendTraits {
  charliebot_registry::backend::BackendTraits {
    resume: charliebot_registry::backend::ResumeStyle::CliFlag,
    restart_reattach: false,
    preassigned_session_id: false,
    family_prefix: None,
    manual_compact: false,
    compact_hint: None,
  }
}

/// Register the refusing types: an authentication failure with a stated recovery, a missing login recovering at once,
/// and a quota refusal with a stated reset.
fn register_refused_launch(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  let fixed = UtcDateTime::parse(REFUSED_RECOVERY).expect("the fixed recovery time parses");
  for (type_name, failure, resets_at) in [
    ("refused-launch", charliebot_registry::backend::MemberFailure::AccessRefused, Some(fixed)),
    (
      "refused-missing-credentials",
      charliebot_registry::backend::MemberFailure::AccessRefused,
      Some(UtcDateTime::now()),
    ),
    ("refused-quota", charliebot_registry::backend::MemberFailure::Quota, Some(fixed)),
  ] {
    let lifecycle: Arc<dyn charliebot_registry::backend::BackendLifecycle> =
      Arc::new(RefusedLaunchLifecycle { failure, resets_at });
    registry.backend_type(scripted_type(type_name, refused_traits(), Some(lifecycle)))?;
  }
  Ok(())
}

/// The option of one of the refusing types.
fn refused_option(id: &str) -> BackendOption {
  BackendOption {
    id: id.to_string(),
    label: id.to_string(),
    model: format!("{id}-model"),
    prompt_overlay: None,
    billing: None,
    backend_type: id.to_string(),
    type_fields: Mapping::new(),
    visible: true,
  }
}

/// Drive one manager turn on `backend` to its refused end.
async fn refused_manager_turn(backend: &str) -> anyhow::Result<Stack> {
  let stack = Stack::start_with_package(
    vec![refused_option(backend)],
    Vec::new(),
    ("test.refused_launch", register_refused_launch),
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "refused-manager".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Refused manager".to_string()),
        backend: Some(backend.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  stack.dispatch.admit_input(&manager.id, event_types::USER, "continue", "user", json!({})).await?;
  let run = launched_run_id(&stack.dispatch.dispatch_pending(&manager.id).await?)?;
  assert_eq!(stack.wait_terminal(&manager.id, &run).await?, RunOutcomeKind::Failed);
  settle_node(&stack, &manager.id).await?;
  Ok(stack)
}

/// Drive one implement worker run on `backend` to its refused end.
async fn refused_worker_run(backend: &str) -> anyhow::Result<Stack> {
  let scripts = Scripts::new(vec![result_script("manager noted")]);
  let stack = Stack::start_with_package(
    vec![refused_option(backend), scripts.option(BACKEND_ID, BACKEND_TYPE)],
    Vec::new(),
    ("test.refused_launch", register_refused_launch),
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let root = tempfile::tempdir()?;
  let (repo, worktree) = (root.path().join("repo"), root.path().join("worktree"));
  std::fs::create_dir(&repo)?;
  std::fs::create_dir(&worktree)?;
  let task = TaskSpec {
    goal: "implement the requested change".to_string(),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    keep_worktree: true,
    ..TaskSpec::default()
  };
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "refused-worker".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(task),
        name: None,
        backend: Some(backend.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let admitted =
    stack.dispatch.admit_input(&worker.id, event_types::USER, "implement this input batch", "user", json!({})).await?;
  let record = RunRecord {
    backend: Some(backend.to_string()),
    model: Some(format!("{backend}-model")),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    branch_name: Some("charliebot/task-test".to_string()),
    worktree_path: Some(worktree.display().to_string()),
    input_event_ids: vec![admitted["id"].as_str().expect("an admitted input has an id").to_string()],
    ..synthetic_run(&worker.id, "run-initial", RunKind::Work)
  };
  register_run(&stack, record, "implement the requested change").await?;
  stack.adapter.launch(&worker.id, "run-initial")?;
  assert_eq!(stack.wait_terminal(&worker.id, "run-initial").await?, RunOutcomeKind::Failed);
  settle_node(&stack, &worker.id).await?;
  settle_node(&stack, &manager.id).await?;
  Ok(stack)
}

/// A manager turn whose logins all failed authentication records the member as an access refusal that recovers at the
/// earliest login disable end, not the 30-minute quota cooldown.
#[tokio::test]
#[ignore = "integration"]
async fn a_manager_turn_refused_by_its_logins_records_the_earliest_login_recovery() -> anyhow::Result<()> {
  let stack = refused_manager_turn("refused-launch").await?;
  let row = backend_availability(&stack.features)?.row("refused-launch").expect("the refused member is recorded");
  assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  assert_eq!(row.out_until.to_str(), REFUSED_RECOVERY);
  Ok(())
}

/// A worker whose logins all failed authentication records the same access refusal and earliest recovery.
#[tokio::test]
#[ignore = "integration"]
async fn a_worker_refused_by_its_logins_records_the_earliest_login_recovery() -> anyhow::Result<()> {
  let stack = refused_worker_run("refused-launch").await?;
  let row = backend_availability(&stack.features)?.row("refused-launch").expect("the refused member is recorded");
  assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  assert_eq!(row.out_until.to_str(), REFUSED_RECOVERY);
  Ok(())
}

/// Logins without credentials can recover as soon as credentials are written, so the recorded row expires at once.
#[tokio::test]
#[ignore = "integration"]
async fn a_worker_refused_for_missing_credentials_records_a_row_that_expires_at_once() -> anyhow::Result<()> {
  let stack = refused_worker_run("refused-missing-credentials").await?;
  let row =
    backend_availability(&stack.features)?.row("refused-missing-credentials").expect("the refused member is recorded");
  assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  let remaining = row.out_until.into_inner() - OffsetDateTime::now_utc();
  assert!(remaining < TimeDuration::minutes(1), "the row expires at once instead of after a cooldown: {remaining:?}");
  Ok(())
}

/// Quota exhaustion of every login keeps the quota reason and its stated reset.
#[tokio::test]
#[ignore = "integration"]
async fn a_worker_refused_for_quota_records_the_quota_reason_and_reset() -> anyhow::Result<()> {
  let stack = refused_worker_run("refused-quota").await?;
  let row = backend_availability(&stack.features)?.row("refused-quota").expect("the refused member is recorded");
  assert_eq!(row.reason, backend_availability::REASON_QUOTA_ERROR);
  assert_eq!(row.out_until.to_str(), REFUSED_RECOVERY);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn system_event_with_temporary_message_does_not_fail_over() -> anyhow::Result<()> {
  let event = json!({"type": event_types::SYSTEM, "message": CAPACITY_MESSAGE, "content": ""});
  let error = launch_error_event_worker(&[event], true).await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

/// A backend that the delegation named behaves like a named one: a quota failure fails the run to the parent.
#[tokio::test]
#[ignore = "integration"]
async fn a_named_backend_capacity_error_reports_failure_without_failover() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event(CAPACITY_MESSAGE, "")], false).await?;
  assert_named_worker(&error).await?;
  assert_named_member_recorded_out(&error)?;
  assert_no_failover(&error).await
}

// A 502 result error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_502_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: 502 litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let error = launch_error_event_worker(&[claude_error_result(json!(502), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_failover_to_next_member(&error).await
}

// A 529 result error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_529_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
  let error = launch_error_event_worker(&[claude_error_result(json!(529), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_failover_to_next_member(&error).await
}

// A 429 result error fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_429_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: 429 litellm.RateLimitError: RateLimitError: OpenAIException - rate limit exceeded";
  let error = launch_error_event_worker(&[claude_error_result(json!(429), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_failover_to_next_member(&error).await
}

/// The line the aigw gateway returns through the Claude Code CLI when a key's balance is spent: an errored `result`
/// event with status 402. The failure-triage fixtures carry the same line from a real work run.
const AIGW_SPENT_QUOTA: &str = "API Error: 402 litellm.BadRequestError: OpenAIException - Insufficient Balance";

// A spent quota in an errored result fails over on a priority pool.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_spent_quota_fails_over_on_a_priority_pool() -> anyhow::Result<()> {
  let event = claude_error_result(json!(402), AIGW_SPENT_QUOTA);
  let error = launch_error_event_worker_on(&[event], ErrorEventPool::Priority).await?;
  assert_result_logged(&error, AIGW_SPENT_QUOTA)?;
  assert_failover_to_next_member(&error).await
}

// A spent quota in an errored result fails over on a rotate pool.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_spent_quota_fails_over_on_a_rotate_pool() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[claude_error_result(json!(402), AIGW_SPENT_QUOTA)], true).await?;
  assert_result_logged(&error, AIGW_SPENT_QUOTA)?;
  assert_failover_to_next_member(&error).await
}

// A spent quota on a named backend records that model out and ends the run with the quota error, without a switch.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_spent_quota_on_a_named_backend_ends_without_switching() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[claude_error_result(json!(402), AIGW_SPENT_QUOTA)], false).await?;
  assert_result_logged(&error, AIGW_SPENT_QUOTA)?;
  assert_named_worker(&error).await?;
  assert_named_member_recorded_out(&error)?;
  let row = backend_availability(&error.stack.features)?.row("fake").expect("the named model is recorded out");
  assert_eq!(row.reason, backend_availability::REASON_QUOTA_ERROR);
  assert_eq!(row.detail, AIGW_SPENT_QUOTA, "the record keeps the quota error");
  assert_no_failover(&error).await
}

// A request error whose request id carries 429 digits is not a spent quota: it fails fast and cools no member.
#[tokio::test]
#[ignore = "integration"]
async fn request_error_result_with_429_in_its_request_id_does_not_fail_over() -> anyhow::Result<()> {
  let result = "API Error: 400 litellm.BadRequestError: OpenAIException - invalid request \
    (request_id: 0b429c7d-1e2f-4a5b-8c9d-0e1f2a3b4c5d)";
  let error = launch_error_event_worker(&[claude_error_result(json!(400), result)], true).await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

// A result error with no status fails over on the worker path.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_without_status_connection_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: Connection error.";
  let error = launch_error_event_worker(&[claude_error_result(Value::Null, result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_failover_to_next_member(&error).await
}

// Bad request result errors do not fail over: the reasoning case.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_400_reasoning_error_does_not_fail_over() -> anyhow::Result<()> {
  let result = "API Error: 400 litellm.BadRequestError: OpenAIException - The `reasoning_content` in the thinking mode \
    must be passed back to the API. (request_id: f69ad0ec-cd32-4570-917d-7689ec630120)";
  let error = launch_error_event_worker(&[claude_error_result(json!(400), result)], true).await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

// Bad request result errors do not fail over: the bad gateway case.
#[tokio::test]
#[ignore = "integration"]
async fn claude_result_400_mentioning_bad_gateway_does_not_fail_over() -> anyhow::Result<()> {
  let result = "API Error: 400 litellm.BadRequestError: upstream reported Bad Gateway";
  let error = launch_error_event_worker(&[claude_error_result(json!(400), result)], true).await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

#[tokio::test]
#[ignore = "integration"]
async fn non_error_claude_result_with_temporary_message_does_not_fail_over() -> anyhow::Result<()> {
  let event = json!({
    "type": event_types::RESULT,
    "is_error": false,
    "subtype": "error",
    "api_error_status": null,
    "result": "Fixed the Bad Gateway handler.",
  });
  let error = launch_error_event_worker(&[event], true).await?;
  assert_no_member_cooled(&error)?;
  assert_no_failover(&error).await
}

#[tokio::test]
#[ignore = "integration"]
async fn a_named_backend_claude_result_error_reports_failure_without_failover() -> anyhow::Result<()> {
  let result = "API Error: 502 litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let event = claude_error_result(json!(502), result);
  let error = launch_error_event_worker(&[event], false).await?;
  assert_named_worker(&error).await?;
  assert_named_member_recorded_out(&error)?;
  assert_no_failover(&error).await
}

/// A stack over the option ids `ids`, each taking the scripts in launch order, with `pool` as the rotation. The
/// operator's access key is on disk, so the delegation route authorizes the call.
async fn cold_read_stack(scripts: &Scripts, ids: &[&str], pool: &[&str]) -> anyhow::Result<Stack> {
  let options = ids.iter().map(|id| scripts.option(id, BACKEND_TYPE)).collect();
  let stack = Stack::start_with(options, pool.iter().map(|id| id.to_string()).collect()).await?;
  stack.write_access_key()?;
  Ok(stack)
}

/// A script that prints one `error` event carrying `message`, then exits with `code`: a failed attempt's evidence.
fn error_script(message: &str, code: i32) -> String {
  format!("printf '%s\\n' '{}'; exit {code}", json!({"type": "error", "message": message, "content": message}))
}

/// A script that prints one successful result event carrying `text`. The shell argument is single-quoted, so its
/// single quotes are escaped.
fn answer_script(text: &str) -> String {
  let event = json!({"type": "result", "subtype": "success", "result": text}).to_string();
  format!("printf '%s\\n' '{}'", event.replace('\'', r"'\''"))
}

/// Delegate one cold read of `page` under `manager_id`. Returns the child's session id and its first run's id.
async fn delegate_cold_read(stack: &Stack, manager_id: &str, page: &Path) -> anyhow::Result<(String, String)> {
  let (status, response) = post_delegate(&routes::routes(&stack.features), cold_read_body(manager_id, page)).await?;
  anyhow::ensure!(status == StatusCode::OK, "{response}");
  Ok((text(&response, "session_id")?, text(&response, "run_id")?))
}

/// A string field of a response body.
fn text(body: &Value, key: &str) -> anyhow::Result<String> {
  Ok(body[key].as_str().ok_or_else(|| anyhow::anyhow!("the body names no {key}: {body}"))?.to_string())
}

/// The backends of a node's work runs, in run order.
fn work_backends(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<String>> {
  let runs = stack.run_records(session_id)?;
  Ok(runs.into_iter().filter(|run| run.kind == RunKind::Work).filter_map(|run| run.backend).collect())
}

/// The charlie-bot checkout, whose `prompts/` the cold-read run reads.
fn repo_checkout() -> anyhow::Result<&'static Path> {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).ok_or_else(|| anyhow::anyhow!("no repository root"))
}

/// The run's managed instructions hold the cold-reader system prompt alone, and its input is the page with the seven
/// questions.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_run_instructions_hold_only_the_system_prompt_and_the_input_is_the_page_and_questions()
-> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![answer_script("(1) fine.")]);
  let stack = cold_read_stack(&scripts, &[BACKEND_ID, "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (child_id, run_id) = delegate_cold_read(&stack, &manager.id, &page).await?;
  assert_eq!(stack.wait_terminal(&child_id, &run_id).await?, RunOutcomeKind::Success);
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;

  let run_dir = stack.store().run_dir(&child_id, &run_id);
  let snapshot: Value = serde_json::from_slice(&std::fs::read(run_dir.join(SNAPSHOT_FILENAME))?)?;
  let (system_prompt, questions) = cold_read::load_texts(repo_checkout()?)?;
  let block_texts =
    snapshot["blocks"].as_array().into_iter().flatten().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>();
  assert_eq!(block_texts, [system_prompt.as_str()]);
  let launch_text = std::fs::read_to_string(run_dir.join(LAUNCH_TEXT_FILENAME))?;
  let page_text = std::fs::read_to_string(&page)?;
  assert_eq!(launch_text, cold_read::probe_input(&page_text, &questions, "where are we?").unwrap());
  let builds = scripts.builds();
  let prompt = builds[0].prompt.lock().expect("prompt mutex is not poisoned").clone();
  assert_eq!(prompt.as_deref(), Some(launch_text.as_str()));
  Ok(())
}

/// A first reader that fails and a second that answers. The report states the page, the failed attempt, the answering
/// backend and its answer, and the child completes.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_success_report_carries_attempts_backend_and_answers() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts =
    Scripts::new(vec![error_script("alpha exploded", 1), answer_script("(1) The reader's problem.\n(2) confirmed.")]);
  let stack = cold_read_stack(&scripts, &[BACKEND_ID, "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (child_id, _run_id) = delegate_cold_read(&stack, &manager.id, &page).await?;
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;

  let digest = charliebot_store::control_events::sha256_hex(&std::fs::read_to_string(&page)?);
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(report["outcome"], "completed");
  assert_eq!(
    report["summary"],
    format!(
      "cold read of {}\npage sha256 {digest}\nattempt alpha failed: alpha exploded\nbackend beta\n\
       (1) The reader's problem.\n(2) confirmed.",
      page.display()
    )
  );
  assert_eq!(stack.store().task_state(&child_id)?, "completed");
  // The retry took the next pool backend past the tried one.
  assert_eq!(work_backends(&stack, &child_id)?, ["alpha", "beta"]);
  Ok(())
}

/// A reader that exits with an empty answer is a failed attempt, and the next backend answers.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_empty_answer_counts_as_a_failed_run() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![answer_script(""), answer_script("(1) answered.")]);
  let stack = cold_read_stack(&scripts, &[BACKEND_ID, "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  delegate_cold_read(&stack, &manager.id, &page).await?;
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;

  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(report["outcome"], "completed");
  let summary = text(&report, "summary")?;
  let lines = summary.lines().collect::<Vec<_>>();
  assert!(lines.contains(&"attempt alpha failed: empty final answer"), "{summary}");
  assert!(lines.contains(&"backend beta"), "{summary}");
  Ok(())
}

/// The retry passes the failed run's backend as tried, so the selection skips it and the next pool member takes the
/// work. There is no spy on the selection's tried list. Here the selection is called with the same lists, and the
/// retry's run is checked.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_retry_selects_the_next_backend_with_the_tried_list() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![error_script("alpha exploded", 1), answer_script("(1) answered.")]);
  let stack = cold_read_stack(&scripts, &[BACKEND_ID, "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (child_id, _run_id) = delegate_cold_read(&stack, &manager.id, &page).await?;
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  assert_eq!(work_backends(&stack, &child_id)?, ["alpha", "beta"]);

  // The delegation resolves the first backend with an empty tried list; the retry passes ["alpha"].
  let cfg = stack.features.config.get()?;
  let pool = backend_pool::backend_pool(&stack.features)?;
  let first =
    spawner_backends::select_verify_backend(&cfg, stack.store(), &manager.id, CallClass::ColdRead, &[], &pool).await?;
  assert_eq!(first.map(|(backend, _, tried)| (backend, tried)), Some(("alpha".to_string(), vec!["alpha".to_string()])));
  let retry = spawner_backends::select_verify_backend(
    &cfg,
    stack.store(),
    &manager.id,
    CallClass::ColdRead,
    &["alpha".to_string()],
    &pool,
  )
  .await?;
  assert_eq!(
    retry.map(|(backend, _, tried)| (backend, tried)),
    Some(("beta".to_string(), vec!["alpha".to_string(), "beta".to_string()]))
  );
  Ok(())
}

/// Every reader fails, so the report names each attempt and the trailer, and its delivery archives the child once.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_every_backend_failed_reports_and_the_child_is_archived() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![
    error_script("alpha exploded", 1),
    error_script("beta exploded", 1),
    error_script("fake exploded", 1),
  ]);
  let stack = cold_read_stack(&scripts, &[BACKEND_ID, "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (child_id, _run_id) = delegate_cold_read(&stack, &manager.id, &page).await?;
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;

  let digest = charliebot_store::control_events::sha256_hex(&std::fs::read_to_string(&page)?);
  let report = last_child_report(&stack, &manager.id)?;
  assert_eq!(report["outcome"], "failed");
  assert_eq!(
    report["summary"],
    format!(
      "cold read of {}\npage sha256 {digest}\nattempt alpha failed: alpha exploded\n\
       attempt beta failed: beta exploded\nattempt fake failed: fake exploded\nevery model failed (3 tried)",
      page.display()
    )
  );
  // The all-failed report's delivery archived the stopped child.
  settle_until(|| Ok(stack.store().task_state(&child_id)? == "archived")).await?;
  let closes = stack.closes(&child_id)?;
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["outcome"], "archived");
  assert_eq!(closes[0]["actor"], "system");
  Ok(())
}

// ---------------------------------------------------------------------------
// Devin rate-limit retries
// ---------------------------------------------------------------------------

/// The fake `devin` CLI and the export fixture its staged successful response reuses.
const FAKE_DEVIN: &str = include_str!("../../../backends/devin/tests/fixtures/fake_devin.sh");
const DEVIN_EXPORT: &str = include_str!("../../../backends/devin/tests/fixtures/export.json");
/// The recorded incident: devin's `cognition.ai/retryable` rate-limit rejection with a 22-second reset.
const DEVIN_INCIDENT: &str = r#"Error: Agent error: Reached free model rate limit. Upgrade to Max for higher limits, or switch to a different model. Your limit will reset in 22 seconds (at 01:19 UTC).: {
  "cognition.ai/errorKind": "unavailable",
  "cognition.ai/retryable": true
}"#;
const DEVIN_DAILY_RESET: &str = r#"Error: Agent error: Reached free model rate limit. Upgrade to Max for higher limits, or switch to a different model. Your limit will reset in 15 hours 42 minutes (at Nov 8 00:01 UTC).: {
  "cognition.ai/errorKind": "unavailable",
  "cognition.ai/retryable": true
}"#;

thread_local! {
  /// The fake Devin host of the next `test.devin` registration. The test sets it just before registering, on the
  /// thread that registers.
  static DEVIN_HOST: RefCell<Option<CliHost>> = const { RefCell::new(None) };
}

fn register_test_devin(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  let host =
    DEVIN_HOST.with(|cell| cell.borrow_mut().take()).expect("the test sets the fake Devin host before registering");
  charliebot_devin::register_with_host(registry, host)
}

/// The fake `devin` binary in `dir`, with its staged-response contract.
fn stage_fake_devin(dir: &Path) -> anyhow::Result<()> {
  use std::os::unix::fs::PermissionsExt;
  std::fs::create_dir_all(dir)?;
  let binary = dir.join("devin");
  std::fs::write(&binary, FAKE_DEVIN)?;
  let mut permissions = std::fs::metadata(&binary)?.permissions();
  permissions.set_mode(0o755);
  std::fs::set_permissions(binary, permissions)?;
  Ok(())
}

/// Stages `responses/<call>/` of the fake devin: its stderr, its exit code and, on `succeeded`, its export and answer.
fn stage_devin_response(dir: &Path, call: usize, stderr: &str, exit_code: i32, succeeded: bool) -> anyhow::Result<()> {
  let response = dir.join("responses").join(call.to_string());
  std::fs::create_dir_all(&response)?;
  std::fs::write(response.join("stderr.txt"), stderr)?;
  std::fs::write(response.join("exit_code"), exit_code.to_string())?;
  if succeeded {
    std::fs::write(response.join("export.json"), DEVIN_EXPORT)?;
    std::fs::write(response.join("answer.txt"), "The number is 41.\n")?;
  }
  Ok(())
}

/// The helper binary that supervises a final-stdout transport run in tests.
fn fake_program_bin() -> PathBuf {
  let exe = std::env::current_exe().expect("the test binary path");
  exe
    .ancestors()
    .skip(1)
    .map(|dir| dir.join("fake_program"))
    .find(|candidate| candidate.is_file())
    .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
}

/// The `devin` backend option: its type names no fields.
fn devin_option() -> BackendOption {
  BackendOption {
    id: "devin".to_string(),
    label: "Devin".to_string(),
    model: "fake-model".to_string(),
    prompt_overlay: None,
    billing: None,
    backend_type: "devin".to_string(),
    type_fields: Mapping::new(),
    visible: true,
  }
}

/// An implement worker's first run over the staged fake devin, as `launch_error_event_worker` builds it over the
/// scripted type: a pool of `devin` and `fake2`, the manager node on `fake`, the staged calls the test lays down.
struct DevinErrorRun {
  stack: Stack,
  manager_id: String,
  worker_id: String,
  fake_bin: PathBuf,
  first: RunRecord,
  outcome: RunOutcomeKind,
  _root: tempfile::TempDir,
}

/// One staged devin call: the stderr it prints, its exit code and, on `succeeded`, its export and answer.
#[derive(Clone, Copy)]
struct StagedDevinCall<'a> {
  stderr: &'a str,
  exit_code: i32,
  succeeded: bool,
}

/// A staged call that prints the recorded rate-limit incident and exits 1.
const DEVIN_RATE_LIMIT: StagedDevinCall<'static> =
  StagedDevinCall { stderr: DEVIN_INCIDENT, exit_code: 1, succeeded: false };
/// A staged call that ends in the export and the answer.
const DEVIN_SUCCESS: StagedDevinCall<'static> = StagedDevinCall { stderr: "", exit_code: 0, succeeded: true };

/// Launch the devin-first pool's implement worker: `calls` stage the devin's responses in call order, and `scripts`
/// feed the scripted backends that take over after the devin run, in build order.
async fn launch_devin_error_worker(
  pool_chosen: bool,
  scripts: Vec<String>,
  calls: &[StagedDevinCall<'_>],
) -> anyhow::Result<DevinErrorRun> {
  let root = tempfile::tempdir()?;
  let fake_bin = root.path().join("fake-bin");
  stage_fake_devin(&fake_bin)?;
  for (index, call) in calls.iter().enumerate() {
    stage_devin_response(&fake_bin, index + 1, call.stderr, call.exit_code, call.succeeded)?;
  }
  // SAFETY: this test binary races on nothing else that reads the variable.
  unsafe { std::env::set_var("CHARLIEBOT_TEST_FINAL_STDOUT_SUPERVISOR", fake_program_bin()) };
  DEVIN_HOST.with(|cell| {
    *cell.borrow_mut() = Some(CliHost { path: fake_bin.display().to_string(), home: root.path().join("fake-home") })
  });
  let scripts = Scripts::new(scripts);
  let options = vec![
    BackendOption { model: "fake-model".to_string(), ..scripts.option(BACKEND_ID, BACKEND_TYPE) },
    devin_option(),
    BackendOption { model: "fake-model".to_string(), ..scripts.option("fake2", BACKEND_TYPE) },
  ];
  let stack = Stack::start_with_package(
    options,
    vec!["devin".to_string(), "fake2".to_string()],
    ("test.devin", register_test_devin),
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (session_backend, first_backend) = if pool_chosen {
    let pool = backend_pool::backend_pool(&stack.features)?;
    let member = pool.selector(&stack.cfg).select(ROTATION_POOL_ID, &[])?.expect("the pool selects a first member").id;
    (ROTATION_POOL_ID.to_string(), member)
  } else {
    ("devin".to_string(), "devin".to_string())
  };

  let (repo, worktree) = (root.path().join("repo"), root.path().join("worktree"));
  std::fs::create_dir(&repo)?;
  std::fs::create_dir(&worktree)?;
  let task = TaskSpec {
    goal: "implement the requested change".to_string(),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    keep_worktree: true,
    ..TaskSpec::default()
  };
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "worker".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(task),
        name: None,
        backend: Some(session_backend.clone()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  if pool_chosen {
    stack
      .store()
      .update_slot_fields(
        &worker.id,
        model_pool::OWNER,
        Map::from_iter([(model_pool::POOL_MEMBER_FIELD.to_string(), json!(first_backend))]),
      )
      .await?;
  }
  let admitted =
    stack.dispatch.admit_input(&worker.id, event_types::USER, "implement this input batch", "user", json!({})).await?;
  let record = RunRecord {
    backend: Some(first_backend),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    branch_name: Some("charliebot/task-test".to_string()),
    worktree_path: Some(worktree.display().to_string()),
    input_event_ids: vec![admitted["id"].as_str().expect("an admitted input has an id").to_string()],
    ..synthetic_run(&worker.id, "run-initial", RunKind::Work)
  };
  register_run(&stack, record, "implement the requested change").await?;
  stack.adapter.launch(&worker.id, "run-initial")?;
  let outcome = stack.wait_terminal(&worker.id, "run-initial").await?;
  let first =
    stack.run_records(&worker.id)?.into_iter().find(|run| run.id == "run-initial").expect("the first run is recorded");
  Ok(DevinErrorRun { stack, manager_id: manager.id, worker_id: worker.id, fake_bin, first, outcome, _root: root })
}

/// The fake devin's call count after the run, read from `call-count`.
fn devin_call_count(fake_bin: &Path) -> anyhow::Result<String> {
  Ok(std::fs::read_to_string(fake_bin.join("call-count"))?.trim().to_string())
}

/// A worker whose devin run hits the stated rate limit waits it out on devin, pinned or pool-chosen alike, and the
/// retry's success opens the review; nothing cools and nothing fails over.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_worker_retries_in_place_and_reaches_review() -> anyhow::Result<()> {
  for pool_chosen in [false, true] {
    // The review and the manager turn take the two scripts; the third staged call only covers a devin review pick.
    let scripts = vec![result_script("settled"), result_script("manager noted")];
    let run =
      launch_devin_error_worker(pool_chosen, scripts, &[DEVIN_RATE_LIMIT, DEVIN_SUCCESS, DEVIN_SUCCESS]).await?;

    assert_eq!(run.outcome, RunOutcomeKind::Success, "pool_chosen: {pool_chosen}");
    assert_eq!(run.first.backend.as_deref(), Some("devin"));
    assert!(run.fake_bin.join("call-2-argv.txt").is_file(), "devin took a second call");
    let pool = backend_pool::backend_pool(&run.stack.features)?;
    assert!(pool.availability().out_until("devin").is_none(), "a retried reset does not cool the member");
    assert!(quota_failover_events(&run.stack, &run.worker_id)?.is_empty(), "no failover event is written");
    assert_eq!(run.stack.run_ids_of_kind(&run.worker_id, RunKind::Work)?.len(), 1, "no retry run starts");
    settle_until(|| Ok(!run.stack.run_ids_of_kind(&run.worker_id, RunKind::Review)?.is_empty())).await?;
    settle_node(&run.stack, &run.worker_id).await?;
  }
  Ok(())
}

/// A pool-chosen worker that exhausts its three retries cools devin at the reset the error stated, then fails the
/// run over to the pool's next member.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_pool_chosen_devin_worker_exhausts_retries_cools_at_the_reset_and_fails_over() -> anyhow::Result<()> {
  let retry_events =
    [json!({"type": event_types::ASSISTANT, "message": {"content": [{"type": "text", "text": "retry stopped"}]}})];
  let scripts = vec![events_script(&retry_events, 1), result_script("manager noted")];
  let run = launch_devin_error_worker(true, scripts, &[DEVIN_RATE_LIMIT; 4]).await?;

  assert_eq!(run.outcome, RunOutcomeKind::Failed);
  let pool = backend_pool::backend_pool(&run.stack.features)?;
  let remaining = pool.availability().out_until("devin").expect("devin cools at the reset its error stated")
    - OffsetDateTime::now_utc();
  assert!(remaining > TimeDuration::seconds(10) && remaining < TimeDuration::minutes(5), "{remaining}");
  assert!(pool.availability().out_until("fake2").is_none(), "the next member stays selectable");

  let retry = wait_for_retry(&run.stack, &run.worker_id, &run.first.id).await?;
  let retry = retry.expect("the exhausted run fails over to the next member");
  assert_eq!(retry.backend.as_deref(), Some("fake2"));
  assert_eq!(run.stack.wait_terminal(&run.worker_id, &retry.id).await?, RunOutcomeKind::Failed);
  settle_until(|| Ok(run.stack.child_reports(&run.manager_id)?.len() == 1)).await?;
  let switches = quota_failover_events(&run.stack, &run.worker_id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["from"].as_str(), Some("devin"));
  assert_eq!(switches[0]["to"], "fake2");
  assert_eq!(switches[0]["recovers_at"].as_str(), pool.availability().out_until_minutes("devin").as_deref());
  settle_node(&run.stack, &run.worker_id).await
}

/// The worker records the absolute daily reset from Devin's quota rejection even when the retry wait exceeds the cap.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_worker_records_the_daily_reset_outside_the_automatic_wait_cap() -> anyhow::Result<()> {
  let scripts = vec![result_script("settled"), result_script("manager noted")];
  let run = launch_devin_error_worker(
    true,
    scripts,
    &[StagedDevinCall { stderr: DEVIN_DAILY_RESET, exit_code: 1, succeeded: false }],
  )
  .await?;

  assert_eq!(run.first.backend.as_deref(), Some("devin"));
  assert_eq!(devin_call_count(&run.fake_bin)?, "1");
  let until = backend_pool::backend_pool(&run.stack.features)?
    .availability()
    .out_until("devin")
    .expect("the daily quota rejection records Devin cooling");
  assert_eq!(UtcDateTime::from(until), UtcDateTime::parse("2026-11-08T00:01:00Z")?);
  Ok(())
}

/// A manager turn records Devin's stated daily reset through its finish callback, without waiting for it.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_manager_turn_records_the_daily_reset_outside_the_automatic_wait_cap() -> anyhow::Result<()> {
  let root = tempfile::tempdir()?;
  let fake_bin = root.path().join("fake-bin");
  stage_fake_devin(&fake_bin)?;
  stage_devin_response(&fake_bin, 1, DEVIN_DAILY_RESET, 1, false)?;
  unsafe { std::env::set_var("CHARLIEBOT_TEST_FINAL_STDOUT_SUPERVISOR", fake_program_bin()) };
  DEVIN_HOST.with(|cell| {
    *cell.borrow_mut() = Some(CliHost { path: fake_bin.display().to_string(), home: root.path().join("fake-home") });
  });

  let scripts = Scripts::new(Vec::new());
  let stack = Stack::start_with_package(
    vec![scripts.option(BACKEND_ID, BACKEND_TYPE), devin_option()],
    Vec::new(),
    ("test.devin", register_test_devin),
  )
  .await?;
  stack.write_access_key()?;
  let manager = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "manager-daily-reset".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: None,
        backend: Some("devin".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  stack.dispatch.admit_input(&manager.id, event_types::USER, "continue", "user", json!({})).await?;
  let run = launched_run_id(&stack.dispatch.dispatch_pending(&manager.id).await?)?;

  assert_eq!(stack.wait_terminal(&manager.id, &run).await?, RunOutcomeKind::Failed);
  assert_eq!(std::fs::read_to_string(fake_bin.join("call-count"))?.trim(), "1");
  let until = backend_availability(&stack.features)?.out_until("devin").expect("the manager records Devin cooling");
  assert_eq!(UtcDateTime::from(until), UtcDateTime::parse("2026-11-08T00:01:00Z")?);
  Ok(())
}

/// A worker pinned on devin records devin out and does not fail over: the exhausted run reports the original error
/// through the existing path.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_pinned_devin_worker_exhausts_retries_records_it_out_and_reports_the_error() -> anyhow::Result<()> {
  let scripts = vec![result_script("manager noted")];
  let run = launch_devin_error_worker(false, scripts, &[DEVIN_RATE_LIMIT; 4]).await?;

  assert_eq!(run.outcome, RunOutcomeKind::Failed);
  assert_eq!(devin_call_count(&run.fake_bin)?, "4");
  let pool = backend_pool::backend_pool(&run.stack.features)?;
  assert!(pool.availability().out_until("devin").is_some(), "an explicitly pinned run records its backend out");
  assert!(quota_failover_events(&run.stack, &run.worker_id)?.is_empty(), "no failover event is written");
  assert_eq!(run.stack.run_records(&run.worker_id)?.len(), 1, "no retry run starts");
  settle_until(|| Ok(run.stack.child_reports(&run.manager_id)?.len() == 1)).await?;
  let reports = run.stack.child_reports(&run.manager_id)?;
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!([format!("run:{}", run.first.id)]));
  settle_node(&run.stack, &run.worker_id).await
}
