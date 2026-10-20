use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::config::BackendOption;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};
use charliebot_store::{CreateTask, Store, TaskCaller};
use serde_json::Map;

pub fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary cron home");
  std::fs::create_dir_all(home.path().join("sessions")).expect("session directory provisions");
  std::fs::create_dir_all(home.path().join("config.d/cron.d")).expect("cron directory provisions");
  std::fs::create_dir_all(home.path().join("state")).expect("state directory provisions");
  let mut registry = Registry::new();
  registry.register_package("src.features.cron", super::register).expect("cron registration");
  registry.register_package("test.cron_handlers", register_test_handler).expect("handler registration");
  let registry = Arc::new(registry);
  let mut cfg = registry.load_config(home.path()).expect("default config loads");
  cfg.backends.options.push(BackendOption {
    id: "synthetic".to_string(),
    label: "Synthetic".to_string(),
    prompt_overlay: None,
    model: "synthetic-model".to_string(),
    backend_type: "synthetic".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  });
  cfg.backends.pool.push("synthetic".to_string());
  let store = Arc::new(
    Store::new(
      cfg.clone(),
      registry.metadata_slots(),
      charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)),
    )
    .expect("test store opens"),
  );
  std::fs::write(cfg.credentials_file(), "charliebot: {}\n").expect("credentials file writes");
  let config_value = cfg.clone();
  let config = Arc::new(HotReload::new(
    cfg.config_file(),
    "cron_test_config_reload_failed",
    Box::new(move || Ok(config_value.clone())),
  ));
  let credential_home = cfg.charliebot_home.clone();
  let credentials = Arc::new(HotReload::new(
    cfg.credentials_file(),
    "cron_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let startup = Arc::new(StartupState::default());
  startup
    .insert(RepoRoot::new(Some(repo_root().into_os_string())).expect("repo root value"))
    .expect("repo root inserts");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });
  (home, features)
}

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).expect("crate is nested under rust/crates").to_path_buf()
}

fn register_test_handler(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("synthetic_handler", |_features| Box::pin(async { Ok("synthetic result".to_string()) }))
}

pub async fn manager(features: &FeatureContext, request_id: &str) -> anyhow::Result<SessionMetadata> {
  Ok(
    features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: Some(TaskSpec { goal: "Synthetic cron manager".to_string(), ..TaskSpec::default() }),
          name: Some("Synthetic cron manager".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?,
  )
}
