//! Per-session queue for serialized manager turns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::backends::claude_code::{
  claude_accounts, claude_compaction, claude_config, claude_idle_compaction, claude_metadata, login_dirs,
};
use charliebot_agents::runner::AgentRunner;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::backend::BackendLifecycle;
use charliebot_registry::context::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::run_records::RunKind;
use charliebot_usage::SessionUsage;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::autonamer;
use crate::master_run;
use crate::master_state::{MasterResumeRequest, MasterTurnRequest, WorkItem};

pub type PersistAndBroadcastFn = Arc<dyn Fn(String, Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type MarkUnreadFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type PersistNativeIdFn =
  Arc<dyn Fn(String, String, Option<String>) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type PersistAccountLabelFn =
  Arc<dyn Fn(String, String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type ContextStateFn = Arc<
  dyn Fn(String, SessionMetadata) -> BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>>
    + Send
    + Sync,
>;
pub type TaskTreeActivityFn = Arc<dyn Fn(&str) -> anyhow::Result<(bool, String)> + Send + Sync>;
pub type AfterRoundFn =
  Arc<dyn Fn(Arc<CharlieBotConfig>, String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Session block callbacks used by a manager turn.
#[derive(Clone)]
pub struct SessionCallbacks {
  pub features: Arc<FeatureContext>,
  pub persist_and_broadcast: PersistAndBroadcastFn,
  pub mark_unread: MarkUnreadFn,
  pub persist_cc_session_id: PersistNativeIdFn,
  pub persist_account_label: Option<PersistAccountLabelFn>,
  pub context_state: Option<ContextStateFn>,
  pub task_tree_activity: TaskTreeActivityFn,
  pub after_round: Option<AfterRoundFn>,
}

#[derive(Default)]
struct QueueState {
  session_queues: HashMap<String, VecDeque<WorkItem>>,
  session_consumers: HashMap<String, JoinHandle<()>>,
  consumer_cancel: HashMap<String, watch::Sender<bool>>,
  current_items: HashMap<String, Vec<String>>,
  idle_reservations: HashMap<String, watch::Sender<bool>>,
  running_turns: HashMap<String, Arc<Mutex<SessionMetadata>>>,
  active_procs: HashMap<String, Arc<AgentRunner>>,
  shutdown: bool,
}

/// The idle compaction runner: `claude_compaction::run_compaction` in production.
pub type CompactionRunFn = Arc<
  dyn for<'a> Fn(
      claude_compaction::CompactionRun<'a>,
    ) -> BoxFuture<'a, anyhow::Result<claude_compaction::CompactionOutcome>>
    + Send
    + Sync,
>;

/// Owns per-session queues, consumers, current work items, and active backend runners.
pub struct MasterQueue {
  state: Mutex<QueueState>,
  backend_pool: Arc<BackendPool>,
  compaction: CompactionRunFn,
}

fn run_real_compaction<'a>(
  run: claude_compaction::CompactionRun<'a>,
) -> BoxFuture<'a, anyhow::Result<claude_compaction::CompactionOutcome>> {
  Box::pin(claude_compaction::run_compaction(run))
}

struct PersistedAccountLabel {
  label: String,
  read_back: Option<Option<String>>,
}

impl MasterQueue {
  pub fn new(backend_pool: Arc<BackendPool>) -> Self {
    Self::with_compaction_runner(backend_pool, Arc::new(run_real_compaction))
  }

  /// The queue with the idle compaction runner the trigger path starts. Tests replace only the runner.
  pub fn with_compaction_runner(backend_pool: Arc<BackendPool>, compaction: CompactionRunFn) -> Self {
    Self { state: Mutex::new(QueueState::default()), backend_pool, compaction }
  }

  pub fn session_callbacks(
    self: &Arc<Self>,
    features: Arc<FeatureContext>,
    task_tree_activity: TaskTreeActivityFn,
  ) -> SessionCallbacks {
    let persist_features = Arc::clone(&features);
    let persist_and_broadcast = Arc::new(move |session_id: String, event: Value| {
      let features = Arc::clone(&persist_features);
      Box::pin(async move {
        features.store.persist_and_broadcast(&session_id, event).await?;
        Ok(())
      }) as BoxFuture<'static, anyhow::Result<()>>
    });
    let unread_features = Arc::clone(&features);
    let mark_unread = Arc::new(move |session_id: String| {
      let features = Arc::clone(&unread_features);
      Box::pin(async move {
        let was_unread = features.store.get_session(&session_id).await?.is_some_and(|meta| meta.has_unread);
        let saved = features.store.mark_unread(&session_id).await?;
        if !was_unread && saved.is_some() {
          features
            .store
            .broadcast_sidebar(
              &session_id,
              event_types::UNREAD_CHANGED,
              Map::from_iter([("has_unread".to_string(), json!(true))]),
            )
            .await?;
        }
        Ok(())
      }) as BoxFuture<'static, anyhow::Result<()>>
    });
    let anchor_features = Arc::clone(&features);
    let persist_cc_session_id = Arc::new(move |session_id: String, value: String, native_backend: Option<String>| {
      let features = Arc::clone(&anchor_features);
      Box::pin(async move {
        Ok(features.store.persist_cc_session_id(&session_id, &value, native_backend.as_deref()).await?)
      }) as BoxFuture<'static, anyhow::Result<Option<String>>>
    });
    let account_features = Arc::clone(&features);
    let persist_account_label = Arc::new(move |session_id: String, label: String| {
      let features = Arc::clone(&account_features);
      Box::pin(async move { Ok(features.store.persist_account_label(&session_id, &label).await?) })
        as BoxFuture<'static, anyhow::Result<Option<String>>>
    });
    let context_features = Arc::clone(&features);
    let context_state = Arc::new(move |session_id: String, session_meta: SessionMetadata| {
      let features = Arc::clone(&context_features);
      Box::pin(async move {
        let usage = features.startup.require::<SessionUsage>()?;
        Ok(usage.context_state(&session_id, &session_meta)?)
      }) as BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>>
    });
    let after_features = Arc::clone(&features);
    let pool = Arc::clone(&self.backend_pool);
    let after_round = Arc::new(move |cfg: Arc<CharlieBotConfig>, session_id: String| {
      let features = Arc::clone(&after_features);
      let pool = Arc::clone(&pool);
      Box::pin(async move { autonamer::name_after_round(&features, &cfg, &session_id, pool).await })
        as BoxFuture<'static, anyhow::Result<()>>
    });
    SessionCallbacks {
      features,
      persist_and_broadcast,
      mark_unread,
      persist_cc_session_id,
      persist_account_label: Some(persist_account_label),
      context_state: Some(context_state),
      task_tree_activity,
      after_round: Some(after_round),
    }
  }

  pub async fn run_message(self: &Arc<Self>, request: MasterTurnRequest) -> anyhow::Result<Option<String>> {
    let MasterTurnRequest {
      cfg,
      session_meta,
      user_content,
      callbacks,
      user_event_ids,
      task_instructions,
      task_run,
      on_task_spawn,
      on_task_finish,
      auto_trigger,
      backend_option,
      extra_claude_flags,
      uploaded_files,
      extra_env,
    } = request;
    let features = Arc::clone(&callbacks.features);
    let session_dir = cfg.sessions_dir().join(&session_meta.id);
    tokio::fs::create_dir_all(&session_dir)
      .await
      .with_context(|| format!("creating session directory {}", session_dir.display()))?;
    features.registry.before_turn(&session_meta, &cfg, &features).await?;
    let (completion, result) = oneshot::channel();
    let (_cancel_sender, cancel) = watch::channel(false);
    let item = WorkItem {
      cfg,
      session_meta: Arc::new(Mutex::new(session_meta)),
      user_content,
      callbacks,
      auto_trigger,
      backend_option,
      extra_claude_flags,
      task_run,
      user_event_ids,
      uploaded_files,
      resume_record: None,
      resume_is_alive: None,
      task_instructions: Some(task_instructions),
      on_task_spawn,
      on_task_finish,
      extra_env,
      cancel,
      completion: Some(completion),
    };
    self.enqueue_and_notify(item).await?;
    result.await.context("manager turn consumer stopped before returning its result")?
  }

  pub async fn enqueue_master_resume(
    self: &Arc<Self>,
    request: MasterResumeRequest,
  ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<Option<String>>>> {
    let MasterResumeRequest {
      cfg,
      session_meta,
      record,
      callbacks,
      is_alive,
      task_run,
      on_task_spawn,
      on_task_finish,
      extra_env,
    } = request;
    let (completion, result) = oneshot::channel();
    let (_cancel_sender, cancel) = watch::channel(false);
    let item = WorkItem {
      cfg,
      session_meta: Arc::new(Mutex::new(session_meta)),
      user_content: String::new(),
      callbacks,
      auto_trigger: false,
      backend_option: None,
      extra_claude_flags: None,
      task_run,
      user_event_ids: record.user_event_ids.clone(),
      uploaded_files: None,
      resume_record: Some(record),
      resume_is_alive: Some(is_alive),
      task_instructions: None,
      on_task_spawn,
      on_task_finish,
      extra_env,
      cancel,
      completion: Some(completion),
    };
    self.enqueue_and_notify(item).await?;
    Ok(result)
  }

  pub fn queued_user_event_ids(&self, session_id: &str) -> HashSet<String> {
    let state = self.state.lock().expect("master queue mutex is not poisoned");
    let mut ids = state.current_items.get(session_id).into_iter().flatten().cloned().collect::<HashSet<_>>();
    if let Some(queue) = state.session_queues.get(session_id) {
      for item in queue {
        ids.extend(item.user_event_ids.iter().cloned());
      }
    }
    ids
  }

  /// Reserve an idle-compaction start only while no session turn is running or queued.
  fn reserve_idle_compaction(&self, session_id: &str) -> bool {
    let mut state = self.state.lock().expect("master queue mutex is not poisoned");
    let busy = state.current_items.contains_key(session_id)
      || state.session_queues.get(session_id).is_some_and(|queue| !queue.is_empty())
      || state.idle_reservations.contains_key(session_id);
    if busy {
      return false;
    }
    let (sender, _receiver) = watch::channel(true);
    state.idle_reservations.insert(session_id.to_string(), sender);
    true
  }

  fn release_idle_compaction_reservation(&self, session_id: &str) {
    if let Some(sender) =
      self.state.lock().expect("master queue mutex is not poisoned").idle_reservations.remove(session_id)
    {
      sender.send_replace(false);
    }
  }

  async fn wait_for_idle_compaction_reservation(&self, session_id: &str) -> anyhow::Result<()> {
    loop {
      let receiver = self
        .state
        .lock()
        .expect("master queue mutex is not poisoned")
        .idle_reservations
        .get(session_id)
        .map(watch::Sender::subscribe);
      let Some(mut receiver) = receiver else {
        return Ok(());
      };
      while *receiver.borrow() {
        receiver.changed().await.context("waiting for idle compaction start reservation")?;
      }
    }
  }

  pub fn running_user_event_ids(&self, session_id: &str) -> Vec<String> {
    self
      .state
      .lock()
      .expect("master queue mutex is not poisoned")
      .current_items
      .get(session_id)
      .cloned()
      .unwrap_or_default()
  }

  /// Append one acknowledged input id to the running session's binding (`running_user_event_ids`).
  ///
  /// Python's `_current_items` holds the live `_WorkItem`, so `midturn_delivery.record_ack`'s append is visible to a
  /// web reply posted while the turn runs. This queue stores the ids by value, so the ack reaches the reader through
  /// this method as well as through the item.
  pub fn append_running_user_event_id(&self, session_id: &str, input_id: &str) {
    let mut state = self.state.lock().expect("master queue mutex is not poisoned");
    if let Some(ids) = state.current_items.get_mut(session_id)
      && !ids.iter().any(|id| id == input_id)
    {
      ids.push(input_id.to_string());
    }
  }

  pub fn running_turns_except(&self, session_id: &str) -> Vec<SessionMetadata> {
    let metadata = {
      let state = self.state.lock().expect("master queue mutex is not poisoned");
      state
        .running_turns
        .iter()
        .filter(|(running_id, _)| running_id.as_str() != session_id)
        .map(|(_, metadata)| Arc::clone(metadata))
        .collect::<Vec<_>>()
    };
    metadata
      .into_iter()
      .map(|metadata| metadata.lock().expect("running turn metadata mutex is not poisoned").clone())
      .collect()
  }

  pub async fn terminate_active(&self, session_id: &str) -> anyhow::Result<()> {
    let runner = self.state.lock().expect("master queue mutex is not poisoned").active_procs.get(session_id).cloned();
    if let Some(runner) = runner {
      runner.terminate().await?;
    }
    Ok(())
  }

  /// Cancel every per-session consumer at server stop.
  pub async fn shutdown(&self) -> anyhow::Result<()> {
    let handles = {
      let mut state = self.state.lock().expect("master queue mutex is not poisoned");
      state.shutdown = true;
      for cancel in state.consumer_cancel.values() {
        cancel.send_replace(true);
      }
      std::mem::take(&mut state.session_consumers).into_values().collect::<Vec<_>>()
    };
    for handle in handles {
      handle.await.context("joining a master queue consumer")?;
    }
    Ok(())
  }

  pub(crate) fn set_active_runner(&self, session_id: &str, runner: Arc<AgentRunner>) {
    self.state.lock().expect("master queue mutex is not poisoned").active_procs.insert(session_id.to_string(), runner);
  }

  pub(crate) fn clear_active_runner(&self, session_id: &str) {
    self.state.lock().expect("master queue mutex is not poisoned").active_procs.remove(session_id);
  }

  async fn enqueue_and_notify(self: &Arc<Self>, item: WorkItem) -> anyhow::Result<()> {
    let session_id = item.session_metadata().id;
    let auto_trigger = item.auto_trigger;
    let resumed_start = item.resume_record.as_ref().map(|record| record.started_at);
    let callbacks = item.callbacks.clone();
    let (thinking_since, created) = {
      let mut state = self.state.lock().expect("master queue mutex is not poisoned");
      if state.shutdown {
        return Err(anyhow!("master queue shutdown has started"));
      }
      let (thinking_since, created) =
        callbacks.features.store.mark_busy(&session_id, resumed_start.unwrap_or_else(UtcDateTime::now))?;
      state.session_queues.entry(session_id.clone()).or_default().push_back(item);
      if !state.session_consumers.contains_key(&session_id) {
        let (cancel_sender, cancel) = watch::channel(false);
        state.consumer_cancel.insert(session_id.clone(), cancel_sender);
        let queue = Arc::clone(self);
        let task_session = session_id.clone();
        let handle = tokio::spawn(async move { queue.session_consumer(task_session, cancel).await });
        state.session_consumers.insert(session_id.clone(), handle);
      }
      (thinking_since, created)
    };
    if created {
      broadcast_running_changed(&callbacks, &session_id, true, Some(thinking_since), auto_trigger).await?;
    }
    Ok(())
  }

  /// Drain the session's queue, then retire this consumer. A queue that is empty after a turn ends the consumer, as
  /// Python's `_session_consumer` does, so the busy interval closes when the last turn finishes.
  async fn session_consumer(self: Arc<Self>, session_id: String, cancel: watch::Receiver<bool>) {
    let mut last_cc_session_id: Option<String> = None;
    let mut last_account_label: Option<String> = None;
    let mut last_turn: Option<(SessionCallbacks, bool)> = None;
    let retired = loop {
      let mut item = {
        let mut state = self.state.lock().expect("master queue mutex is not poisoned");
        match state.session_queues.get_mut(&session_id).and_then(VecDeque::pop_front) {
          Some(item) => {
            state.current_items.insert(session_id.clone(), item.user_event_ids.clone());
            state.running_turns.insert(session_id.clone(), Arc::clone(&item.session_meta));
            last_turn = Some((item.callbacks.clone(), item.auto_trigger));
            item
          }
          // The consumer pops its first item before it can retire: the enqueue that spawned it pushed that item.
          None => {
            let turn = last_turn.as_ref().expect("a consumer pops an item before it retires").clone();
            drop(state);
            let active = (turn.0.task_tree_activity)(&session_id).map(|(active, _)| active);
            let mut state = self.state.lock().expect("master queue mutex is not poisoned");
            match state.session_queues.get_mut(&session_id).and_then(VecDeque::pop_front) {
              Some(item) => {
                state.current_items.insert(session_id.clone(), item.user_event_ids.clone());
                state.running_turns.insert(session_id.clone(), Arc::clone(&item.session_meta));
                last_turn = Some((item.callbacks.clone(), item.auto_trigger));
                item
              }
              None => break Self::retire_locked(&mut state, &session_id, &turn, active),
            }
          }
        }
      };
      item.cancel = cancel.clone();

      let outcome = self.process_item(&session_id, &mut item, &mut last_cc_session_id, &mut last_account_label).await;
      {
        let mut state = self.state.lock().expect("master queue mutex is not poisoned");
        state.current_items.remove(&session_id);
        state.running_turns.remove(&session_id);
      }
      if let Err(error) = outcome {
        if error.downcast_ref::<master_run::TurnLetGo>().is_none()
          && error.downcast_ref::<master_run::TurnCancelled>().is_none()
        {
          tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "session_consumer_item_error");
        }
        if let Some(completion) = item.completion.take() {
          let _ = completion.send(Err(error));
        }
      } else if *cancel.borrow()
        && let Some(completion) = item.completion.take()
      {
        let _ = completion.send(Err(anyhow!("manager turn cancelled during server shutdown")));
      }
      if *cancel.borrow() {
        let turn = last_turn.as_ref().expect("a consumer pops an item before it retires");
        break self.retire(&session_id, turn);
      }
    };

    let (callbacks, auto_trigger, active) = retired;
    if let Err(error) = &active {
      tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "task_tree_activity_failed");
    }
    let has_running_tasks = active.unwrap_or(false);
    if let Err(error) = broadcast_running_changed(&callbacks, &session_id, has_running_tasks, None, auto_trigger).await
    {
      tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "master_queue_running_changed_failed");
    }
  }

  /// Retire the session's consumer and clear its busy interval, in one section under the queue mutex with no await.
  /// The caller holds the mutex across its empty-queue check, so an enqueue cannot interleave: it either finds the
  /// queue non-empty and extends this consumer, or finds no consumer and starts one that marks busy again.
  fn retire_locked(
    state: &mut QueueState,
    session_id: &str,
    turn: &(SessionCallbacks, bool),
    active: anyhow::Result<bool>,
  ) -> (SessionCallbacks, bool, anyhow::Result<bool>) {
    state.session_consumers.remove(session_id);
    state.consumer_cancel.remove(session_id);
    state.current_items.remove(session_id);
    state.running_turns.remove(session_id);
    state.active_procs.remove(session_id);
    let queued = state.session_queues.remove(session_id).unwrap_or_default();
    for mut item in queued {
      if let Some(completion) = item.completion.take() {
        let _ = completion.send(Err(anyhow!("manager turn cancelled during server shutdown")));
      }
    }
    let (callbacks, auto_trigger) = turn;
    callbacks.features.store.clear_busy(session_id);
    (callbacks.clone(), *auto_trigger, active)
  }

  /// Retire the consumer after a cancelled turn, taking the queue mutex for the retirement section.
  fn retire(
    &self,
    session_id: &str,
    turn: &(SessionCallbacks, bool),
  ) -> (SessionCallbacks, bool, anyhow::Result<bool>) {
    let active = (turn.0.task_tree_activity)(session_id).map(|(active, _)| active);
    let mut state = self.state.lock().expect("master queue mutex is not poisoned");
    Self::retire_locked(&mut state, session_id, turn, active)
  }

  async fn process_item(
    self: &Arc<Self>,
    session_id: &str,
    item: &mut WorkItem,
    last_cc_session_id: &mut Option<String>,
    last_account_label: &mut Option<String>,
  ) -> anyhow::Result<()> {
    let idle = claude_idle_compaction::IdleCompaction::shared();
    idle.settle_for_turn(session_id).await?;
    self.wait_for_idle_compaction_reservation(session_id).await?;
    idle.settle_for_turn(session_id).await?;
    refresh_anchors_from_disk(item, last_cc_session_id, last_account_label)?;
    let mut result = if item.resume_record.is_some() {
      master_run::resume_cc(item).await?
    } else {
      master_run::run_cc(Arc::clone(self), item).await?
    };
    let ran_backend = result.finish_extras.get("native_backend").and_then(Value::as_str).map(str::to_string);
    arm_idle_after_round(self, item, session_id, ran_backend.clone()).await;
    if result.let_go {
      return Err(master_run::TurnLetGo.into());
    }
    if result.cancelled {
      return Err(master_run::TurnCancelled.into());
    }
    if result.finish_extras.get("zero_output").and_then(Value::as_bool) == Some(true) {
      if result.error_msg.is_none() {
        let resume_ref = result.cc_session_id.as_deref().unwrap_or("fresh session");
        let message = format!(
          "Master run produced zero model output (cc_session_id={resume_ref}): the turn settled with an all-zero usage result and no assistant text, thinking, tool use, or manual compaction. The triggering message was left unread. One known cause: opencode message-ID wraparound (LESSONS.md, 2026-08-14)."
        );
        (item.callbacks.persist_and_broadcast)(
          session_id.to_string(),
          charliebot_agents::event_builders::make_error_event(&message),
        )
        .await?;
      }
      result.exit_code = 1;
    }
    if item.task_run.fresh_native_context {
      *last_cc_session_id = None;
    }
    if let Some(cc_session_id) = result.cc_session_id.as_ref().filter(|id| !id.is_empty()) {
      {
        let mut metadata = item.session_meta.lock().expect("work item metadata mutex is not poisoned");
        metadata.cc_session_id = Some(cc_session_id.clone());
        if let Some(backend) = &ran_backend {
          metadata.native_backend = Some(backend.clone());
        }
      }
      *last_cc_session_id = Some(cc_session_id.clone());
      let read_back =
        (item.callbacks.persist_cc_session_id)(session_id.to_string(), cc_session_id.clone(), ran_backend.clone())
          .await?;
      if read_back.as_deref() != Some(cc_session_id) {
        persist_mismatch(item, "resume_anchor", "Resume anchor", cc_session_id, read_back.as_deref()).await?;
      }
    }
    let lifecycles = item.callbacks.features.registry.lifecycles();
    if let Some(account_label) = persist_shared_account_label(
      &item.session_meta,
      &lifecycles,
      session_id,
      item.callbacks.persist_account_label.as_ref(),
    )
    .await?
    {
      *last_account_label = Some(account_label.label.clone());
      if let Some(read_back) = account_label.read_back
        && read_back.as_deref() != Some(account_label.label.as_str())
      {
        persist_mismatch(item, "account", "Account label", &account_label.label, read_back.as_deref()).await?;
      }
    }

    let still_thinking = self
      .state
      .lock()
      .expect("master queue mutex is not poisoned")
      .session_queues
      .get(session_id)
      .is_some_and(|queue| !queue.is_empty());
    let thinking_seconds = if !still_thinking {
      item
        .callbacks
        .features
        .store
        .busy_since(session_id)
        .map(|since| (UtcDateTime::now().into_inner() - since.into_inner()).whole_seconds())
    } else {
      None
    };
    let mut done_event = charliebot_agents::event_builders::make_master_done_event(result.exit_code, still_thinking);
    let done_object = done_event.as_object_mut().expect("master_done builder returns an object");
    if !item.user_event_ids.is_empty() {
      done_object.insert(event_types::INPUT_EVENT_IDS.to_string(), json!(item.user_event_ids));
    }
    if let Some(seconds) = thinking_seconds {
      done_object.insert(event_types::THINKING_SECONDS.to_string(), json!(seconds));
    }
    done_object.extend(result.finish_extras.clone());
    let finish_handled = (item.on_task_finish)(
      result.cc_session_id.clone(),
      result.exit_code,
      result.finish_extras.clone(),
      done_event.clone(),
    )
    .await?;
    if !finish_handled {
      (item.callbacks.persist_and_broadcast)(session_id.to_string(), done_event).await?;
    }
    if let Some(completion) = item.completion.take() {
      let _ = completion.send(Ok(result.cc_session_id.clone()));
    }
    if let Some(after_round) = &item.callbacks.after_round {
      let after_round = Arc::clone(after_round);
      let cfg = Arc::clone(&item.cfg);
      let session_id = session_id.to_string();
      tokio::spawn(async move {
        if let Err(error) = after_round(cfg, session_id.clone()).await {
          tracing::error!(target: "charliebot_tasks", task = %format!("after-round-{session_id}"), error = %error, "after-turn contribution failed");
        }
      });
    }
    Ok(())
  }
}

/// Construct callbacks over the existing session store and an injected task-tree activity read.
pub fn session_callbacks(
  queue: &Arc<MasterQueue>,
  features: Arc<FeatureContext>,
  task_tree_activity: TaskTreeActivityFn,
) -> SessionCallbacks {
  queue.session_callbacks(features, task_tree_activity)
}

/// Arm the session's idle compaction when a round ends on any exit, as Python's after_round does in a finally.
async fn arm_idle_after_round(
  queue: &Arc<MasterQueue>,
  item: &WorkItem,
  session_id: &str,
  ran_backend: Option<String>,
) {
  let Some(backend_id) = ran_backend else {
    claude_idle_compaction::IdleCompaction::shared().disarm(session_id);
    return;
  };
  let armed = arm_idle_compaction(
    Arc::clone(&item.callbacks.features),
    Arc::downgrade(queue),
    Arc::clone(&item.cfg),
    session_id.to_string(),
    backend_id,
    Arc::clone(&item.callbacks.persist_and_broadcast),
  )
  .await;
  if let Err(error) = armed {
    tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "claude_idle_compaction_arm_failed");
  }
}

async fn arm_idle_compaction(
  features: Arc<FeatureContext>,
  queue: std::sync::Weak<MasterQueue>,
  cfg: Arc<CharlieBotConfig>,
  session_id: String,
  backend_id: String,
  persist_and_broadcast: PersistAndBroadcastFn,
) -> anyhow::Result<()> {
  let Some(option) = cfg.get_backend_option(&backend_id).cloned() else {
    claude_idle_compaction::IdleCompaction::shared().disarm(&session_id);
    return Ok(());
  };
  if option.backend_type != "cc-claude" {
    claude_idle_compaction::IdleCompaction::shared().disarm(&session_id);
    return Ok(());
  }
  let Some(metadata) = features.store.read_metadata_fresh(&session_id)? else {
    anyhow::bail!("session {session_id} disappeared before idle compaction was armed");
  };
  let usage = features.startup.require::<SessionUsage>()?;
  let (_, last_request_at) = usage.context_state(&session_id, &metadata)?;
  let idle = claude_idle_compaction::IdleCompaction::shared();
  idle.disarm(&session_id);
  let Some(armed_at) = last_request_at else {
    return Ok(());
  };
  let compaction =
    queue.upgrade().context("master queue disappeared before idle compaction was armed")?.compaction.clone();
  let base_environment = features.startup.require::<AgentBaseEnvironment>()?;
  let mut base_env = base_environment.values.clone();
  base_env.insert("GIT_CEILING_DIRECTORIES".to_string(), cfg.charliebot_home.to_string_lossy().into_owned());
  let cwd = cfg.sessions_dir().join(&session_id);
  let emit_session_id = session_id.clone();
  let emit: charliebot_registry::backend::EmitFn = Arc::new(move |event| {
    let persist = Arc::clone(&persist_and_broadcast);
    let session_id = emit_session_id.clone();
    Box::pin(async move { persist(session_id, event).await })
  });
  let inspect_features = Arc::clone(&features);
  let inspect_option = option.clone();
  let inspect_registry = Arc::clone(&features.registry);
  let inspect_session_id = session_id.clone();
  let inspect_queue = queue.clone();
  let inspect_env = base_env.clone();
  let inspect_cwd = cwd.clone();
  let inspect_emit = Arc::clone(&emit);
  let inspect_armed_at = armed_at;
  let inspect_compaction = Arc::clone(&compaction);
  let reservation_slot = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
  let inspect_reservation_slot = Arc::clone(&reservation_slot);
  let inspect = Arc::new(move |_now| {
    let Some(metadata) = inspect_features.store.read_metadata_fresh(&inspect_session_id)? else {
      return Ok(None);
    };
    let current_cfg = inspect_features.config.get().context("loading config.yaml for idle Claude compaction")?;
    let usage = inspect_features.startup.require::<SessionUsage>()?;
    let (context_tokens, last_request_at) = usage.context_state(&inspect_session_id, &metadata)?;
    let compacted_after_request = if let Some(last_request_at) = last_request_at {
      // Python scans its cached history in asyncio.to_thread (claude_idle_compaction.py:176-195).
      let store = Arc::clone(&inspect_features.store);
      let session_id = inspect_session_id.clone();
      tokio::task::block_in_place(move || {
        let events = store.load_chat_events_shared_sync(&session_id)?;
        let newest_compaction = events
          .iter()
          .rev()
          .find(|event| {
            event.get("type").and_then(CompactValue::as_str) == Some(event_types::CONTEXT_COMPACTED)
              && event.get("timestamp").and_then(CompactValue::as_str).is_some()
          })
          .and_then(|event| event.get("timestamp"))
          .and_then(CompactValue::as_str)
          .map(UtcDateTime::parse)
          .transpose()?;
        Ok::<_, anyhow::Error>(newest_compaction.is_some_and(|timestamp| timestamp > last_request_at))
      })?
    } else {
      false
    };
    let account_label = claude_metadata::account_of(&metadata);
    let default_dir = login_dirs::claude_config_dir()?;
    let account = claude_idle_compaction::account_for_session(
      &current_cfg,
      &inspect_option,
      account_label.as_deref(),
      &default_dir,
    )?;
    let resume_id = metadata.cc_session_id.clone();
    let transcript_present = match account.as_ref().zip(resume_id.as_deref()) {
      Some((account, resume_id)) => {
        !claude_accounts::transcript_matches(Path::new(&account.config_dir), resume_id)?.is_empty()
      }
      None => false,
    };
    let facts = claude_idle_compaction::IdleFacts {
      archived: metadata.status == SessionStatus::Archived,
      backend_unchanged: metadata.backend == inspect_option.id,
      turn_busy: false,
      armed_at: Some(inspect_armed_at),
      last_request_at,
      compacted_after_request,
      context_tokens,
      resume_id: resume_id.clone(),
      transcript_present,
    };
    let reservation = Arc::new(AtomicBool::new(false));
    *inspect_reservation_slot.lock().expect("idle reservation slot is not poisoned") = Some(Arc::clone(&reservation));
    let run_reservation = Arc::clone(&reservation);
    let run_queue = inspect_queue.clone();
    let run_registry = Arc::clone(&inspect_registry);
    let run_option = inspect_option.clone();
    let run_cfg = Arc::clone(&current_cfg);
    let run_account = account.clone();
    let run_session_id = inspect_session_id.clone();
    let run_cc_session_id = resume_id.clone();
    let run_cwd = inspect_cwd.clone();
    let run_env = inspect_env.clone();
    let run_emit = Arc::clone(&inspect_emit);
    let run_tokens = context_tokens;
    let compaction = Arc::clone(&inspect_compaction);
    let run: claude_idle_compaction::RunFn = Arc::new(move || {
      let reserved = Arc::clone(&run_reservation);
      let queue = run_queue.clone();
      let registry = Arc::clone(&run_registry);
      let option = run_option.clone();
      let cfg = Arc::clone(&run_cfg);
      let account = run_account.clone();
      let session_id = run_session_id.clone();
      let cc_session_id = run_cc_session_id.clone();
      let cwd = run_cwd.clone();
      let base_env = run_env.clone();
      let emit = Arc::clone(&run_emit);
      let pre_tokens = run_tokens;
      let compaction = Arc::clone(&compaction);
      Box::pin(async move {
        if reserved.swap(false, Ordering::AcqRel) {
          queue
            .upgrade()
            .context("master queue disappeared after idle compaction was reserved")?
            .release_idle_compaction_reservation(&session_id);
        }
        let account = account.expect("a compaction candidate has an account");
        let cc_session_id = cc_session_id.expect("a compaction candidate has a resume id");
        let (argv, env) = claude_idle_compaction::build_idle_process(
          registry,
          &cfg,
          &option,
          &account,
          claude_idle_compaction::IdleProcessIdentity {
            session_id: session_id.clone(),
            cc_session_id,
            run_kind: RunKind::ManagerTurn,
          },
          &base_env,
        )?;
        let run = claude_idle_compaction::idle_run_options(&option, &account, &cwd, pre_tokens, argv, env);
        let outcome = compaction(claude_compaction::CompactionRun {
          argv: run.argv,
          env: run.env,
          cwd: &run.cwd,
          config_dir: Path::new(&run.config_dir),
          session_id: &session_id,
          family: &run.family,
          model: &run.model,
          trigger: &run.trigger,
          failure_trigger: run.failure_trigger.as_deref(),
          pre_tokens: run.pre_tokens,
          emit: &emit,
          cfg: &cfg,
          session_cgroup_id: Some(&session_id),
          account_label: &account.label,
          timeout_duration: claude_compaction::CLAUDE_COMPACTION_TIMEOUT,
        })
        .await?;
        if outcome.ok {
          tracing::info!(
            target: "charliebot_tasks",
            session = %session_id,
            pre_tokens,
            post_tokens = ?outcome.post_tokens,
            cache_read_tokens = ?outcome.cache_read_tokens,
            account = account.label,
            "claude_idle_compaction_done"
          );
        }
        Ok(())
      }) as BoxFuture<'static, anyhow::Result<()>>
    });
    Ok(Some(claude_idle_compaction::IdleCandidate {
      facts,
      idle_tokens: current_cfg.section::<claude_config::AccountsConfig>("accounts").claude_compaction.idle_tokens,
      run,
    }))
  });
  let busy_queue = queue.clone();
  let is_busy: claude_idle_compaction::BusyFn = Arc::new(move |session_id| {
    let reservation = reservation_slot
      .lock()
      .expect("idle reservation slot is not poisoned")
      .clone()
      .context("idle compaction candidate has no reservation state")?;
    let queue = busy_queue.upgrade().ok_or_else(|| anyhow!("master queue disappeared before idle compaction"))?;
    let reserved = queue.reserve_idle_compaction(session_id);
    if reserved {
      reservation.store(true, Ordering::Release);
    }
    Ok(!reserved)
  });
  idle.arm(claude_idle_compaction::IdleTimerInput { session_id, armed_at, inspect, is_busy });
  Ok(())
}

/// Return the event ids queued or running for one session.
pub fn queued_user_event_ids(features: &FeatureContext, session_id: &str) -> anyhow::Result<HashSet<String>> {
  let queue = features.startup.require::<MasterQueue>()?;
  Ok(queue.queued_user_event_ids(session_id))
}

fn refresh_anchors_from_disk(
  item: &mut WorkItem,
  last_cc_session_id: &Option<String>,
  last_account_label: &Option<String>,
) -> anyhow::Result<()> {
  let session_id = item.session_metadata().id;
  let fresh = item.callbacks.features.store.read_metadata_fresh(&session_id)?;
  let mut metadata = item.session_meta.lock().expect("work item metadata mutex is not poisoned");
  if let Some(fresh) = fresh {
    if metadata.cc_session_id.is_some() {
      metadata.cc_session_id = fresh.cc_session_id.clone();
    }
    if metadata.native_backend.is_some() {
      metadata.native_backend = fresh.native_backend.clone();
    }
    if let Some(backend_id) = metadata.native_backend.as_deref()
      && let Some(lifecycle) = account_lifecycle(item, backend_id)
      && let Some(label) = lifecycle.account_label(&fresh)
    {
      lifecycle.record_account_label(&mut metadata, Some(&label));
    }
  }
  if let Some(last) = last_cc_session_id.as_ref()
    && metadata.cc_session_id.is_none()
  {
    metadata.cc_session_id = Some(last.clone());
  }
  if let Some(last) = last_account_label.as_deref()
    && let Some(backend_id) = metadata.native_backend.as_deref()
    && let Some(lifecycle) = account_lifecycle(item, backend_id)
    && lifecycle.account_label(&metadata).is_none()
  {
    lifecycle.record_account_label(&mut metadata, Some(last));
  }
  Ok(())
}

fn account_lifecycle(item: &WorkItem, backend_id: &str) -> Option<Arc<dyn BackendLifecycle>> {
  let backend_type = item
    .cfg
    .get_backend_option(backend_id)
    .map(|option| option.backend_type.as_str())
    .or_else(|| item.callbacks.features.registry.type_for_family_prefix(backend_id))?;
  item.callbacks.features.registry.lifecycle_for(backend_type).ok()
}

async fn persist_shared_account_label(
  session_meta: &Arc<Mutex<SessionMetadata>>,
  lifecycles: &[Arc<dyn BackendLifecycle>],
  session_id: &str,
  persist: Option<&PersistAccountLabelFn>,
) -> anyhow::Result<Option<PersistedAccountLabel>> {
  let metadata = session_meta.lock().expect("work item metadata mutex is not poisoned").clone();
  // The label's keeper is the first lifecycle that reads one, as in Python's account_keeper.
  let label = lifecycles.iter().find_map(|lifecycle| lifecycle.account_label(&metadata));
  let Some(label) = label.filter(|label| !label.is_empty()) else {
    return Ok(None);
  };
  let read_back =
    if let Some(persist) = persist { Some(persist(session_id.to_string(), label.clone()).await?) } else { None };
  Ok(Some(PersistedAccountLabel { label, read_back }))
}

async fn persist_mismatch(
  item: &WorkItem,
  source: &str,
  subject: &str,
  written: &str,
  read_back: Option<&str>,
) -> anyhow::Result<()> {
  let session_id = item.session_metadata().id;
  tracing::error!(target: "charliebot_tasks", session = %session_id, written, read_back, "persist_mismatch");
  let message = format!("{subject} persist mismatch: wrote {written:?}, read back {read_back:?} from disk");
  (item.callbacks.persist_and_broadcast)(
    session_id,
    json!({"type": event_types::ERROR, "source": source, "message": message}),
  )
  .await
}

async fn broadcast_running_changed(
  callbacks: &SessionCallbacks,
  session_id: &str,
  has_running_tasks: bool,
  thinking_since: Option<UtcDateTime>,
  auto_trigger: bool,
) -> anyhow::Result<()> {
  let mut fields = Map::new();
  fields.insert("has_running_tasks".to_string(), json!(has_running_tasks));
  fields.insert(
    "thinking_since".to_string(),
    thinking_since.map(|value| json!(value.to_python_isoformat())).unwrap_or(Value::Null),
  );
  fields.insert("auto_trigger".to_string(), json!(auto_trigger));
  callbacks.features.store.broadcast_sidebar(session_id, event_types::RUNNING_CHANGED, fields).await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use charliebot_agents::backends::claude_code::EXCLUDE_DYNAMIC_SECTIONS_FLAG;
  use charliebot_agents::backends::claude_code::claude_lifecycle::ClaudeCodeLifecycle;
  use charliebot_agents::backends::claude_code::claude_metadata;
  use charliebot_core::config::BackendOption;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::backend::Env;
  use charliebot_registry::backend::UsageResolver;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{Registry, RegistryError};
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::{SharedChatEvent, Store};
  use serde_json::Map;

  use super::*;
  use crate::master_state::{TaskFinishCallback, TaskRunBinding};
  use crate::test_fixtures;

  struct SyntheticUsageResolver;

  impl UsageResolver for SyntheticUsageResolver {
    fn resolve(
      &self,
      _session_id: &str,
      _meta: &SessionMetadata,
      _events: &[SharedChatEvent],
    ) -> anyhow::Result<Option<Map<String, Value>>> {
      Ok(Some(Map::from_iter([("context_tokens".to_string(), json!(321))])))
    }
  }

  fn register_synthetic_usage(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.usage_resolver("synthetic_usage", Arc::new(|_host| Arc::new(SyntheticUsageResolver)))
  }

  fn context_features(home: &Path) -> Arc<FeatureContext> {
    let mut registry = Registry::new();
    registry.register_package("tests.synthetic_usage", register_synthetic_usage).unwrap();
    let registry = Arc::new(registry);
    let mut config = registry.load_config(home).unwrap();
    config.backends.options.push(BackendOption {
      id: "synthetic-resolver".to_string(),
      label: "Synthetic resolver".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic_usage".to_string(),
      type_fields: serde_yaml::Mapping::new(),
    });
    let store_config = config.clone();
    let reload_config = config.clone();
    let config = Arc::new(HotReload::new(
      config.config_file(),
      "master_queue_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(store_config, registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap(),
    );
    let credential_home = home.to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.join("credentials.yaml"),
      "master_queue_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    startup.insert(SessionUsage::new(Arc::clone(&config), Arc::clone(&store), Arc::clone(&registry))).unwrap();
    Arc::new(FeatureContext { config, credentials, store, registry, startup })
  }

  async fn append_events(store: &Store, session_id: &str, events: &[Value]) {
    for event in events {
      store.append_chat_event(session_id, event.clone()).await.unwrap();
    }
  }

  fn manager_metadata(session_id: &str, backend: &str) -> SessionMetadata {
    let mut meta = SessionMetadata::new(session_id, "Synthetic session", TaskProfile::Manager);
    meta.backend = backend.to_string();
    meta
  }

  #[test]
  fn running_turn_snapshot_reads_the_shared_metadata_object() {
    let queue = MasterQueue::new(Arc::new(BackendPool::default()));
    let mut running = SessionMetadata::new("running", "Running", TaskProfile::Manager);
    running.native_backend = Some("fable".to_string());
    let running = Arc::new(Mutex::new(running));
    queue
      .state
      .lock()
      .expect("master queue mutex is not poisoned")
      .running_turns
      .insert("running".to_string(), Arc::clone(&running));

    let lifecycle = ClaudeCodeLifecycle::new();
    lifecycle.record_account_label(&mut running.lock().unwrap(), Some("synthetic-a"));

    let other_turns = queue.running_turns_except("observer");
    assert_eq!(other_turns.len(), 1);
    assert_eq!(claude_metadata::account_of(&other_turns[0]).as_deref(), Some("synthetic-a"));
    assert!(queue.running_turns_except("running").is_empty());
  }

  #[tokio::test]
  async fn post_turn_persists_the_lifecycle_label_from_shared_metadata() {
    let lifecycle = Arc::new(ClaudeCodeLifecycle::new());
    let lifecycles: Vec<Arc<dyn BackendLifecycle>> = vec![lifecycle.clone()];
    let session_meta = Arc::new(Mutex::new(SessionMetadata::new("running", "Running", TaskProfile::Manager)));
    lifecycle.record_account_label(&mut session_meta.lock().unwrap(), Some("synthetic-a"));
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let persist: PersistAccountLabelFn = {
      let persisted = Arc::clone(&persisted);
      Arc::new(move |session_id, label| {
        let persisted = Arc::clone(&persisted);
        Box::pin(async move {
          persisted.lock().unwrap().push((session_id, label.clone()));
          Ok(Some(label))
        })
      })
    };

    let account_label =
      persist_shared_account_label(&session_meta, &lifecycles, "running", Some(&persist)).await.unwrap().unwrap();

    assert_eq!(account_label.label, "synthetic-a");
    assert_eq!(account_label.read_back, Some(Some("synthetic-a".to_string())));
    assert_eq!(*persisted.lock().unwrap(), [("running".to_string(), "synthetic-a".to_string())]);
  }

  #[tokio::test]
  async fn context_state_callback_uses_the_resolver_and_event_fold() {
    let home = tempfile::tempdir().unwrap();
    let features = context_features(home.path());
    let queue = Arc::new(MasterQueue::new(Arc::new(BackendPool::default())));
    let callbacks = queue.session_callbacks(Arc::clone(&features), Arc::new(|_| Ok((false, String::new()))));
    let context_state = callbacks.context_state.as_ref().unwrap();

    append_events(
      &features.store,
      "resolver-session",
      &[
        json!({
          "type": "system", "subtype": "context_reading",
          "context_reading": {"context_tokens": 11},
        }),
        json!({"type": "result", "timestamp": "2026-10-18T10:00:00Z"}),
      ],
    )
    .await;
    let (context_tokens, last_request_at) =
      context_state("resolver-session".to_string(), manager_metadata("resolver-session", "synthetic-resolver"))
        .await
        .unwrap();
    assert_eq!(context_tokens, Some(321));
    assert_eq!(last_request_at.map(|value| value.to_python_string()), Some("2026-10-18T10:00:00Z".to_string()));

    append_events(
      &features.store,
      "assistant-session",
      &[
        json!({
          "type": "result", "timestamp": "2026-10-18T10:00:01Z",
          "usage": {"input_tokens": 1_000_000},
        }),
        json!({
          "type": "assistant", "timestamp": "2026-10-18T10:00:02Z", "parent_tool_use_id": "tool-1",
          "message": {"usage": {"input_tokens": 500}},
        }),
        json!({
          "type": "assistant", "timestamp": "2026-10-18T10:00:03Z",
          "message": {"usage": {"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30}},
        }),
        json!({"type": "assistant", "timestamp": "2026-10-18T10:00:04Z", "message": {"content": "hello"}}),
      ],
    )
    .await;
    let (context_tokens, last_request_at) =
      context_state("assistant-session".to_string(), manager_metadata("assistant-session", "unregistered-backend"))
        .await
        .unwrap();
    assert_eq!(context_tokens, Some(150));
    assert_eq!(last_request_at.map(|value| value.to_python_string()), Some("2026-10-18T10:00:04Z".to_string()));

    append_events(
      &features.store,
      "reading-session",
      &[
        json!({"type": "result", "usage": {"input_tokens": 1_000_000}}),
        json!({
          "type": "system", "subtype": "context_reading",
          "context_reading": {"context_tokens": 10_000},
        }),
        json!({
          "type": "system", "subtype": "context_reading",
          "context_reading": {"context_tokens": 118_234},
        }),
      ],
    )
    .await;
    let (context_tokens, _) =
      context_state("reading-session".to_string(), manager_metadata("reading-session", "unregistered-backend"))
        .await
        .unwrap();
    assert_eq!(context_tokens, Some(118_234));

    append_events(
      &features.store,
      "compact-session",
      &[
        json!({"type": "assistant", "message": {"usage": {"input_tokens": 500}}}),
        json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"post_tokens": 120}}),
        json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {}}),
      ],
    )
    .await;
    let (context_tokens, _) =
      context_state("compact-session".to_string(), manager_metadata("compact-session", "unregistered-backend"))
        .await
        .unwrap();
    assert_eq!(context_tokens, Some(120));

    append_events(
      &features.store,
      "unknown-session",
      &[
        json!({"type": "assistant", "message": {"content": "hello"}}),
        json!({"type": "result", "usage": {"input_tokens": 999_999}}),
      ],
    )
    .await;
    let (context_tokens, _) =
      context_state("unknown-session".to_string(), manager_metadata("unknown-session", "unregistered-backend"))
        .await
        .unwrap();
    assert_eq!(context_tokens, None);
  }

  /// A manager turn that starts no agent: its session pins no backend, so the queue refuses it.
  fn refused_turn(
    queue: &Arc<MasterQueue>,
    features: &Arc<FeatureContext>,
    session_id: &str,
    on_task_finish: TaskFinishCallback,
  ) -> MasterTurnRequest {
    let callbacks = SessionCallbacks {
      after_round: None,
      ..queue.session_callbacks(Arc::clone(features), Arc::new(|_| Ok((false, String::new()))))
    };
    MasterTurnRequest {
      cfg: features.config.get().unwrap(),
      session_meta: manager_metadata(session_id, ""),
      user_content: "synthetic turn".to_string(),
      callbacks,
      user_event_ids: Vec::new(),
      task_instructions: String::new(),
      task_run: TaskRunBinding {
        session_id: session_id.to_string(),
        run_id: "synthetic-run".to_string(),
        transport_dir: PathBuf::from("synthetic-transport"),
        fresh_native_context: false,
      },
      on_task_spawn: Arc::new(|_: u32, _: Option<String>| -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
      }),
      on_task_finish,
      auto_trigger: false,
      backend_option: None,
      extra_claude_flags: None,
      uploaded_files: None,
      extra_env: None,
    }
  }

  /// A finish callback that records whether the turn's session was busy when the turn finished.
  fn busy_at_finish(
    features: &Arc<FeatureContext>,
    session_id: &str,
    observed: Arc<Mutex<Vec<bool>>>,
  ) -> TaskFinishCallback {
    let features = Arc::clone(features);
    let session_id = session_id.to_string();
    Arc::new(move |_: Option<String>, _: i32, _: Map<String, Value>, _: Value| {
      let features = Arc::clone(&features);
      let session_id = session_id.clone();
      let observed = Arc::clone(&observed);
      Box::pin(async move {
        observed.lock().unwrap().push(features.store.busy_since(&session_id).is_some());
        Ok(false)
      })
    })
  }

  /// Whether the session's busy interval closes within one second. A consumer that never retires keeps it open.
  async fn busy_interval_closes(features: &FeatureContext, session_id: &str) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while features.store.busy_since(session_id).is_some() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
      }
    })
    .await
    .is_ok()
  }

  #[tokio::test]
  async fn retirement_runs_task_tree_activity_without_the_queue_mutex() {
    let home = tempfile::tempdir().unwrap();
    let features = context_features(home.path());
    let queue = Arc::new(MasterQueue::new(Arc::new(BackendPool::default())));
    let queue_ref = Arc::downgrade(&queue);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let activity_calls = Arc::clone(&calls);
    let activity: TaskTreeActivityFn = Arc::new(move |_session_id| {
      let queue = queue_ref.upgrade().unwrap();
      let _state = queue.state.try_lock().expect("activity callback runs outside the queue mutex");
      activity_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      Ok((false, String::new()))
    });
    let turn = (queue.session_callbacks(features, activity), false);

    let (callbacks, auto_trigger, active) = queue.retire("synthetic-retire", &turn);

    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(!active.unwrap());
    assert!(!auto_trigger);
    assert_eq!(callbacks.features.store.busy_since("synthetic-retire"), None);
  }

  #[tokio::test]
  async fn a_drained_manager_turn_clears_the_busy_state_it_opened() {
    let home = tempfile::tempdir().unwrap();
    let features = context_features(home.path());
    let queue = Arc::new(MasterQueue::new(Arc::new(BackendPool::default())));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let on_finish = busy_at_finish(&features, "drained-session", Arc::clone(&observed));

    queue.run_message(refused_turn(&queue, &features, "drained-session", on_finish)).await.unwrap();

    assert!(busy_interval_closes(&features, "drained-session").await);
    assert_eq!(*observed.lock().unwrap(), [true]);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_enqueue_racing_the_consumer_teardown_runs_once_and_marks_busy() {
    let home = tempfile::tempdir().unwrap();
    let features = context_features(home.path());
    let queue = Arc::new(MasterQueue::new(Arc::new(BackendPool::default())));
    for delay in 0..8 {
      let session_id = format!("race-session-{delay}");
      let first_observed = Arc::new(Mutex::new(Vec::new()));
      let second_observed = Arc::new(Mutex::new(Vec::new()));
      let first = refused_turn(
        &queue,
        &features,
        &session_id,
        busy_at_finish(&features, &session_id, Arc::clone(&first_observed)),
      );
      let second = refused_turn(
        &queue,
        &features,
        &session_id,
        busy_at_finish(&features, &session_id, Arc::clone(&second_observed)),
      );

      let first_queue = Arc::clone(&queue);
      let first_run = tokio::spawn(async move { first_queue.run_message(first).await });
      for _ in 0..delay {
        tokio::task::yield_now().await;
      }
      let second_queue = Arc::clone(&queue);
      let second_run = tokio::spawn(async move { second_queue.run_message(second).await });
      first_run.await.unwrap().unwrap();
      second_run.await.unwrap().unwrap();

      assert_eq!(*first_observed.lock().unwrap(), [true], "delay {delay}");
      assert_eq!(*second_observed.lock().unwrap(), [true], "delay {delay}");
      assert!(busy_interval_closes(&features, &session_id).await, "delay {delay}");
    }
  }

  const CC_SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

  /// One idle compaction the fake runner received, with the fields the trigger path passes.
  #[derive(Clone, Debug)]
  struct RecordedRun {
    argv: Vec<String>,
    env: Env,
    cwd: PathBuf,
    config_dir: PathBuf,
    account_label: String,
    pre_tokens: Option<i64>,
    trigger: String,
    failure_trigger: Option<String>,
    model: String,
    family: String,
  }

  /// A compaction runner that records each run and reports a successful compaction.
  fn recording_runner(runs: Arc<Mutex<Vec<RecordedRun>>>) -> CompactionRunFn {
    runner(move |run| {
      runs.lock().unwrap().push(RecordedRun {
        argv: run.argv.clone(),
        env: run.env.clone(),
        cwd: run.cwd.to_path_buf(),
        config_dir: run.config_dir.to_path_buf(),
        account_label: run.account_label.to_string(),
        pre_tokens: run.pre_tokens,
        trigger: run.trigger.to_string(),
        failure_trigger: run.failure_trigger.map(str::to_string),
        model: run.model.to_string(),
        family: run.family.to_string(),
      });
      Box::pin(async {
        Ok(claude_compaction::CompactionOutcome {
          ok: true,
          error: None,
          models: Vec::new(),
          post_tokens: Some(9_111),
          cache_read_tokens: Some(118_000),
        })
      })
    })
  }

  /// Wrap a closure as a compaction runner; the bound gives the closure its lifetime-generic signature.
  fn runner<F>(run: F) -> CompactionRunFn
  where
    F: for<'a> Fn(
        claude_compaction::CompactionRun<'a>,
      ) -> BoxFuture<'a, anyhow::Result<claude_compaction::CompactionOutcome>>
      + Send
      + Sync
      + 'static,
  {
    Arc::new(run)
  }

  /// Arm the trigger path for a session's last round, as `arm_idle_after_round` does at the round's end.
  async fn arm_round(queue: &Arc<MasterQueue>, features: &Arc<FeatureContext>, session_id: &str, backend: &str) {
    let persist: PersistAndBroadcastFn =
      Arc::new(|_: String, _: Value| -> BoxFuture<'static, anyhow::Result<()>> { Box::pin(async { Ok(()) }) });
    arm_idle_compaction(
      Arc::clone(features),
      Arc::downgrade(queue),
      features.config.get().unwrap(),
      session_id.to_string(),
      backend.to_string(),
      persist,
    )
    .await
    .unwrap();
  }

  // Python: tests/test_claude_idle_compaction.py:280.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_turn_queued_before_the_trigger_registers_makes_it_skip() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    test_fixtures::write_transcript(&account_dir, CC_SESSION_ID);
    let features = test_fixtures::claude_features(
      &root.path().join("home"),
      &test_fixtures::claude_config(Some(account_dir.as_path())),
    );
    let meta = test_fixtures::manager_session(
      &features,
      "a_turn_queued_before_the_trigger_registers_makes_it_skip",
      "fable",
      Some("acct-a"),
      CC_SESSION_ID,
    )
    .await;
    let request = UtcDateTime::now().to_python_string();
    test_fixtures::append_reading_and_request(&features, &meta.id, 120_000, &request).await;
    let runs = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(MasterQueue::with_compaction_runner(
      Arc::new(BackendPool::default()),
      recording_runner(Arc::clone(&runs)),
    ));
    arm_round(&queue, &features, &meta.id, "fable").await;
    let queued = test_fixtures::work_item(&queue, &features, meta.clone());
    queue.state.lock().unwrap().session_queues.entry(meta.id.clone()).or_default().push_back(queued);

    let started = claude_idle_compaction::IdleCompaction::shared().fire_at(&meta.id, UtcDateTime::now()).unwrap();

    assert!(started.is_none());
    assert!(runs.lock().unwrap().is_empty());
    assert!(!claude_idle_compaction::IdleCompaction::shared().is_running(&meta.id));
  }

  // Python: tests/test_claude_idle_compaction.py:309.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_trigger_path_starts_one_idle_run_on_the_sessions_own_command_line() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    test_fixtures::write_transcript(&account_dir, CC_SESSION_ID);
    let features = test_fixtures::claude_features(
      &root.path().join("home"),
      &test_fixtures::claude_config(Some(account_dir.as_path())),
    );
    let meta = test_fixtures::manager_session(
      &features,
      "the_trigger_path_starts_one_idle_run_on_the_sessions_own_command_line",
      "fable",
      Some("acct-a"),
      CC_SESSION_ID,
    )
    .await;
    let request = UtcDateTime::now().to_python_string();
    test_fixtures::append_reading_and_request(&features, &meta.id, 120_000, &request).await;
    let runs = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(MasterQueue::with_compaction_runner(
      Arc::new(BackendPool::default()),
      recording_runner(Arc::clone(&runs)),
    ));
    arm_round(&queue, &features, &meta.id, "fable").await;

    let started = claude_idle_compaction::IdleCompaction::shared()
      .fire_at(&meta.id, UtcDateTime::now())
      .unwrap()
      .expect("every idle condition holds");
    started.await.unwrap();

    let [run]: [RecordedRun; 1] = runs.lock().unwrap().clone().try_into().unwrap();
    let cfg = features.config.get().unwrap();
    // The command line resumes the session's Claude session and keeps the per-machine sections out of the prompt.
    assert!(run.argv.windows(2).any(|pair| pair[0] == "--resume" && pair[1] == CC_SESSION_ID));
    assert!(run.argv.iter().any(|arg| arg == EXCLUDE_DYNAMIC_SECTIONS_FLAG));
    assert!(run.argv.windows(2).any(|pair| pair[0] == "--model" && pair[1] == "claude-fable-5-1"));
    assert_eq!(run.cwd, cfg.sessions_dir().join(&meta.id));
    assert_eq!(run.config_dir, account_dir);
    assert_eq!(run.pre_tokens, Some(120_000));
    assert_eq!((run.trigger.as_str(), run.failure_trigger.as_deref()), ("idle", Some("idle")));
    assert_eq!((run.model.as_str(), run.family.as_str()), ("claude-fable-5-1", "fable"));
    assert!(!claude_idle_compaction::IdleCompaction::shared().is_running(&meta.id));
  }

  // Python: tests/test_claude_idle_compaction.py:341.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_unpooled_session_uses_its_default_login_directory() {
    let root = tempfile::tempdir().unwrap();
    let default_dir = root.path().join("default-claude");
    test_fixtures::write_transcript(&default_dir, CC_SESSION_ID);
    let _login = ClaudeConfigDirGuard::set(&default_dir);
    let features = test_fixtures::claude_features(&root.path().join("home"), &test_fixtures::claude_config(None));
    let meta = test_fixtures::manager_session(
      &features,
      "an_unpooled_session_uses_its_default_login_directory",
      "fable",
      None,
      CC_SESSION_ID,
    )
    .await;
    let request = UtcDateTime::now().to_python_string();
    test_fixtures::append_reading_and_request(&features, &meta.id, 120_000, &request).await;
    let runs = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(MasterQueue::with_compaction_runner(
      Arc::new(BackendPool::default()),
      recording_runner(Arc::clone(&runs)),
    ));
    arm_round(&queue, &features, &meta.id, "fable").await;

    let started = claude_idle_compaction::IdleCompaction::shared()
      .fire_at(&meta.id, UtcDateTime::now())
      .unwrap()
      .expect("every idle condition holds");
    started.await.unwrap();

    let [run]: [RecordedRun; 1] = runs.lock().unwrap().clone().try_into().unwrap();
    assert_eq!(run.config_dir, default_dir);
    assert_eq!(run.env.get(login_dirs::CLAUDE_CONFIG_DIR_ENV), Some(&default_dir.to_string_lossy().into_owned()));
    assert_eq!(run.account_label, "default");
  }

  fn panicking_run<'a>(
    _run: claude_compaction::CompactionRun<'a>,
  ) -> BoxFuture<'a, anyhow::Result<claude_compaction::CompactionOutcome>> {
    panic!("synthetic compaction panic")
  }

  // Python: src/backends/claude_code/claude_idle_compaction.py:_compact, whose finally pops the running entry.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_panicking_idle_run_leaves_no_running_entry_and_no_start_reservation() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    test_fixtures::write_transcript(&account_dir, CC_SESSION_ID);
    let features = test_fixtures::claude_features(
      &root.path().join("home"),
      &test_fixtures::claude_config(Some(account_dir.as_path())),
    );
    let meta = test_fixtures::manager_session(
      &features,
      "a_panicking_idle_run_leaves_no_running_entry_and_no_start_reservation",
      "fable",
      Some("acct-a"),
      CC_SESSION_ID,
    )
    .await;
    let request = UtcDateTime::now().to_python_string();
    test_fixtures::append_reading_and_request(&features, &meta.id, 120_000, &request).await;
    let queue = Arc::new(MasterQueue::with_compaction_runner(Arc::new(BackendPool::default()), runner(panicking_run)));
    arm_round(&queue, &features, &meta.id, "fable").await;

    let started = claude_idle_compaction::IdleCompaction::shared()
      .fire_at(&meta.id, UtcDateTime::now())
      .unwrap()
      .expect("every idle condition holds");
    let finished = started.await;

    assert!(finished.is_ok(), "the compaction task ends normally: {finished:?}");
    assert!(!claude_idle_compaction::IdleCompaction::shared().is_running(&meta.id));
    assert!(queue.reserve_idle_compaction(&meta.id), "the start reservation is released");
  }

  /// Sets `CLAUDE_CONFIG_DIR` for one test: the trigger path reads the process login directory from it.
  struct ClaudeConfigDirGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
  }

  static CLAUDE_CONFIG_DIR_LOCK: Mutex<()> = Mutex::new(());

  impl ClaudeConfigDirGuard {
    fn set(dir: &Path) -> Self {
      let lock = CLAUDE_CONFIG_DIR_LOCK.lock().expect("the login directory lock is not poisoned");
      let previous = std::env::var_os(login_dirs::CLAUDE_CONFIG_DIR_ENV);
      // SAFETY: every test that sets this variable holds CLAUDE_CONFIG_DIR_LOCK.
      unsafe { std::env::set_var(login_dirs::CLAUDE_CONFIG_DIR_ENV, dir) };
      Self { _lock: lock, previous }
    }
  }

  impl Drop for ClaudeConfigDirGuard {
    fn drop(&mut self) {
      // SAFETY: the guard still holds CLAUDE_CONFIG_DIR_LOCK here.
      match &self.previous {
        Some(value) => unsafe { std::env::set_var(login_dirs::CLAUDE_CONFIG_DIR_ENV, value) },
        None => unsafe { std::env::remove_var(login_dirs::CLAUDE_CONFIG_DIR_ENV) },
      }
    }
  }
}
