//! Fixtures shared by the session route tests: a feature context over a temporary home.

use std::sync::Arc;

use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use charliebot_store::Store;

pub(crate) fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary home");
  std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
  let registry = Arc::new(Registry::new());
  let config_value = registry.load_config(home.path()).expect("test config");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.path().to_path_buf();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "session_api_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credential_home = home.path().to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.path().join(CREDENTIALS_FILENAME),
    "session_api_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let startup = Arc::new(StartupState::default());
  (home, Arc::new(FeatureContext { config, credentials, store, registry, startup }))
}
