//! The internal web routes behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`.
//!
//! The body or the query is validated before the handler runs, so a malformed request answers 422 whatever the
//! session is. A refusal of the handler answers its status with `{"detail": ...}`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, RequestBody, text_field};
use charliebot_compat::responses;
use charliebot_compat::validation::{self, FieldError, Source};
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use charliebot_registry::colleague_gate::{guard_agent_work, refuse_colleague_work};
use http::StatusCode;
use serde_json::{Value, json};

use crate::reply::{self, WebReplyError};
use crate::status;

/// `POST /api/internal/web/reply`: records the body's text as the session's web reply.
pub async fn post_reply(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match ReplyRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let identity = match caller::resolve_caller(&parts.headers, &features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  if let Some(refusal) = refuse_colleague_work(&features, &identity).await {
    return refusal;
  }
  let claims = identity.claims();
  let request_id = claims.map(|claims| {
    charliebot_store::control_events::derived_agent_send_request_id(
      &claims.run_id,
      "web reply",
      &model.session_id,
      &model.text,
    )
  });
  let run_id = claims.map(|claims| claims.run_id.as_str());
  answer(
    reply::post_web_reply_with_identity(
      &features,
      &model.session_id,
      &model.text,
      model.keep_language.as_deref(),
      request_id.as_deref(),
      run_id,
    )
    .await,
  )
}

/// `GET /api/internal/web/dialog?session_id=`: the session's For-you layer, in log order.
pub async fn get_dialog(
  State(features): State<Arc<FeatureContext>>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  let session_id = match session_id_param(&query) {
    Ok(session_id) => session_id,
    Err(error) => return validation::response(&[*error]),
  };
  answer(dialog(&features, &session_id).await)
}

/// `POST /api/internal/web/status`: stores the items the body's JSON text names and answers the stored status.
pub async fn post_status(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionText::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(refusal) = guard_agent_work(&features, &parts.headers).await {
    return refusal;
  }
  answer(status::set_status(&features, &model.session_id, &model.text).await)
}

/// `POST /api/internal/web/status/confirm`: confirms the calling session's stored status needs no change and answers
/// it.
pub async fn post_status_confirm(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionId::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(refusal) = guard_agent_work(&features, &parts.headers).await {
    return refusal;
  }
  answer(status::confirm_status(&features, &model.session_id).await)
}

/// `POST /api/internal/web/status/unread`: returns the waiting items that have no matching cold-read event.
pub async fn post_status_unread(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionText::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  answer(status::unread_waiting(&features, &model.session_id, &model.text).await)
}

/// `POST /api/internal/web/reply/unread`: returns whether the reply text has not been cold-read yet.
pub async fn post_reply_unread(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionText::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let identity = match caller::resolve_caller(&parts.headers, &features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  let claims = identity.claims();
  let request_id = claims.map(|claims| {
    charliebot_store::control_events::derived_agent_send_request_id(
      &claims.run_id,
      "web reply",
      &model.session_id,
      &model.text,
    )
  });
  answer(
    reply::unread_reply(
      &features,
      &model.session_id,
      &model.text,
      claims.map(|claims| claims.run_id.as_str()),
      request_id.as_deref(),
    )
    .await,
  )
}

/// `GET /api/internal/web/status?session_id=`: the session's status and its waiting descendants.
pub async fn get_status(
  State(features): State<Arc<FeatureContext>>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  let session_id = match session_id_param(&query) {
    Ok(session_id) => session_id,
    Err(error) => return validation::response(&[*error]),
  };
  answer(status::read_status(&features, &session_id).await)
}

/// The For-you layer's answer: `{session_id, entries}`, after the session check every web route makes.
async fn dialog(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  reply::require_manager_session(features, session_id).await?;
  let events = reply::load_events(features, session_id).await?;
  let entries = reply::for_you_entries_shared(&features.store, &events)?;
  Ok(json!({"session_id": session_id, "entries": entries}))
}

/// A handler's answer: the body with 200, a refusal with its status and detail, or the unhandled-exception 500.
fn answer(result: Result<Value, WebReplyError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(WebReplyError::Refused { status: code, detail }) => responses::json(code, &json!({"detail": detail})),
    Err(WebReplyError::Failed(error)) => responses::internal_error("RuntimeError", format!("{error:#}")),
  }
}

/// The required `session_id` query parameter.
fn session_id_param(query: &HashMap<String, String>) -> Result<String, Box<FieldError>> {
  match query.get("session_id") {
    Some(session_id) => Ok(session_id.clone()),
    None => Err(Box::new(validation::missing(Source::Query, "session_id"))),
  }
}

/// The fields of `WebReplyRequest` in declaration order: the calling session's id, the reply text, and the optional
/// reason for posting the text past the language check.
const REPLY_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("text", FieldType::String),
  Field::optional("keep_language", FieldType::NullableString),
];
const REPLY_BODY: BodyModel = BodyModel::required(REPLY_FIELDS).forbid_extra();

/// The fields of `WebStatusRequest`: the calling session's id and a text, both required strings.
const SESSION_TEXT_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("text", FieldType::String)];
const SESSION_TEXT_BODY: BodyModel = BodyModel::required(SESSION_TEXT_FIELDS).forbid_extra();

/// The fields of `WebStatusConfirmRequest`: the calling session's id, a required string.
const SESSION_ID_FIELDS: &[Field] = &[Field::required("session_id", FieldType::String)];
const SESSION_ID_BODY: BodyModel = BodyModel::required(SESSION_ID_FIELDS).forbid_extra();

/// The validated `WebReplyRequest`.
struct ReplyRequest {
  session_id: String,
  text: String,
  keep_language: Option<String>,
}

impl ReplyRequest {
  /// Validates the body against `REPLY_BODY`, which forbids the keys the model does not declare.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let mut fields = body.declared_fields(&REPLY_BODY)?;
    let keep_language = match fields.remove("keep_language") {
      Some(Value::String(reason)) => Some(reason),
      Some(Value::Null) | None => None,
      Some(_) => unreachable!("a validated keep_language is a string or null"),
    };
    Ok(Self { session_id: text_field(&fields, "session_id"), text: text_field(&fields, "text"), keep_language })
  }
}

/// The validated `WebStatusRequest`.
struct SessionText {
  session_id: String,
  text: String,
}

impl SessionText {
  /// Validates the body against `SESSION_TEXT_BODY`, which forbids the keys the model does not declare.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let fields = body.declared_fields(&SESSION_TEXT_BODY)?;
    Ok(Self { session_id: text_field(&fields, "session_id"), text: text_field(&fields, "text") })
  }
}

/// The validated `WebStatusConfirmRequest`.
struct SessionId {
  session_id: String,
}

impl SessionId {
  /// Validates the body against `SESSION_ID_BODY`, which forbids the keys the model does not declare.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let fields = body.declared_fields(&SESSION_ID_BODY)?;
    Ok(Self { session_id: text_field(&fields, "session_id") })
  }
}

#[cfg(test)]
mod tests {
  use axum::body::{Body, to_bytes};
  use axum::http::Method;
  use axum::http::Request;
  use charliebot_core::run_token::{self, RunTokenClaims};
  use charliebot_registry::caller::CallerIdentity;
  use charliebot_registry::colleague_gate::ColleagueGate;
  use charliebot_store::metadata::UtcDateTime;
  use charliebot_store::{RunKind, RunRecord};
  use futures_util::future::BoxFuture;
  use serde_json::Value;
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::Home;

  #[tokio::test]
  async fn an_extra_key_answers_422_extra_forbidden_once_the_declared_fields_validate() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let body = json!({"session_id": sid, "text": "x", "extra": 1});

    for path in ["/api/internal/web/reply", "/api/internal/web/status"] {
      let (status, detail) = home.send(Method::POST, path, Some(body.clone())).await;
      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
      assert_eq!(detail["detail"][0]["type"], "extra_forbidden", "{path}");
      assert_eq!(detail["detail"][0]["loc"], json!(["body", "extra"]), "{path}");
    }
  }

  #[tokio::test]
  async fn the_removed_answers_field_is_forbidden() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let body = json!({"session_id": sid, "text": "x", "answers": ["u1"]});

    let (status, detail) = home.send(Method::POST, "/api/internal/web/reply", Some(body)).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "extra_forbidden");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "answers"]));
  }

  #[tokio::test]
  async fn a_missing_and_a_mistyped_field_list_in_declaration_order() {
    let home = Home::new();

    let (status, detail) = home.send(Method::POST, "/api/internal/web/reply", Some(json!({"text": 5}))).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "missing");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "session_id"]));
    assert_eq!(detail["detail"][1]["type"], "string_type");
    assert_eq!(detail["detail"][1]["loc"], json!(["body", "text"]));
  }

  #[tokio::test]
  async fn extra_keys_are_reported_alongside_missing_and_mistyped_fields() {
    let home = Home::new();
    let body = json!({"text": 5, "extra": 1});

    for path in ["/api/internal/web/reply", "/api/internal/web/status"] {
      let (status, detail) = home.send(Method::POST, path, Some(body.clone())).await;
      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
      assert_eq!(
        detail["detail"].as_array().unwrap().iter().map(|error| error["type"].as_str().unwrap()).collect::<Vec<_>>(),
        ["missing", "string_type", "extra_forbidden"],
        "{path}"
      );
      assert_eq!(
        detail["detail"].as_array().unwrap().iter().map(|error| error["loc"].clone()).collect::<Vec<_>>(),
        [json!(["body", "session_id"]), json!(["body", "text"]), json!(["body", "extra"])],
        "{path}"
      );
    }
  }

  #[tokio::test]
  async fn concurrent_web_reply_replay_returns_one_stored_receipt() {
    let home = Home::new();
    let session_id = home.manager("web reply", None).await;
    let run_id = "synthetic-web-run";
    let claims = RunTokenClaims { session_id: session_id.clone(), run_id: run_id.to_string(), agent: "manager".into() };
    let run = RunRecord {
      id: run_id.to_string(),
      session_id: session_id.clone(),
      kind: RunKind::ManagerTurn,
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
      started_at: Some(UtcDateTime::now()),
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
    let guard = home.features.store.control_lock().await;
    home.features.store.register_run_locked(&guard, &session_id, run, Some("synthetic web reply run")).await.unwrap();
    drop(guard);
    home.features.store.record_launch(&session_id, run_id, 424242, "12345").await.unwrap();
    let token = run_token::sign_run_token(&claims, "synthetic-access-key").unwrap();
    let request = || {
      Request::builder()
        .method(Method::POST)
        .uri("/api/internal/web/reply")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(json!({"session_id": session_id, "text": "same response"}).to_string()))
        .unwrap()
    };
    let router = crate::entry::routes(&home.features);
    let (first, second) = tokio::join!(router.clone().oneshot(request()), router.oneshot(request()));
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first = serde_json::from_slice::<Value>(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let second = serde_json::from_slice::<Value>(&to_bytes(second.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(first, second);
    let replies: Vec<Value> = home
      .events(&session_id)
      .into_iter()
      .filter(|event| event["type"] == charliebot_core::event_types::WEB_REPLY)
      .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
      replies[0]["request_id"],
      charliebot_store::control_events::derived_agent_send_request_id(
        run_id,
        "web reply",
        &session_id,
        "same response"
      )
    );
  }

  /// The run a colleague started in this test. Every other run is the operator's.
  const COLLEAGUE_RUN: &str = "synthetic-colleague-run";
  /// The refusal the colleague gate answers the colleague's requests with.
  const COLLEAGUE_DETAIL: &str = "refused: synthetic colleague thread";

  /// A colleague gate that refuses the agent work and the writes of `COLLEAGUE_RUN`, and reads that run as colleague
  /// started. It answers both questions from the run's claims, as the platform gate answers them.
  struct ColleagueRun;

  impl ColleagueGate for ColleagueRun {
    fn refuse_agent_work<'a>(
      &'a self,
      _features: &'a FeatureContext,
      caller: &'a CallerIdentity,
    ) -> BoxFuture<'a, Option<&'static str>> {
      Box::pin(async move {
        (caller.claims().map(|claims| claims.run_id.as_str()) == Some(COLLEAGUE_RUN)).then_some(COLLEAGUE_DETAIL)
      })
    }

    fn admits_wake<'a>(
      &'a self,
      _features: &'a FeatureContext,
      _session_id: &'a str,
      _label: &'a str,
    ) -> BoxFuture<'a, bool> {
      Box::pin(async { true })
    }

    fn run_read_only<'a>(
      &'a self,
      _features: &'a FeatureContext,
      _session_id: &'a str,
      run_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
      Box::pin(async move { run_id == COLLEAGUE_RUN })
    }
  }

  /// A request to `path` that carries the bearer token `bearer` when one is given.
  fn request(method: Method, path: &str, bearer: Option<&str>, body: Option<&Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(bearer) = bearer {
      builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let body = match body {
      Some(body) => {
        builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
      }
      None => Body::empty(),
    };
    builder.body(body).unwrap()
  }

  #[tokio::test]
  async fn a_colleague_run_is_refused_on_every_write_route_and_the_reads_stay_open() {
    let home = Home::new();
    home.features.startup.insert(Arc::new(ColleagueRun) as Arc<dyn ColleagueGate>).unwrap();
    let session_id = home
      .manager(
        "colleague thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;
    let claims =
      RunTokenClaims { session_id: session_id.clone(), run_id: COLLEAGUE_RUN.to_string(), agent: "manager".into() };
    let token = run_token::sign_run_token(&claims, "synthetic-access-key").unwrap();
    let run = RunRecord {
      id: COLLEAGUE_RUN.to_string(),
      session_id: session_id.clone(),
      kind: RunKind::ManagerTurn,
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
      started_at: Some(UtcDateTime::now()),
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
    let guard = home.features.store.control_lock().await;
    home.features.store.register_run_locked(&guard, &session_id, run, Some("synthetic colleague run")).await.unwrap();
    drop(guard);
    home.features.store.record_launch(&session_id, COLLEAGUE_RUN, 424242, "12345").await.unwrap();

    let router = crate::entry::routes(&home.features);
    let writes = [
      ("/api/internal/web/status", json!({"session_id": session_id, "text": "{\"goal\": \"g\"}"})),
      ("/api/internal/web/status/confirm", json!({"session_id": session_id})),
      (
        "/api/internal/web/cold-read-check",
        json!({"session_id": session_id, "name": "ask", "item": {"text": "t", "context": "c"}, "backend": "b",
          "answer": "a"}),
      ),
      ("/api/internal/web/reply", json!({"session_id": session_id, "text": "hi"})),
      (
        "/api/internal/web/cold-read-check",
        json!({"session_id": session_id, "name": "reply", "text": "hi", "backend": "b", "findings": []}),
      ),
    ];
    for (path, body) in &writes {
      let response = router.clone().oneshot(request(Method::POST, path, Some(&token), Some(body))).await.unwrap();
      assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
      let detail: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
      assert_eq!(detail["detail"], COLLEAGUE_DETAIL, "{path}");
    }

    let read = router
      .clone()
      .oneshot(request(Method::GET, &format!("/api/internal/web/status?session_id={session_id}"), Some(&token), None))
      .await
      .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    let unread = router
      .clone()
      .oneshot(request(
        Method::POST,
        "/api/internal/web/status/unread",
        Some(&token),
        Some(&json!({"session_id": session_id, "text": "{}"})),
      ))
      .await
      .unwrap();
    assert_ne!(unread.status(), StatusCode::FORBIDDEN);

    let operator = router
      .oneshot(request(
        Method::POST,
        "/api/internal/web/status/confirm",
        None,
        Some(&json!({"session_id": session_id})),
      ))
      .await
      .unwrap();
    assert_ne!(operator.status(), StatusCode::FORBIDDEN);
  }
}
