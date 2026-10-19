//! Chat message, chat cancel, retry, and run cancel routes.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::{self, FieldError, Kind, Loc};

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
    ["", "api", "chat", session_id, "message" | "cancel"] => !session_id.is_empty(),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteTaskRequest {
  request_id: String,
  #[serde(default)]
  summary: String,
  #[serde(default)]
  result_refs: Vec<String>,
  #[serde(default)]
  run_ids: Vec<String>,
}

async fn complete_task(
  State(features): State<Arc<FeatureContext>>,
  RequiredTaskSession(meta): RequiredTaskSession,
  caller: CallerIdentity,
  Json(request): Json<CompleteTaskRequest>,
) -> Response {
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  let evidence = CompletionEvidence {
    summary: request.summary,
    result_refs: request.result_refs,
    run_ids: request.run_ids,
    ..CompletionEvidence::default()
  };
  let (status, payload) = match completion.complete_task(&meta.id, &request.request_id, &evidence, &caller).await {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelTaskRequest {
  request_id: String,
  reason: String,
}

async fn cancel_task(
  State(features): State<Arc<FeatureContext>>,
  RequiredTaskSession(meta): RequiredTaskSession,
  caller: CallerIdentity,
  Json(request): Json<CancelTaskRequest>,
) -> Response {
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  if let Err(error) = completion.cancel_task(&meta.id, &request.request_id, &request.reason, &caller).await {
    return task_http_error(error);
  }
  match features.store.session_detail(&meta.id) {
    Ok(detail) => responses::json(StatusCode::OK, &detail),
    Err(error) => task_http_error(error),
  }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgeTaskInputsRequest {
  request_id: String,
  input_ids: Vec<String>,
  #[serde(default)]
  note: String,
}

fn acknowledge_request_error(field: &str, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: vec![Loc::Field("body".to_string()), Loc::Field(field.to_string())], input, kind }
}

fn parse_acknowledge_task_inputs_request(body: Value) -> Result<AcknowledgeTaskInputsRequest, Vec<FieldError>> {
  let Some(fields) = body.as_object() else {
    return Err(vec![FieldError {
      loc: vec![Loc::Field("body".to_string())],
      input: body,
      kind: Kind::ModelAttributesType,
    }]);
  };

  let mut errors = Vec::new();
  let request_id = match fields.get("request_id") {
    Some(Value::String(value)) => Some(value.clone()),
    Some(value) => {
      errors.push(acknowledge_request_error("request_id", value.clone(), Kind::StringType));
      None
    }
    None => {
      errors.push(acknowledge_request_error("request_id", Value::Null, Kind::Missing));
      None
    }
  };
  let input_ids = match fields.get("input_ids") {
    Some(Value::Array(items)) => {
      let mut input_ids = Vec::with_capacity(items.len());
      for (index, item) in items.iter().enumerate() {
        match item.as_str() {
          Some(value) => input_ids.push(value.to_string()),
          None => errors.push(FieldError {
            loc: vec![Loc::Field("body".to_string()), Loc::Field("input_ids".to_string()), Loc::Index(index)],
            input: item.clone(),
            kind: Kind::StringType,
          }),
        }
      }
      Some(input_ids)
    }
    Some(value) => {
      errors.push(acknowledge_request_error("input_ids", value.clone(), Kind::ListType));
      None
    }
    None => {
      errors.push(acknowledge_request_error("input_ids", Value::Null, Kind::Missing));
      None
    }
  };
  let note = match fields.get("note") {
    None => Some(String::new()),
    Some(Value::String(value)) => Some(value.clone()),
    Some(value) => {
      errors.push(acknowledge_request_error("note", value.clone(), Kind::StringType));
      None
    }
  };
  for (field, value) in fields {
    if !matches!(field.as_str(), "request_id" | "input_ids" | "note") {
      errors.push(acknowledge_request_error(field, value.clone(), Kind::ExtraForbidden));
    }
  }
  if !errors.is_empty() {
    return Err(errors);
  }
  Ok(AcknowledgeTaskInputsRequest {
    request_id: request_id.expect("valid request_id is present"),
    input_ids: input_ids.expect("valid input_ids are present"),
    note: note.expect("valid note is present"),
  })
}

async fn acknowledge_task_inputs(
  State(features): State<Arc<FeatureContext>>,
  RequiredTaskSession(meta): RequiredTaskSession,
  caller: CallerIdentity,
  Json(body): Json<Value>,
) -> Response {
  let request = match parse_acknowledge_task_inputs_request(body) {
    Ok(request) => request,
    Err(errors) => return validation::response(&errors),
  };
  let Some(completion) = features.startup.get::<completion::TaskCompletion>() else {
    return responses::internal_error("RuntimeError", "task completion owner is not installed".to_string());
  };
  match completion.acknowledge_inputs(&meta.id, &request.request_id, &request.input_ids, &request.note, &caller).await {
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
  use charliebot_store::Store;
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
}
