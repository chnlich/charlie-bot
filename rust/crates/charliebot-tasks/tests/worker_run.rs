//! The worker-run cases of `tests/test_task_execution.py` over the task-execution stack: a worker Run launches through the
//! adapter, its process is a `/bin/sh` script, and its delivery chain runs on the real store. Each test names the Python
//! case it ports in a `// Python:` comment.
//!
//! The review spawn of an implement Run belongs to the review-chain task. The chain returns its named error at that
//! step, so a case that ends at a review is asserted up to that step. The work and verify launches are asserted here;
//! the review, iteration and scheduled-step launches stay with the review-chain, improve and cron tasks.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use charliebot_core::event_types;
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::write_faults::{self, Hit};
use charliebot_store::{PatchTask, TaskCaller};
use charliebot_tasks::task_sessions;
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, Scripts, Stack, init_repo_with_origin, post_delegate, settle_until, synthetic_git,
  synthetic_run,
};
use serde_json::{Value, json};

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

/// A `/bin/sh` script that prints one successful result line.
fn result_script(text: &str) -> String {
  format!("printf '%s\\n' '{}'", json!({"type": "result", "subtype": "success", "result": text}))
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
async fn settle_node(stack: &Stack, session_id: &str) -> anyhow::Result<()> {
  settle_until(|| {
    let events = stack.run_events(session_id)?;
    for run in stack.run_records(session_id)? {
      if run_records::terminal_outcome(&events, &run.id)?.is_none() {
        return Ok(false);
      }
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
  Ok(())
}

/// Advance origin's main from a second clone, so the fixture's local main falls behind it (`_advance_origin_from_second_clone`).
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
/// the local main ref is untouched. The review that lands the work is the review-chain task's, so the case stops at the
/// work Run's success.
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

/// `test_task_execution.py::test_a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt`: an upload admitted
/// while a worker run holds its slot waits for that run's end. The continuation resumes the run's native conversation,
/// and its prompt names the upload's path.
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

/// `test_task_execution.py::test_worker_run_kinds_deliver_snapshot_bytes_and_task_context`: a script-run work launch and a
/// verify launch ride the same assembler with their own contracts. The work launch carries the worker contract, the goal
/// and the input in its launch text, and a repository sandbox; the verify launch carries the verify contract only.
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

/// `test_task_execution.py::test_worktree_preparation_failure_lands_failed_run_and_reports_to_parent`: a repository whose
/// local base is ahead of origin fails worktree preparation after the Run was admitted. The Run lands its durable failed
/// fact with the actual error as evidence in its own events log, and the parent receives a failure report naming it. The
/// report's delivery archives the worker.
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
  Ok(())
}

/// `test_task_execution.py::test_recovery_after_rule_deletion_uses_the_original_snapshot`: a manager turn's stored snapshot
/// reads on after its rule body is deleted, and an explicit retry is a new Run that assembles from the current sources.
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

/// `test_task_execution.py::test_a_worker_launch_failure_writes_no_turn_launch_error`: a worker Run whose backend cannot
/// be resolved fails at launch, and no manager-turn launch notice is written for it.
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

/// `test_task_execution.py::test_worker_run_finished_enospc_retries_and_lands_without_restart`: the `run_finished` write
/// of a successful worker run fails out of space. The end-landing retry drains the run from its raw log and lands the
/// end record, the parent gets exactly one completed report, and no second process starts.
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

  settle_until(|| Ok(stack.child_reports(&manager.id)?.len() == 1)).await?;
  settle_node(&stack, &manager.id).await?;
  let outcomes: Vec<Value> = stack.child_reports(&manager.id)?.iter().map(|report| report["outcome"].clone()).collect();
  assert_eq!(outcomes, vec![json!("completed")], "success-based, once");
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

/// `test_task_execution.py::test_non_space_end_failure_lands_immediately_and_starts_no_retry`: a non-space failure in the
/// end path lands the run at once and starts no retry. A success fact whose delivery hits EIO sends no failed report.
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
  Ok(())
}
