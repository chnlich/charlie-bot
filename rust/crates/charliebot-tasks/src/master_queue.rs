//! Per-session queue for serialized manager turns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use charliebot_agents::runner::AgentRunner;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::backend::BackendLifecycle;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;

use crate::autonamer::{self, LightBackendPool, RotatingLightBackendPool};
use crate::master_run;
use crate::master_state::{MasterResumeRequest, MasterTurnRequest, WorkItem};

pub type PersistAndBroadcastFn = Arc<dyn Fn(String, Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type MarkUnreadFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type PersistNativeIdFn =
  Arc<dyn Fn(String, String, Option<String>) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type PersistAccountLabelFn =
  Arc<dyn Fn(String, String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type ContextStateFn =
  Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>> + Send + Sync>;
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
  active_procs: HashMap<String, Arc<AgentRunner>>,
  shutdown: bool,
}

/// Owns per-session queues, consumers, current work items, and active backend runners.
pub struct MasterQueue {
  state: Mutex<QueueState>,
  light_backend_pool: Arc<dyn LightBackendPool>,
}

impl Default for MasterQueue {
  fn default() -> Self {
    Self::new()
  }
}

impl MasterQueue {
  pub fn new() -> Self {
    Self::with_light_backend_pool(Arc::new(RotatingLightBackendPool::default()))
  }

  pub fn with_light_backend_pool(pool: Arc<dyn LightBackendPool>) -> Self {
    Self { state: Mutex::new(QueueState::default()), light_backend_pool: pool }
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
    let context_state = Arc::new(move |session_id: String| {
      let features = Arc::clone(&context_features);
      Box::pin(async move { Ok(features.store.context_state(&session_id)?) })
        as BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>>
    });
    let after_features = Arc::clone(&features);
    let pool = Arc::clone(&self.light_backend_pool);
    let after_round = Arc::new(move |cfg: Arc<CharlieBotConfig>, session_id: String| {
      let features = Arc::clone(&after_features);
      let pool = Arc::clone(&pool);
      Box::pin(async move { autonamer::name_after_round(&features, &cfg, &session_id, pool.as_ref()).await })
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
      session_meta,
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
      session_meta,
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
    let session_id = item.session_meta.id.clone();
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
      self.state.lock().expect("master queue mutex is not poisoned").current_items.remove(&session_id);
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
    &self,
    session_id: &str,
    item: &mut WorkItem,
    last_cc_session_id: &mut Option<String>,
    last_account_label: &mut Option<String>,
  ) -> anyhow::Result<()> {
    refresh_anchors_from_disk(item, last_cc_session_id, last_account_label).await?;
    let mut result = if item.resume_record.is_some() {
      master_run::resume_cc(item).await?
    } else {
      master_run::run_cc(self, item).await?
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
      item.session_meta.cc_session_id = Some(cc_session_id.clone());
      if let Some(backend) = &ran_backend {
        item.session_meta.native_backend = Some(backend.clone());
      }
      *last_cc_session_id = Some(cc_session_id.clone());
      let read_back =
        (item.callbacks.persist_cc_session_id)(session_id.to_string(), cc_session_id.clone(), ran_backend.clone())
          .await?;
      if read_back.as_deref() != Some(cc_session_id) {
        persist_mismatch(item, "resume_anchor", "Resume anchor", cc_session_id, read_back.as_deref()).await?;
      }
    }
    if let Some(account_label) = result.account_label.take() {
      *last_account_label = Some(account_label.clone());
      if let Some(backend_id) = ran_backend.as_deref()
        && let Some(lifecycle) = account_lifecycle(item, backend_id)
      {
        lifecycle.record_account_label(&mut item.session_meta, Some(&account_label));
      }
      if let Some(persist) = &item.callbacks.persist_account_label {
        let read_back = persist(session_id.to_string(), account_label.clone()).await?;
        if read_back.as_deref() != Some(account_label.as_str()) {
          persist_mismatch(item, "account", "Account label", &account_label, read_back.as_deref()).await?;
        }
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

async fn refresh_anchors_from_disk(
  item: &mut WorkItem,
  last_cc_session_id: &Option<String>,
  last_account_label: &Option<String>,
) -> anyhow::Result<()> {
  let fresh = item.callbacks.features.store.read_metadata_fresh(&item.session_meta.id)?;
  if let Some(fresh) = fresh {
    if item.session_meta.cc_session_id.is_some() {
      item.session_meta.cc_session_id = fresh.cc_session_id.clone();
    }
    if item.session_meta.native_backend.is_some() {
      item.session_meta.native_backend = fresh.native_backend.clone();
    }
    if let Some(backend_id) = item.session_meta.native_backend.as_deref()
      && let Some(lifecycle) = account_lifecycle(item, backend_id)
      && let Some(label) = lifecycle.account_label(&fresh)
    {
      lifecycle.record_account_label(&mut item.session_meta, Some(&label));
    }
  }
  if let Some(last) = last_cc_session_id.as_ref()
    && item.session_meta.cc_session_id.is_none()
  {
    item.session_meta.cc_session_id = Some(last.clone());
  }
  if let Some(last) = last_account_label.as_deref()
    && let Some(backend_id) = item.session_meta.native_backend.as_deref()
    && let Some(lifecycle) = account_lifecycle(item, backend_id)
    && lifecycle.account_label(&item.session_meta).is_none()
  {
    lifecycle.record_account_label(&mut item.session_meta, Some(last));
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

async fn persist_mismatch(
  item: &WorkItem,
  source: &str,
  subject: &str,
  written: &str,
  read_back: Option<&str>,
) -> anyhow::Result<()> {
  tracing::error!(target: "charliebot_tasks", session = %item.session_meta.id, written, read_back, "persist_mismatch");
  let message = format!("{subject} persist mismatch: wrote {written:?}, read back {read_back:?} from disk");
  (item.callbacks.persist_and_broadcast)(
    item.session_meta.id.clone(),
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
