//! Per-session queue for serialized manager turns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::runner::AgentRunner;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::backend::BackendLifecycle;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_usage::SessionUsage;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, oneshot, watch};
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
  session_notifies: HashMap<String, Arc<Notify>>,
  session_consumers: HashMap<String, JoinHandle<()>>,
  consumer_cancel: HashMap<String, watch::Sender<bool>>,
  current_items: HashMap<String, Vec<String>>,
  running_turns: HashMap<String, Arc<Mutex<SessionMetadata>>>,
  active_procs: HashMap<String, Arc<AgentRunner>>,
  shutdown: bool,
}

/// Owns per-session queues, consumers, current work items, and active backend runners.
pub struct MasterQueue {
  state: Mutex<QueueState>,
  backend_pool: Arc<BackendPool>,
}

struct PersistedAccountLabel {
  label: String,
  read_back: Option<Option<String>>,
}

impl MasterQueue {
  pub fn new(backend_pool: Arc<BackendPool>) -> Self {
    Self { state: Mutex::new(QueueState::default()), backend_pool }
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
      let notify =
        Arc::clone(state.session_notifies.entry(session_id.clone()).or_insert_with(|| Arc::new(Notify::new())));
      state.session_queues.entry(session_id.clone()).or_default().push_back(item);
      notify.notify_one();
      if !state.session_consumers.contains_key(&session_id) {
        let (cancel_sender, cancel) = watch::channel(false);
        state.consumer_cancel.insert(session_id.clone(), cancel_sender);
        let queue = Arc::clone(self);
        let task_session = session_id.clone();
        let handle = tokio::spawn(async move { queue.session_consumer(task_session, notify, cancel).await });
        state.session_consumers.insert(session_id.clone(), handle);
      }
      (thinking_since, created)
    };
    if created {
      broadcast_running_changed(&callbacks, &session_id, true, Some(thinking_since), auto_trigger).await?;
    }
    Ok(())
  }

  async fn session_consumer(
    self: Arc<Self>,
    session_id: String,
    notify: Arc<Notify>,
    mut cancel: watch::Receiver<bool>,
  ) {
    let mut last_cc_session_id: Option<String> = None;
    let mut last_account_label: Option<String> = None;
    let mut teardown: Option<(SessionCallbacks, bool)> = None;
    loop {
      let notified = notify.notified();
      let item = {
        let mut state = self.state.lock().expect("master queue mutex is not poisoned");
        let item = state.session_queues.get_mut(&session_id).and_then(VecDeque::pop_front);
        if let Some(item) = &item {
          state.current_items.insert(session_id.clone(), item.user_event_ids.clone());
          state.running_turns.insert(session_id.clone(), Arc::clone(&item.session_meta));
          teardown = Some((item.callbacks.clone(), item.auto_trigger));
        }
        item
      };
      let Some(mut item) = item else {
        if *cancel.borrow() {
          break;
        }
        tokio::select! {
          _ = notified => continue,
          changed = cancel.changed() => {
            if changed.is_err() || *cancel.borrow() { break; }
          }
        }
        continue;
      };
      item.cancel = cancel.clone();

      let outcome = self.process_item(&session_id, &mut item, &mut last_cc_session_id, &mut last_account_label).await;
      {
        let mut state = self.state.lock().expect("master queue mutex is not poisoned");
        state.current_items.remove(&session_id);
        state.running_turns.remove(&session_id);
      }
      if let Err(error) = outcome {
        tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "session_consumer_item_error");
        if let Some(completion) = item.completion.take() {
          let _ = completion.send(Err(error));
        }
      } else if *cancel.borrow() {
        if let Some(completion) = item.completion.take() {
          let _ = completion.send(Err(anyhow!("manager turn cancelled during server shutdown")));
        }
        break;
      }
      if *cancel.borrow() {
        break;
      }
    }

    let teardown = {
      let mut state = self.state.lock().expect("master queue mutex is not poisoned");
      state.session_consumers.remove(&session_id);
      state.consumer_cancel.remove(&session_id);
      state.current_items.remove(&session_id);
      state.running_turns.remove(&session_id);
      let queued = state.session_queues.remove(&session_id).unwrap_or_default();
      state.session_notifies.remove(&session_id);
      for mut item in queued {
        if let Some(completion) = item.completion.take() {
          let _ = completion.send(Err(anyhow!("manager turn cancelled during server shutdown")));
        }
      }
      if let Some((callbacks, auto_trigger)) = teardown {
        callbacks.features.store.clear_busy(&session_id);
        let active = (callbacks.task_tree_activity)(&session_id).map(|(active, _)| active);
        Some((callbacks, auto_trigger, active))
      } else {
        None
      }
    };
    self.state.lock().expect("master queue mutex is not poisoned").active_procs.remove(&session_id);
    if let Some((callbacks, auto_trigger, active)) = teardown {
      if let Err(error) = &active {
        tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "task_tree_activity_failed");
      }
      let has_running_tasks = active.unwrap_or(false);
      if let Err(error) =
        broadcast_running_changed(&callbacks, &session_id, has_running_tasks, None, auto_trigger).await
      {
        tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "master_queue_running_changed_failed");
      }
    }
  }

  async fn process_item(
    self: &Arc<Self>,
    session_id: &str,
    item: &mut WorkItem,
    last_cc_session_id: &mut Option<String>,
    last_account_label: &mut Option<String>,
  ) -> anyhow::Result<()> {
    refresh_anchors_from_disk(item, last_cc_session_id, last_account_label)?;
    let mut result = if item.resume_record.is_some() {
      master_run::resume_cc(item).await?
    } else {
      master_run::run_cc(Arc::clone(self), item).await?
    };
    if result.cancelled {
      return Err(anyhow!("manager turn cancelled"));
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
    let ran_backend = result.finish_extras.get("native_backend").and_then(Value::as_str).map(str::to_string);
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
    (item.callbacks.persist_and_broadcast)(session_id.to_string(), done_event).await?;
    (item.on_task_finish)(result.cc_session_id.clone(), result.exit_code, result.finish_extras.clone()).await?;
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
  use std::path::Path;

  use charliebot_agents::backends::claude_code::claude_lifecycle::ClaudeCodeLifecycle;
  use charliebot_agents::backends::claude_code::claude_metadata;
  use charliebot_core::config::BackendOption;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::backend::UsageResolver;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{Registry, RegistryError};
  use charliebot_store::Store;
  use charliebot_store::metadata::TaskProfile;
  use serde_json::Map;

  use super::*;

  struct SyntheticUsageResolver;

  impl UsageResolver for SyntheticUsageResolver {
    fn resolve(
      &self,
      _session_id: &str,
      _meta: &SessionMetadata,
      _events: &[Value],
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
}
