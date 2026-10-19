//! Fixtures for the artifacts tests: a store-backed feature context over a temporary home, with this package registered.

use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::context::StartupState;
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, Registry, store_hooks};
use charliebot_store::Store;

/// A feature context over a new temporary home, with the artifacts package registered. The home lives as long as the
/// returned directory.
pub(crate) fn features() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("a temporary home is available");
  let mut registry = Registry::new();
  registry.register_package("src.features.artifacts", crate::register).expect("the artifacts package registers");
  let registry = Arc::new(registry);
  let config_value = registry.load_config(home.path()).expect("the default config loads");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("the store opens over the temporary home"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.path().to_path_buf();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "artifacts_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credentials_home = home.path().to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.path().join(CREDENTIALS_FILENAME),
    "artifacts_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credentials_home)),
  ));
  let startup = StartupState::default();
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../");
  startup
    .insert(Templating::new(&repo_root, &["src.features.artifacts"]))
    .expect("the template engine enters startup state");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
  (home, features)
}
