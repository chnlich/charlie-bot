//! What a registered hook receives: the handles of the running server and the typed startup state.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_store::Store;
use tokio::sync::watch;

use crate::registry::Registry;

/// The handles a feature uses at request time and in its services.
pub struct FeatureContext {
  /// `config.yaml`, reloaded when the file moves. A reload that fails keeps the previous config.
  pub config: Arc<HotReload<CharlieBotConfig>>,
  /// `credentials.yaml`, reloaded when the file moves.
  pub credentials: Arc<HotReload<Credentials>>,
  /// The sole owner of session metadata and chat-event writes.
  pub store: Arc<Store>,
  /// The finished registry, read-only.
  pub registry: Arc<Registry>,
  /// The typed startup state that lifespan steps and services fill and route handlers read.
  pub startup: Arc<StartupState>,
}

/// The typed startup state: one value per Rust type. Lifespan steps and services insert the values they own, and
/// route handlers read them. A second value of one type is an error.
pub struct StartupState {
  values: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
  stop: StartupStop,
}

/// A process-local startup stop request shared by startup steps.
#[derive(Clone)]
pub struct StartupStop {
  sender: watch::Sender<StartupPhase>,
  receiver: watch::Receiver<StartupPhase>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupPhase {
  Starting,
  Stopped,
  Ready,
}

impl Default for StartupStop {
  fn default() -> Self {
    let (sender, receiver) = watch::channel(StartupPhase::Starting);
    Self { sender, receiver }
  }
}

impl StartupStop {
  /// Mark startup as stopped and wake any startup step waiting for the request.
  pub fn cancel(&self) {
    self.sender.send_if_modified(|phase| {
      if *phase == StartupPhase::Starting {
        *phase = StartupPhase::Stopped;
        true
      } else {
        false
      }
    });
  }

  /// Return whether startup has received a stop request.
  pub fn is_cancelled(&self) -> bool {
    *self.receiver.borrow() == StartupPhase::Stopped
  }

  /// Mark startup ready unless a stop request arrived first.
  pub fn mark_ready(&self) -> bool {
    let mut ready = false;
    self.sender.send_if_modified(|phase| match phase {
      StartupPhase::Starting => {
        *phase = StartupPhase::Ready;
        ready = true;
        true
      }
      StartupPhase::Ready => {
        ready = true;
        false
      }
      StartupPhase::Stopped => false,
    });
    ready
  }

  /// Wait until startup is ready, or return false when a stop request arrived first.
  pub async fn ready(&self) -> bool {
    let mut receiver = self.receiver.clone();
    loop {
      match *receiver.borrow() {
        StartupPhase::Ready => return true,
        StartupPhase::Stopped => return false,
        StartupPhase::Starting => {}
      }
      if receiver.changed().await.is_err() {
        return false;
      }
    }
  }

  /// Wait until startup receives a stop request.
  pub async fn cancelled(&self) {
    let mut receiver = self.receiver.clone();
    while *receiver.borrow() != StartupPhase::Stopped {
      receiver.changed().await.expect("the startup stop sender stays alive with its receiver");
    }
  }
}

impl Default for StartupState {
  fn default() -> Self {
    Self { values: RwLock::new(HashMap::new()), stop: StartupStop::default() }
  }
}

/// A state value inserted twice, or required before any step inserted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for StateError {}

impl StartupState {
  /// Create startup state that shares the process's stop request.
  pub fn with_stop(stop: StartupStop) -> Self {
    Self { values: RwLock::new(HashMap::new()), stop }
  }

  /// Return the stop request shared by startup steps.
  pub fn stop(&self) -> StartupStop {
    self.stop.clone()
  }

  /// Store `value` as the one value of its type.
  pub fn insert<T: Any + Send + Sync>(&self, value: T) -> Result<(), StateError> {
    let mut values = self.values.write().expect("the startup state lock is never poisoned");
    if values.contains_key(&TypeId::of::<T>()) {
      return Err(StateError(format!("the startup state already holds {}", type_name::<T>())));
    }
    values.insert(TypeId::of::<T>(), Arc::new(value));
    Ok(())
  }

  /// The value of type `T`, or `None` when no step inserted one.
  pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
    let values = self.values.read().expect("the startup state lock is never poisoned");
    values.get(&TypeId::of::<T>()).map(|value| {
      Arc::clone(value).downcast::<T>().unwrap_or_else(|_| unreachable!("the entry is keyed by its own type"))
    })
  }

  /// The value of type `T`; a missing value fails with the type's name.
  pub fn require<T: Any + Send + Sync>(&self) -> Result<Arc<T>, StateError> {
    self.get::<T>().ok_or_else(|| StateError(format!("the startup state holds no {}", type_name::<T>())))
  }
}

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
pub(crate) fn features_over(home: &std::path::Path, registry: Arc<Registry>) -> Arc<FeatureContext> {
  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
  use charliebot_core::credentials::load_credentials;

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
