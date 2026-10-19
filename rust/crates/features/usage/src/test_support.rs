//! The fixtures the usage crate's tests share: the usage sources the server registers, and a feature context over a
//! home.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME, CharlieBotConfig};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::context::StartupState;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::Store;

/// The usage sources in the order the server registers them: Claude Code, Codex, opencode, Antigravity and CLC. The
/// config types and id prefixes are the ones the backend packages declare.
pub fn register_sources(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.usage_source(UsageSource {
    name: "Claude Code",
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("cc-claude", "Claude Code")?;
  registry.usage_source(UsageSource {
    name: "Codex",
    id_prefixes: &["codex-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("codex", "Codex")?;
  registry.usage_source(UsageSource {
    name: "opencode",
    id_prefixes: &["opencode-", "oc-"],
    run_logs_only: false,
    implementation: None,
  })?;
  registry.attribute_backend_type("opencode", "opencode")?;
  registry.usage_source(UsageSource {
    name: "Antigravity",
    id_prefixes: &["antigravity-", "agy-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("antigravity", "Antigravity")?;
  registry.usage_source(UsageSource {
    name: "CLC",
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("charlie-code", "CLC")?;
  Ok(())
}

/// A registry holding the sources of [`register_sources`].
pub fn registry() -> Registry {
  let mut registry = Registry::new();
  registry.register_package("test.usage_sources", register_sources).expect("the sources register");
  registry
}

/// The feature context over `home`, with `registry` as its registry. The home's `config.yaml` lists no backend, so
/// every charlie-bot account attributes by its id prefix.
pub fn feature_context(home: &Path, registry: Registry) -> Arc<FeatureContext> {
  fs::create_dir_all(home).expect("the home directory is created");
  fs::write(home.join(CONFIG_FILENAME), "backends:\n  options: []\n").expect("the config is written");
  let registry = Arc::new(registry);
  let config_home = home.to_path_buf();
  let config_registry = Arc::clone(&registry);
  let config = Arc::new(HotReload::new(
    home.join(CONFIG_FILENAME),
    "config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credentials_home = home.to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.join(CREDENTIALS_FILENAME),
    "credentials_reload_failed",
    Box::new(move || load_credentials(&credentials_home)),
  ));
  let cfg: CharlieBotConfig = registry.load_config(home).expect("the config loads");
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens");
  Arc::new(FeatureContext {
    config,
    credentials,
    store: Arc::new(store),
    registry,
    startup: Arc::new(StartupState::default()),
  })
}
