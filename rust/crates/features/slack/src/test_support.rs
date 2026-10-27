//! Store-backed Slack test fixtures and a local Slack Web API and Socket Mode fake.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ws::Message as AxumMessage;
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, serve};
use charliebot_core::config::BackendOption;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, RunningTurnInputs, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{CreateTask, Store, TaskCaller};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher};
use charliebot_triggers::triggers::TriggerManager;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::slack_client::SlackClient;

/// Records the pending inputs every manager wake hands to an agent executor.
#[derive(Default)]
pub(crate) struct RecordingExecutor {
  pub launches: Mutex<Vec<(String, Vec<Value>)>>,
}

impl DispatchExecutor for RecordingExecutor {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [charliebot_store::ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, charliebot_store::StoreResult<Option<String>>> {
    Box::pin(async move {
      self
        .launches
        .lock()
        .expect("the test launch log is never poisoned")
        .push((session_id.to_string(), pending_inputs.iter().map(|event| event.to_value()).collect()));
      Ok(None)
    })
  }
}

/// A store, dispatcher, trigger manager and config over a temporary synthetic home.
pub(crate) struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
  pub executor: Arc<RecordingExecutor>,
}

impl Home {
  pub(crate) fn new() -> Self {
    let dir = tempfile::tempdir().expect("the synthetic home exists");
    std::fs::write(dir.path().join("config.yaml"), "slack:\n  allowed_user_ids:\n    - U_ALLOWED\n")
      .expect("the synthetic config writes");
    std::fs::write(
      dir.path().join("credentials.yaml"),
      "slack:\n  bot_token: synthetic-bot\n  app_token: synthetic-app\n",
    )
    .expect("the synthetic credentials write");

    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.slack", crate::register).expect("the Slack package registers");
    let registry = Arc::new(registry);
    let mut loaded = registry.load_config(dir.path()).expect("the synthetic Slack config loads");
    loaded.backends.options.push(BackendOption {
      id: "synthetic-slack".to_string(),
      label: "Synthetic Slack".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    let config_reload = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "slack_test_config_reload_failed",
      Box::new(move || Ok(config_reload.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "slack_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = StartupState::default();
    startup.insert(RunningTurnInputs::new(|_| Vec::new())).expect("running inputs insert once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });

    features.startup.insert(TaskCompletion::new(Arc::clone(&features))).expect("task completion inserts once");
    let completion = features.startup.require::<TaskCompletion>().expect("task completion is present");
    let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
    let run: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = completion;
    let executor = Arc::new(RecordingExecutor::default());
    let dispatcher = Dispatcher::new(Arc::clone(&features.store), Arc::clone(&features), sequence, run);
    let dyn_executor: Arc<dyn DispatchExecutor> = executor.clone();
    dispatcher.set_executor(Some(dyn_executor));
    features.startup.insert(dispatcher).expect("dispatcher inserts once");
    features
      .startup
      .insert(Arc::new(TriggerManager::new(Arc::clone(&features))))
      .expect("trigger manager inserts once");

    Self { _dir: dir, features, executor }
  }

  pub(crate) async fn thread_session(&self, session_id: &str, thread_ts: &str) -> SessionMetadata {
    let request = CreateTask {
      request_id: format!("slack-thread-{session_id}"),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some(format!("Slack session {session_id}")),
      backend: None,
      group: None,
      session_id: Some(session_id.to_string()),
    };
    let slots = Map::from_iter([(
      "slack_origin".to_string(),
      json!({"team_id": "T_TEST", "channel_id": "C_TEST", "thread_ts": thread_ts}),
    )]);
    self.features.store.create_task(request, TaskCaller::System, slots).await.expect("the thread session creates")
  }

  pub(crate) fn launches(&self) -> Vec<(String, Vec<Value>)> {
    self.executor.launches.lock().expect("the test launch log is never poisoned").clone()
  }
}

#[derive(Clone, Copy)]
pub(crate) enum SocketPlan {
  Disconnect,
  Drop,
  Hold,
}

#[derive(Default)]
pub(crate) struct FakeState {
  calls: Mutex<Vec<(String, String, Value)>>,
  thread_messages: Mutex<Vec<Value>>,
  posts: Mutex<Vec<Value>>,
  plans: Mutex<VecDeque<SocketPlan>>,
  open_failures: AtomicUsize,
  open_calls: AtomicUsize,
  no_reaction: AtomicBool,
  accepted_post_failure_once: AtomicBool,
  fail_post_keys: Mutex<HashSet<String>>,
  socket_connections: AtomicUsize,
  socket_times: Mutex<Vec<Instant>>,
  connected: Notify,
  socket_url: Mutex<Option<String>>,
}

/// A local Slack HTTP and Socket Mode fake for SlackClient and reconnect-loop tests.
pub(crate) struct FakeApi {
  pub client: Arc<SlackClient>,
  pub state: Arc<FakeState>,
  task: tokio::task::JoinHandle<()>,
}

impl FakeApi {
  pub(crate) async fn start(plans: &[SocketPlan]) -> Self {
    let state = Arc::new(FakeState::default());
    state.plans.lock().expect("the test socket plan is never poisoned").extend(plans.iter().copied());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("the local Slack fake binds");
    let address = listener.local_addr().expect("the local Slack fake has an address");
    let api_base = format!("http://{address}/api/");
    let socket_url = format!("ws://{address}/socket");
    let app = Router::new()
      .route("/api/apps.connections.open", post(open_connection))
      .route("/api/auth.test", post(auth_test))
      .route("/api/chat.postMessage", post(post_message))
      .route("/api/reactions.add", post(reaction))
      .route("/api/reactions.remove", post(reaction))
      .route("/api/conversations.replies", get(thread_replies))
      .route("/api/conversations.info", get(channel_info))
      .route("/socket", get(socket))
      .with_state(Arc::clone(&state));
    let task = tokio::spawn(async move {
      serve(listener, app).await.expect("the local Slack fake serves");
    });
    let http = charliebot_compat::http_client::client(&charliebot_compat::http_client::Settings {
      https_proxy: None,
      no_proxy: Some("127.0.0.1,localhost".to_string()),
      cert_file: None,
    })
    .expect("the local Slack test HTTP client builds");
    let client =
      Arc::new(SlackClient::with_client(http, "synthetic-bot".to_string(), "synthetic-app".to_string(), api_base));
    state.socket_url.lock().expect("socket url slot is never poisoned").replace(socket_url);
    Self { client, state, task }
  }

  pub(crate) fn set_open_failures(&self, failures: usize) {
    self.state.open_failures.store(failures, Ordering::SeqCst);
  }

  pub(crate) fn set_no_reaction(&self) {
    self.state.no_reaction.store(true, Ordering::SeqCst);
  }

  pub(crate) fn set_accepted_post_failure_once(&self) {
    self.state.accepted_post_failure_once.store(true, Ordering::SeqCst);
  }

  pub(crate) fn set_thread_messages(&self, messages: Vec<Value>) {
    *self.state.thread_messages.lock().expect("the fake thread is never poisoned") = messages;
  }

  pub(crate) fn calls(&self) -> Vec<(String, String, Value)> {
    self.state.calls.lock().expect("the fake call log is never poisoned").clone()
  }

  pub(crate) fn posts(&self) -> Vec<Value> {
    self.state.posts.lock().expect("the fake post log is never poisoned").clone()
  }

  pub(crate) async fn wait_for_sockets(&self, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
      while self.state.socket_connections.load(Ordering::SeqCst) < count {
        self.state.connected.notified().await;
      }
    })
    .await
    .expect("the expected Socket Mode connection arrives");
  }

  pub(crate) fn open_calls(&self) -> usize {
    self.state.open_calls.load(Ordering::SeqCst)
  }

  pub(crate) fn socket_times(&self) -> Vec<Instant> {
    self.state.socket_times.lock().expect("the socket clock log is never poisoned").clone()
  }
}

impl Drop for FakeApi {
  fn drop(&mut self) {
    self.task.abort();
  }
}

impl FakeState {
  fn record(&self, method: &str, path: &str, body: Value) {
    self.calls.lock().expect("the fake call log is never poisoned").push((method.to_string(), path.to_string(), body));
  }
}

async fn open_connection(State(state): State<Arc<FakeState>>) -> Response {
  state.open_calls.fetch_add(1, Ordering::SeqCst);
  let failures = state.open_failures.load(Ordering::SeqCst);
  if failures > 0 && state.open_calls.load(Ordering::SeqCst) <= failures {
    state.record("POST", "/api/apps.connections.open", Value::Null);
    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": "seeded_failure"}))).into_response();
  }
  state.record("POST", "/api/apps.connections.open", Value::Null);
  let url = state
    .socket_url
    .lock()
    .expect("socket url slot is never poisoned")
    .clone()
    .expect("socket URL is set before requests are served");
  Json(json!({"ok": true, "url": url})).into_response()
}

async fn auth_test(State(state): State<Arc<FakeState>>) -> Json<Value> {
  state.record("POST", "/api/auth.test", Value::Null);
  Json(json!({"ok": true, "user_id": "U_BOT", "bot_id": "B_BOT"}))
}

async fn post_message(State(state): State<Arc<FakeState>>, Json(body): Json<Value>) -> Response {
  state.record("POST", "/api/chat.postMessage", body.clone());
  state.posts.lock().expect("the fake post log is never poisoned").push(body.clone());
  let key = body.pointer("/metadata/event_payload/key").and_then(Value::as_str).unwrap_or_default();
  let ts = format!("1700000000.{:06}", state.posts.lock().expect("the fake post log is never poisoned").len());
  let mut message = json!({
    "ts": ts,
    "user": "U_BOT",
    "bot_id": "B_BOT",
    "text": body["text"],
    "metadata": body["metadata"],
  });
  state.thread_messages.lock().expect("the fake thread is never poisoned").push(message.clone());
  let should_fail = state.fail_post_keys.lock().expect("the keyed post failures are never poisoned").contains(key)
    || state.accepted_post_failure_once.swap(false, Ordering::SeqCst);
  if should_fail {
    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": "synthetic response timeout"})))
      .into_response();
  }
  message["ok"] = json!(true);
  Json(json!({"ok": true, "message": message})).into_response()
}

async fn reaction(State(state): State<Arc<FakeState>>, request: Request<Body>) -> Response {
  let path = request.uri().path().to_string();
  let bytes = to_bytes(request.into_body(), usize::MAX).await.expect("the reaction request body reads");
  let body: Value = serde_json::from_slice(&bytes).expect("the reaction request is JSON");
  state.record("POST", &path, body);
  if path == "/api/reactions.remove" && state.no_reaction.load(Ordering::SeqCst) {
    Json(json!({"ok": false, "error": "no_reaction"})).into_response()
  } else {
    Json(json!({"ok": true})).into_response()
  }
}

async fn thread_replies(
  State(state): State<Arc<FakeState>>,
  Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
  state.record("GET", "/api/conversations.replies", json!(query));
  let messages = state.thread_messages.lock().expect("the fake thread is never poisoned").clone();
  Json(json!({"ok": true, "messages": messages, "has_more": false}))
}

async fn channel_info(
  State(state): State<Arc<FakeState>>,
  Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
  state.record("GET", "/api/conversations.info", json!(query));
  Json(json!({"ok": true, "channel": {"id": query.get("channel"), "name": "synthetic-room"}}))
}

async fn socket(State(state): State<Arc<FakeState>>, upgrade: WebSocketUpgrade) -> Response {
  upgrade
    .on_upgrade(move |mut websocket| async move {
      state.socket_connections.fetch_add(1, Ordering::SeqCst);
      state.socket_times.lock().expect("the socket clock log is never poisoned").push(Instant::now());
      state.connected.notify_one();
      websocket
        .send(AxumMessage::Text(r#"{"type":"hello","num_connections":1}"#.into()))
        .await
        .expect("the hello frame sends");
      let plan =
        state.plans.lock().expect("the test socket plan is never poisoned").pop_front().unwrap_or(SocketPlan::Hold);
      match plan {
        SocketPlan::Disconnect => {
          websocket
            .send(AxumMessage::Text(r#"{"type":"disconnect"}"#.into()))
            .await
            .expect("the disconnect frame sends");
          while websocket.recv().await.is_some() {}
        }
        SocketPlan::Drop => drop(websocket),
        SocketPlan::Hold => while websocket.recv().await.is_some() {},
      }
    })
    .into_response()
}
