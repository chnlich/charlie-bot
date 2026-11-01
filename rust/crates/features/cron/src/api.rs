//! The five scheduled-task routes: list, create, update, run and delete.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, put};
use charliebot_compat::request_body::{self, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::{post_only, refuse_other_methods};
use charliebot_core::event_types;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::context::FeatureContext;
use serde_json::{Map, Value, json};

use crate::{config, loader, scheduler};

/// Build the cron task routes.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/cron/tasks", refuse_other_methods(get(list_tasks).post(create_task), "GET"))
    .route("/api/cron/tasks/{name}", refuse_other_methods(put(update_task).delete(delete_task), "PUT"))
    .route("/api/cron/tasks/{name}/run", post_only(run_task))
    .with_state(Arc::clone(features))
}

/// The route patterns `routes` mounts. The registry's trailing-slash redirect matches a path against them.
pub const ROUTE_PATTERNS: &[&str] = &["/api/cron/tasks", "/api/cron/tasks/{name}", "/api/cron/tasks/{name}/run"];

async fn list_tasks(State(features): State<Arc<FeatureContext>>) -> Response {
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let snapshot = match loader::load_snapshot(&features, &cfg) {
    Ok(snapshot) => snapshot,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  let mut rows = Vec::with_capacity(snapshot.tasks.len() + snapshot.errors.len());
  for task in snapshot.tasks {
    let mut value = match serde_json::to_value(task) {
      Ok(value) => value,
      Err(error) => return responses::internal_error("TypeError", error.to_string()),
    };
    if let Some(object) = value.as_object_mut() {
      object.remove("prompt");
      if let Some(steps) = object.get_mut("steps").and_then(Value::as_array_mut) {
        for step in steps {
          if let Some(step) = step.as_object_mut() {
            step.remove("prompt");
          }
        }
      }
    }
    rows.push(value);
  }
  for error in snapshot.errors {
    rows.push(json!({
      "name": error.name,
      "error": error.error,
      "broken": true,
      "path": error.path,
      "enabled": error.enabled,
    }));
  }
  responses::fast_json(StatusCode::OK, &Value::Array(rows))
}

async fn create_task(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, body) = request.into_parts();
  let body = match request_body::read(&parts.headers, body).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let values = match body.validate(&config::CREATE_MODEL) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => return internal_error("ValueError", "create body did not validate as an object"),
    Err(response) => return *response,
  };
  let name = values.get("name").and_then(Value::as_str).expect("the model requires a string name").to_string();
  if let Err(error) = validate_name(&name) {
    return detail(StatusCode::BAD_REQUEST, error);
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  if let Some(backend) = values.get("backend").and_then(Value::as_str).filter(|value| !value.is_empty())
    && cfg.get_backend_option(backend).is_none()
  {
    return detail(StatusCode::BAD_REQUEST, format!("backend '{backend}' is not in backends.options"));
  }
  let path = loader::cron_path(&cfg, &name);
  if path.exists() {
    return detail(StatusCode::CONFLICT, format!("Task \"{name}\" already exists"));
  }
  let body = create_file_body(&values);
  let repo = match features.startup.require::<RepoRoot>() {
    Ok(repo) => repo,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  if let Err(error) = loader::validate_body(&name, Value::Object(body.clone()), repo.path()) {
    return detail(StatusCode::CONFLICT, error);
  }
  if let Err(error) = std::fs::create_dir_all(loader::cron_dir(&cfg)) {
    return responses::internal_error("OSError", error.to_string());
  }
  if let Err(error) = write_yaml(&path, &Value::Object(body.clone())) {
    return responses::internal_error("OSError", error);
  }
  let mut result = Map::new();
  result.insert("name".to_string(), Value::String(name));
  result.extend(body);
  responses::fast_json(StatusCode::OK, &Value::Object(result))
}

async fn update_task(
  State(features): State<Arc<FeatureContext>>,
  Path(name): Path<String>,
  request: Request<Body>,
) -> Response {
  if let Err(error) = validate_name(&name) {
    return detail(StatusCode::BAD_REQUEST, error);
  }
  let (parts, body) = request.into_parts();
  let (body, null_bool_fields) = match request_body::read(&parts.headers, body).await {
    Ok(RequestBody::Json(mut value)) => {
      let mut nulls = Vec::new();
      if let Some(object) = value.as_object_mut() {
        for field in ["enabled", "allow_failure"] {
          if object.get(field) == Some(&Value::Null) {
            nulls.push(field);
            object.insert(field.to_string(), Value::Bool(false));
          }
        }
      }
      (RequestBody::Json(value), nulls)
    }
    Ok(body) => (body, Vec::new()),
    Err(response) => return *response,
  };
  let mut values = match body.validate(&config::UPDATE_MODEL) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => return internal_error("ValueError", "update body did not validate as an object"),
    Err(response) => return *response,
  };
  for field in null_bool_fields {
    values.remove(field);
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let path = loader::cron_path(&cfg, &name);
  if !path.exists() {
    return detail(StatusCode::NOT_FOUND, format!("Task \"{name}\" not found"));
  }
  if let Some(backend) = values.get("backend")
    && !backend.is_null()
    && let Some(backend) = backend.as_str().filter(|value| !value.is_empty())
    && cfg.get_backend_option(backend).is_none()
  {
    return detail(StatusCode::BAD_REQUEST, format!("backend '{backend}' is not in backends.options"));
  }
  let repo = match features.startup.require::<RepoRoot>() {
    Ok(repo) => repo,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  if let Err(error) = loader::load_file(&path, &name, repo.path()) {
    return detail(StatusCode::CONFLICT, error);
  }
  let original = match read_yaml_mapping(&path) {
    Ok(value) => value,
    Err(error) => return detail(StatusCode::CONFLICT, error),
  };
  let candidate = apply_update(original, &values);
  let candidate_task = match loader::validate_body(&name, Value::Object(candidate.clone()), repo.path()) {
    Ok(task) => task,
    Err(error) => return detail(StatusCode::CONFLICT, error),
  };
  if values.contains_key("backend")
    && let Some(session_id) = candidate_task.session_id.as_deref()
  {
    let target =
      candidate_task.backend.as_deref().or_else(|| cfg.backends.options.first().map(|option| option.id.as_str()));
    let Some(target) = target else {
      return detail(
        StatusCode::CONFLICT,
        "scheduled task backend resolution requires a configured backends.options entry".to_string(),
      );
    };
    match features.store.get_session(session_id).await {
      Ok(Some(node)) if node.backend != target => {
        if features.store.busy_since(session_id).is_some() {
          return detail(
            StatusCode::CONFLICT,
            format!(
              "scheduled task '{name}' backend switch from '{}' to '{target}' is blocked because node '{session_id}' has running work; retry when it is idle",
              node.backend
            ),
          );
        }
        if let Err(error) = features.store.switch_backend(session_id, target).await {
          return responses::internal_error("RuntimeError", error.to_string());
        }
      }
      Ok(_) => {}
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    }
  }
  if values.get("enabled") == Some(&Value::Bool(true))
    && let Some(session_id) = candidate_task.session_id.as_deref()
  {
    match features.store.task_state(session_id) {
      Ok(state) if state != "open" => {
        match features.startup.require::<charliebot_tasks::completion::TaskCompletion>() {
          Ok(completion) => {
            let request_id = format!("cron-enable:{}", uuid::Uuid::new_v4());
            if let Err(error) = completion.restore_chain(session_id, &request_id, "cron enable").await {
              return responses::internal_error("RuntimeError", error.to_string());
            }
          }
          Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
        }
      }
      Ok(_) => {}
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    }
  }
  if let Err(error) = write_yaml(&path, &Value::Object(candidate.clone())) {
    return responses::internal_error("OSError", error);
  }
  responses::fast_json(StatusCode::OK, &Value::Object(candidate))
}

async fn delete_task(State(features): State<Arc<FeatureContext>>, Path(name): Path<String>) -> Response {
  if let Err(error) = validate_name(&name) {
    return detail(StatusCode::BAD_REQUEST, error);
  }
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let path = loader::cron_path(&cfg, &name);
  if !path.exists() {
    return detail(StatusCode::NOT_FOUND, format!("Task \"{name}\" not found"));
  }
  match std::fs::remove_file(path) {
    Ok(()) => responses::fast_json(StatusCode::OK, &json!({"ok": true})),
    Err(error) => responses::internal_error("OSError", error.to_string()),
  }
}

async fn run_task(State(features): State<Arc<FeatureContext>>, Path(name): Path<String>) -> Response {
  match scheduler::run_task_now(&features, &name).await {
    Ok(result) => responses::fast_json(
      StatusCode::ACCEPTED,
      &json!({"type":event_types::TASK_TRIGGERED, "task":name, "session_id":result.session_id,
        "thread_id":result.thread_id}),
    ),
    Err(scheduler::RunError::Unavailable) => {
      detail(StatusCode::SERVICE_UNAVAILABLE, "Scheduler not available".to_string())
    }
    Err(scheduler::RunError::Missing(name)) => {
      detail(StatusCode::NOT_FOUND, format!("No scheduled task named '{name}'"))
    }
    Err(scheduler::RunError::Failed(error)) => responses::internal_error("RuntimeError", error),
  }
}

fn create_file_body(values: &Map<String, Value>) -> Map<String, Value> {
  let mut body = Map::new();
  for (key, value) in values {
    if key != "name"
      && (!value.is_null() || matches!(key.as_str(), "cron" | "enabled"))
      && !(key == "backend" && value.as_str().is_some_and(str::is_empty))
    {
      body.insert(key.clone(), value.clone());
    }
  }
  body.entry("timezone".to_string()).or_insert_with(|| Value::String(config::DEFAULT_TIMEZONE.to_string()));
  body.entry("enabled".to_string()).or_insert(Value::Bool(true));
  body.entry("allow_failure".to_string()).or_insert(Value::Bool(false));
  body
}

fn apply_update(mut task: Map<String, Value>, request: &Map<String, Value>) -> Map<String, Value> {
  for key in ["cron", "prompt_file", "timezone", "enabled", "allow_failure"] {
    if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
      task.insert(key.to_string(), value.clone());
    }
  }
  for key in ["repo", "project"] {
    if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
      task.insert(key.to_string(), if value.as_str() == Some("") { Value::Null } else { value.clone() });
    }
  }
  for key in ["backend", "session_id"] {
    if let Some(value) = request.get(key) {
      if value.as_str().is_some_and(|value| !value.is_empty()) {
        task.insert(key.to_string(), value.clone());
      } else {
        task.remove(key);
      }
    }
  }
  task
}

fn validate_name(name: &str) -> Result<(), String> {
  if loader::valid_name(name) { Ok(()) } else { Err(format!("invalid cron name: {name:?}")) }
}

fn read_yaml_mapping(path: &std::path::Path) -> Result<Map<String, Value>, String> {
  loader::read_task_file(path).map_err(|error| error.message_without_path())
}

fn write_yaml(path: &std::path::Path, value: &Value) -> Result<(), String> {
  loader::write_task_file(path, value).map_err(|error| match error {
    loader::TaskFileWriteError::Encode(error) => error.to_string(),
    loader::TaskFileWriteError::Write(error) => format!("{}: {error}", path.display()),
  })
}

fn detail(status: StatusCode, detail: String) -> Response {
  responses::fast_json(status, &json!({"detail": detail}))
}

fn internal_error(kind: &str, message: &str) -> Response {
  responses::internal_error(kind, message.to_string())
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::{Body, to_bytes};
  use axum::http::{Request, StatusCode, header};
  use charliebot_core::event_types;
  use charliebot_registry::route_claims::RouteClaims;
  use serde_json::{Value, json};
  use tower::ServiceExt;

  use super::{ROUTE_PATTERNS, routes};

  #[test]
  fn matches_claims_the_task_routes_and_refuses_empty_or_extra_segments() {
    let claimed = ["/api/cron/tasks", "/api/cron/tasks/n", "/api/cron/tasks/n/run"];
    let refused = [
      "/api/cron/tasks/",
      "/api/cron/tasks//run",
      "/api/cron/tasks/n/",
      "/api/cron/tasks/n/run/",
      "/api/cron/tasks/n/other",
      "/api/cron/tasks/n/run/x",
    ];
    let route_claims = RouteClaims::new(ROUTE_PATTERNS).expect("the route patterns are distinct");
    for path in claimed {
      assert!(route_claims.claims(path), "{path} should be claimed");
    }
    for path in refused {
      assert!(!route_claims.claims(path), "{path} should not be claimed");
    }
  }
  use crate::scheduler::CronScheduler;
  use crate::test_support::{feature_context, feature_context_with_extra_backends, manager};

  async fn send(router: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = router
      .clone()
      .oneshot(
        Request::builder()
          .method(method)
          .uri(path)
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(serde_json::to_vec(&body).unwrap()))
          .unwrap(),
      )
      .await
      .unwrap();
    let status = response.status();
    let failure = response
      .extensions()
      .get::<charliebot_compat::responses::Failure>()
      .map(|failure| format!("{}: {}", failure.kind, failure.detail));
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
      panic!("{method} {path} returned non-JSON body for HTTP {status}: {error}; failure={failure:?}; body={bytes:?}")
    });
    (status, body)
  }

  #[tokio::test]
  async fn create_update_list_and_delete_use_cron_yaml_files() {
    let (home, features) = feature_context();
    let prompt = home.path().join("synthetic-prompt.md");
    std::fs::write(&prompt, "Synthetic scheduled prompt\n").unwrap();
    let router = routes(&features);
    let (status, created) = send(
      &router,
      "POST",
      "/api/cron/tasks",
      json!({
        "name": "synthetic-task",
        "cron": "0 8 * * *",
        "prompt_file": prompt,
        "backend": "synthetic",
        "ignored": "discarded",
      }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["backend"], "synthetic");

    let (status, listed) = send(&router, "GET", "/api/cron/tasks", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["name"], "synthetic-task");
    assert!(listed[0].get("prompt").is_none());

    let (status, updated) =
      send(&router, "PUT", "/api/cron/tasks/synthetic-task", json!({"backend": null, "enabled": false, "project": ""}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(updated.get("backend").is_none());
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["project"], Value::Null);
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    let persisted = serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(persisted.get("backend").is_none());
    assert_eq!(persisted.get("enabled").and_then(serde_yaml::Value::as_bool), Some(false));

    let (status, deleted) = send(&router, "DELETE", "/api/cron/tasks/synthetic-task", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["ok"], true);
    assert!(!path.exists());
  }

  #[tokio::test]
  async fn delete_unlinks_the_yaml_and_keeps_the_bound_node_history() {
    let (home, features) = feature_context();
    let node = manager(&features, "synthetic-cron-node").await.unwrap();
    features.store.persist_and_broadcast(&node.id, json!({"type": "synthetic_history", "text": "kept"})).await.unwrap();
    let prompt = home.path().join("synthetic-prompt.md");
    std::fs::write(&prompt, "Synthetic prompt\n").unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
      &path,
      charliebot_core::yaml::dump(&json!({
        "cron": "0 8 * * *",
        "prompt_file": prompt,
        "session_id": node.id,
      }))
      .unwrap(),
    )
    .unwrap();

    let (status, _) = send(&routes(&features), "DELETE", "/api/cron/tasks/synthetic-task", json!(null)).await;

    assert_eq!(status, StatusCode::OK);
    assert!(features.store.get_session(&node.id).await.unwrap().is_some());
    assert_eq!(features.store.task_state(&node.id).unwrap(), "open");
    assert_eq!(
      features.store.get_session(&node.id).await.unwrap().unwrap().status,
      charliebot_store::metadata::SessionStatus::Active
    );
    let events = features.store.load_chat_events_sync(&node.id).unwrap();
    assert!(events.iter().any(|event| event["type"].as_str() == Some("synthetic_history")));
    assert!(!path.exists());
  }

  #[tokio::test]
  async fn create_refuses_a_backend_missing_from_the_options() {
    let (home, features) = feature_context();
    let prompt = home.path().join("synthetic-prompt.md");
    std::fs::write(&prompt, "Synthetic prompt\n").unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");

    let (status, response) = send(
      &routes(&features),
      "POST",
      "/api/cron/tasks",
      json!({
        "name": "synthetic-task",
        "cron": "0 8 * * *",
        "prompt_file": prompt,
        "backend": "missing-backend",
      }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["detail"], "backend 'missing-backend' is not in backends.options");
    assert!(!path.exists());
  }

  #[tokio::test]
  async fn manual_handler_run_auto_binds_once_and_records_the_result() {
    let (home, features) = feature_context();
    features.startup.insert(CronScheduler::new(Arc::clone(&features))).unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-handler.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
      &path,
      charliebot_core::yaml::dump(&json!({"cron": "0 8 * * *", "handler": "synthetic_handler"})).unwrap(),
    )
    .unwrap();

    let (status, result) = send(&routes(&features), "POST", "/api/cron/tasks/synthetic-handler/run", json!(null)).await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(result["type"], event_types::TASK_TRIGGERED);
    assert_eq!(result["thread_id"], Value::Null);
    let bound = serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let session_id = bound.get("session_id").and_then(serde_yaml::Value::as_str).unwrap();
    let meta = features.store.get_session(session_id).await.unwrap().unwrap();
    assert_eq!(meta.profile, charliebot_store::metadata::TaskProfile::Manager);
    assert_eq!(meta.extra.get("scheduled_task"), Some(&Value::Null));
    assert!(meta.extra.get("last_scheduled_run").and_then(Value::as_str).is_some());
    assert_eq!(meta.extra.get("last_run_status").and_then(Value::as_str), Some("success"));
    let events = features.store.load_chat_events_sync(session_id).unwrap();
    assert!(events.iter().any(|event| event["type"].as_str() == Some(charliebot_core::event_types::HANDLER_RESULT)));
    let projection = features.store.get_message_projection(session_id).unwrap();
    assert!(projection.is_some());
  }

  #[tokio::test]
  async fn cron_editor_switches_backend_on_the_bound_node_without_creating_a_session() {
    let (home, features) = feature_context_with_extra_backends(&["synthetic-alt"]);
    let node = manager(&features, "synthetic-bound-node").await.unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    std::fs::write(
      &path,
      charliebot_core::yaml::dump(&json!({
        "cron": "0 8 * * *",
        "handler": "synthetic_handler",
        "session_id": node.id,
        "backend": "synthetic",
      }))
      .unwrap(),
    )
    .unwrap();

    let (status, updated) =
      send(&routes(&features), "PUT", "/api/cron/tasks/synthetic-task", json!({"backend": "synthetic-alt"})).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["backend"], "synthetic-alt");
    assert_eq!(features.store.get_session(&node.id).await.unwrap().unwrap().backend, "synthetic-alt");
    let sessions = features.store.list_sessions(None, None, None).unwrap();
    assert_eq!(sessions.rows.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), [node.id.as_str()]);
    assert_eq!(
      serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(path).unwrap()).unwrap()["backend"],
      "synthetic-alt"
    );
  }

  #[tokio::test]
  async fn cron_editor_refuses_a_busy_backend_switch_before_writing_yaml() {
    let (home, features) = feature_context_with_extra_backends(&["synthetic-alt"]);
    let node = manager(&features, "synthetic-busy-bound-node").await.unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    let original = charliebot_core::yaml::dump(&json!({
      "cron": "0 8 * * *",
      "handler": "synthetic_handler",
      "session_id": node.id,
      "backend": "synthetic",
    }))
    .unwrap();
    std::fs::write(&path, &original).unwrap();
    features.store.mark_busy(&node.id, charliebot_store::metadata::UtcDateTime::now()).unwrap();

    let (status, body) =
      send(&routes(&features), "PUT", "/api/cron/tasks/synthetic-task", json!({"backend": "synthetic-alt"})).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["detail"].as_str().is_some_and(|detail| detail.contains("running work")));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    assert_eq!(features.store.get_session(&node.id).await.unwrap().unwrap().backend, "synthetic");
  }

  #[tokio::test]
  async fn enabling_a_bound_archived_node_restores_it_with_the_cron_enable_reason() {
    use charliebot_registry::caller::CallerIdentity;

    let (home, features) = feature_context();
    let node = manager(&features, "synthetic-restore-bound-node").await.unwrap();
    features.startup.insert(charliebot_tasks::completion::TaskCompletion::new(Arc::clone(&features))).unwrap();
    let completion = features.startup.require::<charliebot_tasks::completion::TaskCompletion>().unwrap();
    let sequence_completion: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = completion.clone();
    features
      .startup
      .insert(charliebot_tasks::dispatch::Dispatcher::new(
        Arc::clone(&features.store),
        Arc::clone(&features),
        sequence_completion,
        run_completion,
      ))
      .unwrap();
    completion
      .cancel_task(&node.id, "synthetic-archive", "synthetic archive", &CallerIdentity::Operator { session_id: None })
      .await
      .unwrap();
    features.store.archive_session(&node.id).await.unwrap();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    std::fs::write(
      &path,
      charliebot_core::yaml::dump(&json!({
        "cron": "0 8 * * *",
        "handler": "synthetic_handler",
        "session_id": node.id,
        "enabled": false,
      }))
      .unwrap(),
    )
    .unwrap();

    let (status, enabled) =
      send(&routes(&features), "PUT", "/api/cron/tasks/synthetic-task", json!({"enabled": true})).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled["enabled"], true);
    assert_eq!(features.store.task_state(&node.id).unwrap(), "open");
    let reopened = features.store.load_all_facts(&node.id).unwrap();
    let reasons = reopened
      .iter()
      .filter(|event| event["type"].as_str() == Some(event_types::TASK_REOPENED))
      .map(|event| event["reason"].as_str().unwrap())
      .collect::<Vec<_>>();
    assert_eq!(reasons, ["cron enable"]);
  }

  #[tokio::test]
  async fn task_listing_keeps_the_binding_and_mode_fields_used_by_clients() {
    let (home, features) = feature_context();
    let path = home.path().join("config.d/cron.d/synthetic-task.yaml");
    std::fs::write(
      &path,
      charliebot_core::yaml::dump(&json!({
        "cron": "0 8 * * *",
        "handler": "synthetic_handler",
        "session_id": "synthetic-bound-node",
        "mode": "worker",
        "project": "synthetic-project",
        "allow_failure": true,
        "enabled": false,
      }))
      .unwrap(),
    )
    .unwrap();

    let (status, listed) = send(&routes(&features), "GET", "/api/cron/tasks", json!(null)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["session_id"], "synthetic-bound-node");
    assert_eq!(listed[0]["mode"], "worker");
    assert_eq!(listed[0]["project"], "synthetic-project");
    assert_eq!(listed[0]["allow_failure"], true);
    assert_eq!(listed[0]["enabled"], false);
  }
}
