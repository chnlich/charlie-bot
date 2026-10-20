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
use charliebot_agents::backends::claude_code::claude_accounts::{self, ClaudeAccountState};
use charliebot_agents::backends::claude_code::claude_config;
use charliebot_agents::backends::claude_code::claude_lifecycle::ClaudeCodeLifecycle;
use charliebot_agents::backends::claude_code::claude_relay;
use charliebot_agents::backends::claude_code::login_dirs;
use charliebot_core::config::{BackendOption, ConfigSection};
use charliebot_core::event_types;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, ResumeStyle};
use charliebot_registry::{RegisterFn, Registry, RegistryError};
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::write_faults::{self, Hit};
use charliebot_store::{CreateTask, PatchTask, TaskCaller};
use charliebot_tasks::task_sessions;
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, Build, Scripts, Stack, init_repo_with_origin, post_delegate, result_script, scripted_type,
  settle_until, synthetic_git, synthetic_run,
};
use serde_json::{Map, Value, json};
use time::{Duration as TimeDuration, OffsetDateTime};

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
/// when the state above already holds, so a node holding child reports also waits for that turn's run to exist.
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
    if !stack.child_reports(session_id)?.is_empty()
      && stack.run_ids_of_kind(session_id, RunKind::ManagerTurn)?.is_empty()
    {
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
  assert!(scripts.builds().is_empty(), "the backend was never invoked");
  let error = error_text(&stack, &worker.id, "run-work")?;
  assert!(error.contains("BaseBranchResolutionError"), "{error}");
  assert!(error.contains("differs from origin/main"), "{error}");
  assert!(error.contains(&local_tip[..12]), "{error}");
  assert!(!error.contains("fast-forward"), "{error}");
  // The error text is the run's evidence, never an ordinary chat row.
  let rows = stack.store().load_chat_events_sync(&worker.id)?;
  assert!(rows.iter().all(|row| !row.to_string().contains(&error)), "the error text is in no chat row");
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
  assert_eq!(stack.wait_terminal(&worker.id, &run).await?, RunOutcomeKind::Success);
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
  assert_eq!(stack.wait_terminal(&verify.id, &verify_run).await?, RunOutcomeKind::Success);
  let verify_record = stack.store().read_run_sync(&verify.id, &verify_run)?.expect("the verify run is recorded");
  let verify_refs = snapshot_source_refs(&snapshot_of(&verify_record)?);
  assert!(verify_refs.iter().any(|source| source == "prompts/verify.md"), "{verify_refs:?}");
  assert!(!verify_refs.iter().any(|source| source == "prompts/worker.md"), "{verify_refs:?}");
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
    .filter(|event| event["source"] == "turn_launch")
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

  // The task's type is unset, so the completion follow-up auto-completes it (no review evidence needed) while the
  // adapter's implement branch also reports blocked: the pool's only member is the worker's own backend.
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 2)).await?;
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("completed"), json!("blocked")], "success-based, once");
  assert!(stack.adapter.landing_retry_nodes().is_empty(), "a clean round ended the retry task");
  let work_prompts = scripts.builds().iter().filter(|build| build.prompt_contains("Start the task.")).count();
  assert_eq!(work_prompts, 1, "one process: the retry drained, never relaunched");
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
  assert!(scripts.builds().is_empty(), "the worker never reached its backend");
  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("blocked")]);

  // A manager turn launches under the same threshold: the blocked report's own wake started one.
  settle_node(&stack, &manager.id).await?;
  let turns = stack.run_ids_of_kind(&manager.id, RunKind::ManagerTurn)?;
  assert_eq!(turns.len(), 1);
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&manager.id)?, &turns[0])?, Some(RunOutcomeKind::Success));

  // Threshold 0 skips the check, so the queued worker launches.
  stack.set_min_free_disk_gib(0)?;
  let decision = stack.dispatch.dispatch_pending(&worker.id).await?;
  assert_eq!(stack.wait_terminal(&worker.id, &launched_run_id(&decision)?).await?, RunOutcomeKind::Success);
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
  registry.config_section(ConfigSection { key: "accounts", legacy_keys: &[], parse: claude_config::parse_accounts })?;
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
  let mut text = format!("paths:\n  worktree_dir: {}\n", root.path().join("worktrees").display());
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
  let first = scripts.builds().into_iter().next().expect("the retry launched a process");
  assert!(first.prompt_contains(REVIEW_FEEDBACK), "the retry's launch names the feedback it answers");
  assert_eq!(stack.store().load_task_meta(&worker).await?.backend, "fake3", "the node moved");
  let failures: Vec<Value> =
    stack.child_reports(&manager.id)?.into_iter().filter(|report| report["outcome"] == "failed").collect();
  assert!(failures.is_empty(), "a failover sends no failure report: {failures:?}");
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
  settle_node(&stack, &manager.id).await?;
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
  settle_node(&stack, &manager.id).await?;
  Ok(())
}
