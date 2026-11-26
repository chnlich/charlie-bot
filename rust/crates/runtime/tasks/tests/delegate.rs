//! The delegation route's gate, backend choice, and child record, plus the
//! session-message relay that the same router serves. Each case drives `routes::routes` through `post_delegate` (or
//! `post_delegate_as` for a run token, `post_internal` for the relay) on a synthetic task tree.
//!
//! The adapter starts a delegated run's process at once, so the cases that must hold it open use `HANG` and let
//! the guard kill it, or run it to a terminal fact.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::path::Path;

use axum::http::StatusCode;
use charliebot_core::event_types;
use charliebot_core::run_token::{self, RunTokenClaims};
use charliebot_store::control_events;
use charliebot_store::metadata::{
  ColdReadSpec, PARENT_SESSION_ID, SessionMetadata, TaskApprover, TaskProfile, TaskSpec, TaskType,
};
use charliebot_store::run_records::{self, RunKind, RunOutcomeKind, RunRecord};
use charliebot_store::{CreateTask, TaskCaller};
use charliebot_tasks::completion::ArchiveAuthority;
use charliebot_tasks::prompts::MAX_AGENT_MESSAGE_WEIGHTED_CHARS;
use charliebot_tasks::routes;
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, ROTATION_POOL_ID, Scripts, Stack, cold_read_body, init_repo_with_origin, post_delegate,
  post_delegate_as, post_internal, result_script, settle_until, write_sitrep,
};
use serde_json::{Map, Value, json};

/// The script a launched process runs until the guard kills it.
const HANG: &str = "sleep 30";

/// The backend options the pool cases route between: the first is the session's and the first pool member.
const OPTIONS: [&str; 3] = ["opus", "codex-o3", "kimi-k2.5"];

/// A stack whose backend options are `ids`, each serving `scripts` in launch order, with the operator's access key on
/// disk and a rotate pool named `pool` whose members are `pool`.
async fn stack_with(scripts: &Scripts, ids: &[&str], pool: &[&str]) -> anyhow::Result<Stack> {
  let options = ids.iter().map(|id| scripts.option(id, BACKEND_TYPE)).collect();
  let stack = Stack::start_with(options, pool.iter().map(|id| id.to_string()).collect()).await?;
  stack.write_access_key()?;
  Ok(stack)
}

/// A task node on `backend`, created under the operator's caller.
async fn node(
  stack: &Stack,
  request_id: &str,
  parent: Option<&str>,
  profile: TaskProfile,
  backend: &str,
) -> anyhow::Result<SessionMetadata> {
  let task = TaskSpec {
    goal: format!("synthetic {request_id}"),
    repo_path: None,
    base_branch: None,
    task_type: None,
    keep_worktree: false,
    cold_read: None,
    approver: TaskApprover::User,
    improve_host: None,
  };
  Ok(
    stack
      .store()
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: Some(task),
          name: Some(format!("Synthetic {request_id}")),
          backend: Some(backend.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?,
  )
}

/// The operator's user message on `session_id`: the real user input a delegation chain needs.
async fn user_message(stack: &Stack, session_id: &str, content: &str) -> anyhow::Result<()> {
  stack.dispatch.admit_input(session_id, event_types::USER, content, "user", json!({})).await?;
  Ok(())
}

/// The delegation body one test sends (`conftest.delegate_payload`): a repository task on `repo` against `main`.
fn delegate_body(session_id: &str, repo: &Path, task_type: &str) -> Value {
  json!({
    "session_id": session_id,
    "description": "## Goal\n\nfix the thing\n",
    "task_type": task_type,
    "keep_worktree": false,
    "repo_path": repo.display().to_string(),
    "base_branch": "main",
  })
}

/// The repo-less verify delegation body.
fn verify_body(session_id: &str) -> Value {
  json!({
    "session_id": session_id,
    "description": "## Goal\n\ncheck the thing\n",
    "task_type": "verify",
    "keep_worktree": false,
    "repo_path": null,
    "base_branch": null,
  })
}

/// A delegation body that names `backend`.
fn with_backend(mut body: Value, backend: &str) -> Value {
  body["backend"] = json!(backend);
  body
}

/// Point the stack's `backends.routing` at a script that consumes the request and answers `answer` verbatim.
fn install_router(stack: &Stack, answer: &str) -> anyhow::Result<()> {
  let path = stack.cfg.charliebot_home.join("synthetic-router.sh");
  std::fs::write(&path, format!("set -eu\ncat >/dev/null\nprintf '%s' '{answer}'\n"))?;
  let mut cfg = (*stack.cfg).clone();
  cfg.backends.routing =
    Some(charliebot_core::config::RoutingConfig { command: vec!["/bin/sh".to_string(), path.display().to_string()] });
  stack.replace_config(cfg)
}

/// A string field of a response body.
fn text(body: &Value, key: &str) -> anyhow::Result<String> {
  Ok(body[key].as_str().ok_or_else(|| anyhow::anyhow!("the body names no {key}: {body}"))?.to_string())
}

/// The stored session `session_id`.
async fn session_of(stack: &Stack, session_id: &str) -> anyhow::Result<SessionMetadata> {
  Ok(stack.store().get_session(session_id).await?.expect("the session exists"))
}

/// The stored run `run_id` of `session_id`.
fn run_of(stack: &Stack, session_id: &str, run_id: &str) -> anyhow::Result<RunRecord> {
  Ok(stack.store().read_run_sync(session_id, run_id)?.expect("the run is recorded"))
}

/// Wait until the run's process exists, and track it so the guard kills it when the test ends.
async fn track_launch(stack: &Stack, guard: &ProcessGroupGuard, session_id: &str, run_id: &str) -> anyhow::Result<()> {
  settle_until(|| Ok(stack.store().read_run_sync(session_id, run_id)?.is_some_and(|run| run.pid.is_some()))).await?;
  let pid = run_of(stack, session_id, run_id)?.pid.expect("the launch records its pid");
  guard.track(pid);
  Ok(())
}

/// A launched run and the child it runs under name `backend`; the launched process is tracked.
async fn assert_launched_on(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  response: &Value,
  backend: &str,
) -> anyhow::Result<()> {
  assert_first_run(stack, guard, response, backend, backend).await
}

/// A launched run routed through the pool: the child keeps the pool id and the first run launched on `member`, and the
/// parent's delegation event for that run names the pool.
async fn assert_launched_in_pool(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  response: &Value,
  member: &str,
) -> anyhow::Result<()> {
  assert_first_run(stack, guard, response, ROTATION_POOL_ID, member).await?;
  let (parent, run_id) = (text(response, PARENT_SESSION_ID)?, text(response, "run_id")?);
  let delegated = stack
    .run_events(&parent)?
    .into_iter()
    .find(|event| {
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::TASK_DELEGATED)
        && event.get("thread_id").and_then(charliebot_store::compact::CompactValue::as_str) == Some(run_id.as_str())
    })
    .expect("the parent holds the delegation event of the launched run");
  assert_eq!(
    delegated.get(event_types::POOL).and_then(charliebot_store::compact::CompactValue::as_str),
    Some(ROTATION_POOL_ID),
    "the delegation event names the pool"
  );
  Ok(())
}

/// The launched process is tracked; the child record names `child_backend` and the first run `run_backend`.
async fn assert_first_run(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  response: &Value,
  child_backend: &str,
  run_backend: &str,
) -> anyhow::Result<()> {
  let (session_id, run_id) = (text(response, "session_id")?, text(response, "run_id")?);
  track_launch(stack, guard, &session_id, &run_id).await?;
  assert_eq!(session_of(stack, &session_id).await?.backend, child_backend, "the child record");
  assert_eq!(run_of(stack, &session_id, &run_id)?.backend.as_deref(), Some(run_backend), "the first run");
  Ok(())
}

/// The run a verify delegation launched, once it settles.
async fn settled_verify_run(stack: &Stack, response: &Value) -> anyhow::Result<RunRecord> {
  let (session_id, run_id) = (text(response, "session_id")?, text(response, "run_id")?);
  stack.wait_terminal(&session_id, &run_id).await?;
  run_of(stack, &session_id, &run_id)
}

/// Wait until every run of `session_id` has a terminal fact.
async fn settle_runs(stack: &Stack, session_id: &str) -> anyhow::Result<()> {
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

/// An `Authorization` header carrying a run token for `run_id`, signed with the synthetic access key.
fn run_token_header(session_id: &str, run_id: &str, agent: &str) -> anyhow::Result<String> {
  let claims =
    RunTokenClaims { session_id: session_id.to_string(), run_id: run_id.to_string(), agent: agent.to_string() };
  Ok(format!("Bearer {}", run_token::sign_run_token(&claims, "synthetic-access-key")?))
}

/// A child manager with no user message of its own delegates through an authorized ancestor. One leaf and one run are
/// created, and a replay returns the same child and run without a second process.
#[tokio::test]
#[ignore = "integration"]
async fn authorized_ancestor_delegates_without_a_local_user_message() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![result_script("leaf done")]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &root.id, "Take off. Ship the feature.").await?;
  let router = routes::routes(&stack.features);
  let payload = delegate_body(&child.id, &repo, "quick-edit");

  let (status, first) = post_delegate(&router, payload.clone()).await?;
  assert_eq!(status, StatusCode::OK, "{first}");
  let (leaf, run_id) = (text(&first, "session_id")?, text(&first, "run_id")?);
  assert_eq!(first["parent_session_id"], child.id.as_str());
  let leaf_meta = session_of(&stack, &leaf).await?;
  assert_eq!(leaf_meta.profile, TaskProfile::Worker);
  assert_eq!(leaf_meta.task_parent_id.as_deref(), Some(child.id.as_str()));
  let runs: Vec<String> = stack.run_records(&leaf)?.into_iter().map(|run| run.id).collect();
  assert_eq!(runs, vec![run_id.clone()]);

  // The replayed request returns the original product, never a sibling.
  let (replay_status, replay) = post_delegate(&router, payload).await?;
  assert_eq!(replay_status, StatusCode::OK, "{replay}");
  assert_eq!(replay["session_id"], leaf.as_str());
  assert_eq!(replay["run_id"], run_id.as_str());

  stack.wait_terminal(&leaf, &run_id).await?;
  assert_eq!(stack.store().child_ids(&child.id)?, vec![leaf], "one leaf for the operation and its replay");
  assert_eq!(scripts.builds().len(), 1, "one process for the operation and its replay");
  Ok(())
}

/// A question is a real user message, so the root and the child each delegate an implement worker under their own
/// question.
#[tokio::test]
#[ignore = "integration"]
async fn user_questions_admit_root_and_child_delegations() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new((0..4).map(|n| result_script(&format!("done {n}"))).collect());
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &root.id, "Can you fix the flaky test?").await?;
  user_message(&stack, &child.id, "Which file holds it?").await?;
  let router = routes::routes(&stack.features);

  for caller in [&root.id, &child.id] {
    let (status, body) = post_delegate(&router, delegate_body(caller, &repo, "implement")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let leaf = session_of(&stack, &text(&body, "session_id")?).await?;
    assert_eq!(leaf.profile, TaskProfile::Worker);
    assert_eq!(leaf.task_parent_id.as_deref(), Some(caller.as_str()));
    settle_runs(&stack, &leaf.id).await?;
  }
  Ok(())
}

/// A root manager with no user message (shaped like a cron-bound session) delegates a worker child.
#[tokio::test]
#[ignore = "integration"]
async fn root_manager_without_user_message_delegates_worker() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![result_script("leaf done")]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  stack.queue_run(&root.id, "root-run", RunKind::ManagerTurn).await?;
  stack.launch(&root.id, "root-run", 424_242, "ps-root").await?;
  stack
    .store()
    .append_chat_event(
      &root.id,
      json!({
        "type": event_types::SCHEDULED_TRIGGER,
        "actor": "system",
        "source_session_id": root.id,
        "content": "scheduled tick",
      }),
    )
    .await?;
  let auth = run_token_header(&root.id, "root-run", "manager-agent")?;
  let guard = ProcessGroupGuard::new();
  let (status, body) =
    post_delegate_as(&routes::routes(&stack.features), delegate_body(&root.id, &repo, "quick-edit"), &auth).await?;
  assert_eq!(status, StatusCode::OK, "{body}");
  let (leaf, run_id) = (text(&body, "session_id")?, text(&body, "run_id")?);
  track_launch(&stack, &guard, &leaf, &run_id).await?;
  assert_eq!(session_of(&stack, &leaf).await?.profile, TaskProfile::Worker);
  stack.wait_terminal(&leaf, &run_id).await?;
  Ok(())
}

/// The child holds its own user message, but the archived root closes the line. The route refuses and nothing is
/// created.
#[tokio::test]
#[ignore = "integration"]
async fn closed_ancestor_refuses_delegation() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &child.id, "Can you fix it?").await?;
  // The root's archive close fact, which closes the task the gate reads (`archived_close_event`).
  let guard = stack.store().control_lock().await;
  let close = control_events::archived_close_event(
    &root.id,
    "close-root",
    "archived by the user",
    None,
    None,
    None,
    control_events::ACTOR_USER,
    &[],
    &[],
  );
  stack.store().append_control_fact(&guard, &root.id, close).await?;
  drop(guard);

  let (status, body) =
    post_delegate(&routes::routes(&stack.features), delegate_body(&child.id, &repo, "quick-edit")).await?;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert!(text(&body, "detail")?.contains(&root.id), "the refusal names the closed ancestor: {body}");
  assert!(scripts.builds().is_empty());
  assert!(stack.store().child_ids(&child.id)?.is_empty());
  Ok(())
}

/// Agents run only under a manager, so a worker cannot delegate.
#[tokio::test]
#[ignore = "integration"]
async fn worker_caller_cannot_delegate() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  let worker = node(&stack, "w", Some(&child.id), TaskProfile::Worker, BACKEND_ID).await?;
  user_message(&stack, &root.id, "Take off. Ship the feature.").await?;

  let (status, body) =
    post_delegate(&routes::routes(&stack.features), delegate_body(&worker.id, &repo, "quick-edit")).await?;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert!(text(&body, "detail")?.contains(&worker.id), "the refusal names the worker: {body}");
  assert!(scripts.builds().is_empty());
  Ok(())
}

/// A run token bound to one node cannot create a worker under a different node.
#[tokio::test]
#[ignore = "integration"]
async fn foreign_run_token_cannot_delegate() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &root.id, "Take off. Ship the feature.").await?;
  let agents_child = node(&stack, "agent-leaf", Some(&child.id), TaskProfile::Worker, BACKEND_ID).await?;
  stack.queue_run(&agents_child.id, "agent-run", RunKind::Work).await?;
  stack.launch(&agents_child.id, "agent-run", 424_242, "ps-agent-run").await?;
  let auth = run_token_header(&agents_child.id, "agent-run", "worker")?;

  let (status, body) =
    post_delegate_as(&routes::routes(&stack.features), delegate_body(&child.id, &repo, "quick-edit"), &auth).await?;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert_eq!(stack.store().child_ids(&child.id)?, vec![agents_child.id.clone()], "no worker was created");
  assert!(scripts.builds().is_empty());
  Ok(())
}

/// An open manager without a user message launches both verify and repo tasks, and closing the root blocks both.
#[tokio::test]
#[ignore = "integration"]
async fn open_manager_launches_verify_and_repo_without_user_message_and_closed_root_blocks_both() -> anyhow::Result<()>
{
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![result_script("verdict: no"), result_script("leaf done")]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  let router = routes::routes(&stack.features);

  let (status, verify) = post_delegate(&router, verify_body(&child.id)).await?;
  assert_eq!(status, StatusCode::OK, "{verify}");
  let run = settled_verify_run(&stack, &verify).await?;
  assert_eq!(
    run_records::terminal_outcome(&stack.run_events(&run.session_id)?, &run.id)?,
    Some(RunOutcomeKind::Success)
  );

  let (status, leaf) = post_delegate(&router, delegate_body(&child.id, &repo, "quick-edit")).await?;
  assert_eq!(status, StatusCode::OK, "{leaf}");
  let leaf_id = text(&leaf, "session_id")?;
  settle_runs(&stack, &leaf_id).await?;
  assert_eq!(scripts.builds().len(), 2);

  let guard = stack.store().control_lock().await;
  let close = control_events::build_control_event(
    "task_closed",
    control_events::ACTOR_SYSTEM,
    &root.id,
    None,
    None,
    [("outcome".to_string(), json!("completed"))],
  );
  stack.store().append_control_fact(&guard, &root.id, close).await?;
  drop(guard);

  let (blocked_verify, _) = post_delegate(&router, verify_body(&child.id)).await?;
  assert_eq!(blocked_verify, StatusCode::FORBIDDEN);
  let (blocked_repo, _) = post_delegate(&router, delegate_body(&child.id, &repo, "quick-edit")).await?;
  assert_eq!(blocked_repo, StatusCode::FORBIDDEN);
  assert_eq!(stack.store().child_ids(&child.id)?, vec![text(&verify, "session_id")?, leaf_id]);
  Ok(())
}

/// The CLI's run token delegates a read-only verify with no user instruction anywhere, and the repo-less child runs to
/// a result.
#[tokio::test]
#[ignore = "integration"]
async fn agent_run_token_delegates_verify_without_a_takeoff() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  stack.queue_run(&child.id, "child-run", RunKind::ManagerTurn).await?;
  stack.launch(&child.id, "child-run", 424_242, "ps-child-run").await?;
  let auth = run_token_header(&child.id, "child-run", "manager-agent")?;

  let (status, verify) = post_delegate_as(&routes::routes(&stack.features), verify_body(&child.id), &auth).await?;
  assert_eq!(status, StatusCode::OK, "{verify}");
  let leaf = session_of(&stack, &text(&verify, "session_id")?).await?;
  assert_eq!(leaf.profile, TaskProfile::Worker);
  assert_eq!(leaf.task_parent_id.as_deref(), Some(child.id.as_str()));
  let task = leaf.task.expect("the verify child carries its task");
  assert_eq!(task.task_type, Some(TaskType::Verify));
  assert!(task.repo_path.is_none());
  let run = settled_verify_run(&stack, &verify).await?;
  assert_eq!(run_records::terminal_outcome(&stack.run_events(&leaf.id)?, &run.id)?, Some(RunOutcomeKind::Success));
  assert_eq!(scripts.builds().len(), 1);
  Ok(())
}

/// A closed manager delegates a repo task. The gate refuses, and no child or delegation fact is written.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_task_returns_403_when_manager_is_closed() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  let guard = stack.store().control_lock().await;
  let close = control_events::build_control_event(
    "task_closed",
    control_events::ACTOR_SYSTEM,
    &manager.id,
    None,
    None,
    [("outcome".to_string(), json!("completed"))],
  );
  stack.store().append_control_fact(&guard, &manager.id, close).await?;
  drop(guard);

  let (status, body) =
    post_delegate(&routes::routes(&stack.features), delegate_body(&manager.id, &repo, "implement")).await?;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert!(stack.store().child_ids(&manager.id)?.is_empty());
  let delegated = stack.run_events(&manager.id)?.iter().any(|event| {
    event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::TASK_DELEGATED)
  });
  assert!(!delegated, "no delegation fact was written");
  assert!(scripts.builds().is_empty());
  Ok(())
}

/// A verify delegation names no repository, and the schema gate refuses one before the session is read.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_task_verify_rejects_repo_path() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  let body = json!({
    "session_id": manager.id,
    "description": "check",
    "task_type": "verify",
    "repo_path": repo.display().to_string(),
    "base_branch": null,
  });

  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert!(text(&response, "detail")?.contains("repo_path"), "{response}");
  assert!(stack.store().child_ids(&manager.id)?.is_empty());
  Ok(())
}

/// Each repo-scoped type takes `repo_path` and `base_branch` together, so one of them alone is refused. Covers the six
/// parametrized cases (three types, two halves).
#[tokio::test]
#[ignore = "integration"]
async fn repo_scoped_types_take_repo_and_base_together() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  let router = routes::routes(&stack.features);

  for task_type in ["implement", "quick-edit", "script-run"] {
    for half in ["repo", "branch"] {
      let body = json!({
        "session_id": manager.id,
        "description": "do it",
        "task_type": task_type,
        "repo_path": (half == "repo").then(|| repo.display().to_string()),
        "base_branch": (half == "branch").then_some("main"),
      });
      let (status, response) = post_delegate(&router, body).await?;
      assert_eq!(status, StatusCode::BAD_REQUEST, "{task_type} with only the {half}: {response}");
      assert!(text(&response, "detail")?.contains("together"), "{task_type} with only the {half}: {response}");
    }
  }
  assert!(stack.store().child_ids(&manager.id)?.is_empty());
  Ok(())
}

/// Omitting `repo_path` and `base_branch` together is a valid repo-less delegation for each of the three types.
#[tokio::test]
#[ignore = "integration"]
async fn repo_less_request_passes_the_schema_gate() -> anyhow::Result<()> {
  let scripts = Scripts::new((0..3).map(|n| result_script(&format!("done {n}"))).collect());
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;
  let router = routes::routes(&stack.features);

  for task_type in ["implement", "quick-edit", "script-run"] {
    let body = json!({
      "session_id": manager.id,
      "description": "do it",
      "task_type": task_type,
      "repo_path": null,
      "base_branch": null,
    });
    let (status, response) = post_delegate(&router, body).await?;
    assert_eq!(status, StatusCode::OK, "{task_type}: {response}");
    let child = session_of(&stack, &text(&response, "session_id")?).await?;
    assert!(child.task.as_ref().is_some_and(|task| task.repo_path.is_none()), "{task_type} is repo-less");
    stack.wait_terminal(&child.id, &text(&response, "run_id")?).await?;
  }
  Ok(())
}

/// A backend the configuration does not name is refused, and nothing is created.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_task_returns_400_for_invalid_backend() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let body = with_backend(delegate_body(&manager.id, &repo, "implement"), "codex-o3");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert!(text(&response, "detail")?.contains("codex-o3"), "{response}");
  assert!(stack.store().child_ids(&manager.id)?.is_empty());
  Ok(())
}

/// A verify without a backend takes the first pool member that differs from the session's backend.
#[tokio::test]
#[ignore = "integration"]
async fn verify_no_backend_defaults_to_first_differing_preference() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("codex-o3"));
  Ok(())
}

/// A session backend outside the pool leaves the first pool member.
#[tokio::test]
#[ignore = "integration"]
async fn verify_no_backend_session_backend_not_in_preference_uses_first_entry() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "kimi-k2.5").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("opus"));
  Ok(())
}

/// With no pool, a verify keeps the session's backend.
#[tokio::test]
#[ignore = "integration"]
async fn verify_no_backend_empty_preference_keeps_session_backend() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &[]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "codex-o3").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("codex-o3"));
  Ok(())
}

/// A named backend wins over the pool's preference.
#[tokio::test]
#[ignore = "integration"]
async fn verify_explicit_backend_wins_over_preference() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let body = with_backend(verify_body(&manager.id), "kimi-k2.5");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("kimi-k2.5"));
  Ok(())
}

/// A named backend the configuration does not hold is refused.
#[tokio::test]
#[ignore = "integration"]
async fn verify_unknown_explicit_backend_returns_400() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let body = with_backend(verify_body(&manager.id), "nonexistent");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert_eq!(
    text(&response, "detail")?,
    "backend 'nonexistent' is not a backends.options or backends.pools id; valid ids: ['codex-o3', 'kimi-k2.5', \
     'opus', 'pool']"
  );
  assert!(scripts.builds().is_empty());
  Ok(())
}

/// The requested backend stays on the child record, its first work run, and the next work run the dispatcher
/// reserves.
/// The review half (a review on a pool member other than the work run's backend) is not covered: the review spawn
/// is private to the task execution adapter.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_child_and_its_later_runs_keep_the_requested_backend() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string(), HANG.to_string()]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let body = with_backend(delegate_body(&manager.id, &repo, "implement"), "codex-o3");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  let child = text(&response, "session_id")?;
  let first_id = text(&response, "run_id")?;
  assert_launched_on(&stack, &guard, &response, "codex-o3").await?;

  // The first work run settled; the parent's message reserves the next one.
  stack.record_terminal(&child, &first_id, RunOutcomeKind::Success).await?;
  let message = json!({"from_session": manager.id});
  stack.dispatch.admit_input(&child, event_types::AGENT_MESSAGE, "one more tweak", "agent", message).await?;
  let decision = stack.dispatch.dispatch_pending(&child).await?;
  assert_eq!(decision["launch"], true);
  let follow_id = text(&decision, "run_id")?;
  track_launch(&stack, &guard, &child, &follow_id).await?;
  let follow = run_of(&stack, &child, &follow_id)?;
  assert_eq!(follow.kind, RunKind::Work);
  assert_eq!(follow.backend.as_deref(), Some("codex-o3"));
  Ok(())
}

/// A verify without a backend takes the cross-model member; the child record keeps the pool it was routed through.
#[tokio::test]
#[ignore = "integration"]
async fn verify_delegate_child_records_the_cross_model_backend() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  let first = settled_verify_run(&stack, &response).await?;
  assert_eq!(first.backend.as_deref(), Some("codex-o3"), "the first run");
  assert_eq!(session_of(&stack, &text(&response, "session_id")?).await?.backend, ROTATION_POOL_ID, "the child record");
  Ok(())
}

/// An implement without a backend takes the pool, which here holds the parent's backend. The child keeps the pool id,
/// and the first run records the member.
#[tokio::test]
#[ignore = "integration"]
async fn implement_delegate_without_backend_resolves_to_the_parent_backend() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let (status, response) =
    post_delegate(&routes::routes(&stack.features), delegate_body(&manager.id, &repo, "implement")).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_launched_in_pool(&stack, &guard, &response, "opus").await?;
  Ok(())
}

/// An implement that names no backend takes the first pool member, a named backend is never pooled, and a verify takes
/// the next member other than the session's own.
#[tokio::test]
#[ignore = "integration"]
async fn pool_runs_unnamed_workers_and_leaves_named_and_verify_routing() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string(), HANG.to_string(), HANG.to_string()]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3", "kimi-k2.5"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;
  let router = routes::routes(&stack.features);

  let (status, unnamed) = post_delegate(&router, delegate_body(&manager.id, &repo, "implement")).await?;
  assert_eq!(status, StatusCode::OK, "{unnamed}");
  assert_launched_in_pool(&stack, &guard, &unnamed, "opus").await?;

  let named = with_backend(delegate_body(&manager.id, &repo, "quick-edit"), "kimi-k2.5");
  let (status, named) = post_delegate(&router, named).await?;
  assert_eq!(status, StatusCode::OK, "{named}");
  assert_launched_on(&stack, &guard, &named, "kimi-k2.5").await?;

  let (status, verify) = post_delegate(&router, verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{verify}");
  assert_launched_in_pool(&stack, &guard, &verify, "codex-o3").await?;
  Ok(())
}

/// An unpinned implement asks the router when `backends.routing` is set, runs on the backend the router names, and keeps
/// the route and its reason on the child record. Without routing, the same delegation takes the pool.
#[tokio::test]
#[ignore = "integration"]
async fn an_unpinned_implement_takes_the_routers_answer() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  install_router(&stack, r#"{"routed":true,"backends":["kimi-k2.5"],"reason":"context p=0.9"}"#)?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let (status, response) =
    post_delegate(&routes::routes(&stack.features), delegate_body(&manager.id, &repo, "implement")).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_launched_on(&stack, &guard, &response, "kimi-k2.5").await?;
  let child = session_of(&stack, &text(&response, "session_id")?).await?;
  assert_eq!(
    child.extra("route"),
    Some(&json!({"backends": ["kimi-k2.5"], "reason": "context p=0.9"})),
    "the child record keeps the route"
  );
  Ok(())
}

/// A manager with no user message delegates a cold read. The open manager chain admits it, the child is named after
/// the page file, and its first run goes to the first pool member.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_delegation_from_a_manager_succeeds_without_takeoff() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &["opus", "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = node(&stack, "root", None, TaskProfile::Manager, "opus").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), cold_read_body(&manager.id, &page)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  let child = session_of(&stack, &text(&response, "session_id")?).await?;
  assert_eq!(child.task_parent_id.as_deref(), Some(manager.id.as_str()));
  assert_eq!(child.profile, TaskProfile::Worker);
  assert_eq!(child.name, format!("Cold read: {}", page.file_name().unwrap().to_string_lossy()));
  let task = child.task.clone().expect("the cold-read child carries its task");
  assert_eq!(task.task_type, Some(TaskType::ColdRead));
  assert_eq!(
    task.cold_read,
    Some(ColdReadSpec {
      page_path: page.display().to_string(),
      genre: "sitrep".to_string(),
      trigger: "where are we?".to_string(),
    })
  );
  assert!(task.repo_path.is_none() && task.base_branch.is_none());
  assert_launched_in_pool(&stack, &guard, &response, "alpha").await?;
  Ok(())
}

/// A cold-read delegation that names a repository or a base branch is refused with 400, and no child is written.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_delegation_with_a_repo_field_gets_400() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &["opus", "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = node(&stack, "root", None, TaskProfile::Manager, "opus").await?;
  let router = routes::routes(&stack.features);

  for (field, value) in [("repo_path", json!("/synthetic/repo")), ("base_branch", json!("main"))] {
    let mut body = cold_read_body(&manager.id, &page);
    body[field] = value;
    let (status, response) = post_delegate(&router, body).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["detail"], format!("cold-read delegations are repo-less; omit {field}"));
  }
  assert!(stack.store().child_ids(&manager.id)?.is_empty());
  Ok(())
}

/// A cold-read delegation without its `cold_read` spec is refused with 400.
#[tokio::test]
#[ignore = "integration"]
async fn cold_read_delegation_requires_cold_read() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &["opus", "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = node(&stack, "root", None, TaskProfile::Manager, "opus").await?;
  let mut body = cold_read_body(&manager.id, &page);
  body["cold_read"] = Value::Null;

  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert_eq!(response["detail"], "cold-read delegations require cold_read");
  Ok(())
}

/// A verify delegation that carries a `cold_read` spec is refused with 400.
#[tokio::test]
#[ignore = "integration"]
async fn other_task_types_reject_cold_read() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let page = write_sitrep(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &["opus", "alpha", "beta"], &["alpha", "beta"]).await?;
  let manager = node(&stack, "root", None, TaskProfile::Manager, "opus").await?;
  let mut body = cold_read_body(&manager.id, &page);
  body["task_type"] = json!("verify");

  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert_eq!(response["detail"], "verify delegations reject cold_read");
  Ok(())
}

/// One relay from `from` to `to` carrying `content`, as `charliebot session send` posts it.
async fn relay(stack: &Stack, from: &str, to: &str, content: &str) -> anyhow::Result<(StatusCode, Value)> {
  let body = json!({"session_id": from, "target_session_id": to, "content": content});
  post_internal(&routes::routes(&stack.features), "/api/internal/session-message", body, "Bearer synthetic-access-key")
    .await
}

/// The `agent_message` events admitted into `session_id`, in order.
fn admitted_messages(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .run_events(session_id)?
      .iter()
      .map(|event| event.to_value())
      .filter(|event| event["type"] == event_types::AGENT_MESSAGE)
      .collect(),
  )
}

/// A relay to an archived task node is refused with 409 and the archived sentence, and the node stays archived.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_refuses_an_archived_task_node() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;
  stack
    .completion
    .archive_subtree(
      &target.id,
      Some("synthetic-delegate-archive"),
      "archived by the user",
      &[],
      &[],
      &ArchiveAuthority::Operator,
    )
    .await?;

  let (status, body) = relay(&stack, &caller.id, &target.id, "status please").await?;
  assert_eq!(status, StatusCode::CONFLICT, "{body}");
  assert_eq!(body["detail"], format!("task {} is archived", target.id));
  assert_eq!(stack.store().task_state(&target.id)?, "archived");
  Ok(())
}

/// A relay admits one `agent_message` into the target, carrying the sender's identity, and dispatch starts the
/// target's turn with it.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_relay_admits_and_dispatches_a_task_input() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![result_script("status received")]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;

  let (status, body) = relay(&stack, &caller.id, &target.id, "status please").await?;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body, json!({"status": "accepted"}));
  let admitted = admitted_messages(&stack, &target.id)?;
  assert_eq!(admitted.len(), 1);
  assert_eq!(admitted[0]["content"], "status please");
  assert_eq!(admitted[0]["from_session"], caller.id);
  assert_eq!(admitted[0]["from_session_name"], caller.name);
  settle_runs(&stack, &target.id).await?;
  let input_id = text(&admitted[0], "id")?;
  let turns =
    stack.run_records(&target.id)?.into_iter().filter(|run| run.kind == RunKind::ManagerTurn).collect::<Vec<_>>();
  assert!(turns.iter().any(|run| run.input_event_ids.contains(&input_id)), "a turn claims the relayed input");
  Ok(())
}

/// A field the request model does not name is refused with 422.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_request_rejects_extra_fields() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;
  let body = json!({
    "session_id": caller.id,
    "target_session_id": target.id,
    "content": "status please",
    "surprise": "field",
  });

  let (status, response) = post_internal(
    &routes::routes(&stack.features),
    "/api/internal/session-message",
    body,
    "Bearer synthetic-access-key",
  )
  .await?;
  assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
  assert!(admitted_messages(&stack, &target.id)?.is_empty());
  Ok(())
}

/// 1000 CJK ideographs weigh exactly the limit, so the relay is accepted and admits one `agent_message`.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_accepts_a_message_at_the_weighted_limit() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;
  let content = "一".repeat(1000);

  let (status, body) = relay(&stack, &caller.id, &target.id, &content).await?;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body, json!({"status": "accepted"}));
  let admitted = admitted_messages(&stack, &target.id)?;
  assert_eq!(admitted.len(), 1);
  assert_eq!(admitted[0]["content"], content.as_str());
  Ok(())
}

/// 1001 CJK ideographs weigh 2002, so the relay answers 422 with the measured length and the limit, and the target
/// gains no input.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_refuses_a_message_over_the_weighted_limit() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;

  let (status, body) = relay(&stack, &caller.id, &target.id, &"一".repeat(1001)).await?;
  assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
  assert_eq!(
    body["detail"],
    format!(
      "Message is 2002 weighted characters; the limit is {MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n\
       CJK characters, CJK punctuation, and fullwidth characters count 2.\n\
       Write the long content to a file. Send a short message with the conclusion,\n\
       the request, and the file path."
    )
  );
  assert!(admitted_messages(&stack, &target.id)?.is_empty());
  assert!(stack.dispatch.pending_inputs(&target.id)?.is_empty());
  Ok(())
}

/// 2001 ASCII characters weigh 2001, so the relay answers 422 and writes nothing to the target.
#[tokio::test]
#[ignore = "integration"]
async fn session_message_refuses_an_over_limit_ascii_message() -> anyhow::Result<()> {
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;

  let (status, body) = relay(&stack, &caller.id, &target.id, &"a".repeat(2001)).await?;
  assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
  assert!(text(&body, "detail")?.contains("2001"), "{body}");
  assert!(admitted_messages(&stack, &target.id)?.is_empty());
  Ok(())
}

/// Replaying a closed child that has no work run returns 409 Conflict naming the closed child and `--request-id`,
/// and launches nothing.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_closed_child_returns_409_and_launches_nothing() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let closed_child = stack
    .store()
    .create_task(
      CreateTask {
        request_id: "closed-child-req".to_string(),
        task_parent_id: Some(manager.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(TaskSpec {
          goal: "## Goal\n\nfix the thing\n".to_string(),
          repo_path: Some(repo.display().to_string()),
          base_branch: Some("main".to_string()),
          task_type: Some(TaskType::Implement),
          keep_worktree: false,
          cold_read: None,
          approver: TaskApprover::User,
          improve_host: None,
        }),
        name: None,
        backend: Some(BACKEND_ID.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let lock = stack.store().control_lock().await;
  let close = control_events::build_control_event(
    "task_closed",
    control_events::ACTOR_USER,
    &closed_child.id,
    None,
    None,
    [("outcome".to_string(), json!("cancelled")), ("summary".to_string(), json!("cancelled before run"))],
  );
  stack.store().append_control_fact(&lock, &closed_child.id, close).await?;
  drop(lock);

  let router = routes::routes(&stack.features);
  let mut body = delegate_body(&manager.id, &repo, "implement");
  body["request_id"] = json!("closed-child-req");
  let (status, response) = post_delegate(&router, body).await?;
  assert_eq!(status, StatusCode::CONFLICT, "{response}");
  let detail = text(&response, "detail")?;
  assert!(detail.contains(&closed_child.id), "detail should name closed child: {detail}");
  assert!(detail.contains("--request-id"), "detail should mention --request-id: {detail}");
  assert!(scripts.builds().is_empty());
  assert!(stack.run_records(&closed_child.id)?.is_empty());

  // Passing a fresh request_id delegates a new worker normally.
  let mut fresh = delegate_body(&manager.id, &repo, "implement");
  fresh["request_id"] = json!("fresh-req");
  let (fresh_status, fresh_response) = post_delegate(&router, fresh).await?;
  assert_eq!(fresh_status, StatusCode::OK, "{fresh_response}");
  assert_launched_in_pool(&stack, &guard, &fresh_response, BACKEND_ID).await?;
  Ok(())
}

/// Dropping the HTTP client connection while `delegate_task` is mid-route does not cancel the spawned delegation:
/// child creation, work-run registration, process launch, and `task_delegated` still complete.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_task_completes_when_client_disconnects_mid_route() -> anyhow::Result<()> {
  use tokio::io::AsyncWriteExt;

  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;

  let started = dir.path().join("route_started");
  let release = dir.path().join("route_release");
  let router_script = stack.cfg.charliebot_home.join("barrier-router.sh");
  std::fs::write(
    &router_script,
    format!(
      "set -eu\ncat >/dev/null\ntouch '{started}'\ni=0\nwhile [ ! -f '{release}' ] && [ $i -lt 500 ]; do\n  sleep 0.01\n  i=$((i + 1))\ndone\nprintf '%s' '{{\"routed\":true,\"backends\":[\"kimi-k2.5\"],\"reason\":\"detached route\"}}'\n",
      started = started.display(),
      release = release.display(),
    ),
  )?;
  let mut cfg = (*stack.cfg).clone();
  cfg.backends.routing = Some(charliebot_core::config::RoutingConfig {
    command: vec!["/bin/sh".to_string(), router_script.display().to_string()],
  });
  stack.replace_config(cfg)?;

  let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
  let addr = listener.local_addr()?;
  let router = routes::routes(&stack.features);
  let server = tokio::spawn(async move {
    let _ = axum::serve(listener, router).await;
  });

  let mut body = delegate_body(&manager.id, &repo, "implement");
  body["request_id"] = json!("disconnect-mid-route");
  let payload = serde_json::to_vec(&body)?;
  let http_request = format!(
    "POST /api/internal/delegate HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer synthetic-access-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    payload.len()
  );
  let mut stream = tokio::net::TcpStream::connect(addr).await?;
  stream.write_all(http_request.as_bytes()).await?;
  stream.write_all(&payload).await?;
  stream.flush().await?;

  settle_until(|| Ok(started.exists())).await?;
  drop(stream);
  std::fs::write(&release, b"go")?;

  let child_id = control_events::stable_task_id(Some(&manager.id), "disconnect-mid-route");
  let run_id = control_events::stable_run_id(&child_id, "disconnect-mid-route:work");
  track_launch(&stack, &guard, &child_id, &run_id).await?;
  assert_eq!(session_of(&stack, &child_id).await?.backend, "kimi-k2.5");
  assert_eq!(run_of(&stack, &child_id, &run_id)?.backend.as_deref(), Some("kimi-k2.5"));
  settle_until(|| {
    Ok(stack.run_events(&manager.id)?.iter().any(|event| {
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::TASK_DELEGATED)
        && event.get("thread_id").and_then(charliebot_store::compact::CompactValue::as_str) == Some(run_id.as_str())
    }))
  })
  .await?;

  server.abort();
  Ok(())
}

/// Dropping the request future while `delegate_task_tree` awaits `control_lock` still completes child creation,
/// work-run registration, and launch once the lock is released, without exposing an intermediate child without a run.
#[tokio::test]
#[ignore = "integration"]
async fn delegate_task_completes_when_future_dropped_under_control_lock() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![HANG.to_string()]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &manager.id, "Take off. Ship the feature.").await?;
  let router = routes::routes(&stack.features);

  let mut body = delegate_body(&manager.id, &repo, "implement");
  body["request_id"] = json!("drop-under-lock");
  let lock = stack.store().control_lock().await;
  let timed_out = tokio::time::timeout(std::time::Duration::from_millis(50), post_delegate(&router, body)).await;
  assert!(timed_out.is_err(), "request must block on control_lock until dropped");
  assert!(stack.store().child_ids(&manager.id)?.is_empty(), "no child exists before delegate acquires control_lock");
  drop(lock);

  let child_id = control_events::stable_task_id(Some(&manager.id), "drop-under-lock");
  let run_id = control_events::stable_run_id(&child_id, "drop-under-lock:work");
  track_launch(&stack, &guard, &child_id, &run_id).await?;
  settle_until(|| {
    Ok(stack.run_events(&manager.id)?.iter().any(|event| {
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::TASK_DELEGATED)
        && event.get("thread_id").and_then(charliebot_store::compact::CompactValue::as_str) == Some(run_id.as_str())
    }))
  })
  .await?;
  Ok(())
}
