//! The internal improve routes behind `charliebot improve start` and `charliebot improve stop`.

use std::fmt::Display;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_agents::backend_pool;
use charliebot_compat::request_body::{
  self, BodyModel, Field, FieldType, flag_field, integer_field, optional_text_field_ref, text_field,
};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::config::{CallClass, CallTarget, CharlieBotConfig};
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::SequenceTree;
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::SESSION_NOT_FOUND_DETAIL;
use charliebot_store::metadata::SessionMetadata;
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::recovery;
use serde_json::{Map, Value, json};

use crate::command::{self, ImproveLoopAlreadyRunningError, ImproveState, LoopStatus, Reservation};
use crate::sequence::{self, LoopSpec, run_improve_sequence};
use crate::wire;

/// The `ImproveRequest` fields in the model's declaration order, which fixes the order of its 422 errors.
const IMPROVE_FIELDS: &[Field] = &[
  Field::required(wire::SESSION_ID, FieldType::String),
  Field::required(wire::REPO_PATH_FIELD, FieldType::String),
  Field::required(wire::BASE_BRANCH_FIELD, FieldType::String),
  Field::optional(wire::BACKEND, FieldType::NullableString),
  Field::optional(wire::ITERATIONS, FieldType::Integer),
  Field::required(wire::GOAL, FieldType::String),
  Field::optional(wire::PLAN, FieldType::NullableString),
  Field::optional(wire::WORK_BRANCH, FieldType::NullableString),
  Field::optional(wire::MERGE_BACK, FieldType::Boolean),
];
const IMPROVE_BODY: BodyModel = BodyModel::required(IMPROVE_FIELDS).forbid_extra();
const IMPROVE_STOP_FIELDS: &[Field] = &[Field::required(wire::SESSION_ID, FieldType::String)];
const IMPROVE_STOP_BODY: BodyModel = BodyModel::required(IMPROVE_STOP_FIELDS).forbid_extra();

/// The validated body of `POST /api/internal/improve`. An absent `iterations` reads as 3, an absent `merge_back` as
/// false, and the other optional fields as `None`.
struct ImproveRequest {
  session_id: String,
  repo_path: String,
  base_branch: String,
  backend: Option<String>,
  iterations: i64,
  goal: String,
  plan: Option<String>,
  work_branch: Option<String>,
  merge_back: bool,
}

/// The iteration count a start request that names none runs.
const DEFAULT_ITERATIONS: i64 = 3;

/// The start body, read from the validated fields through the wire names.
fn improve_request(fields: &Map<String, Value>) -> ImproveRequest {
  ImproveRequest {
    session_id: text_field(fields, wire::SESSION_ID),
    repo_path: text_field(fields, wire::REPO_PATH_FIELD),
    base_branch: text_field(fields, wire::BASE_BRANCH_FIELD),
    backend: optional_text_field_ref(fields, wire::BACKEND).map(str::to_string),
    iterations: match integer_field(fields, wire::ITERATIONS) {
      Some(value) => i64::try_from(value).expect("a validated iterations fits i64"),
      None => DEFAULT_ITERATIONS,
    },
    goal: text_field(fields, wire::GOAL),
    plan: optional_text_field_ref(fields, wire::PLAN).map(str::to_string),
    work_branch: optional_text_field_ref(fields, wire::WORK_BRANCH).map(str::to_string),
    merge_back: flag_field(fields, wire::MERGE_BACK).unwrap_or(false),
  }
}

/// The improve routes, mounted under the internal API paths.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(wire::START_PATH, post_only(start_improve_loop))
    .route(wire::STOP_PATH, post_only(stop_improve))
    .with_state(Arc::clone(features))
}

/// Whether this package owns one of its internal improve routes.
pub fn matches(path: &str) -> bool {
  matches!(path, wire::START_PATH | wire::STOP_PATH)
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
  let fields = match body.declared_fields(&IMPROVE_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let req = improve_request(&fields);
  match features.store.get_session(&req.session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": SESSION_NOT_FOUND_DETAIL })),
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

  // The loop's child session takes its backend now: the requested id (a model or a pool), the improve class's target, or
  // the session's own default. A pool id stays the child's backend, and each iteration selects its member.
  let meta = store.require_session(&req.session_id).await.map_err(|error| Box::new(task_http_error(error)))?;
  let (resolved_backend, resolved_model) =
    loop_backend(&cfg, &meta, req.backend.as_deref()).map_err(|error| Box::new(bad_request(error)))?;

  let work_branch = req.work_branch.clone().unwrap_or_else(|| format!("improve/{}", unix_seconds()));
  let reservation = Reservation {
    goal: &req.goal,
    work_branch: &work_branch,
    repo_path: &req.repo_path,
    plan: req.plan.as_deref(),
    base_branch: Some(req.base_branch.as_str()),
    merge_back: req.merge_back,
    resolved_backend: &resolved_backend,
    resolved_model: resolved_model.as_deref().unwrap_or_default(),
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
    (wire::STATUS): "started",
    (wire::SESSION_ID): req.session_id,
    (wire::ITERATIONS): req.iterations,
    (wire::LOOP_ID): loop_id,
    (wire::GOAL_PATH): command::loop_goal_path(&cfg, &req.session_id, loop_id).to_string_lossy(),
    (wire::CHILD_SESSION_ID): child.id,
  });
  if req.plan.is_some() {
    response[wire::PLAN_PATH] = json!(command::loop_plan_path(&cfg, &req.session_id, loop_id).to_string_lossy());
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

/// The child session's backend id for one loop, and its model when the id names a model. Each iteration selects the
/// run's model through `Selector`, including when the child keeps a pool id.
fn loop_backend(
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
  requested: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
  let backend_id = match requested {
    Some(backend_id) => backend_id.to_string(),
    None => match cfg.backends.calls.get(CallClass::Improve) {
      Some(CallTarget::Backend(target)) => target.clone(),
      None | Some(CallTarget::Session) => {
        if meta.backend.is_empty() {
          cfg
            .backends
            .new_session_backend()
            .ok_or_else(|| anyhow::anyhow!("session backend resolution requires a configured backends.options entry"))?
        } else {
          meta.backend.clone()
        }
      }
    },
  };
  cfg.backends.check_backend_id(&backend_id).map_err(anyhow::Error::msg)?;
  let model = cfg
    .get_backend_option(&backend_id)
    .map(|option| option.required_model("backend").map_err(anyhow::Error::msg))
    .transpose()?;
  Ok((backend_id, model))
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
  let fields = match body.declared_fields(&IMPROVE_STOP_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, wire::SESSION_ID);
  match features.store.get_session(&session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": SESSION_NOT_FOUND_DETAIL })),
    Err(error) => return task_http_error(error),
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return runtime_error(error),
  };
  match command::stop_improve_loop(&cfg, &session_id) {
    Ok(true) => {
      tracing::info!(target: "charliebot_improve", session = session_id.as_str(), "improve_loop_stopped");
      responses::json(StatusCode::OK, &json!({ (wire::STATUS): "stopped", (wire::SESSION_ID): session_id }))
    }
    Ok(false) => responses::json(StatusCode::CONFLICT, &json!({ "detail": "No active improve loop in this session" })),
    Err(error) => runtime_error(error),
  }
}
