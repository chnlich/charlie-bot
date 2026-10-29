//! Fixtures for the web reply tests: a store-backed feature context over a temporary home, with this package's
//! registrations and the origin slots a thread session carries, and the route calls the tests make.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use charliebot_core::config::{BackendOption, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, RunningTurnInputs, StartupState};
use charliebot_store::ChatEvent;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{CreateTask, Store, Subscription, TaskCaller, session_channel};
use http::{Method, Request};
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::reply;

/// The plan-registry reader the server's artifacts package registers; these tests read the same file the same way.
pub fn read_plans_fixture(session_dir: &Path) -> anyhow::Result<Option<Vec<Value>>> {
  let path = session_dir.join("plans.json");
  let text = match fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(anyhow::anyhow!("{} is unreadable: {error}", path.display())),
  };
  let data: Value = serde_json::from_str(&text)
    .map_err(|error| anyhow::anyhow!("{} holds an unreadable plan registry: {error}", path.display()))?;
  let plans = data.get("plans").ok_or_else(|| anyhow::anyhow!("{} has no plans key", path.display()))?;
  let plans =
    plans.as_array().cloned().ok_or_else(|| anyhow::anyhow!("{} holds plans that are not a list", path.display()))?;
  Ok(Some(plans))
}

fn register_plans(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.plan_registry(read_plans_fixture)
}

/// The origin slots a thread session carries; the web test only reads them, so each is a null field.
fn register_origins(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "slack",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "slack_origin", default: Value::Null }],
  })?;
  registry.metadata_slot(MetadataSlot {
    owner: "discord",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "discord_origin", default: Value::Null }],
  })
}

/// A store over a temporary home, with the web reply package and the origin slots registered, and a
/// `RunningTurnInputs` stub that answers the ids `set_running` last named, for every session.
pub struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
  running: Arc<Mutex<Vec<String>>>,
}

impl Home {
  pub fn new() -> Self {
    Self::with_config("")
  }

  pub fn with_config(config_yaml: &str) -> Self {
    let running = Arc::new(Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().expect("temporary directory exists");
    fs::write(dir.path().join("config.yaml"), config_yaml).expect("the synthetic config writes");
    fs::write(dir.path().join(CREDENTIALS_FILENAME), "charliebot:\n  access_key: synthetic-access-key\n")
      .expect("the synthetic access key writes");
    let mut registry = Registry::new();
    registry
      .register_package("src.features.user_text_checks", charliebot_user_text_checks::register)
      .expect("the user text checks package registers");
    registry.register_package("src.features.web_reply", crate::register).expect("the web reply package registers");
    registry.register_package("tests.origins", register_origins).expect("the origin slots register");
    registry.register_package("tests.plans", register_plans).expect("the plan registry reader registers");
    let registry = Arc::new(registry);
    let mut loaded = registry.load_config(dir.path()).expect("an empty home loads with the defaults");
    // Task creation takes the first configured backend when a create names none.
    loaded.backends.options.push(BackendOption {
      id: "synthetic-web".to_string(),
      label: "Synthetic web".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    let reload_config = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "web_reply_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "web_reply_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = StartupState::default();
    let reader = Arc::clone(&running);
    let inputs = RunningTurnInputs::new(move |_| reader.lock().expect("the stub lock is never poisoned").clone());
    startup.insert(inputs).expect("the stub inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
    Self { _dir: dir, features, running }
  }

  /// The temporary home the store writes under: the CharlieBot home, which the config names.
  pub fn root(&self) -> PathBuf {
    self._dir.path().to_path_buf()
  }

  /// Names the input events the running turn answers, for every session.
  /// Writes a manager run record that the store lists as the session's newest run.
  pub fn launch_run(&self, session_id: &str, run_id: &str) {
    let run_path = self.features.store.run_metadata_path(session_id, run_id);
    std::fs::create_dir_all(run_path.parent().expect("run directory")).expect("run directory");
    let run = json!({"id": run_id, "session_id": session_id, "kind": "manager_turn", "pid": 424242, "pid_start": "1"});
    std::fs::write(&run_path, run.to_string()).expect("run record");
  }

  pub fn set_running(&self, input_event_ids: &[&str]) {
    let ids = input_event_ids.iter().map(|id| id.to_string()).collect();
    *self.running.lock().expect("the stub lock is never poisoned") = ids;
  }

  /// A root manager session named `name`, created as the operator creates one.
  pub async fn manager(&self, name: &str, origin: Option<(&str, Value)>) -> String {
    let mut slots = Map::new();
    if let Some((owner_field, value)) = origin {
      slots.insert(owner_field.to_string(), value);
    }
    let request = CreateTask {
      request_id: format!("root-{name}"),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some(name.to_string()),
      backend: None,
      group: None,
      session_id: None,
    };
    self.create(request, slots).await
  }

  /// A worker node under `parent`.
  pub async fn worker(&self, parent: &str, name: &str) -> String {
    let request = CreateTask {
      request_id: format!("worker-{name}"),
      task_parent_id: Some(parent.to_string()),
      profile: TaskProfile::Worker,
      task: None,
      name: Some(name.to_string()),
      backend: None,
      group: None,
      session_id: None,
    };
    self.create(request, Map::new()).await
  }

  /// A manager node under `parent`.
  pub async fn child(&self, parent: &str, name: &str) -> String {
    let request = CreateTask {
      request_id: format!("child-{name}"),
      task_parent_id: Some(parent.to_string()),
      profile: TaskProfile::Manager,
      task: None,
      name: Some(name.to_string()),
      backend: None,
      group: None,
      session_id: None,
    };
    self.create(request, Map::new()).await
  }

  async fn create(&self, request: CreateTask, slots: Map<String, Value>) -> String {
    let meta: SessionMetadata =
      self.features.store.create_task(request, TaskCaller::Operator, slots).await.expect("the task is created");
    meta.id
  }

  /// The session's chat events in log order.
  pub fn events(&self, session_id: &str) -> Vec<Value> {
    self
      .features
      .store
      .load_chat_events_sync(session_id)
      .expect("the chat log reads")
      .iter()
      .map(ChatEvent::to_value)
      .collect()
  }

  /// Appends one chat event and returns it with its stamped id.
  pub async fn append(&self, session_id: &str, event: Value) -> Value {
    self.features.store.append_chat_event(session_id, event).await.expect("the chat event appends").to_value()
  }

  /// The nudges the audit posted to the session, in log order.
  pub fn nudges(&self, session_id: &str) -> Vec<Value> {
    self.events(session_id).into_iter().filter(|event| reply::nudge_of(event).is_some()).collect()
  }

  /// The session's metadata as the store reads it.
  pub async fn meta(&self, session_id: &str) -> SessionMetadata {
    self.features.store.get_session(session_id).await.expect("the metadata reads").expect("the session exists")
  }

  /// A subscription to the session's channel, for the broadcast frames.
  pub fn subscribe(&self, session_id: &str) -> Subscription {
    self.features.store.subscribe(&session_channel(session_id))
  }

  /// The routes answering one request, as the server mounts them.
  pub async fn send(&self, method: Method, uri: &str, body: Option<Value>) -> (http::StatusCode, Value) {
    let router: Router = crate::routes(&self.features);
    let request = Request::builder()
      .method(method)
      .uri(uri)
      .header(http::header::CONTENT_TYPE, "application/json")
      .body(match body {
        Some(body) => Body::from(body.to_string()),
        None => Body::empty(),
      })
      .expect("the request builds");
    let response = router.oneshot(request).await.expect("the router is infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
  }
}

/// A `user` chat event with `content`.
pub fn user_event(content: &str) -> Value {
  json!({"type": "user", "content": content, "message": {"role": "user", "content": [{"type": "text", "text": content}]}})
}

/// The id the store stamped on an appended event.
pub fn id_of(event: &Value) -> &str {
  event["id"].as_str().expect("an appended event carries its id")
}

/// A `master_done` event that answers `input_event_ids` and ended with `exit_code`.
pub fn master_done(input_event_ids: &[&str], exit_code: i32) -> Value {
  json!({"type": "master_done", "exit_code": exit_code, "input_event_ids": input_event_ids})
}
