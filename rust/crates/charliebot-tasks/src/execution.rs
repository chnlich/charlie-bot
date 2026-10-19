//! The manager-run adapter that connects durable task inputs to the existing master queue.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use charliebot_agents::backend_pool;
use charliebot_agents::launch_loop;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_core::fsutil;
use charliebot_core::run_token::{RunTokenClaims, sign_run_token};
use charliebot_registry::backend::Env;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::{SequenceCompletion, SequenceExecutor, SequenceLaunchSettlement};
use charliebot_store::control_events::{self, ACTOR_SYSTEM};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::{StoreError, StoreResult};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::dispatch::{DispatchExecutor, Dispatcher, RunFinishedCompletion};
use crate::master_queue::MasterQueue;
use crate::master_state::{MasterTurnRequest, TaskFinishCallback, TaskRunBinding, TaskSpawnCallback};
use crate::prompts::{self, PromptEnv, PromptSnapshot, TaskPromptError};
use crate::session_anchors;
use crate::spawner_backends;

/// The launch result visible to sequence callers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSettlement {
  /// The durable terminal outcome when the run executed.
  pub outcome: Option<String>,
  /// The reason a launch was withheld before a process started.
  pub withheld: Option<String>,
}

type LaunchKey = (String, String);

#[derive(Default)]
struct LaunchState {
  inflight: HashSet<LaunchKey>,
  settlements: HashMap<LaunchKey, watch::Sender<Option<LaunchSettlement>>>,
}

/// The manager execution adapter for durable task runs.
#[derive(Clone)]
pub struct TaskExecutionAdapter {
  features: Arc<FeatureContext>,
  queue: Arc<MasterQueue>,
  dispatch: Arc<Dispatcher>,
  launches: Arc<Mutex<LaunchState>>,
}

impl TaskExecutionAdapter {
  /// Build the adapter that dispatches manager turns on `queue`.
  pub fn new(features: Arc<FeatureContext>, queue: Arc<MasterQueue>, dispatch: Arc<Dispatcher>) -> Self {
    Self { features, queue, dispatch, launches: Arc::new(Mutex::new(LaunchState::default())) }
  }

  /// Reserve one fresh input batch or schedule one queued run.
  pub async fn call(
    &self,
    session_id: &str,
    pending_inputs: &[Value],
    launch_run_id: Option<&str>,
  ) -> StoreResult<Option<String>> {
    let first_meta = self.features.store.load_task_meta(session_id).await?;
    refuse_worker(&first_meta)?;
    let mut scheduled = None;
    let run_id;
    {
      let guard = self.features.store.control_lock().await;
      let meta = self.features.store.load_task_meta(session_id).await?;
      refuse_worker(&meta)?;
      if self.features.store.task_state(session_id)? != "open" {
        return Ok(None);
      }
      if let Some(launch_run_id) = launch_run_id {
        let run = self
          .features
          .store
          .read_run_sync(session_id, launch_run_id)?
          .ok_or_else(|| StoreError::not_found(format!("run {launch_run_id} not found in session {session_id}")))?;
        let facts = self.features.store.load_all_facts(session_id)?;
        if run_records::terminal_outcome(&facts, &run.id)?.is_some()
          || run.pid.is_some()
          || run_records::stop_requested_in_events(&facts, &run.id, None)
        {
          return Ok(None);
        }
        if run.kind == RunKind::ManagerTurn && run.input_event_ids.is_empty() {
          if pending_inputs.is_empty() {
            tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, "queued_run_void_reservation");
            return Ok(None);
          }
          self.dispatch.claim_input_batch_locked(&guard, session_id, &run.id, None)?;
        }
        run_id = run.id;
      } else {
        let pending_now = self.dispatch.pending_inputs(session_id)?;
        if pending_now.is_empty() {
          return Ok(None);
        }
        let cfg = self.features.config.get().map_err(|error| StoreError::new(error.to_string()))?;
        let (backend, model) = spawner_backends::resolve_session_default_backend_model(&cfg, &meta)
          .map_err(|error| StoreError::invalid(error.to_string()))?;
        let mut input_ids = pending_now.iter().map(|event| python_string(event.get("id"))).collect::<Vec<_>>();
        input_ids.sort();
        let request_id = format!("dispatch:{}", control_events::sha256_hex(&input_ids.join("\0")));
        run_id = control_events::stable_run_id(session_id, &request_id);
        if self.features.store.read_run_sync(session_id, &run_id)?.is_none() {
          let run = manager_run_record(session_id, &run_id, &backend, model.as_deref());
          let task_spec = canonical_task_spec_text(meta.task.as_ref())?;
          self.features.store.register_run_locked(&guard, session_id, run, task_spec.as_deref()).await?;
        }
        match self.dispatch.claim_input_batch_locked(&guard, session_id, &run_id, None) {
          Ok(bound) if !bound.is_empty() => {}
          Ok(_) => {
            return Err(StoreError::new(format!(
              "dispatch reserved run {run_id} against a batch that vanished within one lock hold ({session_id})"
            )));
          }
          Err(StoreError::Conflict(blockers)) => {
            tracing::info!(target: "charliebot_tasks", session_id, run_id, error = %blockers.join("; "), "dispatch_reservation_contested");
            return Ok(None);
          }
          Err(error) => return Err(error),
        }
      }
      let key = (session_id.to_string(), run_id.clone());
      let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
      if !state.inflight.contains(&key) {
        state.inflight.insert(key.clone());
        let (sender, _) = watch::channel(None);
        state.settlements.insert(key.clone(), sender);
        scheduled = Some(key);
      }
    }
    if let Some(key) = scheduled {
      self.schedule_launch(key.0.clone(), key.1.clone(), None);
    }
    Ok(Some(run_id))
  }

  /// Schedule a registered run once.
  pub fn launch(&self, session_id: &str, run_id: &str) -> StoreResult<()> {
    let key = (session_id.to_string(), run_id.to_string());
    let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
    if state.inflight.contains(&key) {
      return Ok(());
    }
    state.inflight.insert(key.clone());
    let (sender, _) = watch::channel(None);
    state.settlements.insert(key, sender);
    drop(state);
    self.schedule_launch(session_id.to_string(), run_id.to_string(), None);
    Ok(())
  }

  /// Recheck a registered run and execute its manager turn.
  pub async fn execute_run(&self, session_id: &str, run_id: &str) -> anyhow::Result<LaunchSettlement> {
    let initial_meta = self.features.store.load_task_meta(session_id).await?;
    refuse_worker(&initial_meta)?;
    let meta;
    let run;
    let withheld;
    {
      let _guard = self.features.store.control_lock().await;
      meta = self.features.store.load_task_meta(session_id).await?;
      refuse_worker(&meta)?;
      run = self
        .features
        .store
        .read_run_sync(session_id, run_id)?
        .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in session {session_id}")))?;
      if run.kind != RunKind::ManagerTurn {
        return Err(
          StoreError::invalid(format!(
            "run {run_id} (profile={}, kind={}) has no executable adapter",
            profile_name(meta.profile),
            kind_name(run.kind)
          ))
          .into(),
        );
      }
      let state = self.features.store.task_state(session_id)?;
      if state != "open" {
        let reason = format!("task {session_id} is {state}");
        tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "run_launch_withheld");
        withheld = Some((reason, true));
      } else {
        self.features.store.require_open_ancestry(session_id)?;
        let facts = self.features.store.load_all_facts(session_id)?;
        if run_records::terminal_outcome(&facts, run_id)?.is_some() {
          tracing::info!(target: "charliebot_tasks", session_id, run_id, "run_launch_refused_by_facts");
          return Ok(LaunchSettlement {
            outcome: Some(outcome_text(run_records::terminal_outcome(&facts, run_id)?.unwrap())),
            withheld: None,
          });
        }
        if run_records::stop_requested_in_events(&facts, run_id, None) {
          let reason = format!("run {run_id} has a durable stop request");
          tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "run_launch_withheld");
          withheld = Some((reason, false));
        } else {
          withheld = None;
        }
      }
    }
    if let Some((reason, settle_closed)) = withheld {
      self.record_launch_withheld(&meta, &run, &reason).await?;
      if settle_closed {
        self.features.store.request_stop(session_id, run_id, &format!("closed-task:{run_id}")).await?;
      }
      return Ok(LaunchSettlement { outcome: None, withheld: Some(format!("withheld: {reason}")) });
    }

    let cfg = self.features.config.get().map_err(|error| StoreError::new(error.to_string()))?;
    let option = match self.resolve_run_backend(&cfg, &run) {
      Ok(option) => option,
      Err(error) => {
        self.land_launch_failure(&meta, &run, &error, "ValueError").await;
        return Err(error);
      }
    };
    let snapshot = match self.prepare_launch_snapshot(&cfg, &meta, &run, &option).await {
      Ok(snapshot) => snapshot,
      Err(error) if error.downcast_ref::<TaskPromptError>().is_some() => {
        tracing::error!(target: "charliebot_tasks", session_id, run_id, kind = "manager_turn", error = %error, "task_prompt_preparation_failed");
        let reason = format!("prompt preparation failed: {error}");
        self.record_launch_withheld(&meta, &run, &reason).await?;
        return Ok(LaunchSettlement { outcome: None, withheld: Some(format!("withheld: {reason}")) });
      }
      Err(error) => {
        self.land_launch_failure(&meta, &run, &error, "RuntimeError").await;
        return Err(error);
      }
    };
    if meta.profile != TaskProfile::Manager {
      return Err(
        StoreError::invalid(format!(
          "run {run_id} (profile={}, kind={}) has no executable adapter",
          profile_name(meta.profile),
          kind_name(run.kind)
        ))
        .into(),
      );
    }
    if let Err(error) = self.execute_manager_turn(&cfg, &meta, &run, &option, &snapshot).await {
      self.land_launch_failure(&meta, &run, &error, "RuntimeError").await;
      return Err(error);
    }
    let outcome = self.await_terminal(session_id, run_id).await?;
    Ok(LaunchSettlement { outcome: Some(outcome), withheld: None })
  }

  fn schedule_launch(&self, session_id: String, run_id: String, _prompt: Option<String>) {
    let adapter = self.clone();
    tokio::spawn(async move {
      let result = adapter.execute_run(&session_id, &run_id).await;
      let settlement = match result {
        Ok(settlement) => settlement,
        Err(error) => {
          tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %error, "task_run_execution_failed");
          if is_out_of_space_error(error.as_ref()) {
            adapter.note_launch_out_of_space(&session_id, &run_id);
          }
          match adapter.terminal_outcome(&session_id, &run_id) {
            Ok(Some(outcome)) => LaunchSettlement { outcome: Some(outcome), withheld: None },
            Ok(None) => LaunchSettlement { outcome: None, withheld: Some(format!("failed-to-start: {error}")) },
            Err(read_error) => {
              tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %read_error, "run_terminal_read_failed");
              LaunchSettlement { outcome: None, withheld: Some(format!("failed-to-start: {error}")) }
            }
          }
        }
      };
      let key = (session_id, run_id);
      let mut state = adapter.launches.lock().expect("task execution launch state is not poisoned");
      state.inflight.remove(&key);
      if let Some(sender) = state.settlements.remove(&key) {
        let _ = sender.send(Some(settlement));
      }
    });
  }

  fn resolve_run_backend(&self, cfg: &CharlieBotConfig, run: &RunRecord) -> anyhow::Result<BackendOption> {
    let backend = run
      .backend
      .as_deref()
      .filter(|backend| !backend.is_empty())
      .ok_or_else(|| anyhow!("run {} records no backend; refusing to substitute one", run.id))?;
    spawner_backends::resolve_backend_option(cfg, backend, run.model.as_deref())
  }

  async fn prepare_launch_snapshot(
    &self,
    cfg: &CharlieBotConfig,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
  ) -> anyhow::Result<PromptSnapshot> {
    let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
    let env = PromptEnv { cfg, registry: &self.features.registry, checkout: &checkout };
    let coherent =
      prompts::assemble_coherent_snapshot(&self.features.store, &env, meta, kind_name(run.kind), option).await?;
    let run_dir = self.features.store.run_dir(&meta.id, &run.id);
    let snapshot_path = run_dir.join(prompts::SNAPSHOT_FILENAME);
    let snapshot_text = serde_json::to_string_pretty(&coherent.snapshot.to_json_dict())?;
    fsutil::atomic_write_text(&snapshot_path, &snapshot_text)?;
    self
      .features
      .store
      .record_observation(
        &meta.id,
        &run.id,
        Map::from_iter([("prompt_snapshot_ref".to_string(), json!(snapshot_path.to_string_lossy()))]),
      )
      .await?;
    if !coherent.declared || coherent.overlay_error.is_some() {
      self.alert_overlay_inactive(meta, option, coherent.overlay_error.as_ref()).await?;
    }
    Ok(coherent.snapshot)
  }

  async fn alert_overlay_inactive(
    &self,
    meta: &SessionMetadata,
    option: &BackendOption,
    overlay_error: Option<&prompts::OverlayError>,
  ) -> StoreResult<()> {
    let reason = if overlay_error.is_some() { "unreadable" } else { "undeclared" };
    tracing::warn!(target: "charliebot_tasks", session_id = %meta.id, backend = %option.id, reason, "task_overlay_inactive");
    let mut event = Map::from_iter([
      ("type".to_string(), json!(event_types::BACKEND_OVERLAY_INACTIVE)),
      ("backend".to_string(), json!(option.id)),
      ("reason".to_string(), json!(reason)),
    ]);
    if let Some(error) = overlay_error {
      event.insert("overlay".to_string(), json!(option.prompt_overlay));
      event.insert("error".to_string(), json!(error.python_class_name()));
    }
    self.features.store.persist_and_broadcast(&meta.id, Value::Object(event)).await?;
    Ok(())
  }

  async fn execute_manager_turn(
    &self,
    cfg: &CharlieBotConfig,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
    snapshot: &PromptSnapshot,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    let run_id = run.id.as_str();
    let run_dir = self.features.store.run_dir(session_id, run_id);
    let batch_ids = run.input_event_ids.iter().cloned().collect::<HashSet<_>>();
    let events = self
      .features
      .store
      .load_all_facts(session_id)?
      .into_iter()
      .filter(|event| event.get("id").and_then(Value::as_str).is_some_and(|id| batch_ids.contains(id)))
      .collect::<Vec<_>>();
    let provenances = self.dispatch.child_report_provenances(&events).await?;
    let (mut content, uploaded_files) = compose_input_prompt(&self.features, meta, &events, &provenances)?;
    if content.is_empty() {
      anyhow::bail!("run {run_id} claimed no consumable input; nothing to execute");
    }
    if let Some(prefix) = apply_sequence_wake_duties(&self.features, meta, &events).await? {
      content = format!("{prefix}{content}");
    }
    let fresh_native = !native_continuation_matches(
      meta.cc_session_id.as_deref(),
      meta.native_prompt_hash.as_deref(),
      meta.native_backend.as_deref(),
      meta.native_model.as_deref(),
      &snapshot.prompt_hash(),
      option,
    );
    let mut prompt = content;
    if fresh_native && meta.cc_session_id.is_some() {
      let reason = match meta.native_backend.as_deref().filter(|backend| !backend.is_empty() && *backend != option.id) {
        Some(previous) => session_anchors::backend_switch_reset_reason(previous, &option.id),
        None => "this task's managed instructions or sources changed since the previous turn, so this turn starts a fresh native conversation".to_string(),
      };
      let goal = meta.task.as_ref().map_or(meta.name.as_str(), |task| task.goal.as_str());
      prompt = format!("{}\n\n{prompt}", session_anchors::context_reset_note(&reason, goal));
    }
    self.persist_launch_text(session_id, run_id, &prompt)?;
    tracing::info!(
      target: "charliebot_tasks",
      session_id,
      run_id,
      backend = %option.id,
      inputs = run.input_event_ids.len(),
      prompt_hash = %snapshot.prompt_hash().chars().take(12).collect::<String>(),
      fresh_native,
      "manager_turn_launching"
    );

    let features = Arc::clone(&self.features);
    let queue = Arc::clone(&self.queue);
    let tree_activity: crate::master_queue::TaskTreeActivityFn = Arc::new(move |id| {
      let activity = features.store.task_tree_activity(id)?;
      Ok((activity.has_running_tasks, activity.work_state))
    });
    let callbacks = queue.session_callbacks(Arc::clone(&self.features), tree_activity);
    let spawn_features = Arc::clone(&self.features);
    let spawn_meta = meta.clone();
    let spawn_snapshot_hash = snapshot.prompt_hash();
    let spawn_backend = option.id.clone();
    let spawn_model = option.model.clone();
    let spawn_session = session_id.to_string();
    let spawn_run = run_id.to_string();
    let on_task_spawn: TaskSpawnCallback = Arc::new(move |pid, pid_start| {
      let features = Arc::clone(&spawn_features);
      let meta = spawn_meta.clone();
      let prompt_hash = spawn_snapshot_hash.clone();
      let backend = spawn_backend.clone();
      let model = spawn_model.clone();
      let session_id = spawn_session.clone();
      let run_id = spawn_run.clone();
      Box::pin(async move {
        let pid_start = pid_start.ok_or_else(|| anyhow!("run {run_id} spawned without a pinned pid_start"))?;
        features.store.record_launch(&session_id, &run_id, pid, &pid_start).await?;
        features
          .store
          .record_native_anchor_with_reset(
            &session_id,
            &prompt_hash,
            &backend,
            Some(&model),
            !native_anchor_matches(&meta, &prompt_hash, &backend, &model),
          )
          .await?;
        Ok(())
      })
    });
    let finish_features = Arc::clone(&self.features);
    let finish_dispatch = Arc::clone(&self.dispatch);
    let finish_session = session_id.to_string();
    let finish_run_id = run_id.to_string();
    let finish_option = option.clone();
    let raw_log = run_dir.join(run_records::RAW_LOG_NAME);
    let on_task_finish: TaskFinishCallback = Arc::new(move |native_session_id, exit_code, extras| {
      let features = Arc::clone(&finish_features);
      let dispatch = Arc::clone(&finish_dispatch);
      let session_id = finish_session.clone();
      let run_id = finish_run_id.clone();
      let option = finish_option.clone();
      let raw_log = raw_log.clone();
      Box::pin(async move {
        features
          .store
          .record_observation(
            &session_id,
            &run_id,
            Map::from_iter([
              ("native_session_id".to_string(), json!(native_session_id)),
              (
                "model".to_string(),
                json!(
                  extras
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| !model.is_empty())
                    .unwrap_or(&option.model)
                ),
              ),
              ("raw_log_ref".to_string(), json!(raw_log.to_string_lossy())),
              ("result_ref".to_string(), json!(raw_log.to_string_lossy())),
            ]),
          )
          .await?;
        let mut outcome =
          RunOutcome::new(if exit_code == 0 { RunOutcomeKind::Success } else { RunOutcomeKind::Failed });
        outcome.exit_code = Some(exit_code);
        dispatch.finish_run(&session_id, &run_id, outcome).await?;
        dispatch.dispatch_pending(&session_id).await?;
        Ok(())
      })
    });
    let env = self.child_env(session_id, run_id, meta.name.as_str())?;
    queue
      .run_message(MasterTurnRequest {
        cfg: Arc::new(cfg.clone()),
        session_meta: meta.clone(),
        user_content: prompt,
        callbacks,
        user_event_ids: run.input_event_ids.clone(),
        task_instructions: snapshot.instructions_text(),
        task_run: TaskRunBinding {
          session_id: session_id.to_string(),
          run_id: run_id.to_string(),
          transport_dir: run_dir,
          fresh_native_context: fresh_native,
        },
        on_task_spawn,
        on_task_finish,
        auto_trigger: events
          .iter()
          .any(|event| event.get("type").and_then(Value::as_str) == Some(event_types::SCHEDULED_TRIGGER)),
        backend_option: Some(option.clone()),
        extra_claude_flags: None,
        uploaded_files: (!uploaded_files.is_empty()).then_some(uploaded_files),
        extra_env: Some(env),
      })
      .await?;
    Ok(())
  }

  fn child_env(&self, session_id: &str, run_id: &str, agent: &str) -> StoreResult<Env> {
    let key = self
      .features
      .credentials
      .get()
      .map_err(|error| StoreError::new(error.to_string()))?
      .access_key()
      .filter(|key| !key.is_empty())
      .ok_or_else(|| StoreError::new(
        "run-token signing requires credentials.yaml charliebot.access_key; agent child environments cannot be built without it",
      ))?;
    let token = sign_run_token(
      &RunTokenClaims {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        agent: if agent.is_empty() { "worker".to_string() } else { agent.to_string() },
      },
      &key,
    )
    .map_err(|error| StoreError::new(error.to_string()))?;
    Ok(Env::from([
      (launch_loop::SESSION_ID_ENV.to_string(), session_id.to_string()),
      ("CHARLIEBOT_RUN_TOKEN".to_string(), token),
      (
        "CHARLIEBOT_HOME".to_string(),
        self
          .features
          .config
          .get()
          .map_err(|error| StoreError::new(error.to_string()))?
          .charliebot_home
          .to_string_lossy()
          .into_owned(),
      ),
    ]))
  }

  fn persist_launch_text(&self, session_id: &str, run_id: &str, prompt: &str) -> StoreResult<()> {
    let path = self.features.store.run_dir(session_id, run_id).join(prompts::LAUNCH_TEXT_FILENAME);
    fsutil::atomic_write_text(&path, prompt).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
  }

  async fn record_launch_withheld(&self, meta: &SessionMetadata, run: &RunRecord, reason: &str) -> StoreResult<()> {
    let session_id = &meta.id;
    let run_id = &run.id;
    let event_id = control_events::stable_withheld_event_id(run_id, reason);
    let epoch = self.features.store.prime_aggregator(session_id).await?;
    let parent_epoch = match meta.task_parent_id.as_deref() {
      Some(parent) => Some(self.features.store.prime_aggregator(parent).await?),
      None => None,
    };
    let (event, report, report_created) = {
      let guard = self.features.store.control_lock().await;
      let existing = self
        .features
        .store
        .load_all_facts(session_id)?
        .into_iter()
        .find(|fact| fact.get("id").and_then(Value::as_str) == Some(event_id.as_str()));
      let event = match existing {
        Some(event) => event,
        None => {
          let event = control_events::build_control_event(
            event_types::RUN_LAUNCH_WITHHELD,
            ACTOR_SYSTEM,
            session_id,
            Some(&event_id),
            None,
            [("run_id".to_string(), json!(run_id)), ("reason".to_string(), json!(reason))],
          );
          self.features.store.append_control_fact(&guard, session_id, event).await?
        }
      };
      let result = if let Some(parent) = meta.task_parent_id.as_deref() {
        self
          .dispatch
          .deliver_child_report_locked(
            &guard,
            session_id,
            event.clone(),
            "blocked",
            &format!("launch withheld: {reason}"),
            Some(&[format!("run:{run_id}")]),
            Some(parent),
          )
          .await?
      } else {
        (Value::Null, false)
      };
      (event, result.0, result.1)
    };
    self.features.store.announce_appended_event(session_id, &event, epoch).await?;
    if report_created && let (Some(parent), Some(parent_epoch)) = (meta.task_parent_id.as_deref(), parent_epoch) {
      self.features.store.announce_appended_event(parent, &report, parent_epoch).await?;
      self.dispatch.wake_parent(parent, &report).await?;
    }
    if meta.profile == TaskProfile::Manager {
      self.record_turn_launch_notice(session_id, run_id, reason).await?;
    }
    Ok(())
  }

  async fn land_launch_failure(
    &self,
    meta: &SessionMetadata,
    initial_run: &RunRecord,
    error: &anyhow::Error,
    error_kind: &str,
  ) {
    let session_id = meta.id.as_str();
    let run_id = initial_run.id.as_str();
    tracing::error!(target: "charliebot_tasks", session_id, run_id, kind = "manager_turn", error = %error, "task_run_launch_failed");
    let run =
      self.features.store.read_run_sync(session_id, run_id).ok().flatten().unwrap_or_else(|| initial_run.clone());
    let error_text = format!("{error_kind}: {error}").chars().take(2000).collect::<String>();
    if run.pid.is_some() && is_out_of_space_error(error.as_ref()) {
      if let Err(landing_error) = self.record_launch_error_event(session_id, run_id, &error_text, None).await {
        tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %landing_error, "task_run_launch_failure_landing_failed");
      }
      return;
    }
    let landing = async {
      self.record_launch_error_event(session_id, run_id, &error_text, None).await?;
      let mut outcome = RunOutcome::new(RunOutcomeKind::Failed);
      outcome.exit_code = Some(-1);
      self.dispatch.finish_run(session_id, run_id, outcome).await?;
      if meta.profile == TaskProfile::Manager {
        self.record_turn_launch_notice(session_id, run_id, &error_text).await?;
      }
      self.dispatch.dispatch_pending(session_id).await?;
      Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(landing_error) = landing {
      tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %landing_error, "task_run_launch_failure_landing_failed");
      if is_out_of_space_error(landing_error.as_ref()) {
        self.start_run_end_landing_retry(session_id, run_id);
      }
    }
  }

  async fn record_launch_error_event(
    &self,
    session_id: &str,
    run_id: &str,
    error_text: &str,
    quota_exhausted: Option<bool>,
  ) -> anyhow::Result<()> {
    let event_path = self.features.store.run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME);
    let mut event = Map::from_iter([
      ("type".to_string(), json!(event_types::ERROR)),
      ("message".to_string(), json!(error_text)),
      ("content".to_string(), json!(error_text)),
      ("timestamp".to_string(), json!(charliebot_store::metadata::UtcDateTime::now().to_python_isoformat())),
    ]);
    if let Some(quota_exhausted) = quota_exhausted {
      event.insert("quota_exhausted".to_string(), json!(quota_exhausted));
    }
    let mut line = serde_json::to_string(&Value::Object(event))?;
    line.push('\n');
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&event_path)
      .with_context(|| format!("opening {}", event_path.display()))?;
    file.write_all(line.as_bytes()).with_context(|| format!("writing {}", event_path.display()))?;
    file.flush().with_context(|| format!("flushing {}", event_path.display()))?;
    self
      .features
      .store
      .record_observation(
        session_id,
        run_id,
        Map::from_iter([("events_ref".to_string(), json!(event_path.to_string_lossy()))]),
      )
      .await?;
    Ok(())
  }

  async fn record_turn_launch_notice(&self, session_id: &str, run_id: &str, reason: &str) -> StoreResult<()> {
    let run = self
      .features
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in session {session_id}")))?;
    let outcome = if run.pid.is_some() { "Turn failed" } else { "Turn did not start" };
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"charliebot/session-task-tree");
    let event_id = Uuid::new_v5(&namespace, format!("turn-launch:{run_id}:{reason}").as_bytes()).to_string();
    let epoch = self.features.store.prime_aggregator(session_id).await?;
    let event = {
      let _guard = self.features.store.control_lock().await;
      if self
        .features
        .store
        .load_all_facts(session_id)?
        .iter()
        .any(|event| event.get("id").and_then(Value::as_str) == Some(event_id.as_str()))
      {
        return Ok(());
      }
      self
        .features
        .store
        .append_chat_event(
          session_id,
          json!({
            "id": event_id,
            "type": event_types::ERROR,
            "source": "turn_launch",
            "run_id": run_id,
            "content": format!("{outcome}: {reason}"),
            "timestamp": charliebot_store::metadata::UtcDateTime::now().to_python_isoformat(),
          }),
        )
        .await?
    };
    self.features.store.announce_appended_event(session_id, &event, epoch).await?;
    Ok(())
  }

  async fn await_terminal(&self, session_id: &str, run_id: &str) -> StoreResult<String> {
    loop {
      if let Some(outcome) = self.terminal_outcome(session_id, run_id)? {
        return Ok(outcome);
      }
      tokio::time::sleep(Duration::from_millis(200)).await;
    }
  }

  fn terminal_outcome(&self, session_id: &str, run_id: &str) -> StoreResult<Option<String>> {
    Ok(run_records::terminal_outcome(&self.features.store.load_all_facts(session_id)?, run_id)?.map(outcome_text))
  }

  fn note_launch_out_of_space(&self, session_id: &str, run_id: &str) {
    self.start_run_end_landing_retry(session_id, run_id);
  }

  fn start_run_end_landing_retry(&self, session_id: &str, run_id: &str) {
    tracing::error!(target: "charliebot_tasks", session_id, run_id, "run_end_landing_retry_unavailable");
  }
}

impl DispatchExecutor for TaskExecutionAdapter {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [Value],
    launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move { self.call(session_id, pending_inputs, launch_run_id).await })
  }
}

impl SequenceExecutor for TaskExecutionAdapter {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    _prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move {
      let meta = self.features.store.load_task_meta(session_id).await?;
      refuse_worker(&meta)?;
      let key = (session_id.to_string(), run_id.to_string());
      let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
      if state.inflight.contains(&key) {
        return Ok(());
      }
      state.inflight.insert(key.clone());
      let (sender, _) = watch::channel(None);
      state.settlements.insert(key, sender);
      drop(state);
      self.schedule_launch(session_id.to_string(), run_id.to_string(), None);
      Ok(())
    })
  }

  fn launch_and_settle<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    _prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<SequenceLaunchSettlement>> {
    Box::pin(async move {
      if let Some(outcome) = self.terminal_outcome(session_id, run_id)? {
        return Ok(SequenceLaunchSettlement { outcome: Some(outcome), withheld: None });
      }
      let run = self
        .features
        .store
        .read_run_sync(session_id, run_id)?
        .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in session {session_id}")))?;
      if run.pid.is_some() {
        let outcome = self.await_terminal(session_id, run_id).await?;
        return Ok(SequenceLaunchSettlement { outcome: Some(outcome), withheld: None });
      }
      let meta = self.features.store.load_task_meta(session_id).await?;
      refuse_worker(&meta)?;
      <Self as SequenceExecutor>::launch(self, session_id, run_id, None).await?;
      let key = (session_id.to_string(), run_id.to_string());
      let mut receiver = {
        let state = self.launches.lock().expect("task execution launch state is not poisoned");
        state.settlements.get(&key).map(watch::Sender::subscribe)
      };
      if let Some(receiver) = receiver.as_mut() {
        loop {
          if let Some(settlement) = receiver.borrow().clone() {
            return Ok(SequenceLaunchSettlement { outcome: settlement.outcome, withheld: settlement.withheld });
          }
          receiver.changed().await.map_err(|_| StoreError::new(format!("run {run_id} launch settlement closed")))?;
        }
      }
      if let Some(outcome) = self.terminal_outcome(session_id, run_id)? {
        return Ok(SequenceLaunchSettlement { outcome: Some(outcome), withheld: None });
      }
      Err(StoreError::new(format!("run {run_id} settled without a terminal fact or launch verdict")))
    })
  }
}

/// Run the sequence binding's wake duties and return its input prefix.
pub async fn apply_sequence_wake_duties(
  features: &FeatureContext,
  meta: &SessionMetadata,
  input_events: &[Value],
) -> anyhow::Result<Option<String>> {
  match features.registry.binding_for(&meta.id)? {
    Some(binding) => binding.on_wake(meta, input_events).await,
    None => Ok(None),
  }
}

/// Whether the manager anchor can continue under this snapshot and backend.
pub fn native_continuation_matches(
  held_id: Option<&str>,
  recorded_hash: Option<&str>,
  recorded_backend: Option<&str>,
  recorded_model: Option<&str>,
  prompt_hash: &str,
  option: &BackendOption,
) -> bool {
  held_id.is_some()
    && recorded_hash == Some(prompt_hash)
    && recorded_backend == Some(option.id.as_str())
    && recorded_model == Some(option.model.as_str())
}

/// Compose the manager input prompt and uploaded-file list from the claimed facts.
pub fn compose_input_prompt(
  features: &FeatureContext,
  meta: &SessionMetadata,
  events: &[Value],
  report_provenance: &Map<String, Value>,
) -> anyhow::Result<(String, Vec<Value>)> {
  let mut parts = Vec::new();
  let mut uploads = Vec::new();
  for event in events {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let content = event.get("content").and_then(Value::as_str).unwrap_or_default();
    let text = match event_type {
      event_types::USER => match features.registry.input_preamble(meta, event)? {
        Some(preamble) => format!("{preamble}\n{content}"),
        None => content.to_string(),
      },
      event_types::AGENT_MESSAGE => {
        let source = event
          .get("from_session_name")
          .and_then(Value::as_str)
          .filter(|name| !name.is_empty())
          .or_else(|| event.get("from_session").and_then(Value::as_str).filter(|id| !id.is_empty()))
          .unwrap_or("unknown");
        format!("[Message from session {source}] {content}")
      }
      event_types::CHILD_REPORT => {
        let child_id = python_string(event.get("child_session_id"));
        let provenance = report_provenance.get(&child_id).and_then(Value::as_str);
        crate::dispatch::child_report_text(event, provenance)
      }
      event_types::SCHEDULED_TRIGGER => format!("[Scheduled trigger] {content}"),
      _ => content.to_string(),
    };
    let event_uploads = event.get("uploaded_files").and_then(Value::as_array).cloned().unwrap_or_default();
    parts.push(charliebot_store::message_events::build_agent_input_content(&text, &event_uploads));
    uploads.extend(event_uploads);
  }
  Ok((parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"), uploads))
}

/// Whether an error or one of its sources reports ENOSPC or EDQUOT.
pub fn is_out_of_space_error(error: &(dyn Error + 'static)) -> bool {
  let mut current = Some(error);
  let mut seen = HashSet::new();
  while let Some(error) = current {
    let identity = error as *const dyn Error as *const () as usize;
    if !seen.insert(identity) {
      return false;
    }
    if error
      .downcast_ref::<std::io::Error>()
      .and_then(std::io::Error::raw_os_error)
      .is_some_and(|errno| errno == libc::ENOSPC || errno == libc::EDQUOT)
    {
      return true;
    }
    current = error.source();
  }
  false
}

/// Free GiB on the filesystem containing `path`, probing its nearest existing ancestor.
pub fn free_disk_gib(path: &Path) -> anyhow::Result<f64> {
  let mut probe = path;
  while !probe.exists() {
    let parent = probe.parent().ok_or_else(|| anyhow!("no existing ancestor to probe for {}", path.display()))?;
    if parent == probe {
      anyhow::bail!("no existing ancestor to probe for {}", path.display());
    }
    probe = parent;
  }
  let path_c = CString::new(probe.as_os_str().as_bytes())?;
  let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
  // `probe` is an existing path and `path_c` is NUL terminated.
  if unsafe { libc::statvfs(path_c.as_ptr(), stat.as_mut_ptr()) } != 0 {
    return Err(std::io::Error::last_os_error()).with_context(|| format!("reading disk space at {}", probe.display()));
  }
  let stat = unsafe { stat.assume_init() };
  Ok((stat.f_bavail as f64 * stat.f_frsize as f64) / (1024.0_f64.powi(3)))
}

fn refuse_worker(meta: &SessionMetadata) -> StoreResult<()> {
  if meta.profile == TaskProfile::Worker {
    return Err(StoreError::invalid("worker execution is owned by the worker-run task"));
  }
  Ok(())
}

fn manager_run_record(session_id: &str, run_id: &str, backend: &str, model: Option<&str>) -> RunRecord {
  RunRecord {
    id: run_id.to_string(),
    session_id: session_id.to_string(),
    kind: RunKind::ManagerTurn,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some(backend.to_string()),
    model: model.map(str::to_string),
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

fn canonical_task_spec_text(task: Option<&charliebot_store::metadata::TaskSpec>) -> StoreResult<Option<String>> {
  task
    .map(serde_json::to_string)
    .transpose()
    .map_err(|error| StoreError::new(format!("serializing task spec: {error}")))
}

fn native_anchor_matches(meta: &SessionMetadata, prompt_hash: &str, backend: &str, model: &str) -> bool {
  native_continuation_matches(
    meta.cc_session_id.as_deref(),
    meta.native_prompt_hash.as_deref(),
    meta.native_backend.as_deref(),
    meta.native_model.as_deref(),
    prompt_hash,
    &BackendOption {
      id: backend.to_string(),
      label: backend.to_string(),
      model: model.to_string(),
      prompt_overlay: None,
      backend_type: String::new(),
      type_fields: Default::default(),
    },
  )
}

fn python_string(value: Option<&Value>) -> String {
  match value {
    Some(Value::String(value)) => value.clone(),
    Some(Value::Null) | None => "None".to_string(),
    Some(value) => value.to_string(),
  }
}

fn outcome_text(outcome: RunOutcomeKind) -> String {
  match outcome {
    RunOutcomeKind::Success => "success",
    RunOutcomeKind::Failed => "failed",
    RunOutcomeKind::Interrupted => "interrupted",
  }
  .to_string()
}

fn profile_name(profile: TaskProfile) -> &'static str {
  match profile {
    TaskProfile::Manager => "manager",
    TaskProfile::Worker => "worker",
  }
}

fn kind_name(kind: RunKind) -> &'static str {
  match kind {
    RunKind::ManagerTurn => "manager_turn",
    RunKind::Work => "work",
    RunKind::Review => "review",
    RunKind::Iteration => "iteration",
    RunKind::ScheduledStep => "scheduled_step",
  }
}

/// Build the task execution runtime before recovery begins.
pub fn pre_recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let pool = backend_pool::backend_pool(&features)?;
    features.startup.insert(MasterQueue::new(pool))?;
    let queue = features.startup.require::<MasterQueue>()?;
    let sequence_completion: Arc<dyn SequenceCompletion> = Arc::new(CompletionTaskPending);
    let run_completion: Arc<dyn RunFinishedCompletion> = Arc::new(CompletionTaskPending);
    features.startup.insert(Dispatcher::new(Arc::clone(&features.store), sequence_completion, run_completion))?;
    let dispatch = features.startup.require::<Dispatcher>()?;
    let adapter = Arc::new(TaskExecutionAdapter::new(Arc::clone(&features), Arc::clone(&queue), Arc::clone(&dispatch)));
    dispatch.set_executor(Some(adapter.clone()));
    features.startup.insert((*adapter).clone())?;
    Ok(())
  })
}

struct CompletionTaskPending;

impl RunFinishedCompletion for CompletionTaskPending {
  fn after_run_finished<'a>(&'a self, _session_id: &'a str, _run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async { Err(StoreError::conflict(vec!["completion task owns post-finish completion".to_string()])) })
  }
}

impl SequenceCompletion for CompletionTaskPending {
  fn evaluate_automatic_completion<'a>(
    &'a self,
    _session_id: &'a str,
    _run_id: &'a str,
    _summary: Option<&'a str>,
    _result_refs: Option<&'a [String]>,
    _request_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Err(completion_task_error()) })
  }

  fn close_sequence_child<'a>(
    &'a self,
    _session_id: &'a str,
    _request_id: &'a str,
    _outcome: &'a str,
    _evidence: &'a Value,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async { Err(completion_task_error()) })
  }

  fn restore_chain<'a>(
    &'a self,
    _session_id: &'a str,
    _request_id: &'a str,
    _reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async { Err(completion_task_error()) })
  }

  fn restore_chain_locked<'a>(
    &'a self,
    _guard: &'a charliebot_store::ControlGuard,
    _session_id: &'a str,
    _request_id: &'a str,
    _reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<charliebot_registry::sequence::RestoreOutcome>> {
    Box::pin(async { Err(completion_task_error()) })
  }
}

fn completion_task_error() -> StoreError {
  StoreError::conflict(vec!["completion task owns task completion calls".to_string()])
}

#[cfg(test)]
mod tests {
  use super::{free_disk_gib, is_out_of_space_error, native_continuation_matches};
  use charliebot_core::config::BackendOption;
  use std::os::unix::ffi::OsStrExt;

  #[test]
  fn out_of_space_errors_are_found_through_context_and_cause() {
    let enospc = std::io::Error::from_raw_os_error(libc::ENOSPC);
    let wrapped = anyhow::Error::new(enospc).context("launch evidence write failed");
    assert!(is_out_of_space_error(wrapped.as_ref()));

    let edquot = std::io::Error::from_raw_os_error(libc::EDQUOT);
    assert!(is_out_of_space_error(&edquot));
    assert!(!is_out_of_space_error(&std::io::Error::from_raw_os_error(libc::EACCES)));
  }

  #[test]
  fn free_disk_probes_the_nearest_existing_ancestor_and_uses_available_blocks() {
    let directory = tempfile::tempdir().expect("temporary directory exists");
    let missing = directory.path().join("not-created").join("worktree");
    let root_free = free_disk_gib(directory.path()).expect("the temporary directory filesystem is readable");
    assert_eq!(free_disk_gib(&missing).expect("the nearest existing ancestor is readable"), root_free);

    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let path = std::ffi::CString::new(directory.path().as_os_str().as_bytes()).expect("path has no NUL");
    // The temporary directory exists and the NUL-terminated path is valid for statvfs.
    assert_eq!(unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) }, 0);
    let stat = unsafe { stat.assume_init() };
    let expected = stat.f_bavail as f64 * stat.f_frsize as f64 / (1024.0_f64.powi(3));
    assert_eq!(root_free, expected);
  }

  #[test]
  fn manager_native_continuation_requires_anchor_snapshot_backend_and_model_match() {
    let option = BackendOption {
      id: "synthetic-backend".to_string(),
      label: "Synthetic backend".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    };
    assert!(native_continuation_matches(
      Some("native-session"),
      Some("snapshot-hash"),
      Some("synthetic-backend"),
      Some("synthetic-model"),
      "snapshot-hash",
      &option,
    ));
    assert!(!native_continuation_matches(
      None,
      Some("snapshot-hash"),
      Some("synthetic-backend"),
      Some("synthetic-model"),
      "snapshot-hash",
      &option,
    ));
    assert!(!native_continuation_matches(
      Some("native-session"),
      Some("old-hash"),
      Some("synthetic-backend"),
      Some("synthetic-model"),
      "snapshot-hash",
      &option,
    ));
  }
}
