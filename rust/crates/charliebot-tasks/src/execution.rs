//! The manager-run adapter that connects durable task inputs to the existing master queue.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::CString;
use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use charliebot_agents::backend_pool;
use charliebot_agents::backend_types;
use charliebot_agents::launch_loop;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_core::fsutil;
use charliebot_core::pyfmt;
use charliebot_core::run_token::{RunTokenClaims, sign_run_token};
use charliebot_registry::backend::{Env, LaunchArgs, LaunchRefused};
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::{SequenceCompletion, SequenceExecutor, SequenceLaunchSettlement};
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::control_events::{self, ACTOR_SYSTEM};
use charliebot_store::metadata::{
  ColdReadSpec, SessionMetadata, TaskProfile, TaskType, ThreadMetadata, ThreadStatus, UtcDateTime,
};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::{ChatEventHistory, NodeFacts, RunFacts, StoreError, StoreResult};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::cold_read::{self, ColdReadOutcome};
use crate::dispatch::{DispatchExecutor, Dispatcher, RunFinishedCompletion};
use crate::git;
use crate::master_queue::{MasterQueue, PersistAndBroadcastFn, SessionCallbacks, TaskTreeActivityFn};
use crate::master_run;
use crate::master_state::{MasterTurnRequest, TaskFinishCallback, TaskRunBinding, TaskSpawnCallback};
use crate::prompts::{self, PromptSnapshot, TaskPromptError};
use crate::recovery::{self, RecoveryState};
use crate::review;
use crate::review_text;
use crate::run_support::visit_events_from_end;
use crate::session_anchors;
use crate::spawner_backends;
use crate::task_input_prompt::compose_input_prompt;
use crate::worker::{self, Worker, WorkerSpec};

/// The launch result visible to sequence callers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSettlement {
  /// The durable terminal outcome when the run executed.
  pub outcome: Option<String>,
  /// The reason a launch was withheld before a process started.
  pub withheld: Option<String>,
}

#[derive(Debug)]
struct SequenceRunWithoutControllerError {
  message: String,
}

impl std::fmt::Display for SequenceRunWithoutControllerError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl Error for SequenceRunWithoutControllerError {}

type LaunchKey = (String, String);

#[derive(Default)]
pub(crate) struct LaunchState {
  inflight: HashSet<LaunchKey>,
  settlements: HashMap<LaunchKey, watch::Sender<Option<LaunchSettlement>>>,
  /// The task of each scheduled launch until it ends; the let-go step cancels the ones left.
  tasks: HashMap<LaunchKey, JoinHandle<()>>,
  /// Set when the let-go step begins: a launch scheduled after it starts no task.
  let_go: bool,
}

/// The manager execution adapter for durable task runs.
#[derive(Clone)]
pub struct TaskExecutionAdapter {
  pub(crate) features: Arc<FeatureContext>,
  pub(crate) queue: Arc<MasterQueue>,
  pub(crate) dispatch: Arc<Dispatcher>,
  pub(crate) launches: Arc<Mutex<LaunchState>>,
  pub(crate) recovery: Arc<RecoveryState>,
  quota_failures: Arc<Mutex<HashMap<LaunchKey, Option<UtcDateTime>>>>,
}

struct RunContinuation {
  previous: RunRecord,
  native_id: Option<String>,
}

impl TaskExecutionAdapter {
  /// Build the adapter that dispatches manager turns on `queue`.
  pub fn new(features: Arc<FeatureContext>, queue: Arc<MasterQueue>, dispatch: Arc<Dispatcher>) -> Self {
    Self {
      features,
      queue,
      dispatch,
      launches: Arc::new(Mutex::new(LaunchState::default())),
      recovery: Arc::new(RecoveryState::default()),
      quota_failures: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Reserve one fresh input batch or schedule one queued run.
  pub async fn call(
    &self,
    session_id: &str,
    pending_inputs: &[ChatEvent],
    launch_run_id: Option<&str>,
  ) -> StoreResult<Option<String>> {
    let mut scheduled = None;
    let run_id;
    {
      let guard = self.features.store.control_lock().await;
      let meta = self.features.store.load_task_meta(session_id).await?;
      let state = self.features.store.node_state(session_id)?;
      if state.task_state()? != "open" {
        return Ok(None);
      }
      if let Some(launch_run_id) = launch_run_id {
        let run = self
          .features
          .store
          .read_run_sync(session_id, launch_run_id)?
          .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(launch_run_id, session_id)))?;
        if state.runs.terminal_outcome(&run.id)?.is_some()
          || run.pid.is_some()
          || state.runs.stop_requested(&run.id, None)
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
        let mut input_ids = pending_now
          .iter()
          .map(|event| event.get("id").and_then(CompactValue::as_str).map_or("None".to_string(), str::to_string))
          .collect::<Vec<_>>();
        input_ids.sort();
        let request_id = format!("dispatch:{}", control_events::sha256_hex(&input_ids.join("\0")));
        run_id = control_events::stable_run_id(session_id, &request_id);
        if self.features.store.read_run_sync(session_id, &run_id)?.is_none() {
          let run = if meta.profile == TaskProfile::Manager {
            manager_run_record(session_id, &run_id, &backend, model.as_deref())
          } else {
            let mut run = worker_run_record(session_id, &run_id, &backend, model.as_deref());
            self.inherit_worker_continuation(&meta, &mut run)?;
            run
          };
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
    self.execute_run_with_prompt(session_id, run_id, None).await
  }

  async fn execute_run_with_prompt(
    &self,
    session_id: &str,
    run_id: &str,
    launch_prompt: Option<&str>,
  ) -> anyhow::Result<LaunchSettlement> {
    let cfg = self.features.config.get().map_err(|error| StoreError::new(error.to_string()))?;
    let meta;
    let run;
    let withheld;
    {
      let _guard = self.features.store.control_lock().await;
      meta = self.features.store.load_task_meta(session_id).await?;
      run = self
        .features
        .store
        .read_run_sync(session_id, run_id)?
        .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
      let manager_run = meta.profile == TaskProfile::Manager && run.kind == RunKind::ManagerTurn;
      let worker_run = meta.profile == TaskProfile::Worker && is_worker_kind(run.kind);
      if !manager_run && !worker_run {
        return Err(
          StoreError::invalid(format!(
            "run {run_id} (profile={}, kind={}) has no executable adapter",
            profile_name(meta.profile),
            kind_name(run.kind)
          ))
          .into(),
        );
      }
      let facts = self.features.store.node_state(session_id)?;
      let state = facts.task_state()?;
      if state != "open" {
        let reason = format!("task {session_id} is {state}");
        tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "run_launch_withheld");
        withheld = Some((reason, true));
      } else {
        self.features.store.require_open_ancestry(session_id)?;
        if facts.runs.terminal_outcome(run_id)?.is_some() {
          tracing::info!(target: "charliebot_tasks", session_id, run_id, "run_launch_refused_by_facts");
          return Ok(LaunchSettlement {
            outcome: Some(outcome_text(facts.runs.terminal_outcome(run_id)?.expect("the terminal fact was checked"))),
            withheld: None,
          });
        }
        if facts.runs.stop_requested(run_id, None) {
          let reason = format!("run {run_id} has a durable stop request");
          tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "run_launch_withheld");
          withheld = Some((reason, false));
        } else if worker_run {
          if let Some(reason) = disk_headroom_withhold_reason(cfg.as_ref())? {
            tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "run_launch_withheld");
            withheld = Some((reason, false));
          } else {
            withheld = None;
          }
        } else {
          withheld = None;
        }
      }
    }
    if let Some((reason, settle_closed)) = withheld {
      self.record_launch_withheld(&meta, &run, &reason).await?;
      if settle_closed {
        self.features.store.request_stop(session_id, run_id, &recovery::closed_task_stop_request_id(run_id)).await?;
      }
      return Ok(LaunchSettlement { outcome: None, withheld: Some(format!("withheld: {reason}")) });
    }

    let option = match self.resolve_run_backend(&cfg, &run) {
      Ok(option) => option,
      Err(error) => {
        self.land_launch_failure(&meta, &run, &error, "ValueError").await;
        return Err(error);
      }
    };
    let snapshot = match self.prepare_launch_snapshot(Arc::clone(&cfg), &meta, &run, &option).await {
      Ok(snapshot) => snapshot,
      Err(error) if error.downcast_ref::<TaskPromptError>().is_some() => {
        tracing::error!(target: "charliebot_tasks", session_id, run_id, kind = kind_name(run.kind), error = %error, "task_prompt_preparation_failed");
        let reason = format!("prompt preparation failed: {error}");
        self.record_launch_withheld(&meta, &run, &reason).await?;
        return Ok(LaunchSettlement { outcome: None, withheld: Some(format!("withheld: {reason}")) });
      }
      Err(error) => {
        self.land_launch_failure(&meta, &run, &error, "RuntimeError").await;
        return Err(error);
      }
    };
    if meta.profile == TaskProfile::Manager {
      if let Err(error) = self.execute_manager_turn(&cfg, &meta, &run, &option, &snapshot).await {
        if error.downcast_ref::<master_run::TurnLetGo>().is_some()
          || error.downcast_ref::<master_run::TurnCancelled>().is_some()
        {
          return Err(error);
        }
        self.land_launch_failure(&meta, &run, &error, "RuntimeError").await;
        return Err(error);
      }
    } else {
      if let Err(error) = self.execute_worker_run(&cfg, &meta, &run, &option, &snapshot, launch_prompt).await {
        if error.downcast_ref::<master_run::TurnLetGo>().is_some()
          || error.downcast_ref::<master_run::TurnCancelled>().is_some()
        {
          return Err(error);
        }
        self.land_launch_failure(&meta, &run, &error, launch_error_class(&error, "RuntimeError")).await;
        return Err(error);
      }
    }
    let outcome = self.await_terminal(session_id, run_id).await?;
    Ok(LaunchSettlement { outcome: Some(outcome), withheld: None })
  }

  fn schedule_launch(&self, session_id: String, run_id: String, prompt: Option<String>) {
    let key = (session_id.clone(), run_id.clone());
    let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
    if state.let_go {
      // A task started now would outlive the let-go and run into the runtime's teardown. The run stays queued, and the
      // next boot's dispatch launches it.
      state.inflight.remove(&key);
      tracing::info!(target: "charliebot_tasks", session_id, run_id, "run_launch_after_let_go");
      return;
    }
    let adapter = self.clone();
    // The task's own removal of its entry takes this lock, so it cannot run before the insert below.
    let task = tokio::spawn(async move {
      let result = adapter.execute_run_with_prompt(&session_id, &run_id, prompt.as_deref()).await;
      let settlement = match result {
        Ok(settlement) => settlement,
        Err(error) => {
          let shutdown_cancelled = error.downcast_ref::<master_run::TurnLetGo>().is_some()
            || error.downcast_ref::<master_run::TurnCancelled>().is_some();
          if !shutdown_cancelled {
            tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %error, "task_run_execution_failed");
          }
          if is_out_of_space_error(error.as_ref()) {
            adapter.note_launch_out_of_space(&session_id, &run_id);
          }
          match adapter.terminal_outcome(&session_id, &run_id) {
            Ok(Some(outcome)) => LaunchSettlement { outcome: Some(outcome), withheld: None },
            Ok(None) => LaunchSettlement {
              outcome: None,
              withheld: Some(if shutdown_cancelled {
                "manager turn cancelled at server shutdown".to_string()
              } else {
                format!("failed-to-start: {error}")
              }),
            },
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
      state.tasks.remove(&key);
      if let Some(sender) = state.settlements.remove(&key) {
        let _ = sender.send(Some(settlement));
      }
    });
    state.tasks.insert(key, task);
  }

  /// Cancel every launch task at the server's let-go, as `asyncio.run` cancels the tasks left at exit
  /// (`asyncio/runners.py:66-75`), and wait for each to end. The cancel passes every `except Exception` of the launch
  /// (`task_execution.py:719`, `:861`, `:1488`), so it lands no terminal fact, and a started agent keeps running
  /// (`agent_process/base.py:994-1012`). The next boot then launches a run that has no process and re-attaches one that
  /// has (`task_execution._reconcile_node`). A launch that finished before the cancel keeps its own result. The first
  /// panicked launch task is returned after every task has ended.
  pub(crate) async fn let_go_launches(&self) -> anyhow::Result<()> {
    let tasks = {
      let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
      state.let_go = true;
      std::mem::take(&mut state.tasks)
    };
    for task in tasks.values() {
      task.abort();
    }
    let mut failure = None;
    for (key, task) in tasks {
      match task.await {
        Ok(()) => {}
        Err(error) if error.is_cancelled() => {
          tracing::info!(target: "charliebot_tasks", session_id = %key.0, run_id = %key.1, "run_launch_let_go");
          self.launches.lock().expect("task execution launch state is not poisoned").inflight.remove(&key);
        }
        Err(error) => {
          failure.get_or_insert(anyhow::Error::new(error).context(format!("joining the launch task of run {}", key.1)));
        }
      }
    }
    failure.map_or(Ok(()), Err)
  }

  pub(crate) fn resolve_run_backend(&self, cfg: &CharlieBotConfig, run: &RunRecord) -> anyhow::Result<BackendOption> {
    let backend = run
      .backend
      .as_deref()
      .filter(|backend| !backend.is_empty())
      .ok_or_else(|| anyhow!("run {} records no backend; refusing to substitute one", run.id))?;
    spawner_backends::resolve_backend_option(cfg, backend, run.model.as_deref())
  }

  async fn prepare_launch_snapshot(
    &self,
    cfg: Arc<CharlieBotConfig>,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
  ) -> anyhow::Result<PromptSnapshot> {
    let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
    let coherent = prompts::assemble_coherent_snapshot_for_launch(
      &self.features.store,
      cfg,
      Arc::clone(&self.features.registry),
      checkout,
      meta,
      kind_name(run.kind),
      option,
    )
    .await?;
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
    let history = self.features.store.load_all_facts_view_sync(session_id)?;
    let events = history
      .iter()
      .filter(|event| event.get("id").and_then(CompactValue::as_str).is_some_and(|id| batch_ids.contains(id)))
      .cloned()
      .collect::<Vec<_>>();
    drop(history);
    let provenances = self.dispatch.child_report_provenances(&events).await?;
    let (mut content, uploaded_files) = compose_input_prompt(&self.features, meta, &events, &provenances)?;
    if content.is_empty() {
      anyhow::bail!("run {run_id} claimed no consumable input; nothing to execute");
    }
    if let Some(prefix) = apply_sequence_wake_duties(&self.features, meta, &events).await? {
      content = format!("{prefix}{content}");
    }
    let wake_meta = self
      .features
      .store
      .get_session(session_id)
      .await?
      .ok_or_else(|| anyhow!("session {session_id} disappeared after sequence wake duties"))?;
    let fresh_native = !native_continuation_matches(
      wake_meta.cc_session_id.as_deref(),
      wake_meta.native_prompt_hash.as_deref(),
      wake_meta.native_backend.as_deref(),
      wake_meta.native_model.as_deref(),
      &snapshot.prompt_hash(),
      option,
    );
    let mut prompt = content;
    if fresh_native && wake_meta.cc_session_id.is_some() {
      let reason = match wake_meta.native_backend.as_deref().filter(|backend| !backend.is_empty() && *backend != option.id) {
        Some(previous) => session_anchors::backend_switch_reset_reason(previous, &option.id),
        None => "this task's managed instructions or sources changed since the previous turn, so this turn starts a fresh native conversation".to_string(),
      };
      let goal = wake_meta.task.as_ref().map_or(wake_meta.name.as_str(), |task| task.goal.as_str());
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

    let queue = Arc::clone(&self.queue);
    let callbacks = self.turn_callbacks();
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
    let on_task_finish = self.manager_finish_recorder(
      meta,
      run_id,
      option,
      run_dir.join(run_records::RAW_LOG_NAME),
      None,
      Arc::clone(&callbacks.persist_and_broadcast),
    );
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
          .any(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::SCHEDULED_TRIGGER)),
        backend_option: Some(option.clone()),
        extra_claude_flags: None,
        uploaded_files: (!uploaded_files.is_empty()).then_some(uploaded_files),
        extra_env: Some(env),
      })
      .await?;
    Ok(())
  }

  async fn execute_worker_run(
    &self,
    cfg: &CharlieBotConfig,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
    snapshot: &PromptSnapshot,
    launch_prompt: Option<&str>,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    let run_id = run.id.as_str();
    let task_type = prompts::prompt_task_type(meta.task.as_ref());
    let history = self.features.store.load_all_facts_view_sync(session_id)?;
    let facts = self.features.store.node_state(session_id)?;
    let continuation = self.run_continuation(meta, run, option, snapshot, &facts, &facts.runs)?;
    let continued = continuation.as_ref().is_some_and(|continuation| continuation.native_id.is_some());
    let context = if run.kind == RunKind::Review {
      let work_run = self
        .features
        .store
        .read_run_sync(session_id, run.review_of_run_id.as_deref().unwrap_or_default())?
        .ok_or_else(|| anyhow!("review run {run_id} names no recorded work Run"))?;
      if continued {
        self.batch_only_context(meta, run, &history).await?
      } else {
        self.build_review_context(session_id, &work_run, &history).await?
      }
    } else if let Some(sequence_ref) = &run.sequence_ref {
      let step = format!("the launch context of run {run_id}");
      let Some(controller) = recovery::owning_controller(&self.features, &sequence_ref.kind, &step)? else {
        // Python raises TaskInvalidError here (`task_execution.py:1390-1394`), before any process starts.
        return Err(
          SequenceRunWithoutControllerError {
            message: format!(
              "sequence run {run_id} carries owner_ref {} and no registered controller owns its kind",
              pyfmt::str_repr(&sequence_ref.owner_ref)
            ),
          }
          .into(),
        );
      };
      let launch_text = launch_prompt.unwrap_or_default();
      match controller.launch_context(meta, run, launch_text).await? {
        Some(context) => context,
        None => self.build_step_context(meta, run, launch_text, task_type)?,
      }
    } else if let Some(step_prompt) = launch_prompt {
      self.build_step_context(meta, run, step_prompt, task_type)?
    } else if continued {
      self.batch_only_context(meta, run, &history).await?
    } else {
      self.build_work_context(cfg, meta, run, task_type, &history).await?
    };
    let mut prompt = context;
    if let Some(continuation) = continuation.as_ref().filter(|continuation| continuation.native_id.is_none()) {
      let previous = &continuation.previous;
      let reason = if previous.backend.as_deref().is_some_and(|backend| backend != option.id) {
        session_anchors::backend_switch_reset_reason(previous.backend.as_deref().unwrap(), &option.id)
      } else if previous.native_session_id.is_none() {
        "the previous run recorded no native conversation id, so this run starts a fresh native conversation"
          .to_string()
      } else {
        "this task's managed instructions or sources changed since the previous run, so this run starts a fresh native conversation".to_string()
      };
      let previous_log = self.features.store.run_dir(session_id, &previous.id).join(run_records::RUN_EVENTS_NAME);
      prompt = format!("{}\n\n{prompt}", worker_continuation_reset_note(&reason, &previous_log));
    }
    drop(history);
    drop(facts);
    self.persist_launch_text(session_id, run_id, &prompt)?;

    let mut binding = worker_binding(run_id, session_id, run);
    let registration = self
      .features
      .registry
      .backend_type_of(&option.backend_type)
      .ok_or_else(|| anyhow!("unknown backend type {}", option.backend_type))?;
    let lifecycle = self.features.registry.lifecycle_for(&option.backend_type)?;
    let held_native_id = continuation.as_ref().and_then(|continuation| continuation.native_id.clone());
    if registration.traits.preassigned_session_id {
      let preassigned = held_native_id.clone().or_else(|| Some(Uuid::new_v4().to_string()));
      lifecycle.assign_thread_native_id(&mut binding, preassigned.as_deref());
    }

    let run_dir = self.features.store.run_dir(session_id, run_id);
    let fresh_run = self
      .features
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| anyhow!("run {run_id} lost its record while its launch context was built"))?;
    let worktree = if run.kind == RunKind::Review {
      self
        .features
        .store
        .read_run_sync(session_id, fresh_run.review_of_run_id.as_deref().unwrap_or_default())?
        .and_then(|work_run| work_run.worktree_path)
    } else {
      fresh_run.worktree_path.clone()
    };
    let working_dir = worktree.map_or_else(|| run_dir.clone(), PathBuf::from);
    let features = Arc::clone(&self.features);
    let spawned_session = session_id.to_string();
    let spawned_run = run_id.to_string();
    let on_spawned = Arc::new(move |thread: ThreadMetadata| {
      let features = Arc::clone(&features);
      let session_id = spawned_session.clone();
      let run_id = spawned_run.clone();
      Box::pin(async move {
        let pid_start =
          thread.pid_start.as_deref().ok_or_else(|| anyhow!("run {run_id} spawned without a pinned pid_start"))?;
        let pid = thread.pid.ok_or_else(|| anyhow!("run {run_id} spawned without a pid"))?;
        features.store.record_launch(&session_id, &run_id, pid, pid_start).await?;
        Ok(())
      }) as BoxFuture<'static, anyhow::Result<()>>
    });
    let mut worker = Worker::new(
      Arc::clone(&self.features),
      WorkerSpec {
        thread: binding,
        working_dir,
        events_log: run_dir.join(run_records::RUN_EVENTS_NAME),
        task_description: prompt,
        cfg: Arc::new(cfg.clone()),
        backend_option: Some(option.clone()),
        extra_env: self.child_env(session_id, run_id, meta.name.as_str())?,
        on_spawned: Some(on_spawned),
        instructions_content: Some(snapshot.instructions_text()),
        session_meta: Some(meta.clone()),
        held_native_id,
        run_kind: run.kind,
      },
    )?;
    let store = Arc::clone(&self.features.store);
    let session_id_owned = session_id.to_string();
    worker.set_on_session_event(Arc::new(move |event| {
      let store = Arc::clone(&store);
      let session_id = session_id_owned.clone();
      Box::pin(async move {
        store.deliver_to_successor(&session_id, event).await?;
        Ok(())
      })
    }));
    let process_result = worker.run().await;

    let (exit_code, error_text) = match process_result {
      Ok(exit_code) => (exit_code, String::new()),
      Err(error) => {
        worker.terminate().await?;
        if let Some(refusal) = error.downcast_ref::<LaunchRefused>() {
          tracing::warn!(target: "charliebot_tasks", session_id, run_id, error = %refusal.message, quota_exhausted = refusal.quota_exhausted, "task_run_launch_refused");
          if refusal.quota_exhausted {
            self
              .quota_failures
              .lock()
              .expect("worker quota failure map is not poisoned")
              .insert((session_id.to_string(), run_id.to_string()), refusal.resets_at);
          }
          self.record_launch_error_event(session_id, run_id, &refusal.message, Some(refusal.quota_exhausted)).await?;
        } else if let Some(quota) = error.downcast_ref::<worker::QuotaExhaustedError>() {
          tracing::warn!(target: "charliebot_tasks", session_id, run_id, error = %quota, "task_run_quota_exhausted");
          let resets_at = quota
            .resets_at
            .map(|value| value.format(&time::format_description::well_known::Rfc3339))
            .transpose()?
            .map(|value| UtcDateTime::parse(&value))
            .transpose()?;
          self
            .quota_failures
            .lock()
            .expect("worker quota failure map is not poisoned")
            .insert((session_id.to_string(), run_id.to_string()), resets_at);
        } else {
          tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %error, "task_run_failed");
        }
        (-1, error.to_string())
      }
    };
    let durable = self.finalize_worker_run(meta, run, option, exit_code, &error_text, None).await?;
    let fresh = self
      .features
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
    self.after_worker_run(meta, &fresh, &durable).await
  }

  fn inherit_worker_continuation(&self, meta: &SessionMetadata, run: &mut RunRecord) -> StoreResult<()> {
    let Some(latest) = self.features.store.list_run_records_sync(&meta.id)?.last().cloned() else {
      return Ok(());
    };
    let facts = self.features.store.node_state(&meta.id)?;
    if !facts.task()?.close_events.is_empty() {
      return Ok(());
    }
    run.repo_path = latest.repo_path.clone();
    run.base_branch = latest.base_branch.clone();
    run.branch_name = latest.branch_name.clone();
    run.worktree_path = latest.worktree_path.clone();
    if latest.kind == RunKind::Review
      && facts.runs.terminal_outcome(&latest.id)? == Some(RunOutcomeKind::Success)
      && !self.blocked_report_sent(meta)?
    {
      run.kind = RunKind::Review;
      run.review_of_run_id = latest.review_of_run_id;
      run.backend = latest.backend;
      run.model = latest.model;
    }
    Ok(())
  }

  fn blocked_report_sent(&self, meta: &SessionMetadata) -> StoreResult<bool> {
    let Some(parent_id) = meta.task_parent_id.as_deref() else { return Ok(false) };
    Ok(self.features.store.load_all_facts_view_sync(parent_id)?.iter().any(|event| {
      event.get("type").and_then(CompactValue::as_str) == Some(event_types::CHILD_REPORT)
        && event.get("child_session_id").and_then(CompactValue::as_str) == Some(meta.id.as_str())
        && event.get("outcome").and_then(CompactValue::as_str) == Some("blocked")
    }))
  }

  fn run_continuation(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
    snapshot: &PromptSnapshot,
    facts: &NodeFacts,
    runs: &RunFacts,
  ) -> anyhow::Result<Option<RunContinuation>> {
    if run.sequence_ref.is_some() || run.retry_of_run_id.is_some() {
      return Ok(None);
    }
    if meta.task.as_ref().is_some_and(|task| task.task_type == Some(TaskType::ColdRead)) {
      return Ok(None);
    }
    let mut previous = None;
    for record in self.features.store.list_run_records_sync(&meta.id)? {
      if record.id != run.id {
        previous = Some(record);
      }
    }
    let Some(previous) = previous else {
      return Ok(None);
    };
    if run.kind == RunKind::Review {
      if previous.kind != RunKind::Review
        || previous.review_of_run_id != run.review_of_run_id
        || runs.terminal_outcome(&previous.id)? != Some(RunOutcomeKind::Success)
      {
        return Ok(None);
      }
    } else if run.kind == RunKind::Work {
      if !facts.task()?.close_events.is_empty() {
        return Ok(None);
      }
    } else {
      return Ok(None);
    }
    let prior_hash = previous
      .prompt_snapshot_ref
      .as_deref()
      .map(|path| -> anyhow::Result<String> {
        let text = fs::read_to_string(path).with_context(|| format!("reading prompt snapshot {path}"))?;
        let snapshot: Value = serde_json::from_str(&text).with_context(|| format!("parsing prompt snapshot {path}"))?;
        snapshot
          .get("prompt_hash")
          .and_then(Value::as_str)
          .map(str::to_string)
          .ok_or_else(|| anyhow!("prompt snapshot {path} has no prompt_hash"))
      })
      .transpose()?;
    let native_id = if native_continuation_matches(
      previous.native_session_id.as_deref(),
      prior_hash.as_deref(),
      previous.backend.as_deref(),
      previous.model.as_deref(),
      &snapshot.prompt_hash(),
      option,
    ) {
      previous.native_session_id.clone()
    } else {
      None
    };
    Ok(Some(RunContinuation { previous, native_id }))
  }

  async fn batch_only_context(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    let ids = run.input_event_ids.iter().cloned().collect::<HashSet<_>>();
    let batch = history
      .iter()
      .filter(|event| event.get("id").and_then(CompactValue::as_str).is_some_and(|id| ids.contains(id)))
      .cloned()
      .collect::<Vec<_>>();
    let provenance = if batch.is_empty() { Map::new() } else { self.dispatch.child_report_provenances(&batch).await? };
    let (content, _) = compose_input_prompt(&self.features, meta, &batch, &provenance)?;
    if content.trim().is_empty() {
      anyhow::bail!("run {} continues a conversation but claims no consumable input", run.id);
    }
    Ok(content)
  }

  async fn later_messages_section(
    &self,
    meta: &SessionMetadata,
    work_run: &RunRecord,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    let mut claimed_ids = Vec::new();
    let mut found_work_run = false;
    for run in self.features.store.list_run_records_sync(&meta.id)? {
      if run.kind != RunKind::Work {
        continue;
      }
      claimed_ids.extend(run.input_event_ids);
      if run.id == work_run.id {
        found_work_run = true;
        break;
      }
    }
    anyhow::ensure!(found_work_run, "review of run {}: it is not a work run of node {}", work_run.id, meta.id);
    if claimed_ids.is_empty() {
      return Ok(String::new());
    }
    let mut events_by_id = HashMap::new();
    for event in history.iter() {
      if let Some(id) = event.get("id").and_then(CompactValue::as_str) {
        events_by_id.insert(id, event);
      }
    }
    let events = claimed_ids
      .iter()
      .map(|event_id| {
        events_by_id
          .get(event_id.as_str())
          .map(|event| (*event).clone())
          .ok_or_else(|| anyhow!("review input event {event_id} is missing from node {} history", meta.id))
      })
      .collect::<anyhow::Result<Vec<_>>>()?;
    let provenance = self.dispatch.child_report_provenances(&events).await?;
    let (content, _) = compose_input_prompt(&self.features, meta, &events, &provenance)?;
    if content.trim().is_empty() {
      return Ok(String::new());
    }
    Ok(format!("**Later Messages:**\n{content}"))
  }

  async fn build_review_context(
    &self,
    session_id: &str,
    work_run: &RunRecord,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    if work_run.repo_path.as_deref().is_none_or(str::is_empty) {
      return self.build_repo_less_review_context(session_id, work_run, history).await;
    }
    anyhow::ensure!(
      work_run.branch_name.as_deref().is_some_and(|value| !value.is_empty())
        && work_run.worktree_path.as_deref().is_some_and(|value| !value.is_empty())
        && work_run.repo_path.as_deref().is_some_and(|value| !value.is_empty())
        && work_run.base_branch.as_deref().is_some_and(|value| !value.is_empty()),
      "review of work run {} needs its exact repo/base/branch/worktree provenance; an unset base is never silently replaced with main",
      work_run.id
    );
    let meta = self.features.store.load_meta(session_id).await?;
    anyhow::ensure!(
      meta.as_ref().is_some_and(|meta| meta.task.is_some()),
      "review of work run {} needs its node's task record for the spec",
      work_run.id
    );
    let meta = meta.expect("the task record was checked above");
    let spec = review_text::review_spec_text(meta.task.as_ref().expect("the task record was checked above"))?;
    let worker_log_path = self.features.store.run_dir(session_id, &work_run.id).join(run_records::RUN_EVENTS_NAME);
    let worker_summary = review::extract_review_context(session_id, &work_run.id, &worker_log_path).await;
    let context_lines = review_text::review_context_lines(
      &spec,
      &self.later_messages_section(&meta, work_run, history).await?,
      worker_summary.as_deref(),
      &format!("(work run {})", work_run.id),
    );
    Ok(prompts::review_task_context(
      work_run.branch_name.as_deref().expect("the review provenance was checked above"),
      work_run.worktree_path.as_deref().expect("the review provenance was checked above"),
      work_run.base_branch.as_deref().expect("the review provenance was checked above"),
      &self.features.store.chat_events_path(session_id),
      &worker_log_path,
      &context_lines.join("\n"),
    ))
  }

  async fn build_repo_less_review_context(
    &self,
    session_id: &str,
    work_run: &RunRecord,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    let meta = self.features.store.load_meta(session_id).await?;
    anyhow::ensure!(
      meta.as_ref().is_some_and(|meta| meta.task.is_some()),
      "review of work run {} needs its node's task record for the spec",
      work_run.id
    );
    let meta = meta.expect("the task record was checked above");
    let spec = review_text::review_spec_text(meta.task.as_ref().expect("the task record was checked above"))?;
    let worker_log_path = self.features.store.run_dir(session_id, &work_run.id).join(run_records::RUN_EVENTS_NAME);
    let worker_summary = review::extract_review_context(session_id, &work_run.id, &worker_log_path).await;
    Ok(prompts::repo_less_review_task_context(
      &self.features.store.chat_events_path(session_id),
      &worker_log_path,
      &spec,
      &self.later_messages_section(&meta, work_run, history).await?,
      worker_summary.as_deref(),
    ))
  }

  async fn work_description(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    let spec_text =
      meta.task.as_ref().map(|task| task.goal.as_str()).filter(|goal| !goal.trim().is_empty()).unwrap_or_default();
    let ids = run.input_event_ids.iter().cloned().collect::<HashSet<_>>();
    let batch = history
      .iter()
      .filter(|event| event.get("id").and_then(CompactValue::as_str).is_some_and(|id| ids.contains(id)))
      .cloned()
      .collect::<Vec<_>>();
    let provenance = if batch.is_empty() { Map::new() } else { self.dispatch.child_report_provenances(&batch).await? };
    let (content, _) = compose_input_prompt(&self.features, meta, &batch, &provenance)?;
    let description =
      [spec_text, content.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
    if description.trim().is_empty() {
      anyhow::bail!("run {} has no task spec and no input; nothing to execute", run.id);
    }
    Ok(description)
  }

  async fn build_work_context(
    &self,
    cfg: &CharlieBotConfig,
    meta: &SessionMetadata,
    run: &RunRecord,
    task_type: TaskType,
    history: &ChatEventHistory,
  ) -> anyhow::Result<String> {
    if task_type == TaskType::ColdRead {
      return self.build_cold_read_context(meta, run);
    }
    let description = self.work_description(meta, run, history).await?;
    if task_type == TaskType::Verify {
      let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
      return prompts::render_task_body(&checkout, &description);
    }
    let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
    let mut parts = vec![prompts::render_session_info(&checkout, &meta.name)?];
    let task = meta.task.as_ref();
    let Some(task_repo) = task.and_then(|task| task.repo_path.as_deref()) else {
      parts.push(prompts::render_task_body(&checkout, &description)?);
      return Ok(parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"));
    };
    let repo_path = fs::canonicalize(task_repo).with_context(|| format!("resolving repository {task_repo}"))?;
    let mut base_branch = run.base_branch.clone().or_else(|| task.and_then(|task| task.base_branch.clone()));
    let mut branch_name = run.branch_name.clone();
    let mut worktree_path = run.worktree_path.clone();
    let mut start_point = None;
    if base_branch.is_none() || branch_name.is_none() || worktree_path.is_none() {
      let (resolved_base, resolved_branch, resolved_worktree, resolved_start) =
        self.prepare_worktree(cfg, meta, run, &repo_path).await?;
      base_branch = Some(resolved_base);
      branch_name = Some(resolved_branch);
      worktree_path = Some(resolved_worktree);
      start_point = Some(resolved_start);
    }
    let base_branch = base_branch.context("worktree preparation returned no base branch")?;
    let branch_name = branch_name.context("worktree preparation returned no branch name")?;
    let worktree_path = worktree_path.context("worktree preparation returned no worktree path")?;
    let origin = format!("`{base_branch}`") + &start_point.map_or_else(String::new, |point| format!(" @ `{point}`"));
    parts.push(prompts::render_worktree_bindings(
      &checkout,
      task_type,
      &prompts::binding_intro(&checkout, run)?,
      &branch_name,
      &origin,
      &worktree_path,
      &repo_path.to_string_lossy(),
    )?);
    parts.push(prompts::render_task_body(&checkout, &description)?);
    if task.is_some_and(|task| task.keep_worktree) {
      parts.push(prompts::render_worktree_persistence(&checkout)?);
    }
    Ok(parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"))
  }

  /// The cold-read run's whole input: the page text, one blank line, then the questions. The launch records the page's
  /// sha256 on the run's evidence file first, so the report states what this run read (`_build_cold_read_context`).
  fn build_cold_read_context(&self, meta: &SessionMetadata, run: &RunRecord) -> anyhow::Result<String> {
    let cold = cold_spec(meta, &run.id)?;
    let page_bytes = fs::read(&cold.page_path).with_context(|| format!("reading cold-read page {}", cold.page_path))?;
    let evidence = json!({ "page_path": cold.page_path, "page_sha256": cold_read::page_sha256(&page_bytes) });
    let evidence_path = self.features.store.run_dir(&meta.id, &run.id).join(cold_read::EVIDENCE_FILENAME);
    fsutil::atomic_write_text(&evidence_path, &pyfmt::json_dumps(&evidence, &pyfmt::PYTHON_JSON_DEFAULT))?;
    let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
    let (_system_prompt, questions) = cold_read::load_texts(checkout.path())?;
    let page_text =
      String::from_utf8(page_bytes).with_context(|| format!("decoding cold-read page {}", cold.page_path))?;
    Ok(cold_read::probe_input(&page_text, &questions, &cold.trigger))
  }

  fn build_step_context(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    step_prompt: &str,
    task_type: TaskType,
  ) -> anyhow::Result<String> {
    let checkout = self.features.startup.require::<charliebot_core::RepoRoot>()?;
    let mut parts = vec![prompts::render_session_info(&checkout, &meta.name)?];
    if let (Some(repo_path), Some(base_branch), Some(branch_name), Some(worktree_path)) =
      (&run.repo_path, &run.base_branch, &run.branch_name, &run.worktree_path)
    {
      parts.push(prompts::render_worktree_bindings(
        &checkout,
        task_type,
        &prompts::binding_intro(&checkout, run)?,
        branch_name,
        &format!("`{base_branch}`"),
        worktree_path,
        repo_path,
      )?);
    }
    parts.push(prompts::render_task_body(&checkout, step_prompt)?);
    Ok(parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"))
  }

  async fn prepare_worktree(
    &self,
    cfg: &CharlieBotConfig,
    meta: &SessionMetadata,
    run: &RunRecord,
    repo_path: &Path,
  ) -> anyhow::Result<(String, String, String, String)> {
    let (base_branch, remote_tip) =
      match run.base_branch.clone().or_else(|| meta.task.as_ref().and_then(|task| task.base_branch.clone())) {
        Some(base_branch) => (base_branch, None),
        None => {
          let (default_branch, remote_tip) = git::git_remote_default_branch_and_tip(repo_path).await?;
          (format!("origin/{default_branch}"), remote_tip)
        }
      };
    let branch_name = run.branch_name.clone().unwrap_or_else(|| {
      format!("charliebot/task-{}-{}", time::OffsetDateTime::now_utc().unix_timestamp(), &run.id[..8])
    });
    let worktree_root = PathBuf::from(&cfg.paths.worktree_dir);
    fs::create_dir_all(&worktree_root)
      .with_context(|| format!("creating worktree root {}", worktree_root.display()))?;
    let worktree_path = worktree_root.join(git::git_worktree_dir_name(&branch_name));
    let resolution =
      git::git_create_worktree(repo_path, &base_branch, &branch_name, &worktree_path, remote_tip.as_deref()).await?;
    let worktree_path =
      worktree_path.canonicalize().with_context(|| format!("resolving worktree {}", worktree_path.display()))?;
    self
      .features
      .store
      .record_observation(
        &meta.id,
        &run.id,
        Map::from_iter([
          ("repo_path".to_string(), json!(repo_path.to_string_lossy())),
          ("base_branch".to_string(), json!(base_branch)),
          ("branch_name".to_string(), json!(branch_name)),
          ("worktree_path".to_string(), json!(worktree_path.to_string_lossy())),
        ]),
      )
      .await?;
    Ok((base_branch, branch_name, worktree_path.to_string_lossy().into_owned(), resolution.start_point))
  }

  async fn finalize_worker_run(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
    exit_code: i32,
    error: &str,
    ended_at: Option<UtcDateTime>,
  ) -> anyhow::Result<String> {
    let session_id = meta.id.as_str();
    let run_id = run.id.as_str();
    let run_dir = self.features.store.run_dir(session_id, run_id);
    let raw_path = run_dir.join(run_records::RAW_LOG_NAME);
    let native_session_id = self.native_session_id(&raw_path, option).await?;
    let outcome = self.worker_outcome(&run_dir, option).await?;
    self
      .features
      .store
      .record_observation(
        session_id,
        run_id,
        Map::from_iter([
          ("native_session_id".to_string(), json!(native_session_id)),
          ("model".to_string(), json!(option.model)),
          ("raw_log_ref".to_string(), json!(raw_path.to_string_lossy())),
          ("events_ref".to_string(), json!(run_dir.join(run_records::RUN_EVENTS_NAME).to_string_lossy())),
          ("result_ref".to_string(), json!(raw_path.to_string_lossy())),
        ]),
      )
      .await?;
    let mut finish = RunOutcome::new(parse_outcome(&outcome));
    finish.exit_code = Some(if error.is_empty() { exit_code } else { -1 });
    finish.ended_at = ended_at;
    self.dispatch.finish_run(session_id, run_id, finish).await?;
    let durable = self.terminal_outcome(session_id, run_id)?.unwrap_or(outcome);
    if !error.is_empty() {
      tracing::warn!(target: "charliebot_tasks", session_id, run_id, error = %error.chars().take(500).collect::<String>(), "task_run_error");
    }
    Ok(durable)
  }

  async fn worker_outcome(&self, run_dir: &Path, option: &BackendOption) -> anyhow::Result<String> {
    let raw_path = run_dir.join(run_records::RAW_LOG_NAME);
    if raw_path.is_file() {
      let translate = self.fresh_translate(option);
      let (_, result, _) = charliebot_agents::transport::recovery::scan_result_exit(&raw_path, translate)?;
      if result.as_ref().is_some_and(result_success) {
        return Ok("success".to_string());
      }
    }
    let events_path = run_dir.join(run_records::RUN_EVENTS_NAME);
    if events_path.is_file() {
      let events_path = events_path.clone();
      if tokio::task::spawn_blocking(move || events_log_result_success(&events_path))
        .await
        .context("joining run result reader")??
      {
        return Ok("success".to_string());
      }
    }
    Ok("failed".to_string())
  }

  async fn native_session_id(&self, raw_path: &Path, option: &BackendOption) -> anyhow::Result<Option<String>> {
    if !raw_path.is_file() {
      return Ok(None);
    }
    let mut translate = self.fresh_translate(option);
    let raw_path = raw_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
      let raw = fs::File::open(&raw_path).with_context(|| format!("reading raw run log {}", raw_path.display()))?;
      for line in std::io::BufReader::new(raw).split(b'\n') {
        let line = line.with_context(|| format!("reading raw run log {}", raw_path.display()))?;
        for event in charliebot_agents::transport::recovery::parse_raw_lines(&line) {
          if event.get("type").and_then(Value::as_str) == Some("system")
            && event.get("subtype").and_then(Value::as_str) == Some("init")
            && let Some(session_id) = event.get("session_id")
          {
            return Ok(Some(pyfmt::python_str(session_id)));
          }
          for translated in translate(event)? {
            if translated.get("type").and_then(Value::as_str) == Some(event_types::SESSION_ATTACHED)
              && let Some(session_id) = translated.get("session_id")
            {
              return Ok(Some(pyfmt::python_str(session_id)));
            }
          }
        }
      }
      Ok(None)
    })
    .await
    .context("joining native session id reader")?
  }

  fn fresh_translate(&self, option: &BackendOption) -> Box<dyn FnMut(Value) -> anyhow::Result<Vec<Value>> + Send> {
    let cfg = match self.features.config.get() {
      Ok(cfg) => cfg,
      Err(error) => {
        tracing::warn!(target: "charliebot_tasks", backend = %option.id, error = %error, "task_run_translate_unresolved");
        return Box::new(|event| Ok(vec![event]));
      }
    };
    match backend_types::build_backend(Arc::clone(&self.features.registry), option, &cfg, LaunchArgs::default()) {
      Ok(built) => {
        let mut translate = built.backend.translator();
        Box::new(move |event| translate.translate(event))
      }
      Err(error) => {
        tracing::warn!(target: "charliebot_tasks", backend = %option.id, error = %error, "task_run_translate_unresolved");
        Box::new(|event| Ok(vec![event]))
      }
    }
  }

  pub(crate) async fn after_worker_run(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    durable_outcome: &str,
  ) -> anyhow::Result<()> {
    let result = self.after_worker_run_inner(meta, run, durable_outcome).await;
    self.dispatch.dispatch_pending(&meta.id).await?;
    result
  }

  /// Test hooks over the worker's delivery chain (feature `test-hooks`): one run's chain for its durable outcome, and
  /// the quota failure a launch records before its chain runs.
  #[cfg(feature = "test-hooks")]
  pub async fn after_worker_run_for_test(&self, session_id: &str, run_id: &str, outcome: &str) -> anyhow::Result<()> {
    let meta = self.features.store.load_task_meta(session_id).await?;
    let run = self
      .features
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| anyhow!(run_records::run_not_found_in_task_text(run_id, session_id)))?;
    self.after_worker_run(&meta, &run, outcome).await
  }

  #[cfg(feature = "test-hooks")]
  pub fn record_quota_failure_for_test(&self, session_id: &str, run_id: &str, resets_at: Option<UtcDateTime>) {
    self
      .quota_failures
      .lock()
      .expect("worker quota failure map is not poisoned")
      .insert((session_id.to_string(), run_id.to_string()), resets_at);
  }

  async fn after_worker_run_inner(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    durable_outcome: &str,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    if let Some(sequence_ref) = &run.sequence_ref {
      let step = format!("the after-run duty for run {}", run.id);
      let Some(controller) = recovery::owning_controller(&self.features, &sequence_ref.kind, &step)? else {
        // A Run whose owning package is gone only logs its finish (`task_execution.py:2168-2176`).
        tracing::warn!(
          target: "charliebot_tasks",
          session_id,
          run_id = %run.id,
          owner_ref = %sequence_ref.owner_ref,
          "sequence_run_without_controller"
        );
        return Ok(());
      };
      let tree = recovery::sequence_tree(&self.features, self)?;
      controller.after_run(session_id, run, &tree).await?;
      return Ok(());
    }

    let quota_failure = self.take_quota_failure(session_id, &run.id);
    if let (Some(resets_at), Some(backend)) = (quota_failure, run.backend.as_deref())
      && self.pool_chose_backend(meta, run)
    {
      backend_pool::backend_pool(&self.features)?.cool(backend, resets_at);
    }
    let quota_failed = quota_failure.is_some();

    if run.kind == RunKind::Review {
      return self.after_review_run(meta, run, durable_outcome).await;
    }
    let task_type = meta.task.as_ref().and_then(|task| task.task_type);
    if task_type == Some(TaskType::ColdRead) && matches!(durable_outcome, "success" | "failed") {
      return self.after_cold_read_run(meta, run, durable_outcome).await;
    }
    if durable_outcome == "success" {
      // A node without a task record is an implement node; a record whose task type is unset takes the
      // non-implement branch (`src/runtime/task_execution.py:2190`).
      if meta.task.as_ref().is_none_or(|task| task.task_type == Some(TaskType::Implement)) {
        let completion = self.features.startup.require::<crate::completion::TaskCompletion>()?;
        if let Some(reason) = completion.pause_reason(session_id)? {
          tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, reason, "worker_delivery_paused");
          return Ok(());
        }
        if self.maybe_spawn_review(session_id, run).await?.is_none() {
          let cfg = self.features.config.get()?;
          let recovery = backend_pool::backend_pool(&self.features)?.recovery_report(cfg.as_ref());
          self
            .report_failure_to_parent(
              session_id,
              run,
              "blocked",
              Some(&format!(
                "review of work run {} found no available reviewer backend; backends.pool: {recovery}",
                run.id
              )),
            )
            .await?;
        }
        return Ok(());
      }
      self.cleanup_worktree_if_delivered(session_id, run).await?;
      return Ok(());
    }
    if matches!(durable_outcome, "failed" | "interrupted") {
      let mut summary = None;
      if quota_failed && durable_outcome == "failed" && self.fails_over(meta, run) {
        if self.fail_over_quota(meta, run).await? {
          return Ok(());
        }
        let recovery = backend_pool::backend_pool(&self.features)?;
        let cfg = self.features.config.get()?;
        summary = Some(format!(
          "{}\n\nNo backends.pool member is available: {}",
          self.worker_failure_summary(session_id, run).await?,
          recovery.recovery_report(cfg.as_ref())
        ));
      }
      self.report_failure_to_parent(session_id, run, durable_outcome, summary.as_deref()).await?;
    }
    Ok(())
  }

  /// The cold-read delivery chain: report the answer, retry the next backend, or report that every model failed
  /// (`_after_cold_read_run`). An empty answer is a failed attempt. A retry takes a new work run in the same child.
  pub(crate) async fn after_cold_read_run(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    durable_outcome: &str,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    let store = &self.features.store;
    if durable_outcome == "success" {
      let events_log = store.run_dir(session_id, &run.id).join(run_records::RUN_EVENTS_NAME);
      if let Some(answer) = read_worker_summary_file(&events_log).await? {
        return self.complete_cold_read(meta, run, &answer).await;
      }
    }
    let error = if durable_outcome == "success" {
      "empty final answer".to_string()
    } else {
      self.cold_read_attempt_error(session_id, run).await?
    };
    self.record_cold_read_attempt(meta, run, &error)?;
    let work_runs = store
      .list_run_records_sync(session_id)?
      .into_iter()
      .filter(|record| record.kind == RunKind::Work)
      .collect::<Vec<_>>();
    let tried = work_runs.iter().filter_map(|record| record.backend.clone()).collect::<Vec<_>>();
    let cfg = self.features.config.get()?;
    let pool = backend_pool::backend_pool(&self.features)?;
    let parent_id = meta.task_parent_id.as_deref().unwrap_or_default();
    let selection = spawner_backends::select_verify_backend(&cfg, store, parent_id, "cold_read", &tried, &pool).await?;
    if let Some((backend, model, _tried)) = selection {
      let run_id = control_events::stable_run_id(session_id, &format!("cold-read:work:{}", work_runs.len()));
      {
        let guard = store.control_lock().await;
        let record = worker_run_record(session_id, &run_id, &backend, model.as_deref());
        let task_spec = canonical_task_spec_text(meta.task.as_ref())?;
        store.register_run_locked(&guard, session_id, record, task_spec.as_deref()).await?;
      }
      self.launch(session_id, &run_id)?;
      return Ok(());
    }
    let body = self.cold_read_report(meta, run, ColdReadOutcome::AllFailed)?;
    self.report_failure_to_parent(session_id, run, "failed", Some(&body)).await
  }

  /// The success report, then the automatic completion closes the child (`_complete_cold_read`).
  async fn complete_cold_read(&self, meta: &SessionMetadata, run: &RunRecord, answer: &str) -> anyhow::Result<()> {
    let backend_id = run.backend.as_deref().with_context(|| format!("cold-read run {} has no backend", run.id))?;
    let body = self.cold_read_report(meta, run, ColdReadOutcome::Answered { backend_id, answer })?;
    let completion = self.features.startup.require::<crate::completion::TaskCompletion>()?;
    match completion.evaluate_automatic_completion(&meta.id, &run.id, Some(&body), None, None, None).await {
      Ok(_) => Ok(()),
      Err(StoreError::Conflict(blockers)) => {
        tracing::warn!(
          target: "charliebot_tasks",
          session_id = %meta.id,
          run_id = %run.id,
          blockers = ?blockers,
          "cold_read_delivery_close_blocked"
        );
        Ok(())
      }
      Err(error) => Err(error.into()),
    }
  }

  /// The failed run's error for its attempt line: the error event, else its closing text, else a fallback.
  async fn cold_read_attempt_error(&self, session_id: &str, run: &RunRecord) -> anyhow::Result<String> {
    let events_log = self.features.store.run_dir(session_id, &run.id).join(run_records::RUN_EVENTS_NAME);
    if events_log.is_file() {
      if let Some(error) = read_worker_error_file(&events_log).await? {
        return Ok(error);
      }
      if let Some(text) = read_worker_summary_file(&events_log).await? {
        return Ok(text);
      }
    }
    Ok(format!("run {} failed without a reportable output", run.id))
  }

  /// Record one failed attempt on the run's evidence file, which the report reads back (`_record_cold_read_attempt`).
  fn record_cold_read_attempt(&self, meta: &SessionMetadata, run: &RunRecord, error: &str) -> anyhow::Result<()> {
    let cold = cold_spec(meta, &run.id)?;
    let evidence_path = self.features.store.run_dir(&meta.id, &run.id).join(cold_read::EVIDENCE_FILENAME);
    let mut evidence = if evidence_path.is_file() {
      serde_json::from_slice::<Value>(&fs::read(&evidence_path)?)?
    } else {
      json!({ "page_path": cold.page_path, "page_sha256": null })
    };
    evidence["error"] = json!(error);
    fsutil::atomic_write_text(&evidence_path, &pyfmt::json_dumps(&evidence, &pyfmt::PYTHON_JSON_DEFAULT))?;
    Ok(())
  }

  /// The report body for one settled cold read: the page and its sha256, every failed attempt in run order, then the
  /// answer or the all-failed trailer (`_cold_read_report`). The sha256 is the settling run's, read at its launch.
  fn cold_read_report(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    outcome: ColdReadOutcome<'_>,
  ) -> anyhow::Result<String> {
    let cold = cold_spec(meta, &run.id)?;
    let session_id = meta.id.as_str();
    let store = &self.features.store;
    let mut attempts = Vec::new();
    let mut page_sha256 = None;
    for record in store.list_run_records_sync(session_id)? {
      if record.kind != RunKind::Work {
        continue;
      }
      let evidence_path = store.run_dir(session_id, &record.id).join(cold_read::EVIDENCE_FILENAME);
      if !evidence_path.is_file() {
        continue;
      }
      let evidence = serde_json::from_slice::<Value>(&fs::read(&evidence_path)?)?;
      if record.id == run.id {
        page_sha256 = evidence["page_sha256"].as_str().map(str::to_string);
      }
      if let (Some(error), Some(backend)) = (evidence["error"].as_str(), record.backend.as_deref()) {
        attempts.push((backend.to_string(), error.to_string()));
      }
    }
    let page_sha256 = match page_sha256 {
      Some(digest) => digest,
      // The settling run never read the page: its launch failed on the read, so the page is stated as it stands now.
      None => cold_read::page_sha256(&fs::read(&cold.page_path)?),
    };
    Ok(cold_read::format_report(&cold.page_path, &page_sha256, &attempts, outcome))
  }

  pub(crate) async fn maybe_spawn_review(
    &self,
    session_id: &str,
    work_run: &RunRecord,
  ) -> anyhow::Result<Option<String>> {
    let store = &self.features.store;
    let guard = store.control_lock().await;
    if store.task_state(session_id)? != "open" {
      return Ok(None);
    }
    let events = store.load_all_facts(session_id)?;
    if run_records::terminal_outcome(&events, &work_run.id)? != Some(RunOutcomeKind::Success) {
      return Ok(None);
    }
    let mut work_run = work_run.clone();
    if let Some(fresh) = store.read_run_sync(session_id, &work_run.id)? {
      work_run = fresh;
    }
    let runs = store.list_run_records_sync(session_id)?;
    let existing = runs
      .iter()
      .filter(|run| run.kind == RunKind::Review && run.review_of_run_id.as_deref() == Some(work_run.id.as_str()))
      .cloned()
      .collect::<Vec<_>>();
    for review_run in &existing {
      if run_records::terminal_outcome(&events, &review_run.id)? == Some(RunOutcomeKind::Success) {
        return Ok(Some(review_run.id.clone()));
      }
    }
    for review_run in &existing {
      if run_records::terminal_outcome(&events, &review_run.id)?.is_none() {
        return Ok(existing.first().map(|run| run.id.clone()));
      }
    }
    let tried_backends = existing.iter().filter_map(|run| run.backend.clone()).collect::<Vec<_>>();
    let used_backends = runs.iter().filter_map(|run| run.backend.clone()).collect::<HashSet<_>>();
    let cfg = self.features.config.get()?;
    let pool = backend_pool::backend_pool(&self.features)?;
    let meta = store.load_meta(session_id).await?;
    let route = meta
      .as_ref()
      .and_then(|meta| meta.extra("route"))
      .filter(|value| !value.is_null())
      .map(|value| serde_json::from_value::<backend_pool::BackendRoute>(value.clone()))
      .transpose()?;
    let routed_selection = if let Some(route) = route.filter(|route| !route.backends.is_empty()) {
      let mut excluded = vec![work_run.backend.clone().unwrap_or_default()];
      excluded.extend(used_backends.iter().cloned());
      excluded.extend(tried_backends.iter().cloned());
      match pool.pick_ordered(&cfg, "reviewer", &route.backends, &excluded)? {
        Some(option) => {
          let option = spawner_backends::resolve_backend_option(&cfg, &option.id, Some(&option.model))?;
          Some((option.id, Some(option.model), tried_backends.clone()))
        }
        None => None,
      }
    } else {
      None
    };
    let selection = match routed_selection {
      Some(selection) => Some(selection),
      None => review::select_reviewer_backend(
        &cfg,
        &pool,
        "reviewer",
        work_run.backend.as_deref().unwrap_or_default(),
        work_run.model.as_deref(),
        &tried_backends,
        &used_backends,
      )?,
    };
    let Some((backend, model, _tried)) = selection else {
      tracing::warn!(target: "charliebot_tasks", session_id, work_run = %work_run.id, "reviewer_backends_exhausted");
      return Ok(None);
    };
    let attempt = existing.len() + 1;
    let run_id = control_events::stable_run_id(session_id, &format!("review:{}:{attempt}", work_run.id));
    let record = RunRecord {
      kind: RunKind::Review,
      review_of_run_id: Some(work_run.id.clone()),
      backend: Some(backend),
      model,
      repo_path: work_run.repo_path.clone(),
      base_branch: work_run.base_branch.clone(),
      branch_name: work_run.branch_name.clone(),
      worktree_path: work_run.worktree_path.clone(),
      ..worker_run_record(session_id, &run_id, "", None)
    };
    store.register_run_locked(&guard, session_id, record, Some(&format!("review of work run {}", work_run.id))).await?;
    drop(guard);
    self.launch(session_id, &run_id)?;
    Ok(Some(run_id))
  }

  pub(crate) async fn after_review_run(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    durable_outcome: &str,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    let work_run = self
      .features
      .store
      .read_run_sync(session_id, run.review_of_run_id.as_deref().unwrap_or_default())?
      .ok_or_else(|| anyhow!("review run {} names no recorded work Run", run.id))?;
    if durable_outcome == "failed" {
      let retried = self.maybe_spawn_review(session_id, &work_run).await?;
      if retried.is_none() {
        let cfg = self.features.config.get()?;
        let recovery = backend_pool::backend_pool(&self.features)?.recovery_report(cfg.as_ref());
        self
          .report_failure_to_parent_with_decider(
            session_id,
            &work_run,
            "blocked",
            Some(&format!(
              "review of work run {} failed on every configured reviewer backend; backends.pool: {recovery}",
              work_run.id
            )),
            Some(run),
          )
          .await?;
      } else if let Some(retried_id) = retried {
        let events = self.features.store.load_all_facts(session_id)?;
        if run_records::terminal_outcome(&events, &retried_id)? == Some(RunOutcomeKind::Success) {
          self.report_failure_to_parent(session_id, run, durable_outcome, None).await?;
        }
      }
      return Ok(());
    }
    if durable_outcome == "interrupted" {
      self.report_failure_to_parent(session_id, run, durable_outcome, None).await?;
      return Ok(());
    }
    if durable_outcome != "success" {
      return Ok(());
    }
    let task_state = self.features.store.task_state(session_id)?;
    if task_state != "open" {
      tracing::debug!(
        target: "charliebot_tasks",
        session_id,
        run_id = %run.id,
        work_run = %work_run.id,
        task_state,
        "review_followup_skipped_closed_task"
      );
      self.cleanup_worktree_if_delivered(session_id, &work_run).await?;
      return Ok(());
    }
    let completion = self.features.startup.require::<crate::completion::TaskCompletion>()?;
    if let Some(reason) = completion.pause_reason(session_id)? {
      tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, reason, "worker_delivery_paused");
      return Ok(());
    }
    let mut result_refs = vec![format!("run:{}", work_run.id)];
    if let Some(spec_hash) = &work_run.task_spec_hash {
      result_refs.push(format!("spec:{spec_hash}"));
    }
    result_refs.push(format!("review:{}", run.id));
    let evidence = if work_run.repo_path.as_deref().is_some_and(|repo_path| !repo_path.is_empty()) {
      let (landing, reason) = self.landing_for_work(&work_run).await;
      let Some((branch, commit, repo_path)) = landing else {
        // A review that stopped before its push states its verdict in its closing words; the report quotes them.
        let events_log = self.features.store.run_dir(session_id, &run.id).join(run_records::RUN_EVENTS_NAME);
        let verdict = read_worker_summary_file(&events_log)
          .await?
          .unwrap_or_else(|| "The review run left no closing words, so its verdict is unknown.".to_string());
        self
          .report_failure_to_parent_with_decider(
            session_id,
            &work_run,
            "blocked",
            Some(&format!(
              "review run {} ended without landing work run {} on {}: {reason}\n\nReview verdict:\n{verdict}",
              run.id,
              work_run.id,
              work_run.base_branch.as_deref().unwrap_or("the requested base")
            )),
            Some(run),
          )
          .await?;
        self.cleanup_worktree_if_delivered(session_id, &work_run).await?;
        return Ok(());
      };
      result_refs.push(format!("landed:{branch}@{commit}"));
      crate::completion::CompletionEvidence {
        summary: format!(
          "work run {} delivered after review {} landed {} on {branch}",
          work_run.id,
          run.id,
          commit.chars().take(12).collect::<String>()
        ),
        result_refs,
        run_ids: vec![work_run.id.clone()],
        review_run_ids: vec![run.id.clone()],
        landing: Some(crate::completion::LandingEvidence { branch, commit, repo_path: Some(repo_path) }),
      }
    } else {
      crate::completion::CompletionEvidence {
        summary: format!(
          "work run {} delivered after review {} checked the reported paths against the acceptance tests",
          work_run.id, run.id
        ),
        result_refs,
        run_ids: vec![work_run.id.clone()],
        review_run_ids: vec![run.id.clone()],
        landing: None,
      }
    };
    let completion_result =
      completion.evaluate_automatic_completion(session_id, &work_run.id, None, None, None, Some(&evidence)).await;
    let cleanup_result = self.cleanup_worktree_if_delivered(session_id, &work_run).await;
    match completion_result {
      Ok(_) => cleanup_result?,
      Err(StoreError::Conflict(blockers)) => {
        tracing::warn!(
          target: "charliebot_tasks",
          session_id,
          run_id = %work_run.id,
          blockers = ?blockers,
          "task_delivery_close_blocked"
        );
        cleanup_result?;
      }
      Err(error) => {
        cleanup_result?;
        return Err(error.into());
      }
    }
    Ok(())
  }

  async fn landing_for_work(&self, work_run: &RunRecord) -> (Option<(String, String, String)>, String) {
    let (Some(repo_path), Some(branch_name), Some(base_branch)) =
      (work_run.repo_path.as_deref(), work_run.branch_name.as_deref(), work_run.base_branch.as_deref())
    else {
      return (None, format!("work run {} records no repo, branch and base to verify", work_run.id));
    };
    let Some(commit) = git::git_rev_parse(Path::new(repo_path), branch_name).await else {
      return (None, format!("branch {branch_name} does not resolve in {repo_path}"));
    };
    let target = review_text::review_landing_target(base_branch);
    let (landed, reason) = git::git_verify_commit_landed(Path::new(repo_path), &target, &commit).await;
    if !landed {
      return (None, reason);
    }
    (Some((target, commit, repo_path.to_string())), String::new())
  }

  /// Remove and return the quota reset recorded for one run, distinguishing no failure from an unknown reset.
  pub fn take_quota_failure(&self, session_id: &str, run_id: &str) -> Option<Option<UtcDateTime>> {
    self
      .quota_failures
      .lock()
      .expect("worker quota failure map is not poisoned")
      .remove(&(session_id.to_string(), run_id.to_string()))
  }

  fn pool_chose_backend(&self, meta: &SessionMetadata, run: &RunRecord) -> bool {
    if run.kind == RunKind::Review {
      return true;
    }
    // The pool's slot field `backend_from_pool`, which the metadata holds at its top level (`fields_of(meta, owner)`).
    meta.extra("backend_from_pool").and_then(Value::as_bool).unwrap_or(false)
      || meta
        .extra("route")
        .and_then(|route| route.get("backends"))
        .and_then(Value::as_array)
        .is_some_and(|backends| !backends.is_empty())
  }

  fn fails_over(&self, meta: &SessionMetadata, run: &RunRecord) -> bool {
    run.kind == RunKind::Work
      && matches!(
        meta.task.as_ref().and_then(|task| task.task_type),
        Some(TaskType::Implement | TaskType::QuickEdit | TaskType::ScriptRun | TaskType::Verify)
      )
      && self.pool_chose_backend(meta, run)
  }

  async fn fail_over_quota(&self, meta: &SessionMetadata, run: &RunRecord) -> anyhow::Result<bool> {
    let cfg = self.features.config.get()?;
    let pool = backend_pool::backend_pool(&self.features)?;
    let exclude = vec![run.backend.clone().unwrap_or_default()];
    let route = meta
      .extra("route")
      .filter(|value| !value.is_null())
      .map(|value| serde_json::from_value::<backend_pool::BackendRoute>(value.clone()))
      .transpose()?;
    let routed_member = route
      .filter(|route| !route.backends.is_empty())
      .map(|route| pool.pick_ordered(&cfg, "failover", &route.backends, &exclude))
      .transpose()?
      .flatten();
    let member = match routed_member {
      Some(member) => Some(member),
      None => pool.select(&cfg, "failover", &exclude)?,
    };
    let Some(member) = member else { return Ok(false) };
    let guard = self.features.store.control_lock().await;
    self.features.store.set_node_backend_locked(&guard, &meta.id, &member.id).await?;
    drop(guard);
    let retry = crate::task_sessions::create_retry(
      &self.features.store,
      &cfg,
      &meta.id,
      &format!("quota-failover:{}", run.id),
      &run.id,
      Some(&member.id),
      Some(run.input_event_ids.clone()),
    )
    .await?;
    let from_backend = run.backend.as_deref().unwrap_or_default();
    let recovers_at = pool.cooling_until_minutes(from_backend).unwrap_or_default();
    self
      .features
      .store
      .persist_and_broadcast(
        &meta.id,
        json!({
          "type": event_types::QUOTA_FAILOVER,
          "from": from_backend,
          "to": member.id,
          "recovers_at": recovers_at,
        }),
      )
      .await?;
    tracing::warn!(
      target: "charliebot_tasks",
      session_id = %meta.id,
      run_id = %run.id,
      retry_run_id = %retry.id,
      from_backend = run.backend.as_deref(),
      to_backend = %member.id,
      "worker_quota_failover"
    );
    self.launch(&meta.id, &retry.id)?;
    Ok(true)
  }

  pub(crate) async fn report_failure_to_parent(
    &self,
    session_id: &str,
    run: &RunRecord,
    outcome: &str,
    summary: Option<&str>,
  ) -> anyhow::Result<()> {
    self.report_failure_to_parent_with_decider(session_id, run, outcome, summary, None).await
  }

  async fn report_failure_to_parent_with_decider(
    &self,
    session_id: &str,
    run: &RunRecord,
    outcome: &str,
    summary: Option<&str>,
    deciding_run: Option<&RunRecord>,
  ) -> anyhow::Result<()> {
    let Some(meta) = self.features.store.load_meta(session_id).await? else { return Ok(()) };
    let Some(parent_id) = meta.task_parent_id.as_deref() else { return Ok(()) };
    let source = self.features.store.load_all_facts(session_id)?.into_iter().rev().find(|event| {
      event.get("type").and_then(CompactValue::as_str) == Some(event_types::RUN_FINISHED)
        && event.get("run_id").and_then(CompactValue::as_str) == Some(run.id.as_str())
    });
    let Some(source) = source else { return Ok(()) };
    let summary = match summary {
      Some(summary) => summary.to_string(),
      None => self.worker_failure_summary(session_id, run).await?,
    };
    let parent_epoch = self.features.store.prime_aggregator(parent_id).await?;
    let (report, report_created) = {
      let guard = self.features.store.control_lock().await;
      if self.features.store.task_state(session_id)? != "open" {
        return Ok(());
      }
      self
        .dispatch
        .deliver_child_report_locked(
          &guard,
          session_id,
          source,
          outcome,
          &summary,
          Some(&[format!("run:{}", run.id)]),
          Some(parent_id),
        )
        .await?
    };
    if report_created {
      self.features.store.announce_appended_event(parent_id, &report, parent_epoch).await?;
      if self.features.store.task_state(parent_id)? != "open" {
        tracing::info!(
          target: "charliebot_tasks",
          parent = parent_id,
          child = session_id,
          report = ?report.get("id"),
          "child_report_into_archived_parent_kept_as_history"
        );
      } else {
        self.dispatch.wake_parent(parent_id, &report).await?;
      }
    }
    let archive_run = deciding_run.unwrap_or(run);
    let completion = self.features.startup.require::<crate::completion::TaskCompletion>()?;
    completion.archive_unsuccessful_worker(session_id, &archive_run.id, outcome).await?;
    Ok(())
  }

  async fn worker_failure_summary(&self, session_id: &str, run: &RunRecord) -> anyhow::Result<String> {
    let events_path = self.features.store.run_dir(session_id, &run.id).join(run_records::RUN_EVENTS_NAME);
    if events_path.is_file() {
      if let Some(summary) = read_worker_summary_file(&events_path).await? {
        return Ok(summary);
      }
      if let Some(error) = read_worker_error_file(&events_path).await? {
        return Ok(error);
      }
    }
    Ok(format!("run {} failed without a reportable output", run.id))
  }

  async fn cleanup_worktree_if_delivered(&self, session_id: &str, run: &RunRecord) -> anyhow::Result<()> {
    let Some(meta) = self.features.store.load_meta(session_id).await? else { return Ok(()) };
    if meta.task.as_ref().is_some_and(|task| task.keep_worktree)
      || self.features.store.task_state(session_id)? != "completed"
    {
      return Ok(());
    }
    let fields = Map::from_iter([("run_id".to_string(), json!(run.id)), ("session".to_string(), json!(session_id))]);
    git::git_worktree_remove_reporting(
      run.repo_path.as_deref(),
      run.worktree_path.as_deref(),
      run.branch_name.as_deref(),
      &run.id,
      Path::new(&self.features.config.get()?.paths.worktree_dir),
      &fields,
      "Task worktree",
      "task_worktree_cleanup_failed",
      "task_worktree_remove_failed",
    )
    .await;
    Ok(())
  }

  pub(crate) async fn resume_worker_run(
    &self,
    meta: &SessionMetadata,
    run: &RunRecord,
    option: &BackendOption,
    is_alive: Arc<dyn Fn() -> bool + Send + Sync>,
    ended_at: Option<UtcDateTime>,
  ) -> anyhow::Result<()> {
    let session_id = meta.id.as_str();
    let run_id = run.id.as_str();
    let run_dir = self.features.store.run_dir(session_id, run_id);
    let mut thread = worker_binding(run_id, session_id, run);
    let lifecycle = self.features.registry.lifecycle_for(&option.backend_type)?;
    lifecycle.assign_thread_native_id(&mut thread, run.native_session_id.as_deref());
    let mut worker = Worker::new(
      Arc::clone(&self.features),
      WorkerSpec {
        thread,
        working_dir: run.worktree_path.as_deref().map_or_else(|| run_dir.clone(), PathBuf::from),
        events_log: run_dir.join(run_records::RUN_EVENTS_NAME),
        task_description: String::new(),
        cfg: self.features.config.get()?,
        backend_option: Some(option.clone()),
        extra_env: Env::new(),
        on_spawned: None,
        instructions_content: None,
        session_meta: Some(meta.clone()),
        held_native_id: run.native_session_id.clone(),
        run_kind: run.kind,
      },
    )?;
    let store = Arc::clone(&self.features.store);
    let session_id_owned = session_id.to_string();
    worker.set_on_session_event(Arc::new(move |event| {
      let store = Arc::clone(&store);
      let session_id = session_id_owned.clone();
      Box::pin(async move {
        store.deliver_to_successor(&session_id, event).await?;
        Ok(())
      })
    }));
    let exit_code = worker.resume(is_alive, None).await?;
    let durable = self.finalize_worker_run(meta, run, option, exit_code, "", ended_at).await?;
    let fresh = self
      .features
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
    self.after_worker_run(meta, &fresh, &durable).await
  }

  /// The session callbacks of a manager turn, with the task-tree activity read the queue consults.
  pub(crate) fn turn_callbacks(&self) -> SessionCallbacks {
    let features = Arc::clone(&self.features);
    let tree_activity: TaskTreeActivityFn = Arc::new(move |id| {
      let activity = features.store.task_tree_activity(id)?;
      Ok((activity.has_running_tasks, activity.work_state))
    });
    self.queue.session_callbacks(Arc::clone(&self.features), tree_activity)
  }

  /// The finish recorder both manager-turn paths hand the master queue (`task_execution.py:1203`,
  /// `_manager_finish_recorder`): it records the run's evidence, lands the terminal fact with the observed exit, and
  /// dispatches the node's pending inputs. It records the observation before persisting `master_done` and
  /// `run_finished` under the control lock. `ended_at` is the drain rule's end time for a turn that ended unseen; None
  /// keeps the observed write time.
  pub fn manager_finish_recorder(
    &self,
    meta: &SessionMetadata,
    run_id: &str,
    option: &BackendOption,
    raw_log: PathBuf,
    ended_at: Option<UtcDateTime>,
    persist_done: PersistAndBroadcastFn,
  ) -> TaskFinishCallback {
    let features = Arc::clone(&self.features);
    let dispatch = Arc::clone(&self.dispatch);
    let session_id = meta.id.clone();
    let run_id = run_id.to_string();
    let option = option.clone();
    let meta = meta.clone();
    Arc::new(move |native_session_id, exit_code, extras, done_event| {
      let features = Arc::clone(&features);
      let dispatch = Arc::clone(&dispatch);
      let persist_done = Arc::clone(&persist_done);
      let session_id = session_id.clone();
      let run_id = run_id.clone();
      let option = option.clone();
      let meta = meta.clone();
      let raw_log = raw_log.clone();
      Box::pin(async move {
        let finished: anyhow::Result<bool> = async {
          // The finish recorder claims only the echoed pending ids, so a queued message the live Run never took
          // stays pending for the next turn.
          crate::midturn_delivery::claim_finish_inputs(
            &dispatch,
            &meta,
            &run_id,
            raw_log.parent().context("the run's raw log has no transport directory")?,
            exit_code == 0,
          )
          .await?;
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

          let guard = features.store.control_lock().await;
          let facts = features.store.load_all_facts(&session_id)?;
          let stop_requested = run_records::stop_requested_in_events(&facts, &run_id, None);
          let kind = if stop_requested {
            RunOutcomeKind::Interrupted
          } else if exit_code == 0 {
            RunOutcomeKind::Success
          } else {
            RunOutcomeKind::Failed
          };
          let mut outcome = RunOutcome::new(kind);
          outcome.exit_code = Some(exit_code);
          outcome.ended_at = ended_at;
          persist_done(session_id.clone(), done_event).await?;
          features.store.record_finish_locked(&guard, &session_id, &run_id, outcome.clone()).await?;
          drop(guard);

          dispatch.finish_run(&session_id, &run_id, outcome).await?;
          Ok(true)
        }
        .await;
        crate::midturn_delivery::claude_midturn::release_run(&run_id);
        finished?;
        // Inputs admitted during this turn waited for the serialized consumer; the turn's finish is what dispatches
        // their next run.
        dispatch.dispatch_pending(&session_id).await?;
        Ok(true)
      })
    })
  }

  /// Whether this process already drives `run_id`: its launch is in flight, or a resume follow holds it.
  pub(crate) fn drives_run(&self, session_id: &str, run_id: &str) -> bool {
    let key = (session_id.to_string(), run_id.to_string());
    self.launches.lock().expect("task execution launch state is not poisoned").inflight.contains(&key)
      || self.recovery.resume_follows.lock().expect("resume follow set is not poisoned").contains(&key)
  }

  pub(crate) fn child_env(&self, session_id: &str, run_id: &str, agent: &str) -> StoreResult<Env> {
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
        .find(|fact| fact.get("id").and_then(CompactValue::as_str) == Some(event_id.as_str()));
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
        (ChatEvent::Null, false)
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
    tracing::error!(target: "charliebot_tasks", session_id, run_id, kind = kind_name(initial_run.kind), error = %error, "task_run_launch_failed");
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
        self.dispatch.dispatch_pending(session_id).await?;
      } else {
        let run = self
          .features
          .store
          .read_run_sync(session_id, run_id)?
          .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
        let durable = self.terminal_outcome(session_id, run_id)?.unwrap_or_else(|| "failed".to_string());
        self.after_worker_run(meta, &run, &durable).await?;
      }
      Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(landing_error) = landing {
      tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %landing_error, "task_run_launch_failure_landing_failed");
      if is_out_of_space_error(landing_error.as_ref()) {
        self.start_run_end_landing_retry(session_id, Some(run_id));
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
      .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
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
        .any(|event| event.get("id").and_then(CompactValue::as_str) == Some(event_id.as_str()))
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
    Ok(self.features.store.node_state(session_id)?.runs.terminal_outcome(run_id)?.map(outcome_text))
  }

  fn note_launch_out_of_space(&self, session_id: &str, run_id: &str) {
    self.start_run_end_landing_retry(session_id, Some(run_id));
  }

  /// Start the node's end-landing retry task; a second request for the node is a no-op.
  pub(crate) fn start_run_end_landing_retry(&self, session_id: &str, run_id: Option<&str>) {
    recovery::start_end_landing_retry(self.clone(), session_id, run_id);
  }

  /// Follow a live worker run without holding the startup reconcile pass open.
  pub(crate) fn follow_run_in_background(&self, session_id: &str, run_id: &str) {
    let key = (session_id.to_string(), run_id.to_string());
    self.recovery.resume_follows.lock().expect("resume follow set is not poisoned").insert(key);
    let adapter = self.clone();
    let session_id = session_id.to_string();
    let run_id = run_id.to_string();
    tokio::spawn(async move {
      if let Err(error) = recovery::resume_run(&adapter, &session_id, &run_id, None).await {
        if is_out_of_space_error(error.as_ref()) {
          adapter.start_run_end_landing_retry(&session_id, Some(&run_id));
        }
        tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %error, "task_resume_follow_failed");
      }
    });
  }
}

impl DispatchExecutor for TaskExecutionAdapter {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [ChatEvent],
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
    prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move {
      let key = (session_id.to_string(), run_id.to_string());
      let mut state = self.launches.lock().expect("task execution launch state is not poisoned");
      if state.inflight.contains(&key) {
        return Ok(());
      }
      state.inflight.insert(key.clone());
      let (sender, _) = watch::channel(None);
      state.settlements.insert(key, sender);
      drop(state);
      self.schedule_launch(session_id.to_string(), run_id.to_string(), prompt.map(str::to_string));
      Ok(())
    })
  }

  fn launch_and_settle<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<SequenceLaunchSettlement>> {
    Box::pin(async move {
      if let Some(outcome) = self.terminal_outcome(session_id, run_id)? {
        return Ok(SequenceLaunchSettlement { outcome: Some(outcome), withheld: None });
      }
      let run = self
        .features
        .store
        .read_run_sync(session_id, run_id)?
        .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
      if run.pid.is_some() {
        let outcome = self.await_terminal(session_id, run_id).await?;
        return Ok(SequenceLaunchSettlement { outcome: Some(outcome), withheld: None });
      }
      <Self as SequenceExecutor>::launch(self, session_id, run_id, prompt).await?;
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
  input_events: &[ChatEvent],
) -> anyhow::Result<Option<String>> {
  let owned = input_events.iter().map(ChatEvent::to_value).collect::<Vec<_>>();
  match features.registry.binding_for(&meta.id)? {
    Some(binding) => binding.on_wake(meta, &owned).await,
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

/// Whether an error or one of its sources reports ENOSPC or EDQUOT. The store flattens filesystem errors into its
/// message text (`StoreError::Failure`), so a message that carries the errno's `(os error N)` suffix counts too.
pub fn is_out_of_space_error(error: &(dyn Error + 'static)) -> bool {
  let out_of_space_suffixes = [format!("(os error {})", libc::ENOSPC), format!("(os error {})", libc::EDQUOT)];
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
    let message = error.to_string();
    if out_of_space_suffixes.iter().any(|suffix| message.contains(suffix.as_str())) {
      return true;
    }
    current = error.source();
  }
  false
}

/// Free GiB on the filesystem containing `path`, probing its nearest existing ancestor.
pub fn free_disk_gib(path: &Path) -> anyhow::Result<f64> {
  let probe = nearest_existing_ancestor(path)?;
  let path_c = CString::new(probe.as_os_str().as_bytes())?;
  let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
  // `probe` is an existing path and `path_c` is NUL terminated.
  if unsafe { libc::statvfs(path_c.as_ptr(), stat.as_mut_ptr()) } != 0 {
    return Err(std::io::Error::last_os_error()).with_context(|| format!("reading disk space at {}", probe.display()));
  }
  let stat = unsafe { stat.assume_init() };
  Ok(free_disk_gib_from_statvfs(stat.f_bavail, stat.f_frsize))
}

/// The worker launch's free-space check for the data and worktree filesystems (`_disk_headroom_withhold_reason`).
fn disk_headroom_withhold_reason(cfg: &CharlieBotConfig) -> StoreResult<Option<String>> {
  let min_free_gib = cfg.server.min_free_disk_gib;
  if min_free_gib <= 0 {
    return Ok(None);
  }
  for path in [cfg.charliebot_home.as_path(), Path::new(&cfg.paths.worktree_dir)] {
    let free_gib = free_disk_gib(path).map_err(|error| StoreError::new(error.to_string()))?;
    if free_gib < min_free_gib as f64 {
      return Ok(Some(format!("disk free {free_gib:.1} GiB, below {min_free_gib} GiB ({})", path.display())));
    }
  }
  Ok(None)
}

fn nearest_existing_ancestor(path: &Path) -> anyhow::Result<&Path> {
  let mut probe = path;
  while !probe.exists() {
    let parent = probe.parent().ok_or_else(|| anyhow!("no existing ancestor to probe for {}", path.display()))?;
    if parent == probe {
      anyhow::bail!("no existing ancestor to probe for {}", path.display());
    }
    probe = parent;
  }
  Ok(probe)
}

fn free_disk_gib_from_statvfs(available_blocks: libc::fsblkcnt_t, fragment_size: libc::c_ulong) -> f64 {
  (available_blocks as f64 * fragment_size as f64) / (1024.0_f64.powi(3))
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

/// The class name a launch failure's evidence carries (`type(exc).__name__`): the Python class of a Rust error type that
/// has one, and otherwise the class the call site names.
fn launch_error_class(error: &anyhow::Error, call_site: &'static str) -> &'static str {
  if error.downcast_ref::<crate::git::BaseBranchResolutionError>().is_some() {
    return "BaseBranchResolutionError";
  }
  if error.downcast_ref::<SequenceRunWithoutControllerError>().is_some() {
    return "TaskInvalidError";
  }
  call_site
}

fn is_worker_kind(kind: RunKind) -> bool {
  matches!(kind, RunKind::Work | RunKind::Review | RunKind::Iteration | RunKind::ScheduledStep)
}

fn worker_binding(run_id: &str, session_id: &str, run: &RunRecord) -> ThreadMetadata {
  ThreadMetadata {
    id: run_id.to_string(),
    session_id: session_id.to_string(),
    description: String::new(),
    status: ThreadStatus::Idle,
    created_at: UtcDateTime::now(),
    started_at: None,
    completed_at: None,
    pid: run.pid,
    pid_start: run.pid_start.clone(),
    exit_code: None,
    branch_name: None,
    base_branch: None,
    repo_path: None,
    worktree_path: None,
    context: None,
    backend: None,
    model: None,
    keep_worktree: false,
    tried_backends: Vec::new(),
    task_type: None,
    extra: Map::new(),
  }
}

fn worker_continuation_reset_note(reason: &str, previous_log_path: &Path) -> String {
  format!(
    "[Context reset: {reason}. The task continues on the same branch and worktree; its spec and the new input follow. The previous run's log is at {} — read it before acting, so finished work is not repeated.]",
    previous_log_path.display()
  )
}

fn parse_outcome(outcome: &str) -> RunOutcomeKind {
  match outcome {
    "success" => RunOutcomeKind::Success,
    "failed" => RunOutcomeKind::Failed,
    "interrupted" => RunOutcomeKind::Interrupted,
    _ => unreachable!("worker outcome comes from a durable run result"),
  }
}

fn result_success(event: &Value) -> bool {
  charliebot_agents::transport::recovery::result_exit_code(std::slice::from_ref(event)) == 0
}

fn events_log_result_success(path: &Path) -> anyhow::Result<bool> {
  let (success, _) = visit_events_from_end(path, &[event_types::RESULT], None, |event| Some(result_success(&event)))?;
  Ok(success.unwrap_or(false))
}

fn worker_run_record(session_id: &str, run_id: &str, backend: &str, model: Option<&str>) -> RunRecord {
  RunRecord { kind: RunKind::Work, ..manager_run_record(session_id, run_id, backend, model) }
}

async fn read_worker_summary_file(path: &Path) -> anyhow::Result<Option<String>> {
  let path = path.to_path_buf();
  tokio::task::spawn_blocking(move || review::read_worker_summary_file(&path))
    .await
    .context("joining worker summary reader")?
}

async fn read_worker_error_file(path: &Path) -> anyhow::Result<Option<String>> {
  let path = path.to_path_buf();
  tokio::task::spawn_blocking(move || review::read_worker_error_file(&path))
    .await
    .context("joining worker error reader")?
}

/// The cold-read spec of a task. Only a cold-read node carries one, and its runs read the page it names.
fn cold_spec<'a>(meta: &'a SessionMetadata, run_id: &str) -> anyhow::Result<&'a ColdReadSpec> {
  meta
    .task
    .as_ref()
    .and_then(|task| task.cold_read.as_ref())
    .with_context(|| format!("cold-read run {run_id} of task {} carries no cold_read spec", meta.id))
}

pub(crate) fn canonical_task_spec_text(
  task: Option<&charliebot_store::metadata::TaskSpec>,
) -> StoreResult<Option<String>> {
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
    let running_queue = Arc::clone(&queue);
    features.startup.insert(charliebot_registry::RunningTurnInputs::new(move |session_id| {
      running_queue.running_user_event_ids(session_id)
    }))?;
    features.startup.insert(crate::completion::TaskCompletion::new(Arc::clone(&features)))?;
    let completion = features.startup.require::<crate::completion::TaskCompletion>()?;
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = completion;
    features.startup.insert(Dispatcher::new(
      Arc::clone(&features.store),
      Arc::clone(&features),
      sequence_completion,
      run_completion,
    ))?;
    let dispatch = features.startup.require::<Dispatcher>()?;
    let adapter = Arc::new(TaskExecutionAdapter::new(Arc::clone(&features), Arc::clone(&queue), Arc::clone(&dispatch)));
    dispatch.set_executor(Some(adapter.clone()));
    features.startup.insert((*adapter).clone())?;
    Ok(())
  })
}

#[cfg(test)]
mod tests {
  use super::{
    free_disk_gib_from_statvfs, is_out_of_space_error, native_continuation_matches, nearest_existing_ancestor,
    pre_recovery_step,
  };
  use charliebot_core::config::BackendOption;

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
  fn out_of_space_text_from_a_flattened_store_error_is_found() {
    let flattened = charliebot_store::StoreError::new(format!(
      "/synthetic/home/sessions/node/data/runs/run-1/metadata.json: {}",
      std::io::Error::from_raw_os_error(libc::ENOSPC)
    ));
    assert!(is_out_of_space_error(&flattened));
    let other = charliebot_store::StoreError::new("/synthetic/home/sessions: Permission denied (os error 13)");
    assert!(!is_out_of_space_error(&other));
  }

  #[test]
  fn nearest_existing_ancestor_returns_the_closest_existing_path() {
    let directory = tempfile::tempdir().expect("temporary directory exists");
    let ancestor = directory.path().join("existing");
    std::fs::create_dir(&ancestor).expect("the test ancestor is created");
    let missing = ancestor.join("not-created").join("worktree");
    assert_eq!(nearest_existing_ancestor(&missing).unwrap(), ancestor.as_path());
  }

  #[test]
  fn free_disk_gib_arithmetic_uses_available_blocks_and_fragment_size() {
    assert_eq!(free_disk_gib_from_statvfs(4, 1024 * 1024 * 1024), 4.0);
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

  #[tokio::test]
  async fn the_startup_step_answers_running_turn_inputs_as_the_master_state_reader_does() {
    use std::sync::Arc;

    use charliebot_agents::backend_pool::BackendPool;
    use charliebot_core::credentials::load_credentials;
    use charliebot_core::hot_reload::HotReload;
    use charliebot_registry::store_hooks::store_hooks;
    use charliebot_registry::{FeatureContext, Registry, RunningTurnInputs, StartupState};
    use charliebot_store::Store;

    let home = tempfile::tempdir().expect("temporary directory exists");
    let registry = Arc::new(Registry::new());
    let loaded = registry.load_config(home.path()).expect("an empty home loads with the defaults");
    let reload_config = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "execution_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "execution_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    startup.insert(BackendPool::default()).expect("the pool inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });

    pre_recovery_step(Arc::clone(&features)).await.expect("the startup step runs");

    let inputs = features.startup.require::<RunningTurnInputs>().expect("the startup step inserts the inputs");
    let session_id = "synthetic-session";
    let reader = crate::master_state::running_user_event_ids(&features, session_id).expect("the queue is in startup");
    assert_eq!(inputs.user_event_ids(session_id), reader);
    assert!(reader.is_empty(), "no turn runs in an idle queue");
  }

  #[tokio::test]
  async fn running_turn_inputs_returns_all_running_inputs_in_arrival_order() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use crate::master_queue::{MasterQueue, PersistAndBroadcastFn};
    use crate::master_state::{MasterTurnRequest, TaskRunBinding};
    use charliebot_agents::backend_pool::BackendPool;
    use charliebot_core::credentials::load_credentials;
    use charliebot_core::hot_reload::HotReload;
    use charliebot_registry::store_hooks::store_hooks;
    use charliebot_registry::{FeatureContext, Registry, RunningTurnInputs, StartupState};
    use charliebot_store::Store;
    use charliebot_store::metadata::{SessionMetadata, TaskProfile};

    let home = tempfile::tempdir().expect("temporary directory exists");
    let registry = Arc::new(Registry::new());
    let loaded = registry.load_config(home.path()).expect("an empty home loads with the defaults");
    let reload_config = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "execution_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "execution_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    startup.insert(BackendPool::default()).expect("the pool inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });

    pre_recovery_step(Arc::clone(&features)).await.expect("the startup step runs");
    let queue = features.startup.require::<MasterQueue>().expect("the startup step inserts the queue");
    let inputs = features.startup.require::<RunningTurnInputs>().expect("the startup step inserts the inputs");
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let first_persist = Arc::new(AtomicBool::new(true));
    let mut callbacks = queue.session_callbacks(Arc::clone(&features), Arc::new(|_| Ok((false, String::new()))));
    callbacks.persist_and_broadcast = {
      let entered = Arc::clone(&entered);
      let release = Arc::clone(&release);
      let first_persist = Arc::clone(&first_persist);
      let persist: PersistAndBroadcastFn = Arc::new(move |_session_id, _event| {
        let should_pause = first_persist.swap(false, Ordering::AcqRel);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        Box::pin(async move {
          if should_pause {
            entered.notify_one();
            release.notified().await;
          }
          Ok(())
        })
      });
      persist
    };
    callbacks.after_round = None;

    let session_id = "synthetic-running-session";
    let request = MasterTurnRequest {
      cfg: features.config.get().expect("the config loads"),
      session_meta: SessionMetadata::new(session_id, "Synthetic running session", TaskProfile::Manager),
      user_content: "synthetic turn".to_string(),
      callbacks,
      user_event_ids: vec!["input-first".to_string(), "input-second".to_string()],
      task_instructions: String::new(),
      task_run: TaskRunBinding {
        session_id: session_id.to_string(),
        run_id: "synthetic-run".to_string(),
        transport_dir: home.path().join("transport"),
        fresh_native_context: false,
      },
      on_task_spawn: Arc::new(|_pid, _pid_start| Box::pin(async { Ok(()) })),
      on_task_finish: Arc::new(|_cc_session_id, _exit_code, _extras, _done_event| Box::pin(async { Ok(false) })),
      auto_trigger: false,
      backend_option: None,
      extra_claude_flags: None,
      uploaded_files: None,
      extra_env: None,
    };
    let queue_task = {
      let queue = Arc::clone(&queue);
      tokio::spawn(async move { queue.run_message(request).await })
    };

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
      .await
      .expect("the running turn reaches its refusal callback");
    assert_eq!(inputs.user_event_ids(session_id), ["input-first", "input-second"]);
    // A mid-turn delivery joins the running binding: Python's `_current_items` holds the live item, so `record_ack`'s
    // append reaches a web reply posted while the turn runs.
    let mut acked = vec!["input-first".to_string(), "input-second".to_string()];
    crate::midturn_delivery::record_ack(
      &queue,
      session_id,
      &mut acked,
      "synthetic-run",
      &serde_json::json!({
        "type": charliebot_core::event_types::INPUT_DELIVERED,
        "input_event_id": "input-delivered",
        "run_id": "synthetic-run",
      }),
    )
    .expect("the delivered input is acknowledged");
    assert_eq!(inputs.user_event_ids(session_id), ["input-first", "input-second", "input-delivered"]);
    release.notify_one();
    queue_task.await.expect("the queue task joins").expect("the turn settles");
    queue.shutdown().await.expect("the queue shuts down");
  }

  #[tokio::test]
  async fn await_terminal_reuses_one_node_state_fold_across_polls() {
    use std::sync::Arc;

    use crate::test_fixtures;
    use charliebot_agents::backend_pool::BackendPool;
    use serde_json::json;

    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    features.startup.insert(BackendPool::default()).unwrap();
    pre_recovery_step(Arc::clone(&features)).await.unwrap();
    let meta = test_fixtures::manager_session(&features, "await-terminal-count", "fable", None, "claude-await").await;
    features.store.append_chat_event(&meta.id, json!({"type":"user","content":"synthetic input"})).await.unwrap();
    features
      .store
      .append_chat_event(&meta.id, json!({"type":"run_finished","run_id":"terminal-count","outcome":"success"}))
      .await
      .unwrap();
    let adapter = features.startup.require::<super::TaskExecutionAdapter>().unwrap();
    let event_count = features.store.load_chat_events_shared_sync(&meta.id).unwrap().len() as u64;
    let before = features.store.history_work();

    assert_eq!(adapter.await_terminal(&meta.id, "terminal-count").await.unwrap(), "success");
    assert_eq!(adapter.await_terminal(&meta.id, "terminal-count").await.unwrap(), "success");

    let after = features.store.history_work();
    assert_eq!(after.node_state_full_folds - before.node_state_full_folds, 1);
    assert_eq!(after.node_state_folded_events - before.node_state_folded_events, event_count);
  }

  #[tokio::test]
  async fn launch_history_helpers_reuse_one_facts_fold_and_shared_event_list() {
    use std::sync::Arc;

    use charliebot_agents::backend_pool::BackendPool;
    use charliebot_core::config::BackendOption;
    use charliebot_store::HistoryWork;
    use charliebot_store::run_records::RunKind;
    use serde_json::json;

    use crate::prompts::PromptSnapshot;
    use crate::test_fixtures;

    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    features.startup.insert(BackendPool::default()).unwrap();
    pre_recovery_step(Arc::clone(&features)).await.unwrap();
    let meta = test_fixtures::manager_session(&features, "launch-history-count", "fable", None, "claude-launch").await;
    for (id, content) in [("input-a", "first input"), ("input-b", "second input")] {
      features.store.append_chat_event(&meta.id, json!({"id":id,"type":"user","content":content})).await.unwrap();
    }
    let guard = features.store.control_lock().await;
    let mut previous = super::worker_run_record(&meta.id, "run-a", "fable", None);
    previous.input_event_ids = vec!["input-a".to_string()];
    let mut current = super::worker_run_record(&meta.id, "run-b", "fable", None);
    current.input_event_ids = vec!["input-b".to_string()];
    features.store.register_run_locked(&guard, &meta.id, previous, None).await.unwrap();
    features.store.register_run_locked(&guard, &meta.id, current.clone(), None).await.unwrap();
    drop(guard);
    let event_count = features.store.load_chat_events_shared_sync(&meta.id).unwrap().len() as u64;
    features.store.evict_recovery_history(&meta.id);
    let adapter = features.startup.require::<super::TaskExecutionAdapter>().unwrap();
    let option = BackendOption {
      id: "fable".to_string(),
      label: "Fable".to_string(),
      model: "claude-fable-5-1".to_string(),
      prompt_overlay: None,
      backend_type: "cc-claude".to_string(),
      type_fields: Default::default(),
    };
    let snapshot = PromptSnapshot::default();
    let before: HistoryWork = features.store.history_work();
    let history = features.store.load_all_facts_view_sync(&meta.id).unwrap();
    let facts = features.store.node_state(&meta.id).unwrap();

    let mut inherited = current.clone();
    adapter.inherit_worker_continuation(&meta, &mut inherited).unwrap();
    assert!(adapter.run_continuation(&meta, &current, &option, &snapshot, &facts, &facts.runs).unwrap().is_some());
    assert_eq!(adapter.batch_only_context(&meta, &current, &history).await.unwrap(), "second input");
    assert_eq!(adapter.work_description(&meta, &current, &history).await.unwrap(), "second input");
    assert!(adapter.later_messages_section(&meta, &current, &history).await.unwrap().contains("second input"));

    let after = features.store.history_work();
    assert_eq!(after.full_folds - before.full_folds, 1, "the launch's repeated fact queries share one node fold");
    assert_eq!(after.event_history_views - before.event_history_views, 1, "all builders reuse one history view");
    assert_eq!(after.folded_events - before.folded_events, event_count);
    assert_eq!(after.owned_event_clones - before.owned_event_clones, 0, "builders select from the shared event list");
    assert_eq!(current.kind, RunKind::Work);
  }
}
