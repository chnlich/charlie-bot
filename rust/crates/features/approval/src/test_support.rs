//! Fixtures for the approval tests: a store-backed feature context over a temporary home, and the route calls the
//! tests make. The context carries this package's registrations and the web reply slot the seeded statuses write
//! through.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use charliebot_core::approval_policy::TABLE_RELATIVE_PATH;
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, Store, TaskCaller};
use http::{Method, Request};
use serde_json::{Map, Value};
use tempfile::TempDir;
use tower::ServiceExt;

/// The approval policy table the test homes carry: one uncapped policy, one capped at 16, both expiring far out.
pub const APPROVAL_POLICY_TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup, or sends what cannot be recalled.\"}
  - {id: chao-name, rank: 2, text: \"A message in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval for this step.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies:
  - {id: release-restart, scope: \"Release and restart the CharlieBot checkout.\", cap_per_pt_day: null, expires: 2099-01-01}
  - {id: shared-repo-landing, scope: \"Land on a shared non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}
enable_ask_tier_refusal: false
";

/// Writes `text` as the home's approval policy table.
pub fn write_approval_policy_table(home: &Path, text: &str) {
  let path = home.join(TABLE_RELATIVE_PATH);
  fs::create_dir_all(path.parent().expect("the table path has a parent")).expect("config.d");
  fs::write(path, text).expect("the approval policy table writes");
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
      .register_package("src.features.web_reply", charliebot_web_reply::register)
      .expect("the web reply package registers");
    registry.register_package("src.features.approval", crate::register).expect("the approval package registers");
    let registry = Arc::new(registry);
    let loaded = registry.load_config(dir.path()).expect("an empty home loads with the defaults");
    // Task creation takes the first configured backend when a create names none.
    let mut loaded = loaded;
    loaded.backends.options.push(charliebot_core::config::BackendOption {
      id: "synthetic-approval".to_string(),
      label: "Synthetic approval".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
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
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    Self { _dir: dir, features }
  }

  /// The temporary home the store writes under: the CharlieBot home, which the config names.
  pub fn root(&self) -> PathBuf {
    self._dir.path().to_path_buf()
  }

  /// A root manager session named `name`, created as the operator creates one.
  pub async fn manager(&self, name: &str) -> String {
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
