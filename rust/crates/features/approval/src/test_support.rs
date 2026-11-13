//! Fixtures for the approval tests: a store-backed feature context over a temporary home, and the route calls the
//! tests make. The context carries this package's registrations and the web reply slot the seeded statuses write
//! through.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::registry::OpenWaitingRow;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, Store, TaskCaller};
use futures_util::future::BoxFuture;
use http::{Method, Request};
use serde_json::{Map, Value};
use tempfile::TempDir;
use tower::ServiceExt;

pub use charliebot_core::test_support::{APPROVAL_POLICY_TABLE, write_approval_policy_table};

/// A derived-item reader with no derived items: the approval tests hold no host waits.
fn register_no_derived_waiting(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.derived_waiting_items(no_derived_waiting)
}

fn no_derived_waiting(_features: &FeatureContext) -> BoxFuture<'_, anyhow::Result<Vec<OpenWaitingRow>>> {
  Box::pin(async { Ok(Vec::new()) })
}

/// A store over a temporary home, with the web reply slot and this package registered.
pub struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
}

impl Home {
  pub fn new() -> Self {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    fs::write(dir.path().join("config.yaml"), "").expect("the synthetic config writes");
    fs::write(dir.path().join(CREDENTIALS_FILENAME), "charliebot:\n  access_key: synthetic-access-key\n")
      .expect("the synthetic access key writes");
    fs::create_dir_all(dir.path().join("sessions")).expect("the sessions directory creates");
    write_approval_policy_table(dir.path(), APPROVAL_POLICY_TABLE);
    let mut registry = Registry::new();
    registry
      .register_package("features.web_reply", charliebot_web_reply::register)
      .expect("the web reply package registers");
    registry.register_package("features.approval", crate::register).expect("the approval package registers");
    registry
      .register_package("tests.derived_waiting", register_no_derived_waiting)
      .expect("the derived Waiting on you reader registers");
    let registry = Arc::new(registry);
    let loaded = registry.load_config(dir.path()).expect("an empty home loads with the defaults");
    // Task creation takes the first configured backend when a create names none.
    let mut loaded = loaded;
    loaded.backends.options.push(charliebot_core::config::BackendOption {
      id: "synthetic-approval".to_string(),
      label: "Synthetic approval".to_string(),
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
      "approval_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "approval_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = StartupState::default();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let checkout =
      manifest_dir.ancestors().nth(4).expect("the approval crate sits four directories below the checkout");
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

  /// A root manager session named `name`, created as the operator creates one.
  pub async fn manager(&self, name: &str) -> String {
    self.create_manager(None, name).await
  }

  /// A manager session named `name` under `parent`, created as the operator creates one.
  pub async fn child(&self, parent: &str, name: &str) -> String {
    self.create_manager(Some(parent), name).await
  }

  async fn create_manager(&self, parent: Option<&str>, name: &str) -> String {
    let request = CreateTask {
      request_id: format!("manager-{name}"),
      task_parent_id: parent.map(str::to_string),
      profile: TaskProfile::Manager,
      task: None,
      name: Some(name.to_string()),
      backend: None,
      group: None,
      session_id: None,
    };
    self.features.store.create_task(request, TaskCaller::Operator, Map::new()).await.expect("the task is created").id
  }

  /// Store the session's web status the way a status set stores it.
  pub async fn set_web_status(&self, session_id: &str, status: Value) {
    let mut fields = Map::new();
    fields.insert(charliebot_core::event_types::WEB_STATUS.to_string(), status);
    let guard = self.features.store.control_lock().await;
    self
      .features
      .store
      .update_task_slot_fields_locked(&guard, session_id, charliebot_core::event_types::WEB_REPLY, fields)
      .await
      .expect("the web status stores");
  }

  /// The routes answering one request, as the server mounts them.
  pub async fn send(&self, method: Method, uri: &str) -> (http::StatusCode, Value) {
    let router: Router = crate::api::routes(&self.features);
    let request = Request::builder().method(method).uri(uri).body(Body::empty()).expect("the request builds");
    let response = router.oneshot(request).await.expect("the router is infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    let body =
      serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
  }
}
