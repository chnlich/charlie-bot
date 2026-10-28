//! The worker-run cases of `tests/test_task_execution.py` over the task-execution stack: a worker Run launches through
//! the adapter, its process is a `/bin/sh` script, and its delivery chain runs on the real store. Each test names the
//! Python case it ports in a `// Python:` comment.
//!
//! The review spawn and follow-up of an implement Run run for real (the review chain is ported). The work and verify
//! launches are asserted here; the review launch itself, iteration and scheduled-step launches stay with their
//! owners' test files.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use charliebot_agents::backend_pool;
use charliebot_claude_code::claude_accounts::{self, ClaudeAccountState};
use charliebot_claude_code::claude_config;
use charliebot_claude_code::claude_lifecycle::ClaudeCodeLifecycle;
use charliebot_claude_code::claude_relay;
use charliebot_claude_code::login_dirs;
use charliebot_core::config::{BackendOption, ConfigSection, RoutingConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, ResumeStyle};
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::{RegisterFn, Registry, RegistryError};
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::write_faults::{self, Hit};
use charliebot_store::{ChatEvent, CreateTask, PatchTask, TaskCaller};
use charliebot_tasks::prompts::{LAUNCH_TEXT_FILENAME, SNAPSHOT_FILENAME};
use charliebot_tasks::{cold_read, routes, spawner_backends, task_sessions};
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, Build, Scripts, Stack, cold_read_body, init_repo_with_origin, last_child_report,
  post_delegate, result_script, scripted_type, settle_until, synthetic_git, synthetic_run, write_sitrep,
};
use serde_json::{Map, Value, json};
use time::{Duration as TimeDuration, OffsetDateTime};

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

/// The Python `_task_spec` shape: a goal and a task type, no repository.
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
/// making sits at the gate when the stack drops (`tests/test_task_execution.py` `_settle_parent`).
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

/// `test_task_execution.py::test_bare_branch_base_with_unpushed_local_commit_launch_fails`: a repository whose local
/// base branch holds a commit origin lacks fails the base check before any process starts. The Run lands the durable
/// failed fact with the error as its evidence, and the parent receives the failure report.
#[tokio::test]
async fn bare_branch_base_with_unpushed_local_commit_launch_fails() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_bare_branch_base_with_unpushed_local_commit_launch_fails.
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

/// `test_task_execution.py::test_bare_branch_base_behind_starts_from_origin_tip`: the bare base form when the shared
/// checkout's local main is only behind origin. The Run launches anyway, its work branch starts at the origin tip, and
/// the local main ref is untouched. The review that lands the work runs on the review chain, so the case asserts the
/// work Run's launch and success.
#[tokio::test]
async fn bare_branch_base_behind_starts_from_origin_tip() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_bare_branch_base_behind_starts_from_origin_tip.
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

/// `test_task_execution.py::test_an_interrupted_worker_run_reports_once_and_archives_the_worker`: an interrupted run
/// reports on the spot through the failure entry, and the report's delivery archives the worker.
#[tokio::test]
async fn an_interrupted_worker_run_reports_once_and_archives_the_worker() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_an_interrupted_worker_run_reports_once_and_archives_the_worker.
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
async fn a_late_failure_report_is_skipped_after_worker_cancellation() -> anyhow::Result<()> {
  // Python: tests/test_task_completion.py::test_failure_report_reaching_lock_after_cancel_is_skipped.
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
async fn successful_work_on_a_closed_worker_does_not_register_a_review() -> anyhow::Result<()> {
  // Python: tests/test_task_completion.py::test_successful_work_on_closed_worker_does_not_register_review.
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

/// `test_task_execution.py::test_repo_less_quick_edit_closes_without_review`: a repo-less quick edit closes completed
/// when its work Run succeeds, spawns no review, and reports completed to the parent, which takes one turn.
#[tokio::test]
async fn repo_less_quick_edit_closes_without_review() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_repo_less_quick_edit_closes_without_review.
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

/// `test_task_execution.py::test_a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt`: an upload
/// admitted while a worker run holds its slot waits for that run's end. The continuation resumes the run's native
/// conversation, and its prompt names the upload's path.
#[tokio::test]
async fn a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt.
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
  // (`route_resume_session_style`); the Python case reads the same id from its `resume_session_id` keyword.
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

/// `test_task_execution.py::test_worker_run_kinds_deliver_snapshot_bytes_and_task_context`: a script-run work launch
/// and a verify launch ride the same assembler with their own contracts. The work launch carries the worker contract,
/// the goal and the input in its launch text, and a repository sandbox; the verify launch carries the verify contract
/// only.
#[tokio::test]
async fn worker_run_kinds_deliver_snapshot_bytes_and_task_context() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_worker_run_kinds_deliver_snapshot_bytes_and_task_context.
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

/// `test_task_execution.py::test_worktree_preparation_failure_lands_failed_run_and_reports_to_parent`: a repository
/// whose local base is ahead of origin fails worktree preparation after the Run was admitted. The Run lands its durable
/// failed fact with the actual error as evidence in its own events log, and the parent receives a failure report naming
/// it. The report's delivery archives the worker.
#[tokio::test]
async fn worktree_preparation_failure_lands_failed_run_and_reports_to_parent() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_worktree_preparation_failure_lands_failed_run_and_reports_to_parent.
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

/// `test_task_execution.py::test_recovery_after_rule_deletion_uses_the_original_snapshot`: a manager turn's stored
/// snapshot reads on after its rule body is deleted, and an explicit retry is a new Run that assembles from the current
/// sources.
#[tokio::test]
async fn recovery_after_rule_deletion_uses_the_original_snapshot() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_recovery_after_rule_deletion_uses_the_original_snapshot.
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

/// `test_task_execution.py::test_a_worker_launch_failure_writes_no_turn_launch_error`: a worker Run whose backend
/// cannot be resolved fails at launch, and no manager-turn launch notice is written for it.
#[tokio::test]
async fn a_worker_launch_failure_writes_no_turn_launch_error() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_a_worker_launch_failure_writes_no_turn_launch_error.
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

/// The first `run_id` of a dispatch decision that launched.
fn launched_run_id(decision: &Value) -> anyhow::Result<String> {
  Ok(decision["run_id"].as_str().ok_or_else(|| anyhow::anyhow!("the dispatch names no run: {decision}"))?.to_string())
}

/// `test_task_execution.py::test_worker_run_finished_enospc_retries_and_lands_without_restart`: the `run_finished`
/// write of a successful worker run fails out of space. The end-landing retry drains the run from its raw log and lands
/// the end record, the parent gets exactly one completed report, and no second process starts.
#[tokio::test]
async fn worker_run_finished_enospc_retries_and_lands_without_restart() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_worker_run_finished_enospc_retries_and_lands_without_restart.
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
async fn unset_task_type_success_takes_the_non_implement_branch() -> anyhow::Result<()> {
  // Python: src/runtime/task_execution.py:2190
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

/// `test_task_execution.py::test_parent_report_enospc_retry_delivers_report_once`: a successful run's delivery write
/// fails out of space. The end record is already on disk, the retry delivers the success-based report exactly once,
/// and the run's live end time survives the retry.
#[tokio::test]
async fn parent_report_enospc_retry_delivers_report_once() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_parent_report_enospc_retry_delivers_report_once.
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

/// `test_task_execution.py::test_disk_headroom_precheck_withholds_worker_run_but_not_manager_turn`: below the disk
/// threshold a worker-class run stays queued with one blocked report, while a manager turn still launches. A zero
/// threshold skips the check and the queued worker launches.
#[tokio::test]
async fn disk_headroom_precheck_withholds_worker_run_but_not_manager_turn() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_disk_headroom_precheck_withholds_worker_run_but_not_manager_turn.
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

/// `test_task_execution.py::test_non_space_end_failure_lands_immediately_and_starts_no_retry`: a non-space failure in
/// the end path lands the run at once and starts no retry. A success fact whose delivery hits EIO sends no failed
/// report.
#[tokio::test]
async fn non_space_end_failure_lands_immediately_and_starts_no_retry() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_non_space_end_failure_lands_immediately_and_starts_no_retry.
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

// ---------------------------------------------------------------------------
// Pooled cc-claude launches
// ---------------------------------------------------------------------------

/// The pooled Fable option's id; its configuration names the `cc-claude` type under this id.
const POOLED: &str = "claude-fable-5";

thread_local! {
  /// The Claude account state the next `cc-claude` registration drives. The test sets it on the thread that registers.
  static CLAUDE_STATE: RefCell<Option<Arc<ClaudeAccountState>>> = const { RefCell::new(None) };
}

fn register_pooled_claude(registry: &mut Registry) -> Result<(), RegistryError> {
  let state = CLAUDE_STATE
    .with(|cell| cell.borrow_mut().take())
    .expect("the test sets the Claude account state before registering");
  registry.config_section(ConfigSection {
    key: "accounts",
    legacy_keys: &[],
    parse: claude_config::parse_accounts,
    dump: claude_config::dump_accounts,
  })?;
  let lifecycle: Arc<dyn BackendLifecycle> = Arc::new(ClaudeCodeLifecycle::with_state(state));
  let traits = BackendTraits {
    resume: ResumeStyle::CliFlag,
    restart_reattach: true,
    preassigned_session_id: true,
    family_prefix: None,
    manual_compact: true,
    compact_hint: None,
  };
  registry.backend_type(scripted_type("cc-claude", traits, Some(lifecycle)))
}

/// One pooled test's directories: the Claude config dirs of its accounts and the configuration's worktree root.
struct Root {
  dir: tempfile::TempDir,
}

impl Root {
  fn new() -> anyhow::Result<Self> {
    Ok(Self { dir: tempfile::tempdir()? })
  }

  fn path(&self) -> &Path {
    self.dir.path()
  }

  fn claude(&self, label: &str) -> PathBuf {
    self.path().join(format!("claude-{label}"))
  }
}

/// Writes the Claude OAuth credentials that mark an account's login present.
fn plant_credentials(config_dir: &Path) -> anyhow::Result<()> {
  std::fs::create_dir_all(config_dir)?;
  let credentials = json!({"claudeAiOauth": {"accessToken": "token", "refreshToken": "r"}});
  std::fs::write(config_dir.join(login_dirs::CREDENTIALS_FILE), credentials.to_string())?;
  Ok(())
}

/// A session transcript of `session_id` under a config dir, as a Claude Code login writes it.
fn make_transcript(config_dir: &Path, session_id: &str) -> anyhow::Result<PathBuf> {
  let transcript = config_dir.join("projects").join("slug").join(format!("{session_id}.jsonl"));
  std::fs::create_dir_all(transcript.parent().expect("a transcript has a directory"))?;
  std::fs::write(&transcript, "[]")?;
  Ok(transcript)
}

/// The `config.yaml` of a pooled test: the Claude accounts with their credentials, the pools, and the one `cc-claude`
/// option, which draws from the first pool when pools are given.
fn pooled_config(root: &Root, labels: &[&str], pools: &[(&str, &[&str])], token: &str) -> anyhow::Result<String> {
  // The sessions here have no metadata; a zero cap starts no session cgroup, so no spawn walks the task-parent chain.
  let mut text = format!(
    "paths:\n  worktree_dir: {}\nserver:\n  session_memory_max_mb: 0\n",
    root.path().join("worktrees").display()
  );
  if !labels.is_empty() {
    text.push_str("accounts:\n  claude:\n");
    for label in labels {
      plant_credentials(&root.claude(label))?;
      text.push_str(&format!("    - label: {label}\n      config_dir: {}\n", root.claude(label).display()));
    }
    if !pools.is_empty() {
      text.push_str("  claude_pools:\n");
      for (name, members) in pools {
        text.push_str(&format!("    {name}: [{}]\n", members.join(", ")));
      }
    }
  }
  let pool = pools.first().map(|(name, _)| format!("      account_pool: {name}\n")).unwrap_or_default();
  let option = format!("      model: test-model\n      token: {token}\n{pool}");
  text.push_str(&format!(
    "backends:\n  options:\n    - id: {POOLED}\n      label: {POOLED}\n      type: cc-claude\n{option}"
  ));
  Ok(text)
}

/// A stack over a pooled Claude configuration whose one `cc-claude` option runs `scripts` in order. Its lifecycle
/// selects through `state`, which the test has observed its readings into.
async fn pooled_stack(
  root: &Root,
  labels: &[&str],
  pools: &[(&str, &[&str])],
  scripts: &Scripts,
  state: &Arc<ClaudeAccountState>,
) -> anyhow::Result<Stack> {
  CLAUDE_STATE.with(|cell| *cell.borrow_mut() = Some(Arc::clone(state)));
  let config = pooled_config(root, labels, pools, scripts.token())?;
  let stack = Stack::start_from_config(&config, ("test.pooled_claude", register_pooled_claude as RegisterFn)).await?;
  stack.write_access_key()?;
  Ok(stack)
}

/// A work run on a pooled cc-claude option, whose model is the option's test model (`_register_work_run`).
fn pooled_run(session_id: &str, run_id: &str, backend: &str) -> RunRecord {
  RunRecord {
    backend: Some(backend.to_string()),
    model: Some("test-model".to_string()),
    ..synthetic_run(session_id, run_id, RunKind::Work)
  }
}

/// Record a run's process group as soon as the spawn callback persists its pid.
async fn track_run_pid(stack: &Stack, guard: &ProcessGroupGuard, session_id: &str, run_id: &str) -> anyhow::Result<()> {
  settle_until(|| {
    if let Some(pid) = stack.store().read_run_sync(session_id, run_id)?.and_then(|run| run.pid) {
      guard.track(pid);
      return Ok(true);
    }
    Ok(false)
  })
  .await
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

/// A script that prints its events and exits with `code`.
fn exits(events: &[Value], code: i32) -> String {
  script_lines(events, &format!("exit {code}"))
}

/// A script that waits until `gate` exists before it runs, so the test can read the launch it was given first.
fn gated(gate: &Path, script: String) -> String {
  format!("while [ ! -f '{}' ]; do sleep 0.05; done; {script}", gate.display())
}

fn assistant_text(text: &str) -> Value {
  json!({"type": event_types::ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}})
}

fn result() -> Value {
  json!({"type": event_types::RESULT, "subtype": "success", "is_error": false, "result": "done"})
}

/// A rate-limit observation: the five-hour window at `utilization`, resetting three hours from now.
fn rate_limit_info(status: &str, utilization: f64) -> Value {
  let resets_at = (OffsetDateTime::now_utc() + TimeDuration::hours(3)).unix_timestamp();
  json!({
    "status": status,
    "rateLimitType": "five_hour",
    "resetsAt": resets_at,
    "unifiedWindows": {"five_hour": {"utilization": utilization}, "seven_day": {"utilization": 0.3}},
  })
}

fn rate_limit(status: &str, utilization: f64) -> Value {
  json!({"type": event_types::RATE_LIMIT_EVENT, "rate_limit_info": rate_limit_info(status, utilization)})
}

/// The label of the account a launch was given (`claude_account`).
fn account_label(build: &Build) -> Option<&str> {
  build.launch.backend_kwargs.get("claude_account")?.get("label")?.as_str()
}

/// The config dir of the account a launch was given (`claude_account`).
fn account_dir(build: &Build) -> Option<PathBuf> {
  Some(PathBuf::from(build.launch.backend_kwargs.get("claude_account")?.get("config_dir")?.as_str()?))
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

/// `test_task_execution.py::test_pooled_fresh_worker_launches_build_on_the_selected_account`: a fresh work Run and its
/// spawned review on a pooled cc-claude option both launch on the account the pool selects.
#[tokio::test]
async fn pooled_fresh_worker_launches_build_on_the_selected_account() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_pooled_fresh_worker_launches_build_on_the_selected_account.
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![
    exits(&[assistant_text("work done"), result()], 0),
    exits(&[assistant_text("review ok"), result()], 0),
  ]);
  let stack = pooled_stack(&root, &["main", "ext-1", "ext-2"], &[], &scripts, &state).await?;
  let expected =
    claude_accounts::select(&state, &stack.cfg, Some("test-model"), &HashSet::new(), None, None, None, 0.0)?
      .expect("an account is available");
  assert_eq!(expected.label, "main");
  let guard = ProcessGroupGuard::new();
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::Implement)))).await?;
  register_run(&stack, pooled_run(&worker.id, "run-work", POOLED), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-work")?;
  track_run_pid(&stack, &guard, &worker.id, "run-work").await?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  let mut review_pid_tracked = false;
  settle_until(|| {
    let Some(review) = stack.run_records(&worker.id)?.into_iter().find(|run| run.kind == RunKind::Review) else {
      return Ok(false);
    };
    if let Some(pid) = review.pid
      && !review_pid_tracked
    {
      guard.track(pid);
      review_pid_tracked = true;
    }
    Ok(run_records::terminal_outcome(&stack.run_events(&worker.id)?, &review.id)?.is_some())
  })
  .await?;
  let builds = scripts.builds();
  assert_eq!(builds.len(), 2, "the work and review each launch a build");
  assert_eq!(
    builds.iter().map(account_label).collect::<Vec<_>>(),
    vec![Some(expected.label.as_str()), Some(expected.label.as_str())],
    "both launches take the account claude_accounts.select returns"
  );
  Ok(())
}

/// `test_task_execution.py::test_pooled_worker_launch_selects_inside_the_option_pool`: with pools defined, the launch
/// picks an account of its option's pool. The untouched beta accounts hold more headroom than the alpha member, and
/// stay unused.
#[tokio::test]
async fn pooled_worker_launch_selects_inside_the_option_pool() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_pooled_worker_launch_selects_inside_the_option_pool.
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  // main sits at half a window; across all accounts an untouched beta account wins.
  claude_accounts::observe_rate_limit(&state, "main", &rate_limit_info("allowed", 0.50), None);
  let scripts = Scripts::new(vec![exits(&[assistant_text("done"), result()], 0)]);
  let pools: &[(&str, &[&str])] = &[("alpha", &["main"]), ("beta", &["ext-1", "ext-2"])];
  let stack = pooled_stack(&root, &["main", "ext-1", "ext-2"], pools, &scripts, &state).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, pooled_run(&worker.id, "run-pool", POOLED), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-pool")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-pool").await?, RunOutcomeKind::Success);
  assert_eq!(account_label(&scripts.builds()[0]), Some("main"), "the launch stays inside the option's pool");
  let unpooled =
    claude_accounts::select(&state, &stack.cfg, Some("test-model"), &HashSet::new(), None, None, None, 0.0)?
      .expect("an account is available");
  assert_ne!(unpooled.label, "main", "without the pool, the untouched beta account would win");
  Ok(())
}

/// `test_task_execution.py::test_unpooled_fresh_worker_launch_carries_no_account`, over an empty pool: the cc-claude
/// option runs on no account, so its launch carries no `claude_account` and runs on the default login.
#[tokio::test]
async fn unpooled_fresh_worker_launch_carries_no_account_on_an_empty_pool() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_unpooled_fresh_worker_launch_carries_no_account.
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(&[assistant_text("done"), result()], 0)]);
  let stack = pooled_stack(&root, &[], &[], &scripts, &state).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, pooled_run(&worker.id, "run-work", POOLED), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  assert!(scripts.builds()[0].launch.backend_kwargs.get("claude_account").is_none());
  Ok(())
}

/// `test_task_execution.py::test_unpooled_fresh_worker_launch_carries_no_account`, over a non-Claude backend: the
/// launch carries no `claude_account` either.
#[tokio::test]
async fn unpooled_fresh_worker_launch_carries_no_account_on_a_non_claude_backend() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_unpooled_fresh_worker_launch_carries_no_account.
  let scripts = Scripts::new(vec![result_script("done")]);
  let stack = scripted_stack(&scripts).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, work_run(&worker.id, "run-work", None), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-work")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-work").await?, RunOutcomeKind::Success);
  assert!(scripts.builds()[0].launch.backend_kwargs.get("claude_account").is_none());
  Ok(())
}

/// `test_task_execution.py::test_pool_exhausted_launch_fails_the_run_with_evidence_and_no_process`: with no account
/// left to select, the Run fails before any process starts. Its events log carries the pool-exhausted error, which
/// names the earliest reset, and the error event is flagged as quota exhaustion.
#[tokio::test]
async fn pool_exhausted_launch_fails_the_run_with_evidence_and_no_process() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_pool_exhausted_launch_fails_the_run_with_evidence_and_no_process.
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  for label in ["main", "ext-1", "ext-2"] {
    claude_accounts::observe_rate_limit(&state, label, &rate_limit_info("rejected", 1.0), None);
  }
  let scripts = Scripts::new(vec![]);
  let stack = pooled_stack(&root, &["main", "ext-1", "ext-2"], &[], &scripts, &state).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", None))).await?;
  register_run(&stack, pooled_run(&worker.id, "run-x", POOLED), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-x")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-x").await?, RunOutcomeKind::Failed);
  let run = stack.store().read_run_sync(&worker.id, "run-x")?.expect("the run is recorded");
  assert!(run.pid.is_none(), "no process started");
  assert!(scripts.builds().is_empty(), "the backend was never invoked");
  let error = error_text(&stack, &worker.id, "run-x")?;
  assert!(error.contains(claude_relay::POOL_EXHAUSTED_PHRASE), "{error}");
  assert!(error.contains("earliest reset") && error.contains("UTC"), "{error}");
  assert_eq!(error, claude_relay::pool_exhausted_message(&state, &stack.cfg, None, None)?);
  let flags: Vec<Value> = run_log(&stack, &worker.id, "run-x")?
    .iter()
    .filter(|event| event["type"] == event_types::ERROR)
    .map(|event| event[event_types::QUOTA_EXHAUSTED].clone())
    .collect();
  assert_eq!(flags, [json!(true)], "the refusal's flag rides the launch error event");
  Ok(())
}

/// `test_task_execution.py::test_rejected_first_process_relays_to_another_pool_account_and_succeeds`: a Run launched
/// through task execution relays like a directly driven worker. The first process's rejected reading moves the run to
/// the account with the most headroom, on the same Claude conversation, and the Run ends successfully.
#[tokio::test]
async fn rejected_first_process_relays_to_another_pool_account_and_succeeds() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_rejected_first_process_relays_to_another_pool_account_and_succeeds.
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let gate = root.path().join("gate");
  let scripts = Scripts::new(vec![
    gated(&gate, exits(&[assistant_text("halfway"), rate_limit("rejected", 1.0)], 1)),
    exits(&[assistant_text("done after relay"), result()], 0),
  ]);
  let stack = pooled_stack(&root, &["main", "ext-1", "ext-2"], &[], &scripts, &state).await?;
  let worker = stack.node("w", None, TaskProfile::Worker, Some(spec("ship it", Some(TaskType::QuickEdit)))).await?;
  register_run(&stack, pooled_run(&worker.id, "run-relay", POOLED), "ship it").await?;
  stack.adapter.launch(&worker.id, "run-relay")?;

  // The fresh launch binds a Claude conversation of its own: its transcript exists under the first account before the
  // first process is released to reach its rejection, so the relay has a transcript to move.
  settle_until(|| Ok(!scripts.builds().is_empty())).await?;
  let first = scripts.builds()[0].clone();
  let conversation = first.launch.backend_kwargs["claude_session_id"]
    .as_str()
    .expect("a fresh launch pins its conversation")
    .to_string();
  make_transcript(&account_dir(&first).expect("the first launch names its account"), &conversation)?;
  settle_until(|| Ok(stack.store().read_run_sync(&worker.id, "run-relay")?.is_some_and(|run| run.pid.is_some())))
    .await?;
  let first_pid = stack.store().read_run_sync(&worker.id, "run-relay")?.and_then(|run| run.pid);
  std::fs::write(&gate, "")?;

  assert_eq!(stack.wait_terminal(&worker.id, "run-relay").await?, RunOutcomeKind::Success);
  let run = stack.store().read_run_sync(&worker.id, "run-relay")?.expect("the run is recorded");
  assert_eq!(run.exit_code, Some(0));
  assert_ne!(run.pid, first_pid, "the Run's identity follows the relay's second process");
  let builds = scripts.builds();
  assert_eq!([account_label(&builds[0]), account_label(&builds[1])], [Some("main"), Some("ext-1")]);
  let moved = root.claude("ext-1").join("projects").join("slug").join(format!("{conversation}.jsonl"));
  assert!(moved.is_file(), "the transcript moved with the run to {}", moved.display());
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
  let path = directory.join("fake-router.sh");
  std::fs::write(&path, format!("set -eu\n{body}\n"))?;
  let mut command = vec!["/bin/sh".to_string(), path.display().to_string()];
  command.extend(arguments.iter().cloned());
  let mut cfg = stack.cfg.as_ref().clone();
  cfg.backends.routing = Some(RoutingConfig { command });
  stack.replace_config(cfg)
}

/// A router body that answers `backends` in that order. It drains its request first, as a router reads its stdin.
fn router_order_body(backends: &[&str]) -> String {
  format!("cat >/dev/null\nprintf '%s' '{}'", json!({"backends": backends, "reason": "design p=0.9"}))
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

/// `test_backend_router.py` empty route: the route list is empty, so the pool picks the backend and the node records
/// the empty route with its pool flag.
#[tokio::test]
async fn an_empty_route_list_uses_the_pool() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:241.
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&[]), &[])?;

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  let meta = stack.store().load_task_meta(&child_id).await?;
  assert_eq!(meta.backend, "fake", "the pool's first member");
  assert_eq!(meta.extra("route").and_then(|route| route.get("backends")), Some(&json!([])));
  assert_eq!(meta.extra("backend_from_pool"), Some(&json!(true)));
  Ok(())
}

/// `test_backend_router.py` cooling route: the route's first member is cooling, so the next routed member runs the
/// work and the delegation still answers 200.
#[tokio::test]
async fn a_cooling_routed_list_falls_back_to_the_pool() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:269.
  let scripts = Scripts::new(vec![result_script("pool work done")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&["fake2", "fake"]), &[])?;
  backend_pool::backend_pool(&stack.features)?.cool("fake2", None);

  let (status, delegated) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::OK, "{delegated}");
  let child_id = text(&delegated, "session_id")?;
  assert_eq!(stack.store().load_task_meta(&child_id).await?.backend, "fake");
  Ok(())
}

/// `test_backend_router.py` no member available: every pool member is cooling, so the delegation answers 400 and the
/// detail names each member's recovery time.
#[tokio::test]
async fn every_backend_unavailable_returns_400_with_recovery_times() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:281.
  let scripts = Scripts::new(vec![result_script("unused")]);
  let (stack, manager) = delegating_manager(&scripts, &["fake", "fake2"]).await?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), &router_order_body(&[]), &[])?;
  let pool = backend_pool::backend_pool(&stack.features)?;
  pool.cool("fake", None);
  pool.cool("fake2", None);

  let (status, response) = post_delegate(&routes::routes(&stack.features), quick_edit_body(&manager)).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert!(response["detail"].as_str().unwrap_or_default().contains("cooling until"), "{response}");
  Ok(())
}

/// `test_backend_router.py` no routing section: with `backends.routing` unset, no router starts and the pool decides.
#[tokio::test]
async fn without_a_routing_section_no_router_starts_and_the_pool_decides() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:293.
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

/// `test_backend_router.py` pinned delegation: a delegation that names its backend never asks the router.
#[tokio::test]
async fn a_pinned_delegation_never_calls_the_router() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:305.
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

/// `test_backend_router.py` cold read: a cold-read delegation never asks the router, and its node carries no route.
#[tokio::test]
async fn a_cold_read_delegation_never_calls_the_router() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:318.
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

/// `test_backend_router.py` delegation cases: one routed verify request carries the chosen backend and route through
/// both the worker node metadata and its delegation card, and the router sees the manager's backend excluded.
#[tokio::test]
async fn routed_verify_delegation_persists_the_route_and_excludes_its_session_backend() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:221-237, 331-346.
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
    "cat > \"$1\"\nprintf '%s\\n' started >> \"$2\"\nprintf '%s' '{\"backends\":[\"fake2\"],\"reason\":\"verify route\"}'",
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
  assert_eq!(request["text"], "Task type: verify\n\nconfirm the change");
  assert_eq!(request["candidates"], json!([{"id": "fake2", "cooling_until": null}]));
  assert_eq!(std::fs::read_to_string(calls)?, "started\n");
  assert_eq!(stack.wait_terminal(child_id, run_id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// A router failure is returned before the delegation handler creates a worker child.
#[tokio::test]
async fn router_failure_returns_502_without_creating_a_worker() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:254-266.
  let scripts = Scripts::new(vec![result_script("unused")]);
  let options = ["fake", "fake2"].map(|id| scripts.option(id, BACKEND_TYPE)).to_vec();
  let stack = Stack::start_with(options, vec!["fake".to_string(), "fake2".to_string()]).await?;
  stack.write_access_key()?;
  let router_dir = tempfile::tempdir()?;
  install_router(&stack, router_dir.path(), "cat >/dev/null\nprintf '%s' 'router boom' >&2\nexit 3", &[])?;
  let manager = stack.node("router-error-parent", None, TaskProfile::Manager, None).await?;
  stack.dispatch.set_executor(None);
  stack.dispatch.admit_input(&manager.id, event_types::USER, "Take off and delegate this.", "user", json!({})).await?;
  let before = stack.store().child_ids(&manager.id)?;

  let body = json!({
    "session_id": manager.id,
    "description": "run the script",
    "task_type": "script-run",
  });
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;

  assert_eq!(status, StatusCode::BAD_GATEWAY, "{response}");
  assert!(response["detail"].as_str().unwrap_or_default().contains("router boom"), "{response}");
  assert_eq!(stack.store().child_ids(&manager.id)?, before);
  assert!(
    stack.store().load_chat_events_sync(&manager.id)?.iter().all(|event| event["type"] != event_types::TASK_DELEGATED)
  );
  Ok(())
}

/// A worker on `fake2` whose run ended on quota exhaustion, with its revision batch bound (`_interrupted_worker_run`).
/// A pool-chosen node carries the pool's backend-from-pool flag. Returns the worker's id and the run's id.
async fn interrupted_worker_run(
  stack: &Stack,
  parent: &str,
  task_type: TaskType,
  from_pool: bool,
) -> anyhow::Result<(String, String)> {
  let worker = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "worker".to_string(),
        task_parent_id: Some(parent.to_string()),
        profile: TaskProfile::Worker,
        task: Some(spec("do the work", Some(task_type))),
        name: Some("Synthetic worker".to_string()),
        backend: Some("fake2".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  if from_pool {
    let guard = stack.store().control_lock().await;
    let flag = Map::from_iter([("backend_from_pool".to_string(), json!(true))]);
    stack.store().update_task_slot_fields_locked(&guard, &worker.id, backend_pool::METADATA_OWNER, flag).await?;
  }
  let feedback = stack.dispatch.admit_input(&worker.id, event_types::USER, REVIEW_FEEDBACK, "user", json!({})).await?;
  let input = feedback["id"].as_str().expect("an admitted input has an id").to_string();
  let record = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake-model".to_string()),
    input_event_ids: vec![input],
    ..synthetic_run(&worker.id, "run-revision", RunKind::Work)
  };
  register_run(stack, record, "do the work").await?;
  let failed = RunOutcome { exit_code: Some(1), ..RunOutcome::new(RunOutcomeKind::Failed) };
  stack.dispatch.finish_run(&worker.id, "run-revision", failed).await?;
  Ok((worker.id, "run-revision".to_string()))
}

/// The failover of a pool-chosen worker whose run ended on quota exhaustion: the member cools, the node moves to the
/// next member, and a retry run on it binds the interrupted run's input batch and launches with the feedback it
/// answers.
async fn quota_failure_fails_over_to_the_next_member(task_type: TaskType) -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("retry done"), result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, task_type, true).await?;
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
  assert_eq!(stack.store().load_task_meta(&worker).await?.backend, "fake3", "the node moved");
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  let failures: Vec<Value> =
    stack.child_reports(&manager.id)?.into_iter().filter(|report| report["outcome"] == "failed").collect();
  assert!(failures.is_empty(), "a failover sends no failure report: {failures:?}");
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  let report = backend_pool::backend_pool(&stack.features)?.recovery_report(&stack.cfg);
  assert!(report.contains("fake2 cooling until"), "the failed member cooled: {report}");
  // The retry's success wakes the parent in the background; wait for the turn it launches before the stack drops.
  settle_until(|| Ok(!stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.is_empty())).await?;
  settle_node(&stack, &worker).await?;
  settle_node(&stack, &manager.id).await?;
  Ok(())
}

/// `test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member`, over an implement task.
#[tokio::test]
async fn quota_failure_on_an_implement_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member.
  quota_failure_fails_over_to_the_next_member(TaskType::Implement).await
}

/// `test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member`, over a quick edit.
#[tokio::test]
async fn quota_failure_on_a_quick_edit_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member.
  quota_failure_fails_over_to_the_next_member(TaskType::QuickEdit).await
}

/// `test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member`, over a script run.
#[tokio::test]
async fn quota_failure_on_a_script_run_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member.
  quota_failure_fails_over_to_the_next_member(TaskType::ScriptRun).await
}

/// `test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member`, over a verify task.
#[tokio::test]
async fn quota_failure_on_a_verify_pool_node_fails_over_to_the_next_member() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_quota_failure_on_a_pool_node_fails_over_to_the_next_member.
  quota_failure_fails_over_to_the_next_member(TaskType::Verify).await
}

/// A quota retry checks the remaining route order before asking the pool for a backend.
#[tokio::test]
async fn quota_failover_walks_the_route_before_the_pool_and_writes_one_system_event() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:354-386.
  let scripts = Scripts::new(vec![result_script("route retry"), result_script("parent noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("route-failover-parent", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::ScriptRun, true).await?;
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
  assert_eq!(retry.backend.as_deref(), Some("fake"));
  assert_eq!(stack.store().load_task_meta(&worker).await?.backend, "fake");
  let switches = stack
    .store()
    .load_chat_events_sync(&worker)?
    .into_iter()
    .filter(|event| event["type"] == event_types::QUOTA_FAILOVER)
    .collect::<Vec<_>>();
  assert_eq!(switches.len(), 1, "one quota failover event is persisted: {switches:?}");
  assert_eq!(switches[0]["from"], "fake2");
  assert_eq!(switches[0]["to"], "fake");
  assert_eq!(switches[0]["recovers_at"], "2030-01-02T03:04+00:00");
  assert_eq!(stack.wait_terminal(&worker, &retry.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// `test_backend_router.py` spent route: the route names only the member that ran out, so the failover takes the pool's
/// first free member, and the retry runs there.
#[tokio::test]
async fn quota_failover_uses_the_pool_once_the_route_is_spent() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:390.
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

/// `test_task_execution.py::test_a_pinned_worker_is_not_failed_over_and_its_member_is_not_cooled`: a node whose
/// delegation named its backend keeps the quota behavior of before: the failure is reported, nothing retries, and the
/// pool is untouched.
#[tokio::test]
async fn a_pinned_worker_is_not_failed_over_and_its_member_is_not_cooled() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_a_pinned_worker_is_not_failed_over_and_its_member_is_not_cooled.
  let scripts = Scripts::new(vec![result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::Implement, false).await?;
  stack.adapter.record_quota_failure_for_test(&worker, &run, None);
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  assert!(stack.run_records(&worker)?.iter().all(|record| record.retry_of_run_id.is_none()), "no retry starts");
  let reports = stack.child_reports(&manager.id)?;
  assert_eq!(reports.len(), 1, "the failure is reported once: {reports:?}");
  assert_eq!(reports[0]["outcome"], "failed");
  let report = backend_pool::backend_pool(&stack.features)?.recovery_report(&stack.cfg);
  assert!(report.contains("fake2 not cooling"), "the pinned member is not cooled: {report}");
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  assert_eq!(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(), 1);
  Ok(())
}

/// `test_task_execution.py::test_failover_with_every_member_cooling_reports_each_recovery_time`: with the next member
/// cooling too, no retry starts, and the parent's report names each member's recovery time.
#[tokio::test]
async fn failover_with_every_member_cooling_reports_each_recovery_time() -> anyhow::Result<()> {
  // Python: tests/test_task_execution.py::test_failover_with_every_member_cooling_reports_each_recovery_time.
  let scripts = Scripts::new(vec![result_script("report noted")]);
  let stack = failover_stack(&scripts).await?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let (worker, run) = interrupted_worker_run(&stack, &manager.id, TaskType::Implement, true).await?;
  let fake3_reset = UtcDateTime::parse("2030-01-02T03:04:00+00:00").expect("a fixed instant parses");
  backend_pool::backend_pool(&stack.features)?.cool("fake3", Some(fake3_reset));
  let fake2_reset = UtcDateTime::parse("2030-01-02T02:00:00+00:00").expect("a fixed instant parses");
  stack.adapter.record_quota_failure_for_test(&worker, &run, Some(fake2_reset));
  stack.adapter.after_worker_run_for_test(&worker, &run, "failed").await?;

  assert!(stack.run_records(&worker)?.iter().all(|record| record.retry_of_run_id.is_none()), "no retry starts");
  let reports = stack.child_reports(&manager.id)?;
  assert_eq!(reports.len(), 1, "the failure is reported once: {reports:?}");
  let summary = reports[0]["summary"].as_str().expect("the report names the recovery times");
  assert!(summary.contains("fake2 cooling until 2030-01-02T02:00"), "{summary}");
  assert!(summary.contains("fake3 cooling until 2030-01-02T03:04"), "{summary}");
  assert!(
    summary.contains(
      "\n\nno backends.pool member is available: \
       fake2 cooling until 2030-01-02T02:00+00:00; fake3 cooling until 2030-01-02T03:04+00:00"
    ),
    "{summary}"
  );
  settle_until(|| Ok(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  assert_eq!(stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?.len(), 1);
  Ok(())
}

// ---------------------------------------------------------------------------
// Temporary backend failures over the backend pool (the worker-path cases of `tests/test_backend_router.py`)
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

/// Launch an implement worker's first run, which prints `first_events` and exits 1, and wait for it to end. A
/// pool-chosen worker takes the pool's first member; a named worker takes `fake`. The retry on the next member prints
/// one assistant line and exits 1, and the manager's turn after the failure report prints one result.
async fn launch_error_event_worker(first_events: &[Value], pool_chosen: bool) -> anyhow::Result<ErrorEventRun> {
  let retry_events =
    [json!({"type": event_types::ASSISTANT, "message": {"content": [{"type": "text", "text": "retry stopped"}]}})];
  let scripts =
    Scripts::new(vec![events_script(first_events, 1), events_script(&retry_events, 1), result_script("manager noted")]);
  let options = ["fake", "fake2"]
    .map(|id| BackendOption { model: "fake-model".to_string(), ..scripts.option(id, BACKEND_TYPE) })
    .to_vec();
  let stack = Stack::start_with(options, vec!["fake".to_string(), "fake2".to_string()]).await?;
  stack.write_access_key()?;
  let manager = stack.node("root", None, TaskProfile::Manager, None).await?;
  let first_backend = if pool_chosen {
    let pool = backend_pool::backend_pool(&stack.features)?;
    pool.select(&stack.cfg, "test", &[])?.expect("the pool selects a first member").id
  } else {
    "fake".to_string()
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
        backend: Some(first_backend.clone()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  if pool_chosen {
    let guard = stack.store().control_lock().await;
    let flag = Map::from_iter([("backend_from_pool".to_string(), json!(true))]);
    stack.store().update_task_slot_fields_locked(&guard, &worker.id, backend_pool::METADATA_OWNER, flag).await?;
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

/// The failover of a pool-chosen worker whose first run ended on a temporary error: the member cools, the node moves
/// to `fake2`, the retry binds the failed run's inputs and worktree, and the parent hears only the retry's failure.
async fn assert_temporary_failover(error: &ErrorEventRun) -> anyhow::Result<()> {
  let stack = &error.stack;
  let pool = backend_pool::backend_pool(&stack.features)?;
  assert_eq!(error.outcome, RunOutcomeKind::Failed);
  let retry = wait_for_retry(stack, &error.worker_id, &error.failed.id).await?;
  assert!(retry.is_some(), "the temporary error fails the run over to the next member");
  let retry = retry.expect("checked above");
  assert_eq!(stack.wait_terminal(&error.worker_id, &retry.id).await?, RunOutcomeKind::Failed);
  settle_until(|| Ok(stack.child_reports(&error.manager_id)?.len() == 1)).await?;

  let first = error.first_backend.as_str();
  assert!(pool.cooling_until(first).is_some(), "the failed member cools");
  assert!(pool.cooling_until("fake2").is_none(), "the next member stays selectable");
  let remaining = pool.cooling_until(first).expect("checked above") - OffsetDateTime::now_utc();
  assert!(remaining > TimeDuration::minutes(29) && remaining <= backend_pool::DEFAULT_COOLDOWN, "{remaining:?}");
  assert_eq!(retry.backend.as_deref(), Some("fake2"));
  assert_eq!(retry.input_event_ids, error.failed.input_event_ids);
  assert_eq!(retry.worktree_path, error.failed.worktree_path);
  let switches = quota_failover_events(stack, &error.worker_id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["from"].as_str(), Some(first));
  assert_eq!(switches[0]["to"], "fake2");
  assert_eq!(switches[0]["recovers_at"].as_str(), pool.cooling_until_minutes(first).as_deref());
  let reports = stack.child_reports(&error.manager_id)?;
  assert_eq!(reports.len(), 1, "{reports:?}");
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!([format!("run:{}", retry.id)]));
  assert_eq!(stack.cfg.backends.pool, ["fake", "fake2"]);
  settle_node(stack, &error.worker_id).await?;
  settle_node(stack, &error.manager_id).await
}

/// The no-failover case: no member cools, no retry starts, and the parent hears the first run's failure once.
async fn assert_no_failover(error: &ErrorEventRun) -> anyhow::Result<()> {
  let stack = &error.stack;
  assert_eq!(error.outcome, RunOutcomeKind::Failed);
  settle_until(|| Ok(stack.child_reports(&error.manager_id)?.len() == 1)).await?;
  let pool = backend_pool::backend_pool(&stack.features)?;
  assert!(stack.cfg.backends.pool.iter().all(|member| pool.cooling_until(member).is_none()), "no member cools");
  assert_eq!(stack.run_records(&error.worker_id)?.len(), 1, "no retry run starts");
  assert!(quota_failover_events(stack, &error.worker_id)?.is_empty(), "no failover event is written");
  let reports = stack.child_reports(&error.manager_id)?;
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(reports[0]["result_refs"], json!([format!("run:{}", error.failed.id)]));
  settle_node(stack, &error.manager_id).await
}

/// The named-backend worker: its delegation named the backend, so it is not pool-chosen and has no route.
async fn assert_named_worker(error: &ErrorEventRun) -> anyhow::Result<()> {
  let meta = error.stack.store().load_task_meta(&error.worker_id).await?;
  assert_ne!(meta.extra("backend_from_pool"), Some(&Value::Bool(true)), "a named worker is not pool-chosen");
  assert!(meta.extra("route").is_none_or(Value::is_null), "a named worker has no route");
  assert_eq!(error.first_backend, "fake");
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

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (capacity).
#[tokio::test]
async fn at_capacity_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event(CAPACITY_MESSAGE, "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (no output).
#[tokio::test]
async fn no_output_stall_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event("model produced no output for 1200s", "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (bad gateway).
#[tokio::test]
async fn bad_gateway_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (connection).
#[tokio::test]
async fn internal_server_error_with_connection_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "litellm.InternalServerError: InternalServerError: OpenAIException - Connection error.";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (500).
#[tokio::test]
async fn internal_server_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event("Internal Server Error", "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (SSE stall).
#[tokio::test]
async fn opencode_session_stall_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "OpenCode backend failed: OpenCode SSE stream carried no session progress for 3600.0 s \
    (heartbeats during silence: 359)";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_backend_errors_fail_over_on_the_worker_path (closed peer).
#[tokio::test]
async fn opencode_closed_peer_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let message = "OpenCode backend failed: peer closed connection without sending complete message body \
    (incomplete chunked read)";
  let error = launch_error_event_worker(&[error_event(message, "")], true).await?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_stderr_tail_and_request_id_do_not_trigger_backend_failover, the step case.
#[tokio::test]
async fn step_limit_error_with_bad_gateway_content_does_not_fail_over() -> anyhow::Result<()> {
  let error = launch_error_event_worker(
    &[error_event("Step limit (1000) exceeded without task completion.", "Bad Gateway")],
    true,
  )
  .await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_stderr_tail_and_request_id_do_not_trigger_backend_failover, the 400 case.
#[tokio::test]
async fn request_id_with_status_digits_does_not_fail_over() -> anyhow::Result<()> {
  let error =
    launch_error_event_worker(&[error_event("Error code: 400 - invalid request (request id req_502ab1)", "")], true)
      .await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_non_error_event_with_temporary_backend_message_does_not_fail_over
#[tokio::test]
async fn system_event_with_temporary_message_does_not_fail_over() -> anyhow::Result<()> {
  let event = json!({"type": event_types::SYSTEM, "message": CAPACITY_MESSAGE, "content": ""});
  let error = launch_error_event_worker(&[event], true).await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_a_named_backend_capacity_error_reports_failure_without_failover
#[tokio::test]
async fn a_named_backend_capacity_error_reports_failure_without_failover() -> anyhow::Result<()> {
  let error = launch_error_event_worker(&[error_event(CAPACITY_MESSAGE, "")], false).await?;
  assert_named_worker(&error).await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_claude_result_errors_fail_over_on_the_worker_path, status 502.
#[tokio::test]
async fn claude_result_502_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: 502 litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let error = launch_error_event_worker(&[claude_error_result(json!(502), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_claude_result_errors_fail_over_on_the_worker_path, status 529.
#[tokio::test]
async fn claude_result_529_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
  let error = launch_error_event_worker(&[claude_error_result(json!(529), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_claude_result_errors_fail_over_on_the_worker_path, status 429.
#[tokio::test]
async fn claude_result_429_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: 429 litellm.RateLimitError: RateLimitError: OpenAIException - rate limit exceeded";
  let error = launch_error_event_worker(&[claude_error_result(json!(429), result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_temporary_claude_result_errors_fail_over_on_the_worker_path, no status.
#[tokio::test]
async fn claude_result_without_status_connection_error_fails_over_on_the_worker_path() -> anyhow::Result<()> {
  let result = "API Error: Connection error.";
  let error = launch_error_event_worker(&[claude_error_result(Value::Null, result)], true).await?;
  assert_result_logged(&error, result)?;
  assert_temporary_failover(&error).await
}

// Python: tests/test_backend_router.py::test_bad_request_claude_result_errors_do_not_fail_over, the reasoning case.
#[tokio::test]
async fn claude_result_400_reasoning_error_does_not_fail_over() -> anyhow::Result<()> {
  let result = "API Error: 400 litellm.BadRequestError: OpenAIException - The `reasoning_content` in the thinking mode \
    must be passed back to the API. (request_id: f69ad0ec-cd32-4570-917d-7689ec630120)";
  let error = launch_error_event_worker(&[claude_error_result(json!(400), result)], true).await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_bad_request_claude_result_errors_do_not_fail_over, the bad gateway case.
#[tokio::test]
async fn claude_result_400_mentioning_bad_gateway_does_not_fail_over() -> anyhow::Result<()> {
  let result = "API Error: 400 litellm.BadRequestError: upstream reported Bad Gateway";
  let error = launch_error_event_worker(&[claude_error_result(json!(400), result)], true).await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_non_error_claude_result_with_temporary_message_does_not_fail_over
#[tokio::test]
async fn non_error_claude_result_with_temporary_message_does_not_fail_over() -> anyhow::Result<()> {
  let event = json!({
    "type": event_types::RESULT,
    "is_error": false,
    "subtype": "error",
    "api_error_status": null,
    "result": "Fixed the Bad Gateway handler.",
  });
  let error = launch_error_event_worker(&[event], true).await?;
  assert_no_failover(&error).await
}

// Python: tests/test_backend_router.py::test_a_named_backend_claude_result_error_reports_failure_without_failover
#[tokio::test]
async fn a_named_backend_claude_result_error_reports_failure_without_failover() -> anyhow::Result<()> {
  let result = "API Error: 502 litellm.BadGatewayError: BadGatewayError: OpenAIException - Bad Gateway";
  let event = claude_error_result(json!(502), result);
  let error = launch_error_event_worker(&[event], false).await?;
  assert_named_worker(&error).await?;
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

/// `test_artifact_check.py::test_cold_read_run_instructions_hold_only_the_system_prompt_and_the_input_is_the_page_and_questions`:
/// the run's managed instructions hold the cold-reader system prompt alone, and its input is the page with the seven
/// questions.
#[tokio::test]
async fn cold_read_run_instructions_hold_only_the_system_prompt_and_the_input_is_the_page_and_questions()
-> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_run_instructions_hold_only_the_system_prompt_and_the_input_is_the_page_and_questions.
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
  assert_eq!(launch_text, cold_read::probe_input(&page_text, &questions, "where are we?"));
  let builds = scripts.builds();
  let prompt = builds[0].prompt.lock().expect("prompt mutex is not poisoned").clone();
  assert_eq!(prompt.as_deref(), Some(launch_text.as_str()));
  Ok(())
}

/// `test_artifact_check.py::test_cold_read_success_report_carries_attempts_backend_and_answers`: a first reader that
/// fails and a second that answers. The report states the page, the failed attempt, the answering backend and its
/// answer, and the child completes.
#[tokio::test]
async fn cold_read_success_report_carries_attempts_backend_and_answers() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_success_report_carries_attempts_backend_and_answers.
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

/// `test_artifact_check.py::test_cold_read_empty_answer_counts_as_a_failed_run`: a reader that exits with an empty answer
/// is a failed attempt, and the next backend answers.
#[tokio::test]
async fn cold_read_empty_answer_counts_as_a_failed_run() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_empty_answer_counts_as_a_failed_run.
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

/// `test_artifact_check.py::test_cold_read_retry_selects_the_next_backend_with_the_tried_list`: the retry passes the
/// failed run's backend as tried, so the selection skips it and the next pool member takes the work. Python spies the
/// selection's tried list; here the selection is called with the same lists, and the retry's run is checked.
#[tokio::test]
async fn cold_read_retry_selects_the_next_backend_with_the_tried_list() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_retry_selects_the_next_backend_with_the_tried_list.
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
    spawner_backends::select_verify_backend(&cfg, stack.store(), &manager.id, "cold_read", &[], &pool).await?;
  assert_eq!(first.map(|(backend, _, tried)| (backend, tried)), Some(("alpha".to_string(), vec!["alpha".to_string()])));
  let retry = spawner_backends::select_verify_backend(
    &cfg,
    stack.store(),
    &manager.id,
    "cold_read",
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

/// `test_artifact_check.py::test_cold_read_every_backend_failed_reports_and_the_child_is_archived`: every reader fails,
/// so the report names each attempt and the trailer, and its delivery archives the child once.
#[tokio::test]
async fn cold_read_every_backend_failed_reports_and_the_child_is_archived() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_every_backend_failed_reports_and_the_child_is_archived.
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
