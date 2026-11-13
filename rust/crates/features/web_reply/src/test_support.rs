//! Fixtures for the web reply tests: store-backed feature contexts over temporary homes with registrations for each
//! route the tests exercise.

use std::ffi::OsString;
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
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::registry::OpenWaitingRow;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, StartupState};
use charliebot_store::ChatEvent;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{CreateTask, Store, Subscription, TaskCaller, session_channel};
use futures_util::future::BoxFuture;
use http::{Method, Request};
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::reply;

pub use charliebot_core::test_support::{APPROVAL_POLICY_TABLE, write_approval_policy_table};

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

/// The derived Waiting on you rows the stub reader serves, each with the profile home it belongs to.
static DERIVED_ROWS: Mutex<Vec<(PathBuf, OpenWaitingRow)>> = Mutex::new(Vec::new());

/// Set the derived rows that the stub reader serves for the profile home `home`, replacing the earlier ones.
pub fn set_derived_waiting(home: &Path, rows: Vec<OpenWaitingRow>) {
  let mut stored = DERIVED_ROWS.lock().expect("the derived rows are not poisoned");
  stored.retain(|(owner, _)| owner != home);
  stored.extend(rows.into_iter().map(|row| (home.to_path_buf(), row)));
}

fn register_derived_waiting(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.derived_waiting_items(read_derived_waiting)
}

fn read_derived_waiting(features: &FeatureContext) -> BoxFuture<'_, anyhow::Result<Vec<OpenWaitingRow>>> {
  Box::pin(async move {
    let home = features.config.get()?.charliebot_home.clone();
    let stored = DERIVED_ROWS.lock().expect("the derived rows are not poisoned");
    Ok(stored.iter().filter(|(owner, _)| *owner == home).map(|(_, row)| row.clone()).collect())
  })
}

/// The origin slots a thread session carries; the web test only reads them, so each is a null field.
fn register_origins(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "slack",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "slack_origin", default: Value::Null }],
  })?;
  registry.thread_origin_field("slack_origin")?;
  registry.metadata_slot(MetadataSlot {
    owner: "discord",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "discord_origin", default: Value::Null }],
  })?;
  registry.thread_origin_field("discord_origin")
}

/// A store over a temporary home. Each constructor registers the packages and startup services its test path needs.
pub struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
}

#[derive(Clone, Copy)]
enum PackageSet {
  Full,
  StatusOnly,
  ReplyOnly,
}

impl Home {
  pub fn new() -> Self {
    Self::build("", PackageSet::Full)
  }

  pub fn with_config(config_yaml: &str) -> Self {
    Self::build(config_yaml, PackageSet::Full)
  }

  pub fn status_only() -> Self {
    Self::build("", PackageSet::StatusOnly)
  }

  /// The web reply route and its user-text checks, with the running input ids set by `set_running`.
  pub fn reply_only() -> Self {
    Self::build("", PackageSet::ReplyOnly)
  }

  fn build(config_yaml: &str, packages: PackageSet) -> Self {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    fs::write(dir.path().join("config.yaml"), config_yaml).expect("the synthetic config writes");
    if matches!(packages, PackageSet::Full) {
      fs::write(dir.path().join(CREDENTIALS_FILENAME), "charliebot:\n  access_key: synthetic-access-key\n")
        .expect("the synthetic access key writes");
      write_approval_policy_table(dir.path(), APPROVAL_POLICY_TABLE);
    }
    let mut registry = Registry::new();
    if matches!(packages, PackageSet::Full | PackageSet::ReplyOnly) {
      registry
        .register_package("features.user_text_checks", charliebot_user_text_checks::register)
        .expect("the user text checks package registers");
    }
    registry.register_package("features.web_reply", crate::register).expect("the web reply package registers");
    if matches!(packages, PackageSet::Full) {
      registry.register_package("tests.origins", register_origins).expect("the origin slots register");
      registry.register_package("tests.plans", register_plans).expect("the plan registry reader registers");
    }
    if matches!(packages, PackageSet::Full | PackageSet::StatusOnly) {
      registry
        .register_package("tests.derived_waiting", register_derived_waiting)
        .expect("the derived Waiting on you reader registers");
    }
    let registry = Arc::new(registry);
    let mut loaded = registry.load_config(dir.path()).expect("an empty home loads with the defaults");
    // Task creation takes the first configured backend when a create names none.
    loaded.backends.options.push(BackendOption {
      id: "synthetic-web".to_string(),
      label: "Synthetic web".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      billing: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
      visible: true,
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
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .expect("the web-reply crate sits four directories below the checkout");
    startup
      .insert(RepoRoot::new(Some(OsString::from(checkout.as_os_str()))).expect("the test checkout path is set"))
      .expect("the test startup state accepts the checkout");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
    Self { _dir: dir, features }
  }

  /// The temporary home the store writes under: the CharlieBot home, which the config names.
  pub fn root(&self) -> PathBuf {
    self._dir.path().to_path_buf()
  }

  /// Writes the manager run record that the order rule reads as the session's running turn: it received
  /// `input_event_ids` at `started_at` and has not ended.
  pub fn launch_run(&self, session_id: &str, run_id: &str, input_event_ids: &[&str], started_at: &str) {
    let run_path = self.features.store.run_metadata_path(session_id, run_id);
    std::fs::create_dir_all(run_path.parent().expect("run directory")).expect("run directory");
    let run = json!({
      "id": run_id,
      "session_id": session_id,
      "kind": "manager_turn",
      "input_event_ids": input_event_ids,
      "started_at": started_at,
    });
    std::fs::write(&run_path, run.to_string()).expect("run record");
  }

  /// Records the end of the manager run `run_id`, so the order rule reads no running turn.
  pub fn end_run(&self, session_id: &str, run_id: &str, ended_at: &str) {
    let run_path = self.features.store.run_metadata_path(session_id, run_id);
    let mut run: Value = serde_json::from_str(&std::fs::read_to_string(&run_path).expect("the run record reads"))
      .expect("the run record is JSON");
    run["ended_at"] = json!(ended_at);
    std::fs::write(&run_path, run.to_string()).expect("run record");
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

  /// Seeds one manager row for tests whose behavior depends on a stored parent chain.
  pub fn seed_manager(&self, id: &str, parent: Option<&str>) {
    let mut metadata = SessionMetadata::new(id, id, TaskProfile::Manager);
    metadata.backend = "synthetic-web".to_string();
    metadata.task_parent_id = parent.map(str::to_string);
    let path = self.features.store.metadata_path(id);
    fs::create_dir_all(path.parent().expect("metadata path has a session directory")).expect("session directory");
    fs::write(path, metadata.to_json_string(&self.features.registry.metadata_slots(), true))
      .expect("manager metadata writes");
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

/// A `user` chat event with `content` and the id `id`, so that a test names the event's tag.
pub fn user_event_with_id(id: &str, content: &str) -> Value {
  let mut event = user_event(content);
  event["id"] = json!(id);
  event
}

/// The id the store stamped on an appended event.
pub fn id_of(event: &Value) -> &str {
  event["id"].as_str().expect("an appended event carries its id")
}

/// A `master_done` event of `run_id` that answers `input_event_ids` and ended with `exit_code`.
pub fn master_done(run_id: &str, input_event_ids: &[&str], exit_code: i32) -> Value {
  json!({"type": "master_done", "run_id": run_id, "exit_code": exit_code, "input_event_ids": input_event_ids})
}
