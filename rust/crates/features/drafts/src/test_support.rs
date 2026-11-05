//! Fixtures for the draft route tests: a temporary home, a synthetic draft platform, and a session node.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, LaunchArgs, ResumeStyle,
};
use charliebot_registry::draft::{
  DraftContext, DraftFailureOutcome, DraftPlatform, DraftPlatformFactory, DraftSendFailure, DraftSent,
};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, StartupState};
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, Store, TaskCaller};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

/// The access key the fixture's `credentials.yaml` holds.
pub const ACCESS_KEY: &str = "synthetic-access-key";

/// The backend option the fixture's `config.yaml` names.
pub const BACKEND_ID: &str = "synthetic-a";

/// What one send of the synthetic platform does.
#[derive(Clone)]
pub enum Reply {
  Sent,
  Failed { detail: String, outcome: DraftFailureOutcome },
  Delayed { millis: u64, then: Box<Reply> },
}

impl Reply {
  /// A successful send that reports the synthetic sender.
  pub fn sent() -> Reply {
    Reply::Sent
  }

  /// A platform error response.
  pub fn rejected(detail: &str) -> Reply {
    Reply::Failed { detail: detail.to_string(), outcome: DraftFailureOutcome::Rejected }
  }

  /// A timeout or lost response.
  pub fn unknown(detail: &str) -> Reply {
    Reply::Failed { detail: detail.to_string(), outcome: DraftFailureOutcome::Unknown }
  }

  /// A success that lands after `millis`, so a concurrent second send overlaps the first.
  pub fn delayed(millis: u64) -> Reply {
    Reply::Delayed { millis, then: Box::new(Reply::Sent) }
  }
}

/// One recorded send.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
  pub target: Value,
  pub body: String,
}

/// A platform that records every send and answers the scripted replies in order; an unscripted send succeeds.
#[derive(Default)]
pub struct SyntheticPlatform {
  calls: Mutex<Vec<Call>>,
  replies: Mutex<VecDeque<Reply>>,
}

impl SyntheticPlatform {
  pub fn scripted(replies: Vec<Reply>) -> Arc<Self> {
    Arc::new(Self { calls: Mutex::new(Vec::new()), replies: Mutex::new(replies.into()) })
  }

  pub fn calls(&self) -> Vec<Call> {
    self.calls.lock().expect("the call log mutex is not poisoned").clone()
  }

  fn next_reply(&self) -> Reply {
    self.replies.lock().expect("the reply queue mutex is not poisoned").pop_front().unwrap_or(Reply::Sent)
  }
}

impl DraftPlatform for SyntheticPlatform {
  fn name(&self) -> &'static str {
    "synthetic"
  }

  fn validate_target(&self, target: &Value) -> Result<Value, String> {
    match target.get("channel").and_then(Value::as_str) {
      Some(channel) if !channel.is_empty() => Ok(json!({"channel": channel})),
      _ => Err("target.channel must be a non-empty string".to_string()),
    }
  }

  fn target_label(&self, target: &Value) -> String {
    format!("Channel {}", target.get("channel").and_then(Value::as_str).unwrap_or_default())
  }

  fn send<'a>(&'a self, target: &'a Value, body: &'a str) -> BoxFuture<'a, Result<DraftSent, DraftSendFailure>> {
    Box::pin(async move {
      self
        .calls
        .lock()
        .expect("the call log mutex is not poisoned")
        .push(Call { target: target.clone(), body: body.to_string() });
      match self.next_reply() {
        Reply::Sent => Ok(sent()),
        Reply::Failed { detail, outcome } => Err(DraftSendFailure { detail, outcome }),
        Reply::Delayed { millis, then } => {
          tokio::time::sleep(Duration::from_millis(millis)).await;
          match *then {
            Reply::Sent => Ok(sent()),
            Reply::Failed { detail, outcome } => Err(DraftSendFailure { detail, outcome }),
            Reply::Delayed { .. } => unreachable!("the fixture nests one delay at most"),
          }
        }
      }
    })
  }
}

fn sent() -> DraftSent {
  DraftSent { sender: "U0SYNTHETIC".to_string(), url: "https://chat.example/archives/C1/p1".to_string() }
}

thread_local! {
  static CURRENT_PLATFORM: RefCell<Option<Arc<SyntheticPlatform>>> = const { RefCell::new(None) };
}

fn register_test_platform(registry: &mut Registry) -> Result<(), RegistryError> {
  let platform = CURRENT_PLATFORM.with(|slot| slot.borrow().clone()).expect("the fixture installed a platform");
  let factory: DraftPlatformFactory =
    Arc::new(move |_context: &DraftContext| Ok(platform.clone() as Arc<dyn DraftPlatform>));
  registry.draft_platform("synthetic", factory)
}

struct NeverBuilt;

impl BackendFactory for NeverBuilt {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    anyhow::bail!("the drafts test backend is never launched")
  }
}

fn register_test_backend(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "synthetic",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(NeverBuilt),
    lifecycle: None,
    translate_fallback: false,
  })
}

/// A temporary home with the drafts package and the synthetic platform registered.
pub fn home(platform: Arc<SyntheticPlatform>) -> (tempfile::TempDir, Arc<FeatureContext>) {
  CURRENT_PLATFORM.with(|slot| *slot.borrow_mut() = Some(platform));
  let home = tempfile::tempdir().expect("a temporary home");
  std::fs::write(home.path().join("credentials.yaml"), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
    .expect("the credentials file");
  std::fs::write(
    home.path().join("config.yaml"),
    format!("backends:\n  options:\n    - {{id: {BACKEND_ID}, label: Synthetic, type: synthetic, model: model-a}}\n"),
  )
  .expect("the config file");
  let mut registry = Registry::new();
  registry.register_package("features.drafts", crate::register).expect("the drafts package registers");
  registry.register_package("test.backend", register_test_backend).expect("the test backend registers");
  registry.register_package("test.platform", register_test_platform).expect("the test platform registers");
  let registry = Arc::new(registry);
  CURRENT_PLATFORM.with(|slot| *slot.borrow_mut() = None);

  let config_value = registry.load_config(home.path()).expect("the test config loads");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry)))
      .expect("the test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.path().to_path_buf();
  let credential_home = home.path().to_path_buf();
  let features = Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      config_value.config_file(),
      "drafts_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "drafts_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    )),
    store,
    registry,
    startup: Arc::new(StartupState::default()),
  });
  (home, features)
}

/// One manager session node's id, created on the fixture.
pub async fn create_session(features: &FeatureContext, name: &str) -> String {
  let meta = features
    .store
    .create_task(
      CreateTask {
        request_id: name.to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some(name.to_string()),
        backend: Some(BACKEND_ID.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .expect("a synthetic session node");
  meta.id
}

/// One launch the recording executor was asked to make.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
  pub session_id: String,
  pub input_types: Vec<String>,
}

/// The executor that records each launch request and answers with a synthetic run id.
#[derive(Default)]
pub struct RecordingExecutor {
  launches: Mutex<Vec<Launch>>,
}

impl RecordingExecutor {
  pub fn launches(&self) -> Vec<Launch> {
    self.launches.lock().expect("the launch log mutex is not poisoned").clone()
  }
}

impl DispatchExecutor for RecordingExecutor {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [charliebot_store::ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, charliebot_store::StoreResult<Option<String>>> {
    Box::pin(async move {
      let input_types = pending_inputs
        .iter()
        .map(|event| {
          event.get("type").and_then(charliebot_store::compact::CompactValue::as_str).unwrap_or_default().to_string()
        })
        .collect();
      self
        .launches
        .lock()
        .expect("the launch log mutex is not poisoned")
        .push(Launch { session_id: session_id.to_string(), input_types });
      Ok(Some(format!("run-{}", self.launches.lock().unwrap().len())))
    })
  }
}

/// The recorder installed on a dispatcher whose executor records launches.
pub fn install_wake_recorder(features: &Arc<FeatureContext>) -> Arc<RecordingExecutor> {
  let completion = Arc::new(TaskCompletion::new(Arc::clone(features)));
  features.startup.insert(Arc::clone(&completion)).expect("the completion owner inserts once");
  let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
  let run: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = completion;
  let dispatcher = Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence, run);
  let recorder = Arc::new(RecordingExecutor::default());
  dispatcher.set_executor(Some(recorder.clone()));
  features.startup.insert(dispatcher).expect("the dispatcher inserts once");
  recorder
}
