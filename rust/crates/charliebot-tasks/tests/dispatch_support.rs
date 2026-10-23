#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::Registry;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::sequence::{RestoreOutcome, SequenceCompletion};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, ChatEvent, ControlGuard, CreateTask, EmptyRuntimeEventHooks,
  SequenceArchiveHook, SequenceBindingMover, SequenceListingFields, Store, StoreError, StoreHookFuture, StoreHooks,
  StoreResult, TaskCaller,
};
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher, RunFinishedCompletion};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use serde_yaml::Mapping;
use tempfile::TempDir;

const BACKEND_ID: &str = "synthetic";

#[derive(Default)]
pub struct TestHooks;

impl AccountLabelKeeper for TestHooks {
  fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> StoreResult<bool> {
    Ok(false)
  }

  fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
    None
  }
}

impl AnchorFieldReconciler for TestHooks {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
    attempted.cc_session_id = disk.cc_session_id.clone();
    attempted.cc_session_started_at = disk.cc_session_started_at;
    attempted.native_backend = disk.native_backend.clone();
    Ok(())
  }
}

impl SequenceArchiveHook for TestHooks {
  fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async { Ok(()) })
  }
}

impl SequenceBindingMover for TestHooks {
  fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async { Ok(false) })
  }
}

impl SequenceListingFields for TestHooks {
  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(
    &self,
    _session_ids: &[String],
    _now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>> {
    Ok(HashMap::new())
  }

  fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    Ok(HashMap::new())
  }
}

pub struct ScriptedCompletion {
  store: Arc<Store>,
  pub after_finish_count: AtomicUsize,
  pub return_conflict: AtomicBool,
}

impl ScriptedCompletion {
  fn restore<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
    Box::pin(async move {
      self.store.load_task_meta(session_id).await?;
      let mut chain = vec![session_id.to_string()];
      for ancestor_id in self.store.ancestor_ids(session_id)? {
        if self.store.task_state(&ancestor_id)? == "open" {
          break;
        }
        chain.push(ancestor_id);
      }
      if self.store.task_state(session_id)? == "open" {
        chain.clear();
      }
      let mut restored = Vec::new();
      let mut announcements = Vec::new();
      for node_id in chain.into_iter().rev() {
        let history = self.store.load_all_facts(&node_id)?;
        let lifecycle = history
          .iter()
          .filter(|event| {
            matches!(
              event.get("type").and_then(charliebot_store::compact::CompactValue::as_str),
              Some("task_closed" | "task_reopened")
            )
          })
          .collect::<Vec<_>>();
        if lifecycle.last().is_some_and(|event| {
          event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some("task_reopened")
            && event.get("request_id").and_then(charliebot_store::compact::CompactValue::as_str) == Some(request_id)
        }) {
          restored.push(node_id);
          continue;
        }
        let last_close = history.iter().rev().find(|event| {
          event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some("task_closed")
        });
        let close_id =
          last_close.and_then(|event| event.get("id")).and_then(charliebot_store::compact::CompactValue::as_str);
        let event_scope =
          close_id.map_or_else(|| request_id.to_string(), |close_id| format!("{request_id}:{close_id}"));
        let payload = vec![
          ("request_id".to_string(), json!(request_id)),
          ("closed_event_id".to_string(), close_id.map_or(Value::Null, |id| json!(id))),
          ("reason".to_string(), json!(reason)),
        ];
        let event_id = charliebot_store::control_events::stable_reopen_event_id(&node_id, &event_scope);
        let event = charliebot_store::control_events::build_control_event(
          "task_reopened",
          "user",
          &node_id,
          Some(&event_id),
          None,
          payload,
        );
        let epoch = self.store.prime_aggregator(&node_id).await?;
        let event = self.store.append_control_fact(guard, &node_id, event).await?;
        restored.push(node_id.clone());
        announcements.push((node_id, event, epoch));
      }
      self.store.invalidate_tree_index();
      Ok((restored, announcements))
    })
  }
}

impl SequenceCompletion for ScriptedCompletion {
  fn evaluate_automatic_completion<'a>(
    &'a self,
    _session_id: &'a str,
    _run_id: &'a str,
    _summary: Option<&'a str>,
    _result_refs: Option<&'a [String]>,
    _request_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Ok((0, Value::Null)) })
  }

  fn close_sequence_child<'a>(
    &'a self,
    _session_id: &'a str,
    _request_id: &'a str,
    _outcome: &'a str,
    _evidence: &'a Value,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Ok((0, Value::Null)) })
  }

  fn restore_chain<'a>(
    &'a self,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async move {
      let guard = self.store.control_lock().await;
      let (restored, announcements) = self.restore(&guard, session_id, request_id, reason).await?;
      drop(guard);
      for (node_id, event, epoch) in announcements {
        self.store.announce_appended_event(&node_id, &event, epoch).await?;
      }
      Ok(restored)
    })
  }

  fn restore_chain_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
    self.restore(guard, session_id, request_id, reason)
  }
}

impl RunFinishedCompletion for ScriptedCompletion {
  fn after_run_finished<'a>(&'a self, _session_id: &'a str, _run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move {
      self.after_finish_count.fetch_add(1, Ordering::Relaxed);
      if self.return_conflict.load(Ordering::Relaxed) {
        Err(StoreError::conflict(vec!["synthetic completion blocker".to_string()]))
      } else {
        Ok(())
      }
    })
  }
}

pub struct ScriptedExecutor {
  store: Arc<Store>,
  dispatcher: Weak<Dispatcher>,
  next_run: AtomicUsize,
  outcomes: Mutex<Vec<RunOutcomeKind>>,
  outcome_index: AtomicUsize,
}

impl ScriptedExecutor {
  pub fn new(store: Arc<Store>, dispatcher: &Arc<Dispatcher>, outcomes: Vec<RunOutcomeKind>) -> ScriptedExecutor {
    ScriptedExecutor {
      store,
      dispatcher: Arc::downgrade(dispatcher),
      next_run: AtomicUsize::new(0),
      outcomes: Mutex::new(outcomes),
      outcome_index: AtomicUsize::new(0),
    }
  }
}

impl DispatchExecutor for ScriptedExecutor {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    _pending_inputs: &'a [ChatEvent],
    launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move {
      let dispatcher = self.dispatcher.upgrade().ok_or_else(|| StoreError::new("scripted dispatcher was dropped"))?;
      let run_id = match launch_run_id {
        Some(run_id) => run_id.to_string(),
        None => {
          let run_id = format!("run-{}", self.next_run.fetch_add(1, Ordering::Relaxed) + 1);
          let guard = self.store.control_lock().await;
          self.store.register_run_locked(&guard, session_id, new_run(session_id, &run_id), None).await?;
          drop(guard);
          run_id
        }
      };
      let guard = self.store.control_lock().await;
      dispatcher.claim_input_batch_locked(&guard, session_id, &run_id, None)?;
      drop(guard);
      let outcome_index = self.outcome_index.fetch_add(1, Ordering::Relaxed);
      let outcome = *self
        .outcomes
        .lock()
        .expect("scripted outcomes lock is not poisoned")
        .get(outcome_index)
        .ok_or_else(|| StoreError::new("scripted executor has no outcome for this launch"))?;
      dispatcher.finish_run(session_id, &run_id, RunOutcome::new(outcome)).await?;
      Ok(Some(run_id))
    })
  }
}

pub struct Harness {
  pub _home: TempDir,
  pub registry: Arc<Registry>,
  pub store: Arc<Store>,
  pub completion: Arc<ScriptedCompletion>,
  pub dispatcher: Arc<Dispatcher>,
  pub features: Arc<FeatureContext>,
}

impl Harness {
  pub fn new() -> Harness {
    let home = tempfile::tempdir().expect("temporary CharlieBot home is available");
    let cfg = Arc::new(config(home.path()));
    let mut registry = Registry::new();
    // The origin slots a thread session carries; the tests that name a session kind set them through the store.
    registry
      .register_package("dispatch_support_origins", |registry| {
        for (owner, field) in [("slack", "slack_origin"), ("discord", "discord_origin")] {
          registry.metadata_slot(charliebot_core::metadata_slot::MetadataSlot {
            owner,
            on: charliebot_core::metadata_slot::MetadataFile::Session,
            after: None,
            fields: vec![charliebot_core::metadata_slot::SlotField { name: field, default: serde_json::Value::Null }],
          })?;
        }
        Ok(())
      })
      .expect("origin slots register");
    let registry = Arc::new(registry);
    let hooks = Arc::new(TestHooks);
    let store = Arc::new(
      Store::new(
        (*cfg).clone(),
        registry.metadata_slots(),
        StoreHooks {
          account_labels: Some(hooks.clone()),
          anchor_reconciler: Some(hooks.clone()),
          sequence_listing: Some(hooks.clone()),
          sequence_archive: Some(hooks.clone()),
          sequence_binding_mover: Some(hooks),
          runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
        },
      )
      .expect("store opens over the temporary home"),
    );
    let completion = Arc::new(ScriptedCompletion {
      store: Arc::clone(&store),
      after_finish_count: AtomicUsize::new(0),
      return_conflict: AtomicBool::new(false),
    });
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = completion.clone();
    let startup = Arc::new(StartupState::default());
    let config_value = Arc::clone(&cfg);
    let config_reload =
      HotReload::new(cfg.config_file(), "test_config_reload_failed", Box::new(move || Ok((*config_value).clone())));
    let credential_home = cfg.charliebot_home.clone();
    let credentials_reload = HotReload::new(
      cfg.credentials_file(),
      "test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    );
    let features = Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::clone(&store),
      registry: Arc::clone(&registry),
      startup: Arc::clone(&startup),
    });
    startup
      .insert(Dispatcher::new(Arc::clone(&store), Arc::clone(&features), sequence_completion, run_completion))
      .expect("dispatcher enters startup state once");
    let dispatcher = charliebot_tasks::dispatch::dispatcher(&features).expect("dispatcher is in startup state");
    Harness { _home: home, registry, store, completion, dispatcher, features }
  }

  pub async fn create_task(
    &self,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
    name: &str,
  ) -> SessionMetadata {
    self
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: None,
          name: Some(name.to_string()),
          backend: Some(BACKEND_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .expect("task node is created")
  }

  pub fn install_executor(&self, outcomes: Vec<RunOutcomeKind>) {
    self.dispatcher.set_executor(Some(Arc::new(ScriptedExecutor::new(
      Arc::clone(&self.store),
      &self.dispatcher,
      outcomes,
    ))));
  }

  pub async fn close_task(&self, session_id: &str, request_id: &str, report_to: Option<&str>) {
    let guard = self.store.control_lock().await;
    let event = charliebot_store::control_events::build_control_event(
      "task_closed",
      "agent",
      session_id,
      Some(&charliebot_store::control_events::stable_close_event_id(session_id, request_id)),
      None,
      [
        ("request_id".to_string(), json!(request_id)),
        ("outcome".to_string(), json!("completed")),
        ("summary".to_string(), json!("synthetic close")),
        ("result_refs".to_string(), json!([])),
        ("run_ids".to_string(), json!([])),
        ("report_to".to_string(), report_to.map_or(Value::Null, |value| json!(value))),
      ],
    );
    self.store.append_control_fact(&guard, session_id, event).await.expect("close fact persists");
  }

  pub async fn stop_run(&self, session_id: &str, run_id: &str, request_id: &str) {
    let guard = self.store.control_lock().await;
    let event = charliebot_store::control_events::build_control_event(
      "run_stop_requested",
      "system",
      session_id,
      None,
      None,
      [("request_id".to_string(), json!(request_id)), ("run_id".to_string(), json!(run_id))],
    );
    self.store.append_control_fact(&guard, session_id, event).await.expect("stop request persists");
  }

  pub async fn archive_task(&self, session_id: &str) {
    self
      .store
      .archive_session_at(session_id, UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap())
      .await
      .expect("task session archives");
  }

  pub async fn set_successor(&self, session_id: &str, successor_id: &str) {
    let mut meta = self.store.get_session(session_id).await.unwrap().expect("source session exists");
    meta.successor_session_id = Some(successor_id.to_string());
    fs::write(self.store.metadata_path(session_id), meta.to_json_string(&self.registry.metadata_slots(), true))
      .expect("successor metadata writes");
    self.store.invalidate_cache(session_id);
    self.store.invalidate_tree_index();
  }

  pub async fn set_history_parent(&self, session_id: &str, parent_id: &str) {
    let mut meta = self.store.get_session(session_id).await.unwrap().expect("child session exists");
    meta.parent_session_id = Some(parent_id.to_string());
    fs::write(self.store.metadata_path(session_id), meta.to_json_string(&self.registry.metadata_slots(), true))
      .expect("parent metadata writes");
    self.store.invalidate_cache(session_id);
  }

  pub fn output_for(&self, nodes: &[(&str, &str)]) -> Value {
    Value::Array(
      nodes
        .iter()
        .map(|(label, session_id)| {
          let event_text = fs::read_to_string(self.store.chat_events_path(session_id)).expect("event log exists");
          let events = event_text.lines().map(str::to_string).collect::<Vec<_>>();
          let runs = self
            .store
            .list_run_records_sync(session_id)
            .expect("run records are readable")
            .iter()
            .map(|run| {
              serde_json::from_str::<Value>(
                &fs::read_to_string(self.store.run_metadata_path(session_id, &run.id)).expect("run record exists"),
              )
              .expect("run record JSON parses")
            })
            .collect::<Vec<_>>();
          json!({"node":label, "events":events, "runs":runs})
        })
        .collect(),
    )
  }
}

impl Default for Harness {
  fn default() -> Harness {
    Harness::new()
  }
}

pub fn config(home: &Path) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/synthetic/chrome".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18650,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig {
      pool: vec![BACKEND_ID.to_string()],
      routing: None,
      options: vec![BackendOption {
        id: BACKEND_ID.to_string(),
        label: "Synthetic".to_string(),
        model: "synthetic-model".to_string(),
        prompt_overlay: None,
        backend_type: "codex".to_string(),
        type_fields: Mapping::new(),
      }],
    },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

pub fn new_run(session_id: &str, run_id: &str) -> RunRecord {
  RunRecord {
    id: run_id.to_string(),
    session_id: session_id.to_string(),
    kind: RunKind::Work,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: None,
    model: None,
    native_session_id: None,
    pid: None,
    pid_start: None,
    started_at: None,
    ended_at: None,
    exit_code: None,
    repo_path: None,
    base_branch: None,
    branch_name: None,
    worktree_path: None,
    sequence_ref: None,
    raw_log_ref: None,
    events_ref: None,
    result_ref: None,
  }
}

pub fn input_fields(fields: &[(&str, Value)]) -> Value {
  Value::Object(Map::from_iter(fields.iter().map(|(key, value)| ((*key).to_string(), value.clone()))))
}

pub async fn register_run(harness: &Harness, session_id: &str, run_id: &str) {
  let guard = harness.store.control_lock().await;
  harness
    .store
    .register_run_locked(&guard, session_id, new_run(session_id, run_id), None)
    .await
    .expect("queued run registers");
}

pub async fn claim(harness: &Harness, session_id: &str, run_id: &str) -> Vec<String> {
  let guard = harness.store.control_lock().await;
  harness.dispatcher.claim_input_batch_locked(&guard, session_id, run_id, None).expect("run claims pending input")
}
