//! Every agent-work route in the composed server asks the colleague gate before it starts or queues work.

use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::run_token::{self, RunTokenClaims};
use charliebot_registry::colleague_gate::ColleagueGate;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{RegisterFn, Registry};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_triggers::triggers::TriggerManager;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ACCESS_KEY: &str = "synthetic-access-key";
const REFUSAL: &str = "refused: synthetic colleague thread";

const ROUTE_PACKAGES: &[(&str, RegisterFn)] = &[
  ("runtime.session_api", charliebot_session_api::register),
  ("runtime.tasks", charliebot_tasks::register),
  ("runtime.triggers", charliebot_triggers::register),
  ("features.chat_threads", charliebot_chat_threads::register),
  ("features.cron", charliebot_cron::register),
  ("features.slack", charliebot_slack::register),
  ("features.improve", charliebot_improve::register),
];

struct Server {
  _home: TempDir,
  features: Arc<FeatureContext>,
}

impl Server {
  fn new() -> Self {
    let home = tempfile::tempdir().expect("the synthetic home is available");
    fs::write(home.path().join(CONFIG_FILENAME), "").expect("the empty config writes");
    fs::write(home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
      .expect("the synthetic access key writes");

    let mut registry = Registry::new();
    for &(package, register) in ROUTE_PACKAGES {
      registry.register_package(package, register).expect("the route package registers");
    }
    let registry = Arc::new(registry);
    let config_value = registry.load_config(home.path()).expect("the synthetic config loads");
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry)))
        .expect("the store opens over the synthetic home"),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let credentials_home = config_home.clone();
    let features = Arc::new(FeatureContext {
      config: Arc::new(HotReload::new(
        config_value.config_file(),
        "colleague_work_routes_config_reload_failed",
        Box::new(move || config_registry.load_config(&config_home)),
      )),
      credentials: Arc::new(HotReload::new(
        home.path().join(CREDENTIALS_FILENAME),
        "colleague_work_routes_credentials_reload_failed",
        Box::new(move || load_credentials(&credentials_home)),
      )),
      store,
      registry,
      startup: Arc::new(StartupState::default()),
    });

    let session = SessionMetadata::new("slack-thread", "Synthetic Slack thread", TaskProfile::Manager);
    let session_dir = features.store.session_dir(&session.id);
    fs::create_dir_all(session_dir.join("data")).expect("the session directory is created");
    fs::write(session_dir.join(METADATA_FILE_NAME), session.to_json_string(&features.registry.metadata_slots(), true))
      .expect("the session metadata writes");
    fs::write(session_dir.join("data/chat_events.jsonl"), "").expect("the chat event log writes");
    features.store.invalidate_cache(&session.id);

    let run_id = "synthetic-run";
    let run_path = features.store.run_metadata_path(&session.id, run_id);
    fs::create_dir_all(run_path.parent().expect("the run directory has a parent"))
      .expect("the run directory is created");
    fs::write(
      run_path,
      json!({
        "id": run_id,
        "session_id": session.id.as_str(),
        "kind": "manager_turn",
        "input_event_ids": [],
        "pid": 424242,
        "pid_start": "12345"
      })
      .to_string(),
    )
    .expect("the active run record writes");

    let gate: Arc<dyn ColleagueGate> = Arc::new(RefuseAgentWork);
    features.startup.insert(gate).expect("the colleague gate registers");
    Server { _home: home, features }
  }

  fn app(&self) -> Router {
    self
      .features
      .registry
      .routers()
      .iter()
      .fold(Router::new(), |router, route| router.merge((route.build)(&self.features)))
  }
}

struct RefuseAgentWork;

impl ColleagueGate for RefuseAgentWork {
  fn run_read_only<'a>(
    &'a self,
    _features: &'a charliebot_registry::FeatureContext,
    _session_id: &'a str,
    _run_id: &'a str,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(async { false })
  }

  fn refuse_agent_work<'a>(
    &'a self,
    _features: &'a FeatureContext,
    caller: &'a charliebot_registry::caller::CallerIdentity,
  ) -> Pin<Box<dyn Future<Output = Option<&'static str>> + Send + 'a>> {
    Box::pin(
      async move { matches!(caller, charliebot_registry::caller::CallerIdentity::Agent { .. }).then_some(REFUSAL) },
    )
  }

  fn admits_wake<'a>(
    &'a self,
    _features: &'a FeatureContext,
    _session_id: &'a str,
    _label: &'a str,
  ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
    Box::pin(async { true })
  }
}

#[tokio::test]
async fn one_test_refuses_every_agent_work_entry_with_403() {
  let server = Server::new();
  let token = run_token::sign_run_token(
    &RunTokenClaims {
      session_id: "slack-thread".to_string(),
      run_id: "synthetic-run".to_string(),
      agent: "synthetic-agent".to_string(),
    },
    ACCESS_KEY,
  )
  .expect("the agent token signs");
  let app = server.app();
  let cases = [
    (Method::POST, "/api/chat/slack-thread/message", r#"{"content":"synthetic"}"#),
    (Method::POST, "/api/sessions/slack-thread/retry", r#"{"request_id":"retry","run_id":"synthetic-run"}"#),
    (
      Method::POST,
      "/api/internal/session-message",
      r#"{"session_id":"slack-thread","target_session_id":"target","content":"synthetic"}"#,
    ),
    (
      Method::POST,
      "/api/internal/delegate",
      r#"{"session_id":"slack-thread","description":"synthetic work","task_type":"script-run"}"#,
    ),
    (Method::POST, "/api/cron/tasks", "{}"),
    (Method::PUT, "/api/cron/tasks/synthetic-update", "{}"),
    (Method::POST, "/api/cron/tasks/synthetic-run/run", "{}"),
    (Method::POST, "/api/internal/improve", "{}"),
    (Method::POST, "/api/internal/schedule-trigger", "{}"),
    (Method::POST, "/api/sessions/", r#"{"request_id":"child","task_parent_id":"slack-thread","profile":"manager"}"#),
    (Method::POST, "/api/sessions/slack-thread/fork", r#"{"event_index":0}"#),
    (Method::POST, "/api/sessions/slack-thread/elone", r#"{"event_index":0}"#),
  ];

  for (method, path, body) in cases {
    let request = Request::builder()
      .method(&method)
      .uri(path)
      .header(header::AUTHORIZATION, format!("Bearer {token}"))
      .header(header::CONTENT_TYPE, "application/json")
      .body(Body::from(body.to_string()))
      .expect("the request builds");
    let response = app.clone().oneshot(request).await.expect("the router handles the request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the refusal body reads");
    let refusal: Value = serde_json::from_slice(&bytes).expect("the refusal body is JSON");
    assert_eq!(refusal["detail"], REFUSAL, "{method} {path}");
  }
}

#[tokio::test]
async fn trigger_registration_and_session_summary_share_the_pending_count() {
  let server = Server::new();
  let manager = Arc::new(TriggerManager::new(Arc::clone(&server.features)));
  server.features.startup.insert(manager).expect("the trigger manager registers");
  let app = server.app();

  let detail = |app: Router| async move {
    let request =
      Request::builder().uri("/api/sessions/slack-thread").body(Body::empty()).expect("the detail request builds");
    let response = app.oneshot(request).await.expect("the detail route answers");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the detail response reads");
    serde_json::from_slice::<Value>(&bytes).expect("the detail response is JSON")
  };

  let before = detail(app.clone()).await;
  assert_eq!(before["pending_trigger_count"], json!(0));

  let request = Request::builder()
    .method(Method::POST)
    .uri("/api/internal/schedule-trigger")
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(
      json!({
        "session_id": "slack-thread",
        "delay_seconds": 14400,
        "message": "synthetic summary test",
      })
      .to_string(),
    ))
    .expect("the registration request builds");
  let response = app.clone().oneshot(request).await.expect("the registration route answers");
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the registration response reads");
  let registration: Value = serde_json::from_slice(&bytes).expect("the registration response is JSON");
  let trigger_id = registration["trigger_id"].as_str().expect("registration returns the trigger id");

  let pending = detail(app.clone()).await;
  assert!(
    pending["pending_trigger_count"].as_i64().expect("the summary count is an integer") >= 1,
    "the summary includes the trigger registered by the CLI route: {pending}"
  );

  let request = Request::builder()
    .method(Method::POST)
    .uri(format!("/api/internal/triggers/slack-thread/{trigger_id}/cancel"))
    .body(Body::empty())
    .expect("the cancellation request builds");
  let response = app.oneshot(request).await.expect("the cancellation route answers");
  assert_eq!(response.status(), StatusCode::OK);
  let after = detail(server.app()).await;
  assert_eq!(after["pending_trigger_count"], before["pending_trigger_count"]);
}
