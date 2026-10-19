//! Fixtures for the explain tests: a store-backed feature context over a temporary home, and a scripted backend type
//! whose one-shot runs a shell script as its agent process.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{Store, Subscription, session_channel};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tempfile::TempDir;

/// The backend option id the test home configures; its type is `scripted`.
pub const OPTION_ID: &str = "scripted-explain";

/// What one start of the scripted backend does.
pub enum Start {
  /// The agent process runs this shell script.
  Script(String),
  /// The launch fails with an error whose message is empty.
  EmptyFailure,
}

/// An error whose message is empty; the entry's error text falls back to its name, as Python's class name does.
#[derive(Debug)]
pub struct EmptyFailure;

impl fmt::Display for EmptyFailure {
  fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
    Ok(())
  }
}

impl std::error::Error for EmptyFailure {}

/// The factory of the `scripted` type: each build takes the next queued start, and every build is counted.
pub struct Scripted {
  starts: Mutex<VecDeque<Start>>,
  builds: AtomicUsize,
}

impl Scripted {
  pub fn new(starts: Vec<Start>) -> Arc<Self> {
    Arc::new(Self { starts: Mutex::new(starts.into()), builds: AtomicUsize::new(0) })
  }

  /// The number of backends built so far.
  pub fn builds(&self) -> usize {
    self.builds.load(Ordering::SeqCst)
  }
}

impl BackendFactory for Scripted {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    self.builds.fetch_add(1, Ordering::SeqCst);
    match self.starts.lock().expect("the start queue is never poisoned").pop_front() {
      Some(Start::Script(script)) => Ok(Box::new(ScriptedAgent { script })),
      Some(Start::EmptyFailure) => Err(anyhow::Error::new(EmptyFailure)),
      None => bail!("the scripted backend has no start left"),
    }
  }
}

/// The agent process of one start: `/bin/sh -c <script>`, its stdout the raw event log.
struct ScriptedAgent {
  script: String,
}

impl AgentBackend for ScriptedAgent {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(IdentityTranslate)
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }

  fn one_shot_text<'a>(
    &'a mut self,
    _base_env: &'a Env,
    _prompt: &'a str,
    _system: &'a str,
    _timeout: Option<std::time::Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    Some(Box::pin(async { bail!("native one-shot is unused in this test") }))
  }
}

struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, event: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![event])
  }
}

/// The script of an agent that answers with `text` and then finishes.
pub fn answer_script(text: &str) -> String {
  format!(
    "printf '%s\\n' '{}' '{}'",
    json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}),
    json!({"type": "result", "usage": {}}),
  )
}

/// The script of an agent that finishes with no assistant text.
pub fn empty_script() -> String {
  "printf '%s\\n' '{\"type\":\"result\",\"usage\":{}}'".to_string()
}

/// The script of an agent that exits with an error and says nothing.
pub fn failing_script() -> String {
  "exit 3".to_string()
}

/// The script of an agent that outlives any test budget.
pub fn hanging_script() -> String {
  "exec sleep 30".to_string()
}

/// The script of an agent that answers `text` after `seconds`, so a test can observe the pending state.
pub fn delayed_answer_script(seconds: u32, text: &str) -> String {
  format!("sleep {seconds}; {}", answer_script(text))
}

/// A temporary home whose feature context runs the store and the `scripted` backend type.
pub struct Home {
  pub dir: TempDir,
  pub features: Arc<FeatureContext>,
  pub factory: Arc<Scripted>,
}

impl Home {
  /// A home whose `scripted` backend launches the queued starts in order.
  pub fn new(starts: Vec<Start>) -> Self {
    let dir = tempfile::tempdir().expect("a temporary home");
    let factory = Scripted::new(starts);
    let registry = scripted_registry(Arc::clone(&factory));
    fs::write(
      dir.path().join("config.yaml"),
      format!(
        "server:\n  session_memory_max_mb: 0\n  session_tasks_max: 0\nbackends:\n  pool: []\n  options:\n    - id: {OPTION_ID}\n      label: Scripted\n      type: scripted\n      model: scripted-model\n"
      ),
    )
    .expect("the test config");
    let cfg = registry.load_config(dir.path()).expect("the test config loads");
    let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("a store");
    let startup = StartupState::default();
    startup
      .insert(AgentBaseEnvironment { values: Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]) })
      .expect("the agent base environment");
    let config_home = dir.path().to_path_buf();
    let config_registry = Arc::clone(&registry);
    let credential_home = dir.path().to_path_buf();
    let features = Arc::new(FeatureContext {
      config: Arc::new(HotReload::new(
        dir.path().join("config.yaml"),
        "explain_config_reload_failed",
        Box::new(move || config_registry.load_config(&config_home)),
      )),
      credentials: Arc::new(HotReload::new(
        dir.path().join("credentials.yaml"),
        "explain_credentials_reload_failed",
        Box::new(move || load_credentials(&credential_home)),
      )),
      store: Arc::new(store),
      registry,
      startup: Arc::new(startup),
    });
    Self { dir, features, factory }
  }

  /// Creates the session's metadata and its `data/` directory, as the server does for a new session.
  pub fn create_session(&self, session_id: &str) {
    let meta = SessionMetadata::new(session_id, "Explain", TaskProfile::Manager);
    let slots = self.features.registry.metadata_slots();
    let path = self.features.store.metadata_path(session_id);
    fs::create_dir_all(path.parent().expect("a session directory")).expect("the session directory");
    fs::create_dir_all(self.features.store.session_dir(session_id).join("data")).expect("the data directory");
    fs::write(path, meta.to_json_string(&slots, true)).expect("the session metadata");
  }

  /// Appends one round: the user's question, the assistant text when given, and the `master_done` divider.
  pub async fn append_round(&self, session_id: &str, assistant_text: Option<&str>) {
    let store = &self.features.store;
    store
      .append_chat_event(session_id, json!({"type": "user", "message": {"content": [{"type": "text", "text": "Q"}]}}))
      .await
      .expect("the user event");
    if let Some(text) = assistant_text {
      let event = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}});
      store.append_chat_event(session_id, event).await.expect("the assistant event");
    }
    store.append_chat_event(session_id, json!({"type": "master_done"})).await.expect("the divider");
  }
}

thread_local! {
  /// The factory the next `register_scripted` call installs. A package's `register` is a plain function, so the
  /// test hands the factory over on the thread that makes the registration.
  static PENDING_FACTORY: RefCell<Option<Arc<Scripted>>> = const { RefCell::new(None) };
}

fn scripted_registry(factory: Arc<Scripted>) -> Arc<Registry> {
  PENDING_FACTORY.with(|slot| *slot.borrow_mut() = Some(factory));
  let mut registry = Registry::new();
  registry.register_package("scripted-test", register_scripted).expect("the scripted type registers");
  Arc::new(registry)
}

fn register_scripted(registry: &mut Registry) -> Result<(), RegistryError> {
  let factory = PENDING_FACTORY.with(|slot| slot.borrow_mut().take()).expect("a factory is pending");
  let traits = BackendTraits {
    resume: ResumeStyle::NativeId,
    restart_reattach: false,
    preassigned_session_id: false,
    family_prefix: None,
    manual_compact: false,
    compact_hint: None,
  };
  registry.backend_type(BackendTypeRegistration {
    type_name: "scripted",
    traits,
    option_model: |_| Ok(()),
    factory,
    lifecycle: None,
    translate_fallback: false,
  })
}

/// A subscription to one session channel, for the broadcast frames.
pub fn subscribe(store: &Store, session_id: &str) -> Subscription {
  store.subscribe(&session_channel(session_id))
}
