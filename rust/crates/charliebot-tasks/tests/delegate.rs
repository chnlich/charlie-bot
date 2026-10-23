//! The delegation route's gate, backend choice, and child record, ported from the Python delegate tests, and the
//! session-message relay that the same router serves. Each case drives `routes::routes` through `post_delegate` (or
//! `post_delegate_as` for a run token, `post_internal` for the relay) on a synthetic task tree.
//!
//! Python disarms the launch of a delegated run in some cases; the adapter here starts the process at once. Those cases
//! hold the process open with `HANG` and let the guard kill it, or run it to a terminal fact.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::path::Path;

use axum::http::StatusCode;
use charliebot_core::event_types;
use charliebot_core::run_token::{self, RunTokenClaims};
use charliebot_store::control_events;
use charliebot_store::metadata::{ColdReadSpec, SessionMetadata, TaskProfile, TaskSpec, TaskType};
use charliebot_store::run_records::{self, RunKind, RunOutcomeKind, RunRecord};
use charliebot_store::{CreateTask, TaskCaller};
use charliebot_tasks::prompts::MAX_AGENT_MESSAGE_WEIGHTED_CHARS;
use charliebot_tasks::routes;
use process_guard::ProcessGroupGuard;
use recovery_support::{
  BACKEND_ID, BACKEND_TYPE, Scripts, Stack, cold_read_body, init_repo_with_origin, post_delegate, post_delegate_as,
  post_internal, result_script, settle_until, write_sitrep,
};
use serde_json::{Map, Value, json};

/// The script a launched process runs until the guard kills it.
const HANG: &str = "sleep 30";

/// The backend options the pool cases route between: the first is the session's and the first pool member.
const OPTIONS: [&str; 3] = ["opus", "codex-o3", "kimi-k2.5"];

/// A stack whose backend options are `ids`, each serving `scripts` in launch order, with the operator's access key on
/// disk and `pool` as the backends.pool rotation.
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
  let task = TaskSpec { goal: format!("synthetic {request_id}"), ..TaskSpec::default() };
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
  let (session_id, run_id) = (text(response, "session_id")?, text(response, "run_id")?);
  track_launch(stack, guard, &session_id, &run_id).await?;
  assert_eq!(session_of(stack, &session_id).await?.backend, backend, "the child record");
  assert_eq!(run_of(stack, &session_id, &run_id)?.backend.as_deref(), Some(backend), "the first run");
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

/// `test_delegate_inherited_authorization.py::test_authorized_ancestor_delegates_without_a_local_user_message`: a
/// child manager with no user message of its own delegates through an authorized ancestor. One leaf and one run
/// are created, and a replay returns the same child and run without a second process.
// Python: tests/test_delegate_inherited_authorization.py::test_authorized_ancestor_delegates_without_a_local_user_message
#[tokio::test]
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

/// `test_delegate_inherited_authorization.py::test_user_questions_admit_root_and_child_delegations`: a question is a
/// real user message, so the root and the child each delegate an implement worker under their own question.
#[tokio::test]
async fn user_questions_admit_root_and_child_delegations() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_user_questions_admit_root_and_child_delegations.
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

/// `test_task_manager_authorization.py::test_summon_authorizes_a_root_manager_but_a_relayed_agent_message_does_not`: a
/// root manager whose log holds a recorded thread summon delegates, and a root whose only input is an agent message
/// relayed through the session-message route is refused.
#[tokio::test]
async fn summon_authorizes_a_root_manager_but_a_relayed_agent_message_does_not() -> anyhow::Result<()> {
  // Python: tests/test_task_manager_authorization.py:492.
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;

  // Refusal: the root's only user-facing input is a relay admitted by the ordinary relay route.
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let relay_caller = node(&stack, "relay-caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  stack.queue_run(&root.id, "root-run", RunKind::ManagerTurn).await?;
  stack.launch(&root.id, "root-run", 424_242, "ps-root").await?;
  let (status, body) = relay(&stack, &relay_caller.id, &root.id, "Take off. Ship the feature.").await?;
  assert_eq!(status, StatusCode::OK, "{body}");
  let auth = run_token_header(&root.id, "root-run", "manager-agent")?;
  let (status, body) =
    post_delegate_as(&routes::routes(&stack.features), delegate_body(&root.id, &repo, "quick-edit"), &auth).await?;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert!(text(&body, "detail")?.contains("no user request"), "{body}");
  assert!(scripts.builds().is_empty());

  // Admission: a fresh root whose only user-facing input is the event a Slack thread summon writes.
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
        "type": event_types::AGENT_MESSAGE,
        "actor": "agent",
        "source_session_id": root.id,
        "content": "Slack 线程召唤",
        "slack": {"channel_id": "C123", "thread_ts": "1700000000.000100", "mention_ts": "1700000000.000200"},
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

/// `test_delegate_inherited_authorization.py::test_closed_ancestor_refuses_delegation`: the child holds its own user
/// message, but the archived root closes the line. The route refuses and nothing is created.
#[tokio::test]
async fn closed_ancestor_refuses_delegation() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_closed_ancestor_refuses_delegation.
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let root = node(&stack, "root", None, TaskProfile::Manager, BACKEND_ID).await?;
  let child = node(&stack, "child", Some(&root.id), TaskProfile::Manager, BACKEND_ID).await?;
  user_message(&stack, &child.id, "Can you fix it?").await?;
  // The root's archive close fact, which closes the task the gate reads (`archived_close_event`).
  let guard = stack.store().control_lock().await;
  let close = control_events::build_control_event(
    event_types::TASK_CLOSED,
    control_events::ACTOR_USER,
    &root.id,
    Some(&control_events::stable_close_event_id(&root.id, "close-root")),
    None,
    [
      ("request_id".to_string(), json!("close-root")),
      ("outcome".to_string(), json!("archived")),
      ("summary".to_string(), json!("archived by the user")),
      ("result_refs".to_string(), json!([])),
      ("run_ids".to_string(), json!([])),
      ("report_to".to_string(), Value::Null),
    ],
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

/// `test_delegate_inherited_authorization.py::test_worker_caller_cannot_delegate`: agents run only under a manager, so
/// a worker cannot delegate.
#[tokio::test]
async fn worker_caller_cannot_delegate() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_worker_caller_cannot_delegate.
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

/// `test_delegate_inherited_authorization.py::test_foreign_run_token_cannot_delegate`: a run token bound to one node
/// cannot create a worker under a different node.
#[tokio::test]
async fn foreign_run_token_cannot_delegate() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_foreign_run_token_cannot_delegate.
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

/// `test_delegate_inherited_authorization.py::test_verify_exemption_on_the_v2_route_and_launch`: a verify delegation
/// needs no user message and launches on the route, while a repo task under the same tree stays blocked.
#[tokio::test]
async fn verify_exemption_on_the_v2_route_and_launch() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_verify_exemption_on_the_v2_route_and_launch.
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![result_script("verdict: no")]);
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

  let (blocked, _) = post_delegate(&router, delegate_body(&child.id, &repo, "quick-edit")).await?;
  assert_eq!(blocked, StatusCode::FORBIDDEN);
  let leaves = stack.store().child_ids(&child.id)?;
  assert_eq!(leaves, vec![text(&verify, "session_id")?], "the blocked repo task created nothing");
  assert_eq!(scripts.builds().len(), 1);
  Ok(())
}

/// `test_delegate_inherited_authorization.py::test_agent_run_token_delegates_verify_without_a_takeoff`: the CLI's run
/// token delegates a read-only verify with no user instruction anywhere, and the repo-less child runs to a result.
#[tokio::test]
async fn agent_run_token_delegates_verify_without_a_takeoff() -> anyhow::Result<()> {
  // Python: tests/test_delegate_inherited_authorization.py::test_agent_run_token_delegates_verify_without_a_takeoff.
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

/// `test_internal_delegate_takeoff.py::test_delegate_task_returns_403_when_takeoff_gate_blocks`: a manager with no user
/// message delegates a repo task. The gate refuses, and no child or delegation fact is written.
#[tokio::test]
async fn delegate_task_returns_403_when_takeoff_gate_blocks() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_task_returns_403_when_takeoff_gate_blocks.
  let dir = tempfile::tempdir()?;
  let (_origin, repo) = init_repo_with_origin(dir.path())?;
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, BACKEND_ID).await?;

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

/// `test_internal_delegate_takeoff.py::test_delegate_task_verify_rejects_repo_path`: a verify delegation names no
/// repository, and the schema gate refuses one before the session is read.
#[tokio::test]
async fn delegate_task_verify_rejects_repo_path() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_task_verify_rejects_repo_path.
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

/// `test_internal_delegate_takeoff.py::test_delegate_task_repo_scoped_types_take_repo_and_base_together`: each
/// repo-scoped type takes `repo_path` and `base_branch` together, so one of them alone is refused. Covers the six
/// parametrized cases (three types, two halves).
#[tokio::test]
async fn repo_scoped_types_take_repo_and_base_together() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_task_repo_scoped_types_take_repo_and_base_together.
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

/// `test_internal_delegate_takeoff.py::test_delegate_task_repo_less_request_passes_the_schema_gate`: omitting
/// `repo_path` and `base_branch` together is a valid repo-less delegation for each of the three types.
#[tokio::test]
async fn repo_less_request_passes_the_schema_gate() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_task_repo_less_request_passes_the_schema_gate.
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

/// `test_internal_delegate_takeoff.py::test_delegate_task_returns_400_for_invalid_backend`: a backend the configuration
/// does not name is refused, and nothing is created.
#[tokio::test]
async fn delegate_task_returns_400_for_invalid_backend() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_task_returns_400_for_invalid_backend.
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

/// `test_internal_delegate_takeoff.py::test_verify_no_backend_defaults_to_first_differing_preference`: a verify without
/// a backend takes the first pool member that differs from the session's backend.
#[tokio::test]
async fn verify_no_backend_defaults_to_first_differing_preference() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_no_backend_defaults_to_first_differing_preference.
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("codex-o3"));
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_verify_no_backend_session_backend_not_in_preference_uses_first_entry`: a
/// session backend outside the pool leaves the first pool member.
#[tokio::test]
async fn verify_no_backend_session_backend_not_in_preference_uses_first_entry() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_no_backend_session_backend_not_in_preference_uses_first_entry
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "kimi-k2.5").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("opus"));
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_verify_no_backend_empty_preference_keeps_session_backend`: with no pool, a
/// verify keeps the session's backend.
#[tokio::test]
async fn verify_no_backend_empty_preference_keeps_session_backend() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_no_backend_empty_preference_keeps_session_backend.
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &[]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "codex-o3").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("codex-o3"));
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_verify_explicit_backend_wins_over_preference`: a named backend wins over
/// the pool's preference.
#[tokio::test]
async fn verify_explicit_backend_wins_over_preference() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_explicit_backend_wins_over_preference.
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let body = with_backend(verify_body(&manager.id), "kimi-k2.5");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  assert_eq!(settled_verify_run(&stack, &response).await?.backend.as_deref(), Some("kimi-k2.5"));
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_verify_unknown_explicit_backend_returns_400`: a named backend the
/// configuration does not hold is refused.
#[tokio::test]
async fn verify_unknown_explicit_backend_returns_400() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_unknown_explicit_backend_returns_400.
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let body = with_backend(verify_body(&manager.id), "nonexistent");
  let (status, response) = post_delegate(&routes::routes(&stack.features), body).await?;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
  assert!(text(&response, "detail")?.contains("nonexistent"), "{response}");
  assert!(scripts.builds().is_empty());
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_delegate_child_and_its_later_runs_keep_the_requested_backend` (partial):
/// the requested backend stays on the child record, its first work run, and the next work run the dispatcher reserves.
/// The review half (a review on a pool member other than the work run's backend) is not ported: the review spawn
/// is private to the task execution adapter.
#[tokio::test]
async fn delegate_child_and_its_later_runs_keep_the_requested_backend() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_delegate_child_and_its_later_runs_keep_the_requested_backend.
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

/// `test_internal_delegate_takeoff.py::test_verify_delegate_child_records_the_cross_model_backend`: a verify without a
/// backend takes the cross-model member, and the child record keeps the backend its first run got.
#[tokio::test]
async fn verify_delegate_child_records_the_cross_model_backend() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_verify_delegate_child_records_the_cross_model_backend.
  let scripts = Scripts::new(vec![result_script("verdict: yes")]);
  let stack = stack_with(&scripts, &OPTIONS, &["opus", "codex-o3"]).await?;
  let manager = node(&stack, "pm", None, TaskProfile::Manager, "opus").await?;

  let (status, response) = post_delegate(&routes::routes(&stack.features), verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{response}");
  let first = settled_verify_run(&stack, &response).await?;
  assert_eq!(first.backend.as_deref(), Some("codex-o3"));
  assert_eq!(Some(session_of(&stack, &text(&response, "session_id")?).await?.backend), first.backend);
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_implement_delegate_without_backend_resolves_to_the_parent_backend`: an
/// implement without a backend resolves to the parent's backend, and the child record keeps it.
#[tokio::test]
async fn implement_delegate_without_backend_resolves_to_the_parent_backend() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_implement_delegate_without_backend_resolves_to_the_parent_backend
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
  assert_launched_on(&stack, &guard, &response, "opus").await?;
  Ok(())
}

/// `test_internal_delegate_takeoff.py::test_pool_runs_unnamed_workers_and_leaves_named_and_verify_routing`: an
/// implement that names no backend takes the first pool member, a named backend is never pooled, and a verify takes
/// the next member other than the session's own.
#[tokio::test]
async fn pool_runs_unnamed_workers_and_leaves_named_and_verify_routing() -> anyhow::Result<()> {
  // Python: tests/test_internal_delegate_takeoff.py::test_pool_runs_unnamed_workers_and_leaves_named_and_verify_routing
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
  assert_launched_on(&stack, &guard, &unnamed, "opus").await?;

  let named = with_backend(delegate_body(&manager.id, &repo, "quick-edit"), "kimi-k2.5");
  let (status, named) = post_delegate(&router, named).await?;
  assert_eq!(status, StatusCode::OK, "{named}");
  assert_launched_on(&stack, &guard, &named, "kimi-k2.5").await?;

  let (status, verify) = post_delegate(&router, verify_body(&manager.id)).await?;
  assert_eq!(status, StatusCode::OK, "{verify}");
  assert_launched_on(&stack, &guard, &verify, "codex-o3").await?;
  Ok(())
}

/// `test_artifact_check.py::test_cold_read_delegation_from_a_manager_succeeds_without_takeoff`: a manager with no user
/// message delegates a cold read. The read-only exemption admits it, the child is named after the page file, and its
/// first run goes to the first pool member.
#[tokio::test]
async fn cold_read_delegation_from_a_manager_succeeds_without_takeoff() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_delegation_from_a_manager_succeeds_without_takeoff.
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
  assert_launched_on(&stack, &guard, &response, "alpha").await?;
  Ok(())
}

/// `test_artifact_check.py::test_cold_read_delegation_with_a_repo_field_gets_400`: a cold-read delegation that names a
/// repository or a base branch is refused with 400, and no child is written.
#[tokio::test]
async fn cold_read_delegation_with_a_repo_field_gets_400() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_delegation_with_a_repo_field_gets_400.
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

/// `test_artifact_check.py::test_cold_read_delegation_requires_cold_read`: a cold-read delegation without its
/// `cold_read` spec is refused with 400.
#[tokio::test]
async fn cold_read_delegation_requires_cold_read() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_cold_read_delegation_requires_cold_read.
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

/// `test_artifact_check.py::test_other_task_types_reject_cold_read`: a verify delegation that carries a `cold_read`
/// spec is refused with 400.
#[tokio::test]
async fn other_task_types_reject_cold_read() -> anyhow::Result<()> {
  // Python: tests/core/test_artifact_check.py::test_other_task_types_reject_cold_read.
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

/// `test_agent_message.py::test_session_message_refuses_an_archived_task_node`: a relay to an archived task node is
/// refused with 409 and the archived sentence, and the node stays archived.
#[tokio::test]
async fn session_message_refuses_an_archived_task_node() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_refuses_an_archived_task_node.
  let scripts = Scripts::new(vec![]);
  let stack = stack_with(&scripts, &[BACKEND_ID], &[BACKEND_ID]).await?;
  let caller = node(&stack, "caller", None, TaskProfile::Manager, BACKEND_ID).await?;
  let target = node(&stack, "target", None, TaskProfile::Manager, BACKEND_ID).await?;
  stack.store().archive_subtree(&target.id, TaskCaller::Operator).await?;

  let (status, body) = relay(&stack, &caller.id, &target.id, "status please").await?;
  assert_eq!(status, StatusCode::CONFLICT, "{body}");
  assert_eq!(body["detail"], format!("task {} is archived", target.id));
  assert_eq!(stack.store().task_state(&target.id)?, "archived");
  Ok(())
}

/// `test_agent_message.py::test_session_message_relay_admits_and_dispatches_a_task_input`: a relay admits one
/// `agent_message` into the target, carrying the sender's identity, and dispatch starts the target's turn with it.
#[tokio::test]
async fn session_message_relay_admits_and_dispatches_a_task_input() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_relay_admits_and_dispatches_a_task_input.
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

/// `test_agent_message.py::test_session_message_request_rejects_extra_fields`: a field the request model does not name
/// is refused with 422.
#[tokio::test]
async fn session_message_request_rejects_extra_fields() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_request_rejects_extra_fields.
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

/// `test_agent_message.py::test_session_message_accepts_a_message_at_the_weighted_limit`: 1000 CJK ideographs weigh
/// exactly the limit, so the relay is accepted and admits one `agent_message`.
#[tokio::test]
async fn session_message_accepts_a_message_at_the_weighted_limit() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_accepts_a_message_at_the_weighted_limit.
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

/// `test_agent_message.py::test_session_message_refuses_a_message_over_the_weighted_limit`: 1001 CJK ideographs weigh
/// 2002, so the relay answers 422 with the measured length and the limit, and the target gains no input.
#[tokio::test]
async fn session_message_refuses_a_message_over_the_weighted_limit() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_refuses_a_message_over_the_weighted_limit.
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

/// `test_agent_message.py::test_session_message_refuses_an_over_limit_ascii_message`: 2001 ASCII characters weigh 2001,
/// so the relay answers 422 and writes nothing to the target.
#[tokio::test]
async fn session_message_refuses_an_over_limit_ascii_message() -> anyhow::Result<()> {
  // Python: tests/test_agent_message.py::test_session_message_refuses_an_over_limit_ascii_message.
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
