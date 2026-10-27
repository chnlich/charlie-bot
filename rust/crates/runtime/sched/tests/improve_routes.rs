//! The internal improve routes against a synthetic feature context, ported from `tests/test_cli_improve.py` and
//! `tests/test_task_sequences.py`.

#[path = "../../tasks/tests/recovery_support.rs"]
mod recovery_support;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{BackendTraits, ResumeStyle};
use charliebot_registry::context::StartupState;
use charliebot_registry::{FeatureContext, Registry, RegistryError, store_hooks};
use charliebot_sched::improve::api;
use charliebot_sched::improve::command::{self, Reservation};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use serde_json::{Value, json};
use tower::ServiceExt;

const SESSION: &str = "synthetic-improve-route-session";
const START: &str = "/api/internal/improve";
const STOP: &str = "/api/internal/improve/stop";

fn register_route_backend(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(recovery_support::scripted_type(
    "cc-claude",
    BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    None,
  ))
}

fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_config(None)
}

fn feature_context_with_config(config_yaml: Option<&str>) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary home");
  std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
  if let Some(config_yaml) = config_yaml {
    std::fs::write(home.path().join(CONFIG_FILENAME), config_yaml).expect("config file");
  }
  let mut registry = Registry::new();
  if config_yaml.is_some() {
    registry
      .register_package("test.improve_route_backend", register_route_backend)
      .expect("scripted backend registers");
  }
  let registry = Arc::new(registry);
  let config_value = registry.load_config(home.path()).expect("test config");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.path().to_path_buf();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "improve_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credential_home = home.path().to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.path().join(CREDENTIALS_FILENAME),
    "improve_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let startup = StartupState::default();
  startup.insert(BackendPool::default()).expect("the pool inserts once");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
  (home, features)
}

/// A manager session with no user request: its chat stream is empty.
fn add_manager(features: &FeatureContext) {
  let metadata = SessionMetadata::new(SESSION, "Synthetic improve session", TaskProfile::Manager);
  let directory = features.store.session_dir(SESSION);
  std::fs::create_dir_all(directory.join("data")).expect("session data directory");
  std::fs::write(directory.join("metadata.json"), metadata.to_json_string(&features.registry.metadata_slots(), true))
    .expect("session metadata");
  std::fs::write(directory.join("data").join("chat_events.jsonl"), "").expect("empty event stream");
  features.store.invalidate_cache(SESSION);
}

/// The user request that makes the manager's delegation authorized.
async fn add_user_request(features: &FeatureContext) {
  features
    .store
    .append_chat_event(SESSION, json!({"type": "user", "content": "Improve the loop."}))
    .await
    .expect("the user request is appended");
}

fn start_body(session_id: &str, backend: Option<&str>) -> Value {
  json!({
    "session_id": session_id,
    "repo_path": "/tmp/repo",
    "base_branch": "main",
    "goal": "optimize",
    "backend": backend,
  })
}

async fn post(features: &Arc<FeatureContext>, path: &str, body: Value) -> (StatusCode, Value) {
  let request = Request::builder()
    .method(Method::POST)
    .uri(path)
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body.to_string()))
    .expect("the request builds");
  let response = api::routes(features).oneshot(request).await.expect("the router answers");
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_endpoint_returns_404_for_missing_session
async fn start_for_a_missing_session_is_not_found() {
  let (_home, features) = feature_context();

  let (status, body) = post(&features, START, start_body("missing-session", None)).await;

  assert_eq!(status, StatusCode::NOT_FOUND);
  assert_eq!(body["detail"], "Session not found");
}

#[tokio::test]
// Python: tests/test_task_sequences.py::test_improve_without_authorization_is_forbidden_not_a_server_error
// Python: tests/test_internal_delegate_takeoff.py::test_improve_stays_blocked_without_takeoff
async fn start_without_a_user_request_is_forbidden() {
  let (_home, features) = feature_context();
  add_manager(&features);

  let (status, body) = post(&features, START, start_body(SESSION, None)).await;

  assert_eq!(status, StatusCode::FORBIDDEN);
  let detail = body["detail"].as_str().unwrap_or_default();
  assert!(detail.contains("no user request"), "the refusal names the missing request: {body}");
  let cfg = features.config.get().expect("the test config");
  assert!(!command::active_lock_path(&cfg, SESSION).exists(), "a refused takeoff leaves no active lock");
  assert!(!command::loops_dir(&cfg, SESSION).exists(), "a refused takeoff creates no loop directory");
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_endpoint_returns_400_for_invalid_backend
async fn start_with_an_unknown_backend_is_a_bad_request_and_reserves_nothing() {
  let (_home, features) = feature_context();
  add_manager(&features);
  add_user_request(&features).await;

  let (status, body) = post(&features, START, start_body(SESSION, Some("no-such-backend"))).await;

  assert_eq!(status, StatusCode::BAD_REQUEST);
  let detail = body["detail"].as_str().unwrap_or_default();
  assert!(detail.contains("no-such-backend"), "the refusal names the backend: {body}");
  let cfg = features.config.get().expect("the test config");
  assert!(!command::active_lock_path(&cfg, SESSION).exists(), "a refused backend leaves no active lock");
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_endpoint_returns_409_for_running_loop
async fn starting_a_second_loop_returns_conflict_with_the_running_loop_id() {
  let (_home, features) = feature_context_with_config(Some(
    "backends:\n  options:\n    - id: claude-fable\n      label: Synthetic Claude\n      type: cc-claude\n      model: claude-fable-model\n",
  ));
  add_manager(&features);
  add_user_request(&features).await;
  let cfg = features.config.get().expect("the test config");
  let reservation = Reservation {
    goal: "optimize",
    work_branch: "improve/first",
    repo_path: "/tmp/repo",
    plan: None,
    base_branch: Some("main"),
    merge_back: false,
    resolved_backend: "claude-fable",
    resolved_model: "claude-fable-model",
    backend_from_pool: false,
  };
  let first = command::reserve_loop_state(&cfg, SESSION, &reservation).expect("the first loop reserves");

  let (status, body) = post(&features, START, start_body(SESSION, Some("claude-fable"))).await;

  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(
    body["detail"],
    format!("Loop {} is already running for this session. Use charliebot improve-stop first.", first.loop_id)
  );
  assert_eq!(
    command::find_running_loop(&cfg, SESSION).expect("the running loop reads").unwrap().loop_id,
    first.loop_id
  );
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_stop_endpoint_404_for_missing_session
async fn stop_for_a_missing_session_is_not_found() {
  let (_home, features) = feature_context();

  let (status, body) = post(&features, STOP, json!({"session_id": "missing-session"})).await;

  assert_eq!(status, StatusCode::NOT_FOUND);
  assert_eq!(body["detail"], "Session not found");
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_stop_endpoint_409_without_running_loop
async fn stop_without_a_running_loop_is_a_conflict() {
  let (_home, features) = feature_context();
  add_manager(&features);

  let (status, body) = post(&features, STOP, json!({"session_id": SESSION})).await;

  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["detail"], "No active improve loop in this session");
}

#[tokio::test]
// Python: tests/test_cli_improve.py::test_improve_stop_endpoint_marks_loop_stopped
async fn stop_marks_the_running_loop_stopped() {
  let (_home, features) = feature_context();
  add_manager(&features);
  let cfg = features.config.get().expect("the test config");
  let reservation = Reservation {
    goal: "optimize",
    work_branch: "improve/test",
    repo_path: "/tmp/repo",
    plan: None,
    base_branch: Some("main"),
    merge_back: false,
    resolved_backend: "",
    resolved_model: "",
    backend_from_pool: false,
  };
  let state = command::reserve_loop_state(&cfg, SESSION, &reservation).expect("the loop reserves");

  let (status, body) = post(&features, STOP, json!({"session_id": SESSION})).await;

  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, json!({"status": "stopped", "session_id": SESSION}));
  let stored = command::require_loop_state(&cfg, SESSION, state.loop_id).expect("the loop state reads");
  assert_eq!(stored.status, "stopped");
}
