//! The internal improve routes behind `charliebot improve` and `charliebot improve-stop`.

use std::fmt::Display;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_agents::backend_pool;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::SequenceTree;
use charliebot_registry::task_errors::task_http_error;
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::{recovery, spawner_backends};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{self, ImproveLoopAlreadyRunningError, ImproveState, LoopStatus, Reservation};
use crate::sequence::{self, LoopSpec, run_improve_sequence};

/// The `ImproveRequest` fields in the model's declaration order, which fixes the order of its 422 errors.
const IMPROVE_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("repo_path", FieldType::String),
  Field::required("base_branch", FieldType::String),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("iterations", FieldType::Integer),
  Field::required("goal", FieldType::String),
  Field::optional("plan", FieldType::NullableString),
  Field::optional("work_branch", FieldType::NullableString),
  Field::optional("merge_back", FieldType::Boolean),
];
const IMPROVE_BODY: BodyModel = BodyModel::required(IMPROVE_FIELDS).forbid_extra();
const IMPROVE_STOP_FIELDS: &[Field] = &[Field::required("session_id", FieldType::String)];
const IMPROVE_STOP_BODY: BodyModel = BodyModel::required(IMPROVE_STOP_FIELDS).forbid_extra();

/// The validated body of `POST /api/internal/improve`. An absent `iterations` reads as 3, an absent `merge_back` as
/// false, and the other optional fields as `None`.
#[derive(Deserialize)]
struct ImproveRequest {
  session_id: String,
  repo_path: String,
  base_branch: String,
  #[serde(default)]
  backend: Option<String>,
  #[serde(default = "default_iterations")]
  iterations: i64,
  goal: String,
  #[serde(default)]
  plan: Option<String>,
  #[serde(default)]
  work_branch: Option<String>,
  #[serde(default)]
  merge_back: bool,
}

fn default_iterations() -> i64 {
  3
}

/// The validated body of `POST /api/internal/improve/stop`.
#[derive(Deserialize)]
struct ImproveStopRequest {
  session_id: String,
}

/// Register the improve routes under the internal API paths.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/improve", post_only(start_improve_loop))
    .route("/api/internal/improve/stop", post_only(stop_improve))
    .with_state(Arc::clone(features))
}

/// Whether this package owns one of its internal improve routes.
pub fn matches(path: &str) -> bool {
  matches!(path, "/api/internal/improve" | "/api/internal/improve/stop")
}

fn runtime_error(error: impl Display) -> Response {
  responses::internal_error("RuntimeError", format!("{error:#}"))
}

/// A refusal of the start path. The start path threads it through `Result`, so it is boxed.
type Refusal = Box<Response>;
type ImproveRunnerParts = (Arc<dyn SequenceTree>, Arc<RepoRoot>, Arc<TaskExecutionAdapter>);

fn refuse_runtime(error: impl Display) -> Refusal {
  Box::new(runtime_error(error))
}

async fn start_improve_loop(
  State(features): State<Arc<FeatureContext>>,
  request: axum::http::Request<Body>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let values = match body.validate(&IMPROVE_BODY) {
    Ok(Some(values)) => values,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let req: ImproveRequest = serde_json::from_value(values).expect("the body model validated the request");
  match features.store.get_session(&req.session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(error) => return task_http_error(error),
  }
  match start_improve_sequence(&features, req).await {
    Ok(response) => responses::json(StatusCode::OK, &response),
    Err(refusal) => *refusal,
  }
}

/// The v2 improve path: the runtime gate, the backend, the loop reservation, the worker child, and the controller task.
/// A refusal answers with its status; a reservation is released when the child is refused.
async fn start_improve_sequence(features: &Arc<FeatureContext>, req: ImproveRequest) -> Result<Value, Refusal> {
  let store = &features.store;
  store.check_task_authorization(&req.session_id).map_err(|error| Box::new(task_http_error(error)))?;
  let cfg = features.config.get().map_err(refuse_runtime)?;
  let pool = backend_pool::backend_pool(features).map_err(refuse_runtime)?;

  // Backend and model resolve before the reservation, so an unknown backend never leaves a running loop behind. A loop
  // that names no backend takes a backends.pool member for each iteration, so it reserves no backend here.
  let backend_from_pool = req.backend.is_none() && !cfg.backends.pool.is_empty();
  let (resolved_backend, resolved_model) = if backend_from_pool {
    (String::new(), String::new())
  } else {
    // The resolver's role picks only among pool members, and this call never reaches the pool. A pool with a requested
    // backend returns that backend first, and a pool-less config skips the pool branch.
    let (backend, model, _from_pool) = spawner_backends::resolve_requested_subagent_backend_model(
      &cfg,
      store,
      &req.session_id,
      req.backend.as_deref(),
      spawner_backends::SubagentRole::Improve,
      &pool,
    )
    .await
    .map_err(|error| {
      if error.downcast_ref::<spawner_backends::BackendResolutionValueError>().is_some() {
        Box::new(bad_request(error))
      } else {
        refuse_runtime(error)
      }
    })?;
    (backend, model.unwrap_or_default())
  };

  let work_branch = req.work_branch.clone().unwrap_or_else(|| format!("improve/{}", unix_seconds()));
  let reservation = Reservation {
    goal: &req.goal,
    work_branch: &work_branch,
    repo_path: &req.repo_path,
    plan: req.plan.as_deref(),
    base_branch: Some(req.base_branch.as_str()),
    merge_back: req.merge_back,
    resolved_backend: &resolved_backend,
    resolved_model: &resolved_model,
    backend_from_pool,
  };
  let state = command::reserve_loop_state(&cfg, &req.session_id, &reservation).map_err(|error| {
    if error.is::<ImproveLoopAlreadyRunningError>() {
      Box::new(responses::json(StatusCode::CONFLICT, &json!({ "detail": error.to_string() })))
    } else {
      refuse_runtime(error)
    }
  })?;

  // The runner's collaborators come from the startup state. A failure after the reservation releases it, so no running
  // loop is left that no controller owns.
  let (tree, repo_root, adapter) = match runner_parts(features) {
    Ok(parts) => parts,
    Err(error) => {
      fail_reserved_loop(&cfg, &req.session_id, state).map_err(refuse_runtime)?;
      return Err(refuse_runtime(error));
    }
  };

  let child = match sequence::create_improve_child(
    store,
    &req.session_id,
    state.loop_id,
    &req.goal,
    Some(&req.repo_path),
    Some(&req.base_branch),
    state.backend.as_deref(),
  )
  .await
  {
    Ok(child) => child,
    Err(error) => {
      fail_reserved_loop(&cfg, &req.session_id, state).map_err(refuse_runtime)?;
      return Err(Box::new(task_http_error(error)));
    }
  };

  let loop_id = state.loop_id;
  let mut response = json!({
    "status": "started",
    "session_id": req.session_id,
    "iterations": req.iterations,
    "loop_id": loop_id,
    "goal_path": command::loop_goal_path(&cfg, &req.session_id, loop_id).to_string_lossy(),
    "child_session_id": child.id,
  });
  if req.plan.is_some() {
    response["plan_path"] = json!(command::loop_plan_path(&cfg, &req.session_id, loop_id).to_string_lossy());
  }

  tracing::info!(
    target: "charliebot_improve",
    session = req.session_id.as_str(),
    loop_id,
    child = child.id.as_str(),
    iterations = req.iterations,
    "improve_sequence_started"
  );
  let session_id = req.session_id;
  let goal = req.goal;
  let iterations = req.iterations;
  let child_id = child.id;
  tokio::spawn(async move {
    let spec = LoopSpec { session_id: &session_id, loop_id, iterations, child_id: &child_id, goal: &goal };
    run_improve_sequence(&spec, &cfg, tree.as_ref(), &repo_root, &pool, &adapter).await;
  });
  Ok(response)
}

/// The task-tree handle and the repo root the loop runner reads from the startup state.
fn runner_parts(features: &Arc<FeatureContext>) -> anyhow::Result<ImproveRunnerParts> {
  let adapter = features.startup.require::<TaskExecutionAdapter>()?;
  let tree: Arc<dyn SequenceTree> = Arc::new(recovery::sequence_tree(features, &adapter)?);
  let repo_root = features.startup.require::<RepoRoot>()?;
  Ok((tree, repo_root, adapter))
}

/// The 400 answer for a refused backend or model: its detail is the refusal's message.
fn bad_request(error: anyhow::Error) -> Response {
  responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": error.to_string() }))
}

/// Fail a freshly reserved loop whose child was refused: the state is marked failed and the active lock cleared.
/// The refused request then leaves no running loop that no controller owns.
fn fail_reserved_loop(cfg: &CharlieBotConfig, session_id: &str, mut state: ImproveState) -> anyhow::Result<()> {
  state.status = LoopStatus::Failed;
  command::save_loop_state(cfg, session_id, &state)?;
  command::clear_active_loop_lock(cfg, session_id)
}

fn unix_seconds() -> u64 {
  SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is after the Unix epoch").as_secs()
}

async fn stop_improve(State(features): State<Arc<FeatureContext>>, request: axum::http::Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let values = match body.validate(&IMPROVE_STOP_BODY) {
    Ok(Some(values)) => values,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let req: ImproveStopRequest = serde_json::from_value(values).expect("the body model validated the request");
  match features.store.get_session(&req.session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(error) => return task_http_error(error),
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return runtime_error(error),
  };
  match command::stop_improve_loop(&cfg, &req.session_id) {
    Ok(true) => {
      tracing::info!(target: "charliebot_improve", session = req.session_id.as_str(), "improve_loop_stopped");
      responses::json(StatusCode::OK, &json!({ "status": "stopped", "session_id": req.session_id }))
    }
    Ok(false) => responses::json(StatusCode::CONFLICT, &json!({ "detail": "No active improve loop in this session" })),
    Err(error) => runtime_error(error),
  }
}
