//! Session metadata write routes.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::delete;
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::{post_only, put_only, refuse_other_methods};
use charliebot_core::cgroup::ResourceClass;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::metadata::RoundRating;
use charliebot_store::{Store, TaskCaller};
use serde_json::{Map, Value, json};

const RENAME_GROUP_FIELDS: &[Field] =
  &[Field::required("old_name", FieldType::String), Field::required("new_name", FieldType::String)];
const RENAME_GROUP_BODY: BodyModel = BodyModel::required(RENAME_GROUP_FIELDS);
const DELETE_GROUP_FIELDS: &[Field] = &[Field::required("group", FieldType::String)];
const DELETE_GROUP_BODY: BodyModel = BodyModel::required(DELETE_GROUP_FIELDS);
const RATE_ROUND_FIELDS: &[Field] =
  &[Field::required("rating", FieldType::NullableLiteral(&["thumbs_up", "thumbs_down"]))];
const RATE_ROUND_BODY: BodyModel = BodyModel::required(RATE_ROUND_FIELDS);
const SET_GROUP_FIELDS: &[Field] = &[Field::optional("group", FieldType::NullableString)];
const SET_GROUP_BODY: BodyModel = BodyModel::required(SET_GROUP_FIELDS);
const RESOURCE_CLASS_FIELDS: &[Field] = &[Field::required("resource_class", FieldType::Literal(&["agent", "repair"]))];
const RESOURCE_CLASS_BODY: BodyModel = BodyModel::required(RESOURCE_CLASS_FIELDS).forbid_extra();

/// Register fixed group routes before the routes whose first segment is a session id.
pub(crate) fn routes() -> Router<Arc<FeatureContext>> {
  Router::new()
    .route("/api/sessions/groups/rename", post_only(rename_group))
    .route("/api/sessions/groups/delete", post_only(delete_group))
    .route("/api/sessions/{session_id}/read", post_only(mark_session_read))
    .route("/api/sessions/{session_id}/star", post_only(star_session))
    .route("/api/sessions/{session_id}/unstar", post_only(unstar_session))
    .route("/api/sessions/{session_id}/group", post_only(set_session_group))
    .route("/api/sessions/{session_id}/rounds/{round_id}/rate", post_only(rate_round))
    .route("/api/sessions/{session_id}/permanent", refuse_other_methods(delete(delete_session_permanently), "DELETE"))
    .route("/api/sessions/{session_id}/resource-class", put_only(set_resource_class))
}

/// `PUT /api/sessions/{session_id}/resource-class`: the operator's repair mark. The body is validated before the
/// caller's scope is checked, and the session is looked up last, as in `set_session_resource_class`.
async fn set_resource_class(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let caller = match resolve_request_caller(&request, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let body = match read_body(request).await {
    Ok(body) => body,
    Err(response) => return response,
  };
  let payload = match body.validate(&RESOURCE_CLASS_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  if !caller.is_operator() {
    return responses::json(
      StatusCode::FORBIDDEN,
      &json!({"detail": "setting a session's resource class requires operator credentials"}),
    );
  }
  let resource_class = match payload["resource_class"].as_str().expect("validated literal field") {
    "agent" => ResourceClass::Agent,
    "repair" => ResourceClass::Repair,
    other => unreachable!("validated literal field, got {other:?}"),
  };
  match features.store.set_resource_class(&session_id, resource_class).await {
    Ok(Some(meta)) => {
      responses::fast_json(StatusCode::OK, &json!({"id": meta.id, "resource_class": meta.resource_class}))
    }
    Ok(None) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

async fn rename_group(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let body = match read_body(request).await {
    Ok(body) => body,
    Err(response) => return response,
  };
  let payload = match body.validate(&RENAME_GROUP_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let old_name = payload["old_name"].as_str().expect("validated string field");
  let new_name = payload["new_name"].as_str().expect("validated string field");
  match features.store.rename_group(old_name, new_name).await {
    Ok(updated) => responses::fast_json(StatusCode::OK, &json!({"updated": updated})),
    Err(error) => task_http_error(error),
  }
}

async fn delete_group(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let body = match read_body(request).await {
    Ok(body) => body,
    Err(response) => return response,
  };
  let payload = match body.validate(&DELETE_GROUP_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let group = payload["group"].as_str().expect("validated string field");
  match features.store.delete_group(group).await {
    Ok(updated) => responses::fast_json(StatusCode::OK, &json!({"updated": updated})),
    Err(error) => task_http_error(error),
  }
}

async fn mark_session_read(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  if let Err(error) = features.store.require_session(&session_id).await {
    return task_http_error(error);
  }
  match features.store.mark_read(&session_id).await {
    Ok(Some(_)) => responses::fast_json(StatusCode::OK, &json!({"session_id": session_id, "has_unread": false})),
    Ok(None) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

async fn star_session(State(features): State<Arc<FeatureContext>>, AxumPath(session_id): AxumPath<String>) -> Response {
  match features.store.star_session(&session_id).await {
    Ok(Some(meta)) => metadata_response(&features.store, meta),
    Ok(None) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

async fn unstar_session(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  match features.store.unstar_session(&session_id).await {
    Ok(Some(meta)) => metadata_response(&features.store, meta),
    Ok(None) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

async fn set_session_group(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let body = match read_body(request).await {
    Ok(body) => body,
    Err(response) => return response,
  };
  let session = features.store.require_session(&session_id).await;
  let payload = match body.validate(&SET_GROUP_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  if let Err(error) = session {
    return task_http_error(error);
  }
  let group = payload.get("group").and_then(Value::as_str);
  let meta = match features.store.set_group(&session_id, group).await {
    Ok(Some(meta)) => meta,
    Ok(None) => return session_not_found(),
    Err(error) => return task_http_error(error),
  };
  let mut fields = Map::new();
  fields.insert("group".to_string(), group.map_or(Value::Null, |value| json!(value)));
  if let Err(error) =
    features.store.broadcast_sidebar(&session_id, charliebot_core::event_types::SESSION_GROUP_CHANGED, fields).await
  {
    return task_http_error(error);
  }
  metadata_response(&features.store, meta)
}

async fn rate_round(
  State(features): State<Arc<FeatureContext>>,
  AxumPath((session_id, round_id)): AxumPath<(String, String)>,
  request: Request<Body>,
) -> Response {
  let body = match read_body(request).await {
    Ok(body) => body,
    Err(response) => return response,
  };
  if let Err(error) = features.store.require_session(&session_id).await {
    return task_http_error(error);
  }
  let payload = match body.validate(&RATE_ROUND_BODY) {
    Ok(Some(payload)) => payload,
    Ok(None) => unreachable!("a required request body validates to an object"),
    Err(response) => return *response,
  };
  let rating = match payload.get("rating") {
    Some(Value::Null) => None,
    Some(Value::String(value)) if value == "thumbs_up" => Some(RoundRating::ThumbsUp),
    Some(Value::String(value)) if value == "thumbs_down" => Some(RoundRating::ThumbsDown),
    _ => unreachable!("validated nullable literal field"),
  };
  match features.store.rate_round(&session_id, &round_id, rating).await {
    Ok(Some(meta)) => metadata_response(&features.store, meta),
    Ok(None) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

pub(crate) async fn archive_session(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let caller = match resolve_request_caller(&request, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  if !caller.is_operator() {
    return responses::json(
      StatusCode::FORBIDDEN,
      &json!({"detail": "archiving a task requires operator credentials"}),
    );
  }
  match features.store.archive_subtree(&session_id, TaskCaller::Operator).await {
    Ok(archived) => responses::fast_json(StatusCode::OK, &json!({"archived": archived})),
    Err(error) => task_http_error(error),
  }
}

async fn delete_session_permanently(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let caller = match resolve_request_caller(&request, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  match features.store.delete_permanently(&session_id, task_caller(caller)).await {
    Ok(true) => {
      Response::builder().status(StatusCode::NO_CONTENT).body(Body::empty()).expect("a no-content response is valid")
    }
    Ok(false) => session_not_found(),
    Err(error) => task_http_error(error),
  }
}

#[allow(clippy::result_large_err)]
async fn read_body(request: Request<Body>) -> Result<RequestBody, Response> {
  let (parts, body) = request.into_parts();
  fastapi_body::read(&parts.headers, body).await.map_err(|response| *response)
}

#[allow(clippy::result_large_err)]
fn resolve_request_caller(request: &Request<Body>, features: &FeatureContext) -> Result<CallerIdentity, Response> {
  caller::resolve_caller(request.headers(), features)
}

fn task_caller(caller: CallerIdentity) -> TaskCaller {
  match caller {
    CallerIdentity::Operator { .. } => TaskCaller::Operator,
    CallerIdentity::Agent { claims } => TaskCaller::Agent { session_id: claims.session_id },
  }
}

fn metadata_response(store: &Store, meta: charliebot_store::metadata::SessionMetadata) -> Response {
  let value = store.session_metadata_value(&meta);
  responses::fast_json(StatusCode::OK, &value)
}

fn session_not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Session not found"}))
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::fs;
  use std::sync::Mutex as StdMutex;

  use axum::Router;
  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_core::run_token::{self, RunTokenClaims};
  use charliebot_core::test_support::EventSink;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::{Registry, store_hooks};
  use charliebot_store::CreateTask;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
  use serde_json::{Value, json};
  use tower::ServiceExt;
  use tracing::Event;
  use tracing::field::{Field, Visit};

  use super::*;

  const ACCESS_KEY: &str = "synthetic-access-key";
  const SESSION_ID: &str = "synthetic-write-session";

  #[derive(Clone)]
  struct MessageCapture(Arc<StdMutex<Vec<String>>>);

  impl EventSink for MessageCapture {
    fn event(&self, event: &Event<'_>) {
      let mut visitor = MessageVisitor::default();
      event.record(&mut visitor);
      if let Some(message) = visitor.message {
        self.0.lock().unwrap().push(message);
      }
    }
  }

  #[derive(Default)]
  struct MessageVisitor {
    message: Option<String>,
  }

  impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
      if field.name() == "message" {
        self.message = Some(format!("{value:?}").trim_matches('"').to_string());
      }
    }
  }

  fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n")).unwrap();
    let registry = Arc::new(Registry::new());
    let config_value = registry.load_config(home.path()).unwrap();
    let store = Arc::new(
      charliebot_store::Store::new(
        config_value.clone(),
        registry.metadata_slots(),
        store_hooks::store_hooks(Arc::clone(&registry)),
      )
      .unwrap(),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "session_write_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "session_write_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    (home, Arc::new(FeatureContext { config, credentials, store, registry, startup }))
  }

  fn add_session(features: &FeatureContext, session_id: &str, group: Option<&str>, has_unread: bool) {
    let mut meta = SessionMetadata::new(session_id, format!("Synthetic {session_id}"), TaskProfile::Manager);
    meta.group = group.map(str::to_string);
    meta.has_unread = has_unread;
    meta.updated_at = UtcDateTime::parse("2026-10-18T10:00:00Z").unwrap();
    let path = features.store.metadata_path(session_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, meta.to_json_string(&features.registry.metadata_slots(), true)).unwrap();
  }

  async fn add_task(features: &FeatureContext, session_id: &str) {
    features
      .store
      .create_task(
        CreateTask {
          request_id: session_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some(format!("Synthetic {session_id}")),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: Some(session_id.to_string()),
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
  }

  fn add_run(features: &FeatureContext, session_id: &str, run_id: &str, launched: bool) {
    let path = features.store.run_metadata_path(session_id, run_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = if launched {
      json!({"id": run_id, "session_id": session_id, "kind": "work", "pid": 424242, "pid_start": "1"})
    } else {
      json!({"id": run_id, "session_id": session_id, "kind": "work"})
    };
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
  }

  async fn send(
    router: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
    authorization: Option<&str>,
  ) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
      builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(authorization) = authorization {
      builder = builder.header(header::AUTHORIZATION, authorization);
    }
    let body = body.map_or_else(Body::empty, |value| Body::from(serde_json::to_vec(&value).unwrap()));
    let response = router.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let raw = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if raw.is_empty() { Value::Null } else { serde_json::from_slice(&raw).unwrap() };
    (status, value)
  }

  #[tokio::test(flavor = "current_thread")]
  async fn metadata_write_routes_match_success_errors_broadcasts_and_logs() {
    let (_home, features) = feature_context();
    add_session(&features, SESSION_ID, Some("Old group"), true);
    add_session(&features, "group-session", Some("Old group"), false);
    let router = crate::routes::routes(&features);
    let messages = Arc::new(StdMutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(MessageCapture(Arc::clone(&messages))));

    let mut sidebar = features.store.subscribe(charliebot_store::streaming::SIDEBAR_CHANNEL);
    let (status, body) = send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/read"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"session_id": SESSION_ID, "has_unread": false}));
    let frame: Value = serde_json::from_str(&sidebar.recv().await.unwrap()).unwrap();
    assert_eq!(frame, json!({"type": "unread_changed", "session_id": SESSION_ID, "has_unread": false}));
    let stored = features.store.read_metadata_fresh(SESSION_ID).unwrap().unwrap();
    assert_eq!(stored.updated_at, UtcDateTime::parse("2026-10-18T10:00:00Z").unwrap());
    let (status, body) = send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/read"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["has_unread"], false);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(1), sidebar.recv()).await.is_err());

    let (status, body) = send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/star"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["starred"], true);
    let (status, body) = send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/unstar"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["starred"], false);

    let (status, body) = send(
      &router,
      Method::POST,
      &format!("/api/sessions/{SESSION_ID}/group"),
      Some(json!({"group": "New group"})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["group"], "New group");
    let frame: Value = serde_json::from_str(&sidebar.recv().await.unwrap()).unwrap();
    assert_eq!(frame, json!({"type": "session_group_changed", "session_id": SESSION_ID, "group": "New group"}));
    let (status, body) =
      send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/group"), Some(json!({})), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["group"], Value::Null);
    let frame: Value = serde_json::from_str(&sidebar.recv().await.unwrap()).unwrap();
    assert_eq!(frame["group"], Value::Null);

    let (status, body) = send(
      &router,
      Method::POST,
      "/api/sessions/groups/rename",
      Some(json!({"old_name": "Old group", "new_name": "Renamed group"})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"updated": 1}));
    let (status, body) =
      send(&router, Method::POST, "/api/sessions/groups/delete", Some(json!({"group": "Renamed group"})), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"updated": 1}));

    let (status, body) = send(
      &router,
      Method::POST,
      &format!("/api/sessions/{SESSION_ID}/rounds/round-a/rate"),
      Some(json!({"rating": "thumbs_up"})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["round_ratings"]["round-a"], "thumbs_up");
    let (status, body) = send(
      &router,
      Method::POST,
      &format!("/api/sessions/{SESSION_ID}/rounds/round-a/rate"),
      Some(json!({"rating": null})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["round_ratings"], json!({}));

    let (status, body) = send(&router, Method::POST, "/api/sessions/groups/rename", Some(json!({})), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "missing");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "old_name"]));
    assert_eq!(body["detail"][1]["loc"], json!(["body", "new_name"]));
    let (status, body) = send(&router, Method::POST, "/api/sessions/groups/delete", Some(json!({})), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["loc"], json!(["body", "group"]));
    let (status, body) = send(&router, Method::POST, &format!("/api/sessions/{SESSION_ID}/group"), None, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["loc"], json!(["body"]));
    let (status, body) = send(
      &router,
      Method::POST,
      &format!("/api/sessions/{SESSION_ID}/rounds/round-a/rate"),
      Some(json!({"rating": "neutral"})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "literal_error");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "rating"]));

    for path in [
      format!("/api/sessions/{SESSION_ID}/star"),
      format!("/api/sessions/{SESSION_ID}/unstar"),
      format!("/api/sessions/{SESSION_ID}/read"),
    ] {
      let (status, body) = send(&router, Method::POST, &path.replace(SESSION_ID, "missing-session"), None, None).await;
      assert_eq!(status, StatusCode::NOT_FOUND);
      assert_eq!(body, json!({"detail": "Session not found"}));
    }
    let (status, body) =
      send(&router, Method::POST, "/api/sessions/missing-session/group", Some(json!({"group": 1})), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "string_type");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "group"]));
    let (status, body) = send(
      &router,
      Method::POST,
      "/api/sessions/missing-session/rounds/round-a/rate",
      Some(json!({"rating": "neutral"})),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "Session not found"}));

    let captured = messages.lock().unwrap().clone();
    for event in
      ["session_starred", "session_unstarred", "session_group_set", "group_renamed", "group_deleted", "round_rated"]
    {
      assert!(captured.iter().any(|message| message == event), "missing log event {event}: {captured:?}");
    }
  }

  #[tokio::test(flavor = "current_thread")]
  async fn task_delete_routes_match_scope_errors_broadcast_and_permanent_guards() {
    let (_home, features) = feature_context();
    for session_id in ["archive-session", "queued-archive", "permanent-session", "permanent-conflict", "agent-session"]
    {
      add_task(&features, session_id).await;
    }
    add_run(&features, "queued-archive", "queued-run", false);
    add_run(&features, "agent-session", "agent-run", true);
    let token = run_token::sign_run_token(
      &RunTokenClaims {
        session_id: "agent-session".to_string(),
        run_id: "agent-run".to_string(),
        agent: "synthetic-agent".to_string(),
      },
      ACCESS_KEY,
    )
    .unwrap();
    let agent_auth = format!("Bearer {token}");
    let router = crate::routes::routes(&features);
    let messages = Arc::new(StdMutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(MessageCapture(Arc::clone(&messages))));

    let mut sidebar = features.store.subscribe(charliebot_store::streaming::SIDEBAR_CHANNEL);
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/archive-session", None, None).await;
    assert_eq!(status, StatusCode::OK, "archive response: {body}");
    assert_eq!(body, json!({"archived": ["archive-session"]}));
    let frame: Value = serde_json::from_str(&sidebar.recv().await.unwrap()).unwrap();
    assert_eq!(
      frame,
      json!({"type": "task_tree_changed", "session_id": "archive-session", "fact_type": "task_closed"})
    );

    let (status, body) = send(&router, Method::DELETE, "/api/sessions/missing-session", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "task missing-session not found"}));
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/archive-session", None, Some(&agent_auth)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"detail": "archiving a task requires operator credentials"}));
    let (status, body) =
      send(&router, Method::DELETE, "/api/sessions/archive-session", None, Some("Bearer broken")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"detail": "invalid run token: malformed run token"}));
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/queued-archive", None, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"]["blockers"], json!(["queued-archive: run queued-run is queued (pending dispatch)"]));

    let conflict_path = features.store.chat_events_path("permanent-conflict");
    fs::create_dir_all(conflict_path.parent().unwrap()).unwrap();
    fs::write(&conflict_path, "{\"id\":\"preserved-user\",\"type\":\"user\",\"content\":\"synthetic\"}\n").unwrap();
    features.store.clear_chat_events_cache("permanent-conflict");
    assert_eq!(features.store.facts_of("permanent-conflict").unwrap().events_by_id["preserved-user"]["type"], "user");
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/permanent-conflict/permanent", None, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["detail"]["blockers"], json!(["has preserved conversation/evidence: user"]));
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/missing-session/permanent", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "task missing-session not found"}));
    let (status, body) =
      send(&router, Method::DELETE, "/api/sessions/permanent-session/permanent", None, Some(&agent_auth)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"detail": "permanent delete requires operator credentials"}));
    let (status, body) = send(&router, Method::DELETE, "/api/sessions/permanent-session/permanent", None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    assert!(!features.store.session_dir("permanent-session").exists());
    assert!(messages.lock().unwrap().iter().any(|message| message == "session_deleted_permanently"));
  }

  fn stored_resource_class(features: &FeatureContext, session_id: &str) -> Value {
    let raw = fs::read_to_string(features.store.metadata_path(session_id)).unwrap();
    serde_json::from_str::<Value>(&raw).unwrap()["resource_class"].clone()
  }

  #[tokio::test(flavor = "current_thread")]
  async fn resource_class_route_refuses_an_agent_run_and_accepts_the_operator() {
    let (_home, features) = feature_context();
    add_task(&features, "root-session").await;
    add_run(&features, "root-session", "run-agent", true);
    let token = run_token::sign_run_token(
      &RunTokenClaims {
        session_id: "root-session".to_string(),
        run_id: "run-agent".to_string(),
        agent: "synthetic-agent".to_string(),
      },
      ACCESS_KEY,
    )
    .unwrap();
    let agent_auth = format!("Bearer {token}");
    let router = crate::routes::routes(&features);
    let path = "/api/sessions/root-session/resource-class";

    let (status, detail) =
      send(&router, Method::PUT, path, Some(json!({"resource_class": "repair"})), Some(&agent_auth)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(detail, json!({"detail": "setting a session's resource class requires operator credentials"}));
    assert_eq!(stored_resource_class(&features, "root-session"), json!("agent"));

    let (status, body) = send(&router, Method::PUT, path, Some(json!({"resource_class": "repair"})), None).await;
    assert_eq!(status, StatusCode::OK, "resource-class response: {body}");
    assert_eq!(body, json!({"id": "root-session", "resource_class": "repair"}));
    assert_eq!(stored_resource_class(&features, "root-session"), json!("repair"));
  }
}
