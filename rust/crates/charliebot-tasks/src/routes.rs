//! Chat message, chat cancel, retry, and run cancel routes.

use std::sync::Arc;

use axum::Json;
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
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::{self, RunOutcomeKind};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::completion::{self, CompletionEvidence};
use crate::dispatch;
use crate::task_sessions;

struct RequiredTaskSession(SessionMetadata);

impl FromRequestParts<Arc<FeatureContext>> for RequiredTaskSession {
  type Rejection = Response;

  async fn from_request_parts(parts: &mut Parts, features: &Arc<FeatureContext>) -> Result<Self, Self::Rejection> {
    let Path(session_id) =
      Path::<String>::from_request_parts(parts, features).await.map_err(IntoResponse::into_response)?;
    features.store.require_session(&session_id).await.map(Self).map_err(task_http_error)
  }
}

/// Build the four task orchestration routes.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/chat/{session_id}/upload", post_only(crate::upload::upload_file))
    .route("/api/chat/{session_id}/message", post_only(send_message))
    .route("/api/chat/{session_id}/cancel", post_only(cancel_chat))
    .route("/api/sessions/{session_id}/retry", post_only(retry_run))
    .route("/api/sessions/{session_id}/runs/{run_id}/cancel", post_only(cancel_run))
    .route("/api/sessions/{session_id}/complete", post_only(complete_task))
    .route("/api/sessions/{session_id}/cancel", post_only(cancel_task))
    .route("/api/sessions/{session_id}/task-inputs/acknowledge", post_only(acknowledge_task_inputs))
    .route("/api/sessions/{session_id}/unarchive", post_only(unarchive_task))
    .with_state(Arc::clone(features))
}

/// Whether `path` belongs to one of the registered routes.
pub fn matches(path: &str) -> bool {
  let parts = path.split('/').collect::<Vec<_>>();
  match parts.as_slice() {
    ["", "api", "chat", session_id, "upload" | "message" | "cancel"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "retry"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "runs", run_id, "cancel"] => !session_id.is_empty() && !run_id.is_empty(),
    ["", "api", "sessions", session_id, "complete" | "cancel" | "unarchive"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "task-inputs", "acknowledge"] => !session_id.is_empty(),
    _ => false,
  }
}

#[derive(Deserialize)]
struct UploadedFileRef {
  filename: String,
  path: String,
  #[serde(default)]
  size: Option<i64>,
}

#[derive(Deserialize)]
struct SendMessageRequest {
  content: String,
  #[serde(default)]
  uploaded_files: Vec<UploadedFileRef>,
  #[serde(default)]
  input_mode: Option<String>,
}

async fn send_message(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  headers: HeaderMap,
  Json(request): Json<SendMessageRequest>,
) -> Response {
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let event_type = dispatch::input_event_type_for_caller(&caller);
  let (from_session, from_session_name) =
    if event_type == charliebot_core::event_types::USER { (None, None) } else { dispatch::agent_provenance(&caller) };
  let files = request
    .uploaded_files
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
    "uploaded_files": files,
    "from_session": from_session,
    "from_session_name": from_session_name,
    "input_mode": request.input_mode,
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
  let decision = match dispatch.dispatch_pending(&session_id).await {
    Ok(decision) => decision,
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
  let Some(input_event_id) = admitted.get("id").and_then(Value::as_str) else {
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
  responses::json(StatusCode::ACCEPTED, &response)
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
  let events = match features.store.load_all_facts(&session_id) {
    Ok(events) => events,
    Err(error) => return task_http_error(error),
  };
  let runs = match features.store.list_run_records_sync(&session_id) {
    Ok(runs) => runs,
    Err(error) => return task_http_error(error),
  };
  let mut requested = 0;
  for run in runs {
    let terminal = match run_records::terminal_outcome(&events, &run.id) {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryRunRequest {
  request_id: String,
  run_id: String,
}

async fn retry_run(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  headers: HeaderMap,
  Json(request): Json<RetryRunRequest>,
) -> Response {
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRunRequest {
  request_id: String,
}

async fn cancel_run(
  State(features): State<Arc<FeatureContext>>,
  Path((session_id, run_id)): Path<(String, String)>,
  headers: HeaderMap,
  Json(request): Json<CancelRunRequest>,
) -> Response {
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
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

/// The POST /api/sessions/{session_id}/complete body model (`src/infra/models.py::CompleteTaskRequest`).
const COMPLETE_TASK_BODY: BodyModel = BodyModel::required(&[
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

async fn complete_task(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (meta, caller, body) = match body_session_caller(&features, request).await {
    Ok(parts) => parts,
    Err(response) => return response,
  };
  let fields = match body.validate(&COMPLETE_TASK_BODY) {
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
    match completion.complete_task(&meta.id, &validated_string(&fields, "request_id"), &evidence, &caller).await {
      Ok(result) => result,
      Err(error) => return task_http_error(error),
    };
  if status == 202 {
    return responses::json(StatusCode::ACCEPTED, &payload);
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
  if let Err(error) = completion
    .cancel_task(&meta.id, &validated_string(&fields, "request_id"), &validated_string(&fields, "reason"), &caller)
    .await
  {
    return task_http_error(error);
  }
  match features.store.session_detail(&meta.id) {
    Ok(detail) => responses::json(StatusCode::OK, &detail),
    Err(error) => task_http_error(error),
  }
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
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry};
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::{CreateTask, Store, TaskCaller};
  use serde_json::{Value, json};
  use tower::ServiceExt;

  use super::*;

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
    let request = Request::builder()
      .method(method)
      .uri(path)
      .header(header::AUTHORIZATION, format!("Bearer {INVALID_RUN_TOKEN}"))
      .header(header::CONTENT_TYPE, "application/json")
      .body(Body::from(body.to_string()))
      .unwrap();
    router.clone().oneshot(request).await.unwrap()
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
      ("/api/chat/synthetic-session/message", r#"{"content":"synthetic message"}"#),
      ("/api/chat/synthetic-session/cancel", ""),
      ("/api/sessions/synthetic-session/retry", r#"{"request_id":"synthetic-request","run_id":"synthetic-run"}"#),
      ("/api/sessions/synthetic-session/runs/synthetic-run/cancel", r#"{"request_id":"synthetic-request"}"#),
    ];
    // The four task-completion routes take the session first, so an unknown session is 404, not 401.
    let session_first_route_cases = [
      ("/api/sessions/synthetic-session/complete", r#"{"request_id":"synthetic-request"}"#),
      ("/api/sessions/synthetic-session/cancel", r#"{"request_id":"synthetic-request","reason":"synthetic"}"#),
      (
        "/api/sessions/synthetic-session/task-inputs/acknowledge",
        r#"{"request_id":"synthetic-request","input_ids":[]}"#,
      ),
      ("/api/sessions/synthetic-session/unarchive", "{}"),
    ];

    for (path, post_body) in route_cases {
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
      assert_eq!(post.status(), StatusCode::UNAUTHORIZED, "POST {path} must reach its handler");
    }

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
}
