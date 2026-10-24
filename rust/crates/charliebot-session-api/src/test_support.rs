//! Fixtures shared by the session route tests: a feature context over a temporary home.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::bail;
use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, HeaderValue};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, LaunchArgs, ResumeStyle,
};
use charliebot_registry::sequence::{
  RestoreOutcome, SequenceBinding, SequenceCompletion, SequenceController, SequenceTree,
};
use charliebot_registry::store_hooks;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::{ControlGuard, CreateTask, RunRecord, Store, StoreError, StoreResult, TaskCaller};
use charliebot_tasks::dispatch::{Dispatcher, RunFinishedCompletion};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

struct TestBackendFactory;

struct TestSequenceBinding;

impl SequenceBinding for TestSequenceBinding {
  fn name(&self) -> &str {
    "session_api_test"
  }

  fn dedicated_backend(&self) -> bool {
    true
  }

  fn backend_lock_detail(&self, target_backend: &str) -> String {
    format!("backend '{target_backend}' is locked by the session API test sequence")
  }

  fn on_wake<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _input_events: &'a [Value],
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn on_archive<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }
}

struct TestSequenceController;

impl SequenceController for TestSequenceController {
  fn sequence_kind(&self) -> &'static str {
    "session_api_test"
  }

  fn redrive<'a>(&'a self, _session_id: &'a str, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _run: &'a RunRecord,
    _launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn after_run<'a>(
    &'a self,
    _session_id: &'a str,
    _run: &'a RunRecord,
    _tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn recover_run<'a>(
    &'a self,
    _session_id: &'a str,
    _run: &'a RunRecord,
    _outcome: Option<&'a str>,
    _tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async { Ok(false) })
  }

  fn reconcile_interrupted<'a>(&'a self, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn binding(&self, session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    (session_id == "sequence-locked-node").then(|| Arc::new(TestSequenceBinding) as Arc<dyn SequenceBinding>)
  }

  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    HashMap::new()
  }
}

impl BackendFactory for TestBackendFactory {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    bail!("the session API test backend is not launched")
  }
}

fn register_test_backend(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
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
    factory: Arc::new(TestBackendFactory),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.sequence_controller("session_api_test", Arc::new(TestSequenceController))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "session_api_test",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "test_session_value", default: Value::Null }],
    },
    validate_test_session_fields,
  )
}

fn validate_test_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  values
    .get("test_session_value")
    .is_some_and(|value| !value.is_null() && !value.is_string())
    .then(|| "test_session_value must be a string or null".to_string())
}

pub(crate) fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary home");
  std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
  std::fs::write(
    home.path().join("config.yaml"),
    "backends:\n  options:\n    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a}\n    - {id: synthetic-b, label: Synthetic B, type: synthetic, model: model-b}\n",
  )
  .expect("backend config file");
  let mut registry = Registry::new();
  registry.register_package("session_api_test", register_test_backend).expect("register synthetic backend");
  let registry = Arc::new(registry);
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

/// Completion hooks for the route tests, which never finish a run or a sequence child: any call is a test error.
struct NoCompletion;

impl SequenceCompletion for NoCompletion {
  fn evaluate_automatic_completion<'a>(
    &'a self,
    _session_id: &'a str,
    _run_id: &'a str,
    _summary: Option<&'a str>,
    _result_refs: Option<&'a [String]>,
    _request_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Err(StoreError::new("unused sequence completion")) })
  }

  fn close_sequence_child<'a>(
    &'a self,
    _session_id: &'a str,
    _request_id: &'a str,
    _outcome: &'a str,
    _evidence: &'a Value,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Err(StoreError::new("unused sequence completion")) })
  }

  fn restore_chain<'a>(
    &'a self,
    _session_id: &'a str,
    _request_id: &'a str,
    _reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async { Err(StoreError::new("unused sequence completion")) })
  }

  fn restore_chain_locked<'a>(
    &'a self,
    _guard: &'a ControlGuard,
    _session_id: &'a str,
    _request_id: &'a str,
    _reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
    Box::pin(async { Err(StoreError::new("unused sequence completion")) })
  }
}

impl RunFinishedCompletion for NoCompletion {
  fn after_run_finished<'a>(&'a self, _session_id: &'a str, _run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async { Err(StoreError::new("unused run completion")) })
  }
}

pub(crate) fn install_dispatcher(features: &Arc<FeatureContext>) {
  let dispatcher =
    Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), Arc::new(NoCompletion), Arc::new(NoCompletion));
  features.startup.insert(dispatcher).expect("one dispatcher per startup");
}

pub(crate) async fn create_node(
  store: &Store,
  request_id: &str,
  parent: Option<&str>,
  profile: TaskProfile,
) -> SessionMetadata {
  let request = CreateTask {
    request_id: request_id.to_string(),
    task_parent_id: parent.map(str::to_string),
    profile,
    task: None,
    name: Some(request_id.to_string()),
    backend: Some("synthetic-a".to_string()),
    group: None,
    session_id: None,
  };
  store.create_task(request, TaskCaller::System, Map::new()).await.expect("synthetic task node")
}

pub(crate) fn json_request(body: Value) -> (HeaderMap, Body) {
  let mut headers = HeaderMap::new();
  headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
  (headers, Body::from(body.to_string()))
}

pub(crate) async fn answer(response: Response) -> (StatusCode, Value) {
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("response body");
  (status, serde_json::from_slice(&bytes).expect("JSON response"))
}
