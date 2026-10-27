//! Handles and startup state for registered features.

use std::sync::Arc;

pub use crate::registry::{FeatureContext, StartupState, StateError};
pub use crate::startup_stop::StartupStop;

/// The reader of a session's running-turn input ids.
type UserEventIdsFn = dyn Fn(&str) -> Vec<String> + Send + Sync;

/// The input event ids that a session's running turn answers, read live from the master queue
/// (`src/runtime/master_cc_state.py::running_user_event_ids`). The task runtime inserts it at startup.
pub struct RunningTurnInputs {
  user_event_ids: Arc<UserEventIdsFn>,
}

impl RunningTurnInputs {
  /// Wrap the reader that answers the running turn's input ids for a session.
  pub fn new(user_event_ids: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
    Self { user_event_ids: Arc::new(user_event_ids) }
  }

  /// The input event ids of the session's running turn, in arrival order; empty when no turn runs.
  pub fn user_event_ids(&self, session_id: &str) -> Vec<String> {
    (self.user_event_ids)(session_id)
  }
}

/// The feature context over the tempdir `home`: the config and credentials reload from it, and the store runs with the
/// registry's hooks.
#[cfg(test)]
pub(crate) fn features_over(home: &std::path::Path, registry: Arc<crate::registry::Registry>) -> Arc<FeatureContext> {
  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_store::Store;

  use crate::store_hooks::store_hooks;

  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let credentials_home = home.to_path_buf();
  let cfg = registry.load_config(home).unwrap();
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
  Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      home.join(CONFIG_FILENAME),
      "config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.join(CREDENTIALS_FILENAME),
      "credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    )),
    store: Arc::new(store),
    registry,
    startup: Arc::new(StartupState::default()),
  })
}
