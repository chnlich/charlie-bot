//! Chat message, chat cancel, retry, and run cancel routes.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;

use charliebot_registry::FeatureContext;
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::compact::CompactValue;
use charliebot_store::control_events;
use charliebot_store::metadata::{ColdReadSpec, SessionMetadata, TaskProfile, TaskSpec, TaskType};
use charliebot_store::run_records::RunOutcomeKind;
use charliebot_store::{CreateTask, StoreError, TaskCaller};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::completion::{self, CompletionEvidence};
use crate::dispatch;
use crate::execution::TaskExecutionAdapter;
use crate::prompts::MAX_AGENT_MESSAGE_WEIGHTED_CHARS;
use crate::task_sessions;
use crate::text_length;

struct RequiredTaskSession(SessionMetadata);

impl FromRequestParts<Arc<FeatureContext>> for RequiredTaskSession {
  type Rejection = Response;

  async fn from_request_parts(parts: &mut Parts, features: &Arc<FeatureContext>) -> Result<Self, Self::Rejection> {
    let Path(session_id) =
      Path::<String>::from_request_parts(parts, features).await.map_err(IntoResponse::into_response)?;
    features.store.require_session(&session_id).await.map(Self).map_err(task_http_error)
  }
}

/// Build the task orchestration routes.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/chat/{session_id}/upload", post_only(crate::upload::upload_file))
    .route("/api/chat/{session_id}/message", post_only(send_message))
    .route("/api/chat/{session_id}/cancel", post_only(cancel_chat))
    .route("/api/sessions/{session_id}/retry", post_only(retry_run))
    .route("/api/sessions/{session_id}/runs/{run_id}/cancel", post_only(cancel_run))
    .route("/api/sessions/{session_id}/archive", post_only(archive_task))
    .route("/api/sessions/{session_id}/cancel", post_only(cancel_task))
    .route("/api/sessions/{session_id}/task-inputs/acknowledge", post_only(acknowledge_task_inputs))
    .route("/api/sessions/{session_id}/unarchive", post_only(unarchive_task))
    .route("/api/internal/delegate", post_only(delegate_task))
    .route("/api/internal/session-message", post_only(session_message))
    .with_state(Arc::clone(features))
}

/// Whether `path` belongs to one of the registered routes.
pub fn matches(path: &str) -> bool {
  let parts = path.split('/').collect::<Vec<_>>();
  match parts.as_slice() {
    ["", "api", "chat", session_id, "upload" | "message" | "cancel"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "retry"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "runs", run_id, "cancel"] => !session_id.is_empty() && !run_id.is_empty(),
    ["", "api", "sessions", session_id, "archive" | "cancel" | "unarchive"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "task-inputs", "acknowledge"] => !session_id.is_empty(),
    ["", "api", "internal", "delegate" | "session-message"] => true,
    _ => false,
  }
}

struct UploadedFileRef {
  filename: String,
  path: String,
  size: Option<i64>,
}

struct SendMessageRequest {
  content: String,
  uploaded_files: Vec<UploadedFileRef>,
  input_mode: Option<Option<String>>,
  request_id: String,
}

const UPLOADED_FILE_BODY_FIELDS: &[Field] = &[
  Field::required("filename", FieldType::String),
  Field::required("path", FieldType::String),
  Field::optional("size", FieldType::NullableInteger),
];
const UPLOADED_FILE_BODY: BodyModel = BodyModel::required(UPLOADED_FILE_BODY_FIELDS);
const SEND_MESSAGE_BODY_FIELDS: &[Field] = &[
  Field::required("content", FieldType::String),
  Field::optional("uploaded_files", FieldType::ModelList { class_name: "UploadedFileRef", model: &UPLOADED_FILE_BODY }),
  Field::optional("input_mode", FieldType::NullableString),
  Field::required("request_id", FieldType::String).min_length(1),
];
const SEND_MESSAGE_BODY: BodyModel = BodyModel::required(SEND_MESSAGE_BODY_FIELDS);

const RETRY_RUN_BODY_FIELDS: &[Field] =
  &[Field::required("request_id", FieldType::String), Field::required("run_id", FieldType::String)];
const RETRY_RUN_BODY: BodyModel = BodyModel::required(RETRY_RUN_BODY_FIELDS).forbid_extra();

const CANCEL_RUN_BODY_FIELDS: &[Field] = &[Field::required("request_id", FieldType::String)];
const CANCEL_RUN_BODY: BodyModel = BodyModel::required(CANCEL_RUN_BODY_FIELDS).forbid_extra();

const DELEGATE_TASK_TYPE: &[&str] = &["implement", "quick-edit", "script-run", "verify", "cold-read"];
const COLD_READ_FIELDS: &[Field] = &[
  Field::required("page_path", FieldType::String),
  Field::required("genre", FieldType::String),
  Field::required("trigger", FieldType::String),
];
const COLD_READ_BODY: BodyModel = BodyModel::required(COLD_READ_FIELDS).forbid_extra();
const DELEGATE_INVOCATION_FIELDS: &[Field] = &[
  Field::required("task_type", FieldType::Enum(DELEGATE_TASK_TYPE)),
  Field::optional("repo_path", FieldType::NullableString),
  Field::optional("base_branch", FieldType::NullableString),
  Field::optional("task_spec_file", FieldType::NullableString),
  Field::optional("reviewer_context_file", FieldType::NullableString),
  Field::required("keep_worktree", FieldType::Boolean),
  Field::optional("backend", FieldType::NullableString),
];
const DELEGATE_INVOCATION_BODY: BodyModel = BodyModel::required(DELEGATE_INVOCATION_FIELDS).forbid_extra();
const DELEGATE_BODY_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("description", FieldType::String),
  Field::optional("base_branch", FieldType::NullableString),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("repo_path", FieldType::NullableString),
  Field::optional("task_type", FieldType::Enum(DELEGATE_TASK_TYPE)),
  Field::optional("keep_worktree", FieldType::Boolean),
  Field::optional(
    "delegate_invocation",
    FieldType::NullableModel { class_name: "DelegateInvocationMetadata", model: &DELEGATE_INVOCATION_BODY },
  ),
  Field::optional("request_id", FieldType::NullableString),
  Field::optional("cold_read", FieldType::NullableModel { class_name: "ColdReadSpec", model: &COLD_READ_BODY }),
];
const DELEGATE_BODY: BodyModel = BodyModel::required(DELEGATE_BODY_FIELDS);

struct DelegateRequest {
  session_id: String,
  description: String,
  base_branch: Option<String>,
  backend: Option<String>,
  repo_path: Option<String>,
  task_type: TaskType,
  keep_worktree: bool,
  delegate_invocation: Option<Value>,
  request_id: Option<String>,
  cold_read: Option<ColdReadSpec>,
}

fn parse_delegate_request(body: &RequestBody) -> Result<DelegateRequest, Box<Response>> {
  let values = body.validate(&DELEGATE_BODY)?;
  let values = values.expect("a required request body validates to an object");
  let task_type = match values.get("task_type").and_then(Value::as_str).unwrap_or("implement") {
    "implement" => TaskType::Implement,
    "quick-edit" => TaskType::QuickEdit,
    "script-run" => TaskType::ScriptRun,
    "verify" => TaskType::Verify,
    "cold-read" => TaskType::ColdRead,
    _ => unreachable!("validated task_type is a known task type"),
  };
  let cold_read = values
    .get("cold_read")
    .filter(|value| !value.is_null())
    .map(|value| serde_json::from_value(value.clone()).expect("validated cold_read matches ColdReadSpec"));
  Ok(DelegateRequest {
    session_id: values["session_id"].as_str().expect("validated session_id is a string").to_string(),
    description: values["description"].as_str().expect("validated description is a string").to_string(),
    base_branch: optional_string(&values, "base_branch"),
    backend: optional_string(&values, "backend"),
    repo_path: optional_string(&values, "repo_path"),
    task_type,
    keep_worktree: values.get("keep_worktree").and_then(Value::as_bool).unwrap_or(false),
    delegate_invocation: values.get("delegate_invocation").filter(|value| !value.is_null()).cloned(),
    request_id: optional_string(&values, "request_id"),
    cold_read,
  })
}

fn optional_string(values: &Value, key: &str) -> Option<String> {
  values.get(key).and_then(Value::as_str).map(str::to_string)
}

fn task_type_name(task_type: TaskType) -> &'static str {
  match task_type {
    TaskType::Implement => "implement",
    TaskType::QuickEdit => "quick-edit",
    TaskType::ScriptRun => "script-run",
    TaskType::Verify => "verify",
    TaskType::ColdRead => "cold-read",
  }
}

fn delegate_field_contract(request: &DelegateRequest) -> Result<(), String> {
  let task_type = task_type_name(request.task_type);
  if request.task_type == TaskType::ColdRead && request.cold_read.is_none() {
    return Err("cold-read delegations require cold_read".to_string());
  }
  if request.task_type != TaskType::ColdRead && request.cold_read.is_some() {
    return Err(format!("{task_type} delegations reject cold_read"));
  }
  if matches!(request.task_type, TaskType::Verify | TaskType::ColdRead) {
    if request.repo_path.is_some() {
      return Err(format!("{task_type} delegations are repo-less; omit repo_path"));
    }
    if request.base_branch.is_some() {
      return Err(format!("{task_type} delegations are repo-less; omit base_branch"));
    }
  } else if request.repo_path.is_some() != request.base_branch.is_some() {
    return Err(format!(
      "{task_type} delegations take repo_path and base_branch together; give both for a repo task, neither for a repo-less one"
    ));
  }
  Ok(())
}

fn delegate_task_spec(request: &DelegateRequest) -> TaskSpec {
  TaskSpec {
    goal: request.description.clone(),
    repo_path: request.repo_path.clone(),
    base_branch: request.base_branch.clone(),
    task_type: Some(request.task_type),
    keep_worktree: request.keep_worktree,
    cold_read: request.cold_read.clone(),
    ..TaskSpec::default()
  }
}

fn delegate_request_id(request: &DelegateRequest) -> String {
  request.request_id.as_deref().filter(|request_id| !request_id.is_empty()).map(str::to_string).unwrap_or_else(|| {
    control_events::derived_delegate_request_id(
      &request.session_id,
      task_type_name(request.task_type),
      &request.description,
    )
  })
}

fn delegate_invocation_event_payload(request: &DelegateRequest) -> Value {
  let invocation = request.delegate_invocation.as_ref().unwrap_or(&Value::Null);
  if invocation.is_null() {
    return json!({
      "task_type": task_type_name(request.task_type),
      "repo_path": request.repo_path,
      "base_branch": request.base_branch,
      "task_spec_file": null,
      "reviewer_context_file": null,
      "keep_worktree": request.keep_worktree,
      "backend": request.backend,
    });
  }
  json!({
    "task_type": invocation.get("task_type"),
    "repo_path": invocation.get("repo_path").filter(|value| !value.is_null()),
    "base_branch": invocation.get("base_branch").filter(|value| !value.is_null()),
    "task_spec_file": invocation.get("task_spec_file").filter(|value| !value.is_null()),
    "reviewer_context_file": invocation.get("reviewer_context_file").filter(|value| !value.is_null()),
    "keep_worktree": invocation.get("keep_worktree"),
    "backend": invocation.get("backend").filter(|value| !value.is_null()),
  })
}

async fn delegate_task(State(features): State<Arc<FeatureContext>>, request: axum::http::Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let request = match parse_delegate_request(&body) {
    Ok(request) => request,
    Err(response) => return *response,
  };
  if let Err(detail) = delegate_field_contract(&request) {
    return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": detail }));
  }
  let caller_identity = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let session_meta = match features.store.get_session(&request.session_id).await {
    Ok(Some(meta)) => meta,
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(error) => return task_http_error(error),
  };

  let task_spec = delegate_task_spec(&request);
  if !charliebot_store::is_read_only_exempt(Some(&task_spec))
    && let Err(error) = features.store.check_task_authorization(&request.session_id)
  {
    return task_http_error(error);
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let pool = match charliebot_agents::backend_pool::backend_pool(&features) {
    Ok(pool) => pool,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let mut route = None;
  let routed = if request.backend.is_none()
    && matches!(request.task_type, TaskType::Implement | TaskType::QuickEdit | TaskType::ScriptRun | TaskType::Verify)
    && cfg.backends.routing.is_some()
  {
    let avoided = if request.task_type == TaskType::Verify {
      match crate::spawner_backends::session_backend_id(&cfg, &session_meta) {
        Ok(backend_id) => Some(backend_id),
        Err(error) => return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": error.to_string() })),
      }
    } else {
      None
    };
    let candidates = cfg
      .backends
      .options
      .iter()
      .filter(|option| avoided.as_deref() != Some(option.id.as_str()))
      .map(|option| option.id.clone())
      .collect::<Vec<_>>();
    let role = task_type_name(request.task_type);
    let text = format!("Task type: {role}\n\n{}", request.description);
    let routed = match pool.route(&cfg, role, &text, &candidates).await {
      Ok(route) => route,
      Err(error) => {
        return responses::json(
          StatusCode::BAD_GATEWAY,
          &json!({ "detail": format!("backend router failed: {error}") }),
        );
      }
    };
    route = Some(routed.clone());
    match pool.pick_ordered(&cfg, role, &routed.backends, &[]) {
      Ok(Some(option)) => {
        match crate::spawner_backends::resolve_backend_option(&cfg, &option.id, Some(&option.model)) {
          Ok(option) => Some((option.id, Some(option.model), false)),
          Err(error) => return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": error.to_string() })),
        }
      }
      Ok(None) => None,
      Err(error) => return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": error.to_string() })),
    }
  } else {
    None
  };
  let resolved = if let Some(routed) = routed {
    Ok(Some(routed))
  } else if matches!(request.task_type, TaskType::Verify | TaskType::ColdRead) && request.backend.is_none() {
    crate::spawner_backends::select_verify_backend(
      &cfg,
      &features.store,
      &request.session_id,
      task_type_name(request.task_type),
      &[],
      &pool,
    )
    .await
    .map(|resolved| {
      resolved.map(|(backend, model, _tried_backends)| {
        let backend_from_pool = cfg.backends.pool.iter().any(|member| member == &backend);
        (backend, model, backend_from_pool)
      })
    })
  } else {
    crate::spawner_backends::resolve_requested_subagent_backend_model(
      &cfg,
      &features.store,
      &request.session_id,
      request.backend.as_deref(),
      request.task_type.into(),
      &pool,
    )
    .await
    .map(Some)
  };
  let (resolved_backend, resolved_model, backend_from_pool) = match resolved {
    Ok(Some(resolved)) => resolved,
    Ok(None) => return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": "no backend is available" })),
    Err(error) => return responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": error.to_string() })),
  };
  let delegated_backend =
    DelegateBackend { id: resolved_backend, model: resolved_model, from_pool: backend_from_pool, route };
  match delegate_task_tree(&features, &request, &caller_identity, &task_spec, &delegated_backend).await {
    Ok(response) => responses::json(StatusCode::OK, &response),
    Err(DelegateError::Task(error)) => task_http_error(error),
    Err(DelegateError::Internal(error)) => responses::internal_error("RuntimeError", error),
    Err(DelegateError::AdapterMissing) => {
      responses::json(StatusCode::SERVICE_UNAVAILABLE, &json!({ "detail": "task execution adapter is not installed" }))
    }
  }
}

enum DelegateError {
  Task(charliebot_store::StoreError),
  Internal(String),
  AdapterMissing,
}

struct DelegateBackend {
  id: String,
  model: Option<String>,
  from_pool: bool,
  route: Option<charliebot_agents::backend_pool::BackendRoute>,
}

impl From<charliebot_store::StoreError> for DelegateError {
  fn from(error: charliebot_store::StoreError) -> Self {
    Self::Task(error)
  }
}

async fn delegate_task_tree(
  features: &FeatureContext,
  request: &DelegateRequest,
  caller_identity: &CallerIdentity,
  task_spec: &TaskSpec,
  backend: &DelegateBackend,
) -> Result<Value, DelegateError> {
  let caller = match caller_identity {
    CallerIdentity::Operator { .. } => TaskCaller::Operator,
    CallerIdentity::Agent { claims } => TaskCaller::Agent { session_id: claims.session_id.clone() },
  };
  let request_id = delegate_request_id(request);
  let name = if request.task_type == TaskType::ColdRead {
    let path = request.cold_read.as_ref().expect("cold-read field contract was checked").page_path.as_str();
    Some(format!(
      "Cold read: {}",
      std::path::Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or_default()
    ))
  } else {
    None
  };
  let create_request = CreateTask {
    request_id: request_id.clone(),
    task_parent_id: Some(request.session_id.clone()),
    profile: TaskProfile::Worker,
    task: Some(task_spec.clone()),
    name,
    backend: Some(backend.id.clone()),
    group: None,
    session_id: None,
  };
  features.store.runtime_gate(&create_request, &caller)?;
  let child = features.store.create_task(create_request, caller, Default::default()).await?;
  if backend.from_pool || backend.route.is_some() {
    let mut slot_fields = serde_json::Map::from_iter([("backend_from_pool".to_string(), json!(backend.from_pool))]);
    if let Some(route) = backend.route.as_ref() {
      slot_fields.insert("route".to_string(), json!(route));
    }
    let guard = features.store.control_lock().await;
    features
      .store
      .update_task_slot_fields_locked(&guard, &child.id, charliebot_agents::backend_pool::METADATA_OWNER, slot_fields)
      .await?;
  }
  let run_id = control_events::stable_run_id(&child.id, &format!("{request_id}:work"));
  let existing = features.store.read_run_sync(&child.id, &run_id)?;
  if existing.is_none() {
    if features.store.task_state(&child.id)? != "open" {
      tracing::info!(target: "charliebot_tasks", parent = %request.session_id, child = %child.id, "delegate_child_closed");
      return Ok(json!({
        "session_id": child.id,
        "parent_session_id": request.session_id,
        "run_id": null,
        "thread_id": null,
        "description": request.description,
      }));
    }
    let record = charliebot_store::RunRecord {
      id: run_id.clone(),
      session_id: child.id.clone(),
      kind: charliebot_store::RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some(backend.id.clone()),
      model: backend.model.clone(),
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: request.repo_path.clone(),
      base_branch: request.base_branch.clone(),
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let task_spec_text = crate::execution::canonical_task_spec_text(Some(task_spec))
      .map_err(|error| DelegateError::Internal(error.to_string()))?;
    let guard = features.store.control_lock().await;
    features.store.register_run_locked(&guard, &child.id, record, task_spec_text.as_deref()).await?;
  }
  let adapter = features.startup.require::<TaskExecutionAdapter>().map_err(|_| DelegateError::AdapterMissing)?;
  adapter.launch(&child.id, &run_id)?;
  let task_event = json!({
    "type": charliebot_core::event_types::TASK_DELEGATED,
    "thread_id": run_id,
    "description": request.description,
    "backend": backend.id,
    "model": backend.model.as_deref().unwrap_or_default(),
    "child_session_id": child.id,
    "route": backend.route.as_ref(),
    charliebot_core::event_types::DELEGATE_INVOCATION: delegate_invocation_event_payload(request),
  });
  features.store.persist_and_broadcast(&request.session_id, task_event).await?;
  tracing::info!(target: "charliebot_tasks", session = %request.session_id, child = %child.id, run_id, "task_delegated_task_tree");
  Ok(json!({
    "session_id": child.id,
    "parent_session_id": request.session_id,
    "run_id": run_id,
    "thread_id": run_id,
    "description": request.description,
  }))
}

async fn require_session(features: &FeatureContext, session_id: &str) -> Result<bool, Box<Response>> {
  match features.store.get_session(session_id).await {
    Ok(Some(_)) => Ok(true),
    Ok(None) => Ok(false),
    Err(error) => Err(Box::new(task_http_error(error))),
  }
}

fn parse_send_message_request(body: &RequestBody) -> Result<SendMessageRequest, Box<Response>> {
  let values = body.validate(&SEND_MESSAGE_BODY)?;
  let values = values.expect("a required request body validates to an object");
  let content = values["content"].as_str().expect("validated content is a string").to_string();
  let uploaded_files = match values.get("uploaded_files") {
    None | Some(Value::Null) => Vec::new(),
    Some(Value::Array(items)) => items.iter().map(uploaded_file).collect(),
    _ => unreachable!("validated uploaded_files is a list"),
  };
  let input_mode = values.get("input_mode").map(|value| value.as_str().map(str::to_string));
  let request_id = values["request_id"].as_str().expect("validated request_id is a string").to_string();
  Ok(SendMessageRequest { content, uploaded_files, input_mode, request_id })
}

fn uploaded_file(item: &Value) -> UploadedFileRef {
  let object = item.as_object().expect("a validated uploaded file is an object");
  UploadedFileRef {
    filename: object["filename"].as_str().expect("validated filename is a string").to_string(),
    path: object["path"].as_str().expect("validated path is a string").to_string(),
    size: object.get("size").and_then(Value::as_i64),
  }
}

fn parse_retry_run_request(body: &RequestBody) -> Result<RetryRunRequest, Box<Response>> {
  let values = body.validate(&RETRY_RUN_BODY)?;
  let values = values.expect("a required request body validates to an object");
  Ok(RetryRunRequest {
    request_id: values["request_id"].as_str().expect("validated request_id is a string").to_string(),
    run_id: values["run_id"].as_str().expect("validated run_id is a string").to_string(),
  })
}

fn parse_cancel_run_request(body: &RequestBody) -> Result<CancelRunRequest, Box<Response>> {
  let values = body.validate(&CANCEL_RUN_BODY)?;
  let values = values.expect("a required request body validates to an object");
  Ok(CancelRunRequest {
    request_id: values["request_id"].as_str().expect("validated request_id is a string").to_string(),
  })
}

async fn send_message(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  request: axum::http::Request<Body>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  match require_session(&features, &session_id).await {
    Ok(true) => {}
    Ok(false) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(response) => return *response,
  }
  let caller = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let request = match parse_send_message_request(&body) {
    Ok(request) => request,
    Err(response) => return *response,
  };
  let uploaded_files = request.uploaded_files;
  let input_mode = request.input_mode.unwrap_or(None);
  let event_type = dispatch::input_event_type_for_caller(&caller);
  let (from_session, from_session_name) =
    if event_type == charliebot_core::event_types::USER { (None, None) } else { dispatch::agent_provenance(&caller) };
  let files = uploaded_files
    .into_iter()
    .map(|file| {
      let mut value = json!({ "filename": file.filename, "path": file.path });
      if let Some(size) = file.size {
        value["size"] = json!(size);
      }
      value
    })
    .collect::<Vec<_>>();
  let fields = json!({
    "request_id": request.request_id.clone(),
    "uploaded_files": files,
    "from_session": from_session,
    "from_session_name": from_session_name,
    "input_mode": input_mode,
  });
  let dispatch = match dispatch::dispatcher(&features) {
    Ok(dispatch) => dispatch,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let admitted = match dispatch
    .admit_input(
      &session_id,
      event_type,
      &request.content,
      if event_type == charliebot_core::event_types::USER { "user" } else { "agent" },
      fields,
    )
    .await
  {
    Ok(event) => event,
    Err(error) => return task_http_error(error),
  };
  let mut dispatch_error = None;
  let decision = match dispatch.dispatch_pending(&session_id).await {
    Ok(decision) => decision,
    Err(
      error @ (StoreError::Conflict(_) | StoreError::Forbidden(_) | StoreError::Invalid(_) | StoreError::NotFound(_)),
    ) => {
      let message = error.to_string();
      tracing::warn!(
        target: "charliebot_tasks",
        session = %session_id,
        request_id = %request.request_id,
        input_event_id = %admitted.get("id").and_then(CompactValue::as_str).expect("admitted input event has an id"),
        error = %message,
        "send_message_dispatch_failed"
      );
      dispatch_error = Some(message);
      json!({"launch": false})
    }
    Err(error) => return task_http_error(error),
  };
  tracing::info!(
    target: "charliebot_tasks",
    session = %session_id,
    content_chars = request.content.chars().count(),
    uploaded_files_count = files.len(),
    "send_message"
  );
  let Some(launch) = decision.get("launch").and_then(Value::as_bool) else {
    return responses::internal_error("RuntimeError", "dispatch decision has no boolean launch field".to_string());
  };
  let Some(input_event_id) = admitted.get("id").and_then(CompactValue::as_str) else {
    return responses::internal_error("RuntimeError", "admitted input event has no id".to_string());
  };
  let mut response = json!({
    "status": "accepted",
    "input_event_id": input_event_id,
    "launch": launch,
  });
  if let Some(reason) =
    decision.get("reason").filter(|value| !value.is_null() && value.as_str().is_some_and(|text| !text.is_empty()))
  {
    response["reason"] = reason.clone();
  }
  if let Some(dispatch_error) = dispatch_error {
    response["dispatch_error"] = json!(dispatch_error);
  }
  responses::json(StatusCode::ACCEPTED, &response)
}

const SESSION_MESSAGE_BODY_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("target_session_id", FieldType::String),
  Field::required("content", FieldType::String),
];
const SESSION_MESSAGE_BODY: BodyModel = BodyModel::required(SESSION_MESSAGE_BODY_FIELDS).forbid_extra();

struct SessionMessageRequest {
  session_id: String,
  target_session_id: String,
  content: String,
}

fn parse_session_message_request(body: &RequestBody) -> Result<SessionMessageRequest, Box<Response>> {
  let values = body.validate(&SESSION_MESSAGE_BODY)?;
  let values = values.expect("a required request body validates to an object");
  Ok(SessionMessageRequest {
    session_id: validated_string(&values, "session_id"),
    target_session_id: validated_string(&values, "target_session_id"),
    content: validated_string(&values, "content"),
  })
}

/// The agent relay behind `charliebot session send`: one `agent_message` admitted into the target session, then
/// dispatched (`src/runtime/api/internal.py` `session_message`). An over-limit message is refused before any lookup.
async fn session_message(State(features): State<Arc<FeatureContext>>, request: axum::http::Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let request = match parse_session_message_request(&body) {
    Ok(request) => request,
    Err(response) => return *response,
  };
  let weighted_chars = text_length::weighted_length(&request.content);
  if weighted_chars > MAX_AGENT_MESSAGE_WEIGHTED_CHARS {
    tracing::info!(
      target: "charliebot_tasks",
      session = %request.session_id,
      target_session = %request.target_session_id,
      weighted_chars,
      limit = MAX_AGENT_MESSAGE_WEIGHTED_CHARS,
      "session_message_refused"
    );
    let detail = format!(
      "Message is {weighted_chars} weighted characters; the limit is {MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n\
       CJK characters, CJK punctuation, and fullwidth characters count 2.\n\
       Write the long content to a file. Send a short message with the conclusion,\n\
       the request, and the file path."
    );
    return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "detail": detail }));
  }
  let identity = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let caller = match features.store.get_session(&request.session_id).await {
    Ok(Some(caller)) => caller,
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(error) => return task_http_error(error),
  };
  match features.store.get_session(&request.target_session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Target session not found" })),
    Err(error) => return task_http_error(error),
  }
  let dispatch = match dispatch::dispatcher(&features) {
    Ok(dispatch) => dispatch,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let request_id = identity.claims().map(|claims| {
    control_events::derived_agent_send_request_id(
      &claims.run_id,
      "session send",
      &request.target_session_id,
      &request.content,
    )
  });
  let fields = json!({
    "request_id": request_id,
    "from_session": caller.id,
    "from_session_name": caller.name,
  });
  if let Err(error) = dispatch
    .admit_input(
      &request.target_session_id,
      charliebot_core::event_types::AGENT_MESSAGE,
      &request.content,
      "agent",
      fields,
    )
    .await
  {
    return task_http_error(error);
  }
  if let Err(error) = dispatch.dispatch_pending(&request.target_session_id).await {
    return task_http_error(error);
  }
  tracing::info!(
    target: "charliebot_tasks",
    session = %request.session_id,
    target_session = %request.target_session_id,
    content_chars = request.content.chars().count(),
    "session_message_dispatched"
  );
  responses::json(StatusCode::OK, &json!({ "status": "accepted" }))
}

async fn cancel_chat(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  headers: HeaderMap,
) -> Response {
  if let Err(response) = caller::resolve_caller(&headers, &features) {
    return response;
  }
  if let Err(error) = features.store.load_task_meta(&session_id).await {
    return task_http_error(error);
  }
  let facts = match features.store.node_state(&session_id) {
    Ok(facts) => facts,
    Err(error) => return task_http_error(error),
  };
  let runs = match features.store.list_run_records_sync(&session_id) {
    Ok(runs) => runs,
    Err(error) => return task_http_error(error),
  };
  let mut requested = 0;
  for run in runs {
    let terminal = match facts.runs.terminal_outcome(&run.id) {
      Ok(outcome) => outcome.is_some(),
      Err(error) => return task_http_error(error),
    };
    if run.pid.is_none() || terminal {
      continue;
    }
    match features.store.request_stop(&session_id, &run.id, &format!("chat-cancel:{}", run.id)).await {
      Ok(result) if result.stop_requested => requested += 1,
      Ok(_) => {}
      Err(error) => return task_http_error(error),
    }
  }
  if requested == 0 {
    if let Err(error) = features
      .store
      .persist_and_broadcast(
        &session_id,
        json!({ "type": charliebot_core::event_types::ASSISTANT_ERROR, "content": "No active master agent to cancel." }),
      )
      .await
    {
      return task_http_error(error);
    }
    return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "No active master agent" }));
  }
  responses::json(StatusCode::OK, &json!({ "ok": true }))
}

struct RetryRunRequest {
  request_id: String,
  run_id: String,
}

async fn retry_run(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  request: axum::http::Request<Body>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  match require_session(&features, &session_id).await {
    Ok(true) => {}
    Ok(false) => return responses::json(StatusCode::NOT_FOUND, &json!({ "detail": "Session not found" })),
    Err(response) => return *response,
  }
  let caller = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let request = match parse_retry_run_request(&body) {
    Ok(request) => request,
    Err(response) => return *response,
  };
  if !caller.is_operator() {
    return responses::json(
      StatusCode::FORBIDDEN,
      &json!({ "detail": "retrying a run requires operator credentials" }),
    );
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let run = match task_sessions::create_retry(
    &features.store,
    &cfg,
    &session_id,
    &request.request_id,
    &request.run_id,
    None,
    None,
  )
  .await
  {
    Ok(run) => run,
    Err(error) => return task_http_error(error),
  };
  match dispatch::dispatcher(&features) {
    Ok(dispatch) => {
      if let Err(error) = dispatch.dispatch_pending(&session_id).await {
        return task_http_error(error);
      }
    }
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  }
  responses::json(StatusCode::OK, &json!({ "session_id": session_id, "run_id": run.id }))
}

struct CancelRunRequest {
  request_id: String,
}

async fn cancel_run(
  State(features): State<Arc<FeatureContext>>,
  Path((session_id, run_id)): Path<(String, String)>,
  request: axum::http::Request<Body>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let caller = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let request = match parse_cancel_run_request(&body) {
    Ok(request) => request,
    Err(response) => return *response,
  };
  if let CallerIdentity::Agent { claims } = &caller
    && (claims.session_id != session_id || claims.run_id != run_id)
  {
    return responses::json(StatusCode::FORBIDDEN, &json!({ "detail": "an agent may only stop its own bound run" }));
  }
  let result = match features.store.request_stop(&session_id, &run_id, &request.request_id).await {
    Ok(result) => result,
    Err(error) => return task_http_error(error),
  };
  let outcome = result.outcome.map(|outcome| match outcome {
    RunOutcomeKind::Success => "success",
    RunOutcomeKind::Failed => "failed",
    RunOutcomeKind::Interrupted => "interrupted",
  });
  responses::json(
    StatusCode::OK,
    &json!({ "run_id": result.run_id, "stop_requested": result.stop_requested, "outcome": outcome }),
  )
}

/// The POST /api/sessions/{session_id}/archive body model (`src/infra/models.py::ArchiveTaskRequest`).
const ARCHIVE_TASK_BODY: BodyModel = BodyModel::required(&[
  Field::required("request_id", FieldType::String),
  Field::optional("summary", FieldType::String),
  Field::optional("result_refs", FieldType::StringList),
  Field::optional("run_ids", FieldType::StringList),
]);

/// The POST /api/sessions/{session_id}/cancel body model (`src/infra/models.py::CancelTaskRequest`).
const CANCEL_TASK_BODY: BodyModel = BodyModel::required(&[
  Field::required("request_id", FieldType::String),
  Field::required("reason", FieldType::String),
]);

/// The task-inputs/acknowledge body model (`src/infra/models.py::AcknowledgeTaskInputsRequest`).
const ACKNOWLEDGE_TASK_INPUTS_BODY: BodyModel = BodyModel::required(&[
  Field::required("request_id", FieldType::String),
  Field::required("input_ids", FieldType::StringList),
  Field::optional("note", FieldType::String),
]);

/// Read the body first, then the session and caller dependencies, as FastAPI resolves a body-bearing route.
#[allow(clippy::result_large_err)]
async fn body_session_caller(
  features: &Arc<FeatureContext>,
  request: Request<Body>,
) -> Result<(SessionMetadata, CallerIdentity, RequestBody), Response> {
  let (mut parts, body) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, body).await {
    Ok(body) => body,
    Err(response) => return Err(*response),
  };
  let Path(session_id) =
    Path::<String>::from_request_parts(&mut parts, features).await.map_err(IntoResponse::into_response)?;
  let meta = features.store.require_session(&session_id).await.map_err(task_http_error)?;
  let caller = CallerIdentity::from_request_parts(&mut parts, features).await.map_err(IntoResponse::into_response)?;
  Ok((meta, caller, body))
}

fn validated_string(fields: &Value, name: &str) -> String {
  fields[name].as_str().expect("validated field is a string").to_string()
}

fn optional_validated_string(fields: &Value, name: &str) -> String {
  fields.get(name).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn validated_string_list(fields: &Value, name: &str) -> Vec<String> {
  fields[name]
    .as_array()
    .expect("validated field is a list")
    .iter()
    .map(|value| value.as_str().expect("validated list item is a string").to_string())
    .collect()
}

fn optional_validated_string_list(fields: &Value, name: &str) -> Vec<String> {
  match fields.get(name).and_then(Value::as_array) {
    Some(items) => {
      items.iter().map(|item| item.as_str().expect("validated list item is a string").to_string()).collect()
    }
    None => Vec::new(),
  }
}

/// `POST /api/sessions/{session_id}/archive`: an operator archives the open subtree; an agent's own manager task
/// archives after its Run succeeds. Both paths reach the one completion owner.
async fn archive_task(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (meta, caller, body) = match body_session_caller(&features, request).await {
    Ok(parts) => parts,
    Err(response) => return response,
  };
  let fields = match body.validate(&ARCHIVE_TASK_BODY) {
    Ok(Some(fields)) => fields,
    Ok(None) => unreachable!("a required model body is present after validation"),
    Err(response) => return *response,
  };
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  let evidence = CompletionEvidence {
    summary: optional_validated_string(&fields, "summary"),
    result_refs: optional_validated_string_list(&fields, "result_refs"),
    run_ids: optional_validated_string_list(&fields, "run_ids"),
    ..CompletionEvidence::default()
  };
  let (status, payload) =
    match completion.archive_task(&meta.id, &validated_string(&fields, "request_id"), &evidence, &caller).await {
      Ok(result) => result,
      Err(error) => return task_http_error(error),
    };
  // An operator archive names the ids it archived; every other outcome answers the session detail.
  if status == 202 || payload.get("archived").is_some() {
    let status_code = if status == 202 { StatusCode::ACCEPTED } else { StatusCode::OK };
    return responses::json(status_code, &payload);
  }
  let detail = match features.store.session_detail(&meta.id) {
    Ok(detail) => detail,
    Err(error) => return task_http_error(error),
  };
  responses::json(StatusCode::OK, &detail)
}

async fn cancel_task(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (meta, caller, body) = match body_session_caller(&features, request).await {
    Ok(parts) => parts,
    Err(response) => return response,
  };
  let fields = match body.validate(&CANCEL_TASK_BODY) {
    Ok(Some(fields)) => fields,
    Ok(None) => unreachable!("a required model body is present after validation"),
    Err(response) => return *response,
  };
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  let result = match completion
    .cancel_task(&meta.id, &validated_string(&fields, "request_id"), &validated_string(&fields, "reason"), &caller)
    .await
  {
    Ok(result) => result,
    Err(error) => return task_http_error(error),
  };
  let mut detail = match features.store.session_detail(&meta.id) {
    Ok(detail) => detail,
    Err(error) => return task_http_error(error),
  };
  let detail = detail.as_object_mut().expect("session detail is a JSON object");
  let stopped_runs = result.get("stopped_runs").expect("cancel result has stopped_runs").clone();
  detail.insert("stopped_runs".to_string(), stopped_runs);
  responses::json(StatusCode::OK, &Value::Object(detail.clone()))
}

async fn acknowledge_task_inputs(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (meta, caller, body) = match body_session_caller(&features, request).await {
    Ok(parts) => parts,
    Err(response) => return response,
  };
  let fields = match body.validate(&ACKNOWLEDGE_TASK_INPUTS_BODY) {
    Ok(Some(fields)) => fields,
    Ok(None) => unreachable!("a required model body is present after validation"),
    Err(response) => return *response,
  };
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  match completion
    .acknowledge_inputs(
      &meta.id,
      &validated_string(&fields, "request_id"),
      &validated_string_list(&fields, "input_ids"),
      &optional_validated_string(&fields, "note"),
      &caller,
    )
    .await
  {
    Ok(response) => responses::json(StatusCode::OK, &response),
    Err(error) => task_http_error(error),
  }
}

async fn unarchive_task(
  State(features): State<Arc<FeatureContext>>,
  RequiredTaskSession(meta): RequiredTaskSession,
  caller: CallerIdentity,
) -> Response {
  if !caller.is_operator() {
    return responses::json(
      StatusCode::FORBIDDEN,
      &json!({ "detail": "unarchiving a task requires operator credentials" }),
    );
  }
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  match completion.restore_task(&meta.id, &Uuid::new_v4().to_string(), "sidebar unarchive", &caller).await {
    Ok(response) => responses::json(StatusCode::OK, &json!({ "restored": response["restored"] })),
    Err(error) => task_http_error(error),
  }
}
#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_core::run_token::{self, RunTokenClaims};
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry};
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use charliebot_store::{CreateTask, RunKind, RunRecord, Store, StoreError, StoreResult, TaskCaller};
  use futures_util::future::BoxFuture;
  use serde_json::{Value, json};
  use tower::ServiceExt;

  use super::*;

  #[test]
  fn matches_claims_each_route_and_refuses_empty_or_extra_segments() {
    let claimed = [
      "/api/chat/s/upload",
      "/api/chat/s/message",
      "/api/chat/s/cancel",
      "/api/sessions/s/retry",
      "/api/sessions/s/runs/r/cancel",
      "/api/sessions/s/archive",
      "/api/sessions/s/cancel",
      "/api/sessions/s/task-inputs/acknowledge",
      "/api/sessions/s/unarchive",
      "/api/internal/delegate",
      "/api/internal/session-message",
    ];
    let refused = [
      "/api/chat//upload",
      "/api/chat/s",
      "/api/chat/s/other",
      "/api/chat/s/upload/",
      "/api/sessions//retry",
      "/api/sessions/s/runs//cancel",
      "/api/sessions/s/runs/r/cancel/",
      "/api/sessions//task-inputs/acknowledge",
      "/api/sessions/s/retry/",
      "/api/internal/delegate/",
      "/api/internal/delegate/x",
      "/",
      "",
    ];
    for path in claimed {
      assert!(matches(path), "{path} should be claimed");
    }
    for path in refused {
      assert!(!matches(path), "{path} should not be claimed");
    }
  }

  const INVALID_RUN_TOKEN: &str = "invalid-synthetic-run-token";

  fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot:\n  access_key: synthetic-access-key\n")
      .unwrap();
    let registry = Arc::new(Registry::new());
    let config_value = registry.load_config(home.path()).unwrap();
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .unwrap(),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "task_routes_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "task_routes_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    (home, features)
  }

  async fn send(router: &Router, method: Method, path: &str, body: &str) -> axum::response::Response {
    send_body(
      router,
      method,
      path,
      body.as_bytes().to_vec(),
      Some("application/json"),
      Some(&format!("Bearer {INVALID_RUN_TOKEN}")),
    )
    .await
  }

  async fn send_operator(router: &Router, path: &str, body: Vec<u8>, content_type: Option<&str>) -> Response {
    send_body(router, Method::POST, path, body, content_type, Some("Bearer synthetic-access-key")).await
  }

  async fn send_body(
    router: &Router,
    method: Method,
    path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
    authorization: Option<&str>,
  ) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(authorization) = authorization {
      request = request.header(header::AUTHORIZATION, authorization);
    }
    if let Some(content_type) = content_type {
      request = request.header(header::CONTENT_TYPE, content_type);
    }
    router.clone().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap()
  }

  fn add_session(features: &FeatureContext, session_id: &str) {
    let metadata = SessionMetadata::new(session_id, "Synthetic task route session", TaskProfile::Manager);
    let directory = features.store.session_dir(session_id);
    std::fs::create_dir_all(directory.join("data")).unwrap();
    std::fs::write(directory.join("metadata.json"), metadata.to_json_string(&features.registry.metadata_slots(), true))
      .unwrap();
    std::fs::write(directory.join("data").join("chat_events.jsonl"), "").unwrap();
    features.store.invalidate_cache(session_id);
  }

  /// Install the dispatcher that the send-message route reads, the way the execution stage does.
  fn install_dispatcher(features: &Arc<FeatureContext>) {
    features.startup.insert(crate::completion::TaskCompletion::new(Arc::clone(features))).unwrap();
    let completion = features.startup.require::<crate::completion::TaskCompletion>().unwrap();
    let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
    let run: Arc<dyn dispatch::RunFinishedCompletion> = completion;
    features
      .startup
      .insert(dispatch::Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence, run))
      .unwrap();
  }

  struct FailingExecutor;

  impl dispatch::DispatchExecutor for FailingExecutor {
    fn launch<'a>(
      &'a self,
      _session_id: &'a str,
      _pending_inputs: &'a [charliebot_store::ChatEvent],
      _launch_run_id: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<Option<String>>> {
      Box::pin(async { Err(StoreError::conflict(vec!["run already active".to_string()])) })
    }
  }

  /// Assert the 202 body Python returns for a valid chat message when no executor launches a run.
  async fn assert_accepted(response: Response) {
    let status = response.status();
    let body = response_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
    assert_eq!(body["status"], "accepted");
    assert!(body["input_event_id"].as_str().is_some_and(|id| !id.is_empty()), "body: {body}");
    assert_eq!(body["launch"], false);
    assert_eq!(body["reason"], "execution adapters register in the next stage");
  }

  async fn response_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
  }

  async fn assert_validation(response: Response, error_type: &str, location: Value) {
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"][0]["type"], error_type);
    assert_eq!(body["detail"][0]["loc"], location);
  }

  fn utf16le_json(value: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in value.encode_utf16() {
      bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
  }

  async fn create_upload_session(features: &FeatureContext, session_id: &str) {
    features
      .store
      .create_task(
        CreateTask {
          request_id: format!("synthetic-upload-{session_id}"),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some(format!("Synthetic {session_id}")),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: Some(session_id.to_string()),
        },
        TaskCaller::Operator,
        serde_json::Map::new(),
      )
      .await
      .expect("the upload test session is created");
  }

  async fn register_agent_run(features: &FeatureContext, session_id: &str, run_id: &str) -> String {
    let claims = RunTokenClaims {
      session_id: session_id.to_string(),
      run_id: run_id.to_string(),
      agent: "synthetic-agent".to_string(),
    };
    let record = RunRecord {
      id: run_id.to_string(),
      session_id: session_id.to_string(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let guard = features.store.control_lock().await;
    features.store.register_run_locked(&guard, session_id, record, Some("synthetic run")).await.unwrap();
    drop(guard);
    features.store.record_launch(session_id, run_id, 424242, "12345").await.unwrap();
    run_token::sign_run_token(&claims, "synthetic-access-key").unwrap()
  }

  #[tokio::test]
  async fn session_message_replays_for_a_run_and_newly_admits_for_an_operator() {
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-sender").await;
    create_upload_session(&features, "synthetic-target").await;
    install_dispatcher(&features);
    let token = register_agent_run(&features, "synthetic-sender", "synthetic-run").await;
    let router = routes(&features);
    let body = br#"{"session_id":"synthetic-sender","target_session_id":"synthetic-target","content":"same send"}"#;
    for _ in 0..2 {
      let response = send_body(
        &router,
        Method::POST,
        "/api/internal/session-message",
        body.to_vec(),
        Some("application/json"),
        Some(&format!("Bearer {token}")),
      )
      .await;
      assert_eq!(response.status(), StatusCode::OK);
    }
    for _ in 0..2 {
      let response =
        send_operator(&router, "/api/internal/session-message", body.to_vec(), Some("application/json")).await;
      assert_eq!(response.status(), StatusCode::OK);
    }

    let events = features.store.load_chat_events_sync("synthetic-target").unwrap();
    let agent_sends: Vec<_> =
      events.iter().filter(|event| event.get("type").and_then(CompactValue::as_str) == Some("agent_message")).collect();
    let expected_request_id = format!(
      "agent-send-{}",
      &charliebot_store::control_events::sha256_hex("synthetic-run\0session send\0synthetic-target\0same send")[..24]
    );
    assert_eq!(agent_sends.len(), 3, "one agent replay and two distinct operator sends: {agent_sends:?}");
    assert_eq!(agent_sends[0]["request_id"], expected_request_id);
    assert!(agent_sends[1].get("request_id").is_none());
    assert!(agent_sends[2].get("request_id").is_none());
  }

  fn multipart_upload(path: &str, filename: &str, payload: &[u8]) -> Request<Body> {
    const BOUNDARY: &str = "synthetic-upload-boundary";
    let mut body = format!(
      "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(payload);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Request::builder()
      .method(Method::POST)
      .uri(path)
      .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
      .body(Body::from(body))
      .expect("the upload request builds")
  }

  #[tokio::test]
  async fn task_routes_refuse_head_and_put_and_serve_post() {
    let (_home, features) = feature_context();
    let router = routes(&features);
    // Python: src/runtime/api/sessions.py:1747-1823 resolves the session dependency before the caller, so
    // the four completion routes answer 404 for an unknown session before any credential check.
    let route_cases = [
      ("/api/chat/synthetic-session/message", r#"{"content":"synthetic message"}"#, StatusCode::NOT_FOUND),
      ("/api/chat/synthetic-session/cancel", "", StatusCode::UNAUTHORIZED),
      (
        "/api/sessions/synthetic-session/retry",
        r#"{"request_id":"synthetic-request","run_id":"synthetic-run"}"#,
        StatusCode::NOT_FOUND,
      ),
      (
        "/api/sessions/synthetic-session/runs/synthetic-run/cancel",
        r#"{"request_id":"synthetic-request"}"#,
        StatusCode::UNAUTHORIZED,
      ),
    ];
    // The three task-completion routes take the session first, so an unknown session is 404, not 401.
    let session_first_route_cases = [
      ("/api/sessions/synthetic-session/archive", r#"{"request_id":"synthetic-request"}"#),
      ("/api/sessions/synthetic-session/cancel", r#"{"request_id":"synthetic-request","reason":"synthetic"}"#),
      (
        "/api/sessions/synthetic-session/task-inputs/acknowledge",
        r#"{"request_id":"synthetic-request","input_ids":[]}"#,
      ),
      ("/api/sessions/synthetic-session/unarchive", "{}"),
    ];

    for (path, post_body, expected_status) in route_cases {
      let head = send(&router, Method::HEAD, path, "").await;
      assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED, "HEAD {path}");
      assert_eq!(head.headers()[header::ALLOW], "POST", "HEAD {path}");

      let put = send(&router, Method::PUT, path, "").await;
      assert_eq!(put.status(), StatusCode::METHOD_NOT_ALLOWED, "PUT {path}");
      assert_eq!(put.headers()[header::ALLOW], "POST", "PUT {path}");
      let body = to_bytes(put.into_body(), usize::MAX).await.unwrap();
      assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({ "detail": "Method Not Allowed" }),
        "PUT {path} body"
      );

      let post = send(&router, Method::POST, path, post_body).await;
      assert_eq!(post.status(), expected_status, "POST {path} must reach its handler");
    }

    // The removed close route is absent, as Python's archive-only contract leaves it.
    let removed =
      send(&router, Method::POST, "/api/sessions/synthetic-session/complete", r#"{"request_id":"gone"}"#).await;
    assert_eq!(removed.status(), StatusCode::NOT_FOUND);

    for (path, post_body) in session_first_route_cases {
      let head = send(&router, Method::HEAD, path, "").await;
      assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED, "HEAD {path}");
      assert_eq!(head.headers()[header::ALLOW], "POST", "HEAD {path}");

      let put = send(&router, Method::PUT, path, "").await;
      assert_eq!(put.status(), StatusCode::METHOD_NOT_ALLOWED, "PUT {path}");
      assert_eq!(put.headers()[header::ALLOW], "POST", "PUT {path}");

      let post = send(&router, Method::POST, path, post_body).await;
      assert_eq!(post.status(), StatusCode::NOT_FOUND, "POST {path} must reach its handler");
    }
  }

  #[tokio::test]
  async fn delegate_field_contract_and_unknown_session_match_python() {
    // Python: tests/test_task_execution.py::test_delegate_creates_one_child_and_replays_are_stable.
    let (_home, features) = feature_context();
    let router = routes(&features);
    let missing_cold_read = send_operator(
      &router,
      "/api/internal/delegate",
      br#"{"session_id":"synthetic-missing","description":"synthetic","task_type":"cold-read"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_eq!(missing_cold_read.status(), StatusCode::BAD_REQUEST);

    let incomplete_repo_pair = send_operator(
      &router,
      "/api/internal/delegate",
      br#"{"session_id":"synthetic-missing","description":"synthetic","repo_path":"/synthetic/repo"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_eq!(incomplete_repo_pair.status(), StatusCode::BAD_REQUEST);

    let unknown_session = send_operator(
      &router,
      "/api/internal/delegate",
      br#"{"session_id":"synthetic-missing","description":"synthetic","task_type":"script-run"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_eq!(unknown_session.status(), StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn delegate_checks_the_runtime_gate_before_creating_a_child() {
    // Python: tests/test_task_manager_authorization.py::test_implementation_blocked_until_real_user_authorizes_then_delegates_from_depth.
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-manager").await;
    let before_children = features.store.child_ids("synthetic-manager").unwrap();
    let before_events = features.store.load_chat_events_sync("synthetic-manager").unwrap();
    let router = routes(&features);
    let response = send_operator(
      &router,
      "/api/internal/delegate",
      br#"{"session_id":"synthetic-manager","description":"synthetic task","task_type":"script-run"}"#.to_vec(),
      Some("application/json"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(features.store.child_ids("synthetic-manager").unwrap(), before_children);
    assert_eq!(features.store.load_chat_events_sync("synthetic-manager").unwrap(), before_events);
  }

  #[tokio::test]
  async fn chat_upload_saves_file_name_and_overwrites_existing_file() {
    let (_home, features) = feature_context();
    const SESSION_ID: &str = "synthetic-upload-session";
    create_upload_session(&features, SESSION_ID).await;
    let router = routes(&features);
    let config = features.config.get().unwrap();
    let uploads_dir = config.sessions_dir().join(SESSION_ID).join("uploads");

    let response = router
      .clone()
      .oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "../synthetic.txt", b"first"))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let saved_path = uploads_dir.join("synthetic.txt").canonicalize().unwrap();
    assert_eq!(body, json!({"filename": "../synthetic.txt", "path": saved_path.to_string_lossy(), "size": 5}));
    assert_eq!(std::fs::read(&saved_path).unwrap(), b"first");

    let response = router
      .clone()
      .oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "synthetic.txt", b"replacement"))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(std::fs::read(&saved_path).unwrap(), b"replacement");

    let response =
      router.oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "", b"nameless")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let nameless_path = uploads_dir.join("upload").canonicalize().unwrap();
    assert_eq!(body, json!({"filename": "", "path": nameless_path.to_string_lossy(), "size": 8}));
    assert_eq!(std::fs::read(nameless_path).unwrap(), b"nameless");
  }

  #[tokio::test]
  async fn chat_upload_copies_a_payload_across_the_copy_buffer_boundary() {
    let (_home, features) = feature_context();
    const SESSION_ID: &str = "synthetic-large-upload-session";
    create_upload_session(&features, SESSION_ID).await;
    let router = routes(&features);
    let config = features.config.get().unwrap();
    // Two and a half 1 MiB copy buffers plus three bytes: the copy crosses buffer boundaries and ends mid-buffer.
    let payload: Vec<u8> = (0..2_621_443_u32).map(|index| (index % 251) as u8).collect();

    let response = router
      .oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "synthetic-large.bin", &payload))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let saved_path = config.sessions_dir().join(SESSION_ID).join("uploads").join("synthetic-large.bin");
    assert_eq!(body["size"], payload.len());
    assert_eq!(std::fs::read(saved_path).unwrap(), payload);
  }

  #[tokio::test]
  async fn chat_upload_checks_session_after_multipart_parsing() {
    let (_home, features) = feature_context();
    let response = routes(&features)
      .oneshot(multipart_upload("/api/chat/synthetic-missing-session/upload", "synthetic.txt", b"content"))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({ "detail": "Session not found" }));
  }

  #[tokio::test]
  async fn chat_upload_returns_server_error_when_upload_directory_is_a_file() {
    let (_home, features) = feature_context();
    const SESSION_ID: &str = "synthetic-upload-failure";
    create_upload_session(&features, SESSION_ID).await;
    let config = features.config.get().unwrap();
    let uploads_dir = config.sessions_dir().join(SESSION_ID).join("uploads");
    std::fs::write(&uploads_dir, b"not a directory").unwrap();

    let response = routes(&features)
      .oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "synthetic.txt", b"content"))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"Internal Server Error");
  }

  #[tokio::test]
  async fn chat_upload_reports_an_unwritable_destination_as_save_failure() {
    let (_home, features) = feature_context();
    const SESSION_ID: &str = "synthetic-upload-unwritable";
    create_upload_session(&features, SESSION_ID).await;
    let config = features.config.get().unwrap();
    let uploads_dir = config.sessions_dir().join(SESSION_ID).join("uploads");
    std::fs::create_dir_all(uploads_dir.join("synthetic.txt")).unwrap();

    let response = routes(&features)
      .oneshot(multipart_upload(&format!("/api/chat/{SESSION_ID}/upload"), "synthetic.txt", b"content"))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({ "detail": "Failed to save uploaded file" }));
  }

  #[tokio::test]
  async fn python_send_message_model_reads_valid_utf16_and_defaulted_fields() {
    // Python: src/runtime/api/chat.py:48-80 answers 202 for a valid body; without an executor the
    // dispatch records "execution adapters register in the next stage"
    // (src/runtime/session_dispatch.py:430-502), so the body carries launch false with that reason.
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-session").await;
    install_dispatcher(&features);
    let router = routes(&features);
    let path = "/api/chat/synthetic-session/message";
    let valid = r#"{"content":"synthetic message","request_id":"synthetic-send-1","uploaded_files":[{"filename":"sample.txt","path":"/tmp/sample.txt","size":2}],"input_mode":"chat"}"#;
    let response = send_operator(&router, path, valid.as_bytes().to_vec(), Some("application/json")).await;
    assert_accepted(response).await;

    // Python: src/infra/models.py:635 defaults uploaded_files to an empty list when the key is absent.
    let utf16 = utf16le_json(r#"{"content":"synthetic UTF-16 message","request_id":"synthetic-send-2"}"#);
    let response = send_operator(&router, path, utf16, Some("application/json")).await;
    assert_accepted(response).await;

    // Python: SendMessageRequest ignores extra keys (pydantic's default extra behavior).
    let response = send_operator(
      &router,
      path,
      br#"{"content":"extra fields are ignored","request_id":"synthetic-send-3","unknown":"value"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_accepted(response).await;

    // Python: src/infra/models.py:635 accepts two files, and UploadedFileRef ignores an item's extra keys.
    let two_files = r#"{"content":"synthetic message","request_id":"synthetic-send-4","uploaded_files":[{"filename":"first.txt","path":"/tmp/first.txt","size":2},{"filename":"second.txt","path":"/tmp/second.txt","unknown":1}]}"#;
    let response = send_operator(&router, path, two_files.as_bytes().to_vec(), Some("application/json")).await;
    assert_accepted(response).await;
  }

  #[tokio::test]
  async fn python_send_message_reports_every_uploaded_file_error_with_the_content_error() {
    // Python: src/infra/models.py:630-640 validates content and each uploaded_files item in declaration
    // order, so a wrong content and a bad item give two errors in one 422 body.
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-session").await;
    let router = routes(&features);
    let path = "/api/chat/synthetic-session/message";
    let response = send_operator(
      &router,
      path,
      br#"{"content":1,"request_id":"invalid-uploaded-file","uploaded_files":[{"filename":1,"path":"x"}]}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"].as_array().map(Vec::as_slice).unwrap().len(), 2);
    assert_eq!(body["detail"][0]["type"], "string_type");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "content"]));
    assert_eq!(body["detail"][1]["type"], "string_type");
    assert_eq!(body["detail"][1]["loc"], json!(["body", "uploaded_files", 0, "filename"]));

    // Python: a non-list uploaded_files reports list_type at the field
    // (pydantic's list check runs before the items).
    assert_validation(
      send_operator(
        &router,
        path,
        br#"{"content":"x","request_id":"invalid-upload-list","uploaded_files":"wrong"}"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "list_type",
      json!(["body", "uploaded_files"]),
    )
    .await;

    // Python: a non-object item reports model_attributes_type at its index, with no trailing field
    // (FastAPI validates each list item as the model).
    assert_validation(
      send_operator(
        &router,
        path,
        br#"{"content":"x","request_id":"invalid-upload-item","uploaded_files":[42]}"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "model_attributes_type",
      json!(["body", "uploaded_files", 0]),
    )
    .await;

    // Python: UploadedFileRef requires path (src/infra/models.py:631-633).
    assert_validation(
      send_operator(
        &router,
        path,
        br#"{"content":"x","request_id":"invalid-upload-path","uploaded_files":[{"filename":"x"}]}"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "missing",
      json!(["body", "uploaded_files", 0, "path"]),
    )
    .await;
  }

  #[tokio::test]
  async fn python_send_message_validation_reports_fastapi_body_errors() {
    let (_home, features) = feature_context();
    add_session(&features, "synthetic-session");
    let router = routes(&features);
    let path = "/api/chat/synthetic-session/message";

    assert_validation(
      send_operator(&router, path, Vec::new(), Some("application/json")).await,
      "missing",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"content":"synthetic message"}"#.to_vec(), Some("text/plain")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"content":"#.to_vec(), Some("application/json")).await,
      "json_invalid",
      json!(["body", 11]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, b"[]".to_vec(), Some("application/json")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;

    let wrong_types = [
      (r#"{"content":1}"#, "string_type", json!(["body", "content"])),
      (r#"{"content":"x","uploaded_files":"wrong"}"#, "list_type", json!(["body", "uploaded_files"])),
      (r#"{"content":"x","input_mode":1}"#, "string_type", json!(["body", "input_mode"])),
      (
        r#"{"content":"x","uploaded_files":[{"filename":1,"path":"x"}]}"#,
        "string_type",
        json!(["body", "uploaded_files", 0, "filename"]),
      ),
      (
        r#"{"content":"x","uploaded_files":[{"filename":"x","path":1}]}"#,
        "string_type",
        json!(["body", "uploaded_files", 0, "path"]),
      ),
      (
        r#"{"content":"x","uploaded_files":[{"filename":"x","path":"x","size":"large"}]}"#,
        "int_parsing",
        json!(["body", "uploaded_files", 0, "size"]),
      ),
    ];
    for (body, error_type, location) in wrong_types {
      assert_validation(
        send_operator(&router, path, body.as_bytes().to_vec(), Some("application/json")).await,
        error_type,
        location,
      )
      .await;
    }
    assert_validation(
      send_operator(&router, path, b"{}".to_vec(), Some("application/json")).await,
      "missing",
      json!(["body", "content"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"content":"x","uploaded_files":[{}]}"#.to_vec(), Some("application/json"))
        .await,
      "missing",
      json!(["body", "uploaded_files", 0, "filename"]),
    )
    .await;

    assert_validation(
      send_operator(
        &router,
        "/api/chat/synthetic-missing-session/message",
        br#"{"content":"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "json_invalid",
      json!(["body", 11]),
    )
    .await;
  }

  #[tokio::test]
  async fn python_send_message_requires_request_id_and_replays_one_event() {
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-request-session").await;
    install_dispatcher(&features);
    let router = routes(&features);
    let path = "/api/chat/synthetic-request-session/message";

    for (body, error_type) in
      [(r#"{"content":"missing"}"#, "missing"), (r#"{"content":"empty","request_id":""}"#, "string_too_short")]
    {
      let response = send_operator(&router, path, body.as_bytes().to_vec(), Some("application/json")).await;
      assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
      let body = response_json(response).await;
      assert_eq!(body["detail"][0]["type"], error_type);
      assert_eq!(body["detail"][0]["loc"], json!(["body", "request_id"]));
    }

    let first = send_operator(
      &router,
      path,
      br#"{"content":"same","request_id":"browser-send-1"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    let second = send_operator(
      &router,
      path,
      br#"{"content":"same","request_id":"browser-send-1"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    let first_body = response_json(first).await;
    let second_body = response_json(second).await;
    assert_eq!(first_body["input_event_id"], second_body["input_event_id"]);
    let events = features.store.load_all_facts_view_sync("synthetic-request-session").unwrap();
    let inputs: Vec<_> = events
      .iter()
      .filter(|event| event.get("request_id").and_then(CompactValue::as_str) == Some("browser-send-1"))
      .collect();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["id"].as_str(), first_body["input_event_id"].as_str());
  }

  #[tokio::test]
  async fn python_dispatch_error_after_admission_returns_accepted_with_error() {
    let (_home, features) = feature_context();
    create_upload_session(&features, "synthetic-dispatch-error").await;
    install_dispatcher(&features);
    features.startup.require::<dispatch::Dispatcher>().unwrap().set_executor(Some(Arc::new(FailingExecutor)));
    let router = routes(&features);

    let response = send_operator(
      &router,
      "/api/chat/synthetic-dispatch-error/message",
      br#"{"content":"stored","request_id":"dispatch-error"}"#.to_vec(),
      Some("application/json"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await;
    assert_eq!(body["launch"], false);
    assert_eq!(body["dispatch_error"], "run already active");
    let events = features.store.load_all_facts_view_sync("synthetic-dispatch-error").unwrap();
    assert_eq!(
      events
        .iter()
        .filter(|event| event.get("request_id").and_then(CompactValue::as_str) == Some("dispatch-error"))
        .count(),
      1
    );
  }

  #[tokio::test]
  async fn python_retry_run_model_requires_strings_and_forbids_extras() {
    let (_home, features) = feature_context();
    add_session(&features, "synthetic-session");
    let router = routes(&features);
    let path = "/api/sessions/synthetic-session/retry";
    let valid = r#"{"request_id":"synthetic-request","run_id":"synthetic-run"}"#;
    let response = send_operator(&router, path, valid.as_bytes().to_vec(), Some("application/json")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send_operator(&router, path, utf16le_json(valid), Some("application/json")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    assert_validation(
      send_operator(&router, path, Vec::new(), Some("application/json")).await,
      "missing",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, valid.as_bytes().to_vec(), Some("text/plain")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"request_id":"#.to_vec(), Some("application/json")).await,
      "json_invalid",
      json!(["body", 14]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, b"[]".to_vec(), Some("application/json")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    for (body, location) in [
      (r#"{"request_id":1,"run_id":"x"}"#, json!(["body", "request_id"])),
      (r#"{"request_id":"x","run_id":1}"#, json!(["body", "run_id"])),
    ] {
      assert_validation(
        send_operator(&router, path, body.as_bytes().to_vec(), Some("application/json")).await,
        "string_type",
        location,
      )
      .await;
    }
    assert_validation(
      send_operator(&router, path, br#"{"run_id":"x"}"#.to_vec(), Some("application/json")).await,
      "missing",
      json!(["body", "request_id"]),
    )
    .await;
    assert_validation(
      send_operator(
        &router,
        path,
        br#"{"request_id":"x","run_id":"y","extra":true}"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "extra_forbidden",
      json!(["body", "extra"]),
    )
    .await;
    // pydantic reports the field error and the extra key together, fields first.
    let response =
      send_operator(&router, path, br#"{"request_id":1,"run_id":"x","extra":true}"#.to_vec(), Some("application/json"))
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"][0]["type"], "string_type");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "request_id"]));
    assert_eq!(body["detail"][1]["type"], "extra_forbidden");
    assert_eq!(body["detail"][1]["loc"], json!(["body", "extra"]));
    assert_eq!(body["detail"].as_array().map(Vec::len), Some(2));
    assert_validation(
      send_operator(
        &router,
        "/api/sessions/synthetic-missing-session/retry",
        br#"{"request_id":"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "json_invalid",
      json!(["body", 14]),
    )
    .await;
  }

  #[tokio::test]
  async fn python_cancel_run_model_requires_request_id_and_forbids_extras() {
    let (_home, features) = feature_context();
    let router = routes(&features);
    let path = "/api/sessions/synthetic-session/runs/synthetic-run/cancel";
    let valid = br#"{"request_id":"synthetic-request"}"#.to_vec();
    assert_eq!(
      send_operator(&router, path, valid.clone(), Some("application/json")).await.status(),
      StatusCode::NOT_FOUND
    );
    assert_eq!(
      send_operator(&router, path, utf16le_json(r#"{"request_id":"synthetic-request"}"#), Some("application/json"))
        .await
        .status(),
      StatusCode::NOT_FOUND
    );
    assert_validation(
      send_operator(&router, path, Vec::new(), Some("application/json")).await,
      "missing",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, valid.clone(), Some("text/plain")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"request_id":"#.to_vec(), Some("application/json")).await,
      "json_invalid",
      json!(["body", 14]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, b"[]".to_vec(), Some("application/json")).await,
      "model_attributes_type",
      json!(["body"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"request_id":1}"#.to_vec(), Some("application/json")).await,
      "string_type",
      json!(["body", "request_id"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, b"{}".to_vec(), Some("application/json")).await,
      "missing",
      json!(["body", "request_id"]),
    )
    .await;
    assert_validation(
      send_operator(&router, path, br#"{"request_id":"x","extra":true}"#.to_vec(), Some("application/json")).await,
      "extra_forbidden",
      json!(["body", "extra"]),
    )
    .await;
    // pydantic reports the field error and the extra key together, fields first.
    let response =
      send_operator(&router, path, br#"{"request_id":1,"extra":true}"#.to_vec(), Some("application/json")).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await;
    assert_eq!(body["detail"][0]["type"], "string_type");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "request_id"]));
    assert_eq!(body["detail"][1]["type"], "extra_forbidden");
    assert_eq!(body["detail"][1]["loc"], json!(["body", "extra"]));
    assert_eq!(body["detail"].as_array().map(Vec::len), Some(2));
    assert_validation(
      send_operator(
        &router,
        "/api/sessions/synthetic-missing-session/runs/synthetic-run/cancel",
        br#"{"request_id":"#.to_vec(),
        Some("application/json"),
      )
      .await,
      "json_invalid",
      json!(["body", 14]),
    )
    .await;
  }
}
