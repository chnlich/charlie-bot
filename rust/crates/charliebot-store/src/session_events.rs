use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};

use charliebot_core::event_types;

use crate::aggregator::MessageAggregator;
use crate::compact::CompactValue;
use crate::projection::MessageProjection;
use crate::streaming::{self, SocketHandle, Subscription};
use crate::{ChatEvent, Store, StoreError, StoreResult};

const PROJECTION_LRU_LIMIT: usize = 64;
const AGGREGATOR_INIT_SLICE_EVENTS: usize = 256;

/// Raw event types whose rendered content is carried by message or stream deltas.
pub const RAW_EVENTS_REPLACED_BY_DELTAS: &[&str] = &[
  "assistant",
  "user",
  "scheduled_trigger",
  "child_report",
  "task_closed",
  "task_reopened",
  "web_reply",
  "web_reply_notice",
  "web_status",
  "web_status_confirmed",
  "web_ask_check",
];

/// Per-session aggregator and message-projection state.
pub(crate) struct SessionEventState {
  aggregators: StdMutex<HashMap<String, Arc<Mutex<MessageAggregator>>>>,
  init_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  event_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  epochs: StdMutex<HashMap<String, u64>>,
  projections: StdMutex<ProjectionCache>,
}

impl Default for SessionEventState {
  fn default() -> Self {
    Self {
      aggregators: StdMutex::new(HashMap::new()),
      init_locks: StdMutex::new(HashMap::new()),
      event_locks: StdMutex::new(HashMap::new()),
      epochs: StdMutex::new(HashMap::new()),
      projections: StdMutex::new(ProjectionCache::default()),
    }
  }
}

#[derive(Default)]
struct ProjectionCache {
  values: HashMap<String, Arc<MessageProjection>>,
  order: VecDeque<String>,
}

impl ProjectionCache {
  fn get(&mut self, session_id: &str) -> Option<Arc<MessageProjection>> {
    let value = self.values.get(session_id).cloned()?;
    self.order.retain(|id| id != session_id);
    self.order.push_back(session_id.to_string());
    Some(value)
  }

  fn store(&mut self, session_id: &str, projection: Arc<MessageProjection>) {
    self.order.retain(|id| id != session_id);
    self.order.push_back(session_id.to_string());
    self.values.insert(session_id.to_string(), projection);
    while self.order.len() > PROJECTION_LRU_LIMIT {
      let oldest = self.order.pop_front().expect("projection cache order is non-empty");
      self.values.remove(&oldest);
    }
  }

  fn remove(&mut self, session_id: &str) {
    self.values.remove(session_id);
    self.order.retain(|id| id != session_id);
  }
}

impl Store {
  /// Append an event, aggregate its message frames, then broadcast raw side-effect events.
  pub async fn persist_and_broadcast(&self, session_id: &str, event: Value) -> StoreResult<ChatEvent> {
    let _event_guard = self.event_lock(session_id).await;
    let aggregator = self.get_or_init_aggregator(session_id).await?;
    let meta = self.get_session(session_id).await?;
    let appended = self.append_chat_event(session_id, event).await?;
    let mut event = appended;
    self.stamp_event_index(session_id, &mut event)?;
    self.feed_and_broadcast(session_id, &event, &aggregator).await?;

    if event.get("type").and_then(CompactValue::as_str) == Some(event_types::MASTER_DONE)
      && let Some(meta) = meta
    {
      for work in self.runtime_hooks.after_turn(meta, event.to_value(), self.cfg.clone()) {
        let session_id = session_id.to_string();
        let name = format!("after-turn-{}-{session_id}", work.name);
        tokio::spawn(async move {
          if let Err(error) = work.future.await {
            tracing::error!(task_name = %name, error = %error, "background_task_failed");
          }
        });
      }
    }
    Ok(event)
  }

  /// Ensure the live aggregator is caught up before a caller appends under the control lock.
  pub async fn prime_aggregator(&self, session_id: &str) -> StoreResult<u64> {
    self.get_or_init_aggregator(session_id).await?;
    Ok(self.epoch(session_id))
  }

  /// Announce an event that was appended before this call.
  pub async fn announce_appended_event(&self, session_id: &str, event: &ChatEvent, epoch: u64) -> StoreResult<()> {
    let _event_guard = self.event_lock(session_id).await;
    let aggregator = {
      let aggregators = self.session_events.aggregators.lock().expect("aggregator map mutex is not poisoned");
      aggregators.get(session_id).cloned()
    };
    let Some(aggregator) = aggregator else {
      tracing::debug!(session_id, event_type = ?event.get("type"), "announce skipped without live aggregator");
      return Ok(());
    };
    if self.epoch(session_id) != epoch {
      tracing::debug!(session_id, event_type = ?event.get("type"), "announce skipped after aggregator rebuild");
      return Ok(());
    }
    let mut event = event.clone();
    self.stamp_event_index(session_id, &mut event)?;
    self.feed_and_broadcast(session_id, &event, &aggregator).await
  }

  /// Broadcast one frame on the session channel without persisting it.
  pub async fn broadcast_only(&self, session_id: &str, frame: Value) -> StoreResult<()> {
    self.broadcast(&streaming::session_channel(session_id), frame).await
  }

  /// Broadcast a session-scoped event on the shared sidebar channel.
  pub async fn broadcast_sidebar(
    &self,
    session_id: &str,
    event_type: &str,
    fields: Map<String, Value>,
  ) -> StoreResult<()> {
    let mut frame = Map::new();
    frame.insert("type".to_string(), json!(event_type));
    frame.insert("session_id".to_string(), json!(session_id));
    frame.extend(fields);
    self.broadcast(streaming::SIDEBAR_CHANNEL, Value::Object(frame)).await
  }

  /// Notify sidebar subscribers that one task-tree fact changed.
  pub async fn broadcast_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) -> StoreResult<()> {
    let mut fields = Map::new();
    fields.insert("fact_type".to_string(), event_type.map_or(Value::Null, |value| json!(value)));
    self.broadcast_sidebar(session_id, "task_tree_changed", fields).await
  }

  /// Best-effort sidebar notification for one node's durable task fact. Call it after the fact is durable: a failure is
  /// logged and never fails the write (`control_sink.py::ControlEventSink.notify_tree_changed`).
  pub async fn notify_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) {
    if let Err(error) = self.broadcast_task_tree_changed(session_id, event_type).await {
      tracing::error!(session_id, event_type = ?event_type, %error, "task_tree_changed_broadcast_failed");
    }
  }

  /// Broadcast a frame on any named channel.
  pub async fn broadcast(&self, channel: &str, frame: Value) -> StoreResult<()> {
    self.streaming_manager.broadcast(channel, frame).await
  }

  /// Subscribe to any channel, including the shared sidebar channel.
  pub fn subscribe(&self, channel: &str) -> Subscription {
    self.streaming_manager.subscribe(channel)
  }

  /// Remove one subscriber from its channel.
  pub fn unsubscribe(&self, subscription: &mut Subscription) {
    self.streaming_manager.unsubscribe(subscription);
  }

  /// Register one open WebSocket, so that the shutdown closes it.
  pub fn register_socket(&self) -> SocketHandle {
    self.streaming_manager.register_socket()
  }

  /// Discard pending stream deltas, then close every registered WebSocket and wait for each close frame.
  pub async fn close_subscribers(&self) {
    self.streaming_manager.close_all().await;
  }

  /// Build a message aggregator with the event renderers registered for this store.
  pub fn new_message_aggregator(&self, event_index_offset: usize, emit_stream_deltas: bool) -> MessageAggregator {
    MessageAggregator::with_renderers(event_index_offset, emit_stream_deltas, self.event_renderers.clone())
  }

  /// Return the memoized message projection for an unarchived session.
  pub fn get_message_projection(&self, session_id: &str) -> StoreResult<Option<Arc<MessageProjection>>> {
    let archive_offset = self.read_archive_offset(session_id)?;
    if archive_offset != 0 {
      return Ok(None);
    }
    let live = self.load_chat_events_shared_sync(session_id)?;
    let mut cache = self.session_events.projections.lock().expect("projection cache mutex is not poisoned");
    if let Some(cached) = cache.get(session_id)
      && cached.event_count() == live.len()
    {
      return Ok(Some(cached));
    }
    let cached = cache.get(session_id);
    let projection = match cached {
      Some(cached) if live.len() >= cached.event_count() => cached.advanced(&live[cached.event_count()..])?,
      _ => MessageProjection::new(live.as_slice(), 0, self.event_renderers.clone())?,
    };
    let projection = Arc::new(projection);
    cache.store(session_id, Arc::clone(&projection));
    Ok(Some(projection))
  }

  /// Return a memoized projection only when the in-memory event list proves it current.
  pub fn projection_memo_hit(&self, session_id: &str) -> Option<Arc<MessageProjection>> {
    let live_len = self.cached_chat_events(session_id)?.len();
    let projection =
      self.session_events.projections.lock().expect("projection cache mutex is not poisoned").get(session_id)?;
    (projection.event_count() == live_len).then_some(projection)
  }

  /// Drop the cached events, live aggregator, projection, and feature-owned runtime state.
  pub fn drop_session_runtime_state(&self, session_id: &str) {
    self.clear_chat_events_cache(session_id);
    self.session_events.aggregators.lock().expect("aggregator map mutex is not poisoned").remove(session_id);
    self.session_events.init_locks.lock().expect("aggregator lock map mutex is not poisoned").remove(session_id);
    self.session_events.event_locks.lock().expect("event lock map mutex is not poisoned").remove(session_id);
    let mut epochs = self.session_events.epochs.lock().expect("aggregator epoch mutex is not poisoned");
    let epoch = epochs.entry(session_id.to_string()).or_default();
    *epoch += 1;
    drop(epochs);
    self.session_events.projections.lock().expect("projection cache mutex is not poisoned").remove(session_id);
    self.runtime_hooks.drop_session_runtime_state(session_id);
  }

  /// Persist synthesized compaction events for compact-boundary and compact-failure input events.
  pub async fn handle_compaction_event(&self, session_id: &str, event: &Value) -> StoreResult<Option<ChatEvent>> {
    if event.get("type").and_then(Value::as_str) != Some("system") {
      return Ok(None);
    }
    let subtype = event.get("subtype").and_then(Value::as_str);
    if subtype == Some("compact_boundary") {
      let metadata = event.get("compact_metadata").cloned().unwrap_or_else(|| json!({}));
      let metadata = metadata.as_object().ok_or_else(|| StoreError::new("compact_metadata must be an object"))?;
      let trigger = metadata.get("trigger").cloned().unwrap_or_else(|| json!("unknown"));
      tracing::info!(
        session_id,
        trigger = ?trigger,
        pre_tokens = ?metadata.get("pre_tokens"),
        post_tokens = ?metadata.get("post_tokens"),
        "cc_context_compacted"
      );
      return self
        .persist_and_broadcast(
          session_id,
          json!({"type": "context_compacted", "trigger": trigger, "compact_metadata": metadata}),
        )
        .await
        .map(Some);
    }
    if subtype == Some("status") && event.get("compact_result").and_then(Value::as_str) == Some("failed") {
      let error = event.get("compact_error").cloned().unwrap_or(Value::Null);
      tracing::info!(session_id, error = ?error, "cc_context_compact_failed");
      return self
        .persist_and_broadcast(session_id, json!({"type": "context_compact_failed", "error": error}))
        .await
        .map(Some);
    }
    Ok(None)
  }

  async fn feed_and_broadcast(
    &self,
    session_id: &str,
    event: &ChatEvent,
    aggregator: &Arc<Mutex<MessageAggregator>>,
  ) -> StoreResult<()> {
    let deltas = aggregator.lock().await.feed(&event.to_value())?;
    let channel = streaming::session_channel(session_id);
    for delta in deltas {
      self.broadcast(&channel, delta).await?;
    }
    let event_type = event.get("type").and_then(CompactValue::as_str);
    if !event_type.is_some_and(|event_type| RAW_EVENTS_REPLACED_BY_DELTAS.contains(&event_type)) {
      self.broadcast(&channel, event.to_value()).await?;
    }
    Ok(())
  }

  async fn get_or_init_aggregator(&self, session_id: &str) -> StoreResult<Arc<Mutex<MessageAggregator>>> {
    if let Some(aggregator) =
      self.session_events.aggregators.lock().expect("aggregator map mutex is not poisoned").get(session_id).cloned()
    {
      return Ok(aggregator);
    }
    loop {
      let epoch = self.epoch(session_id);
      let lock = async_lock_for(&self.session_events.init_locks, session_id);
      let _init_guard = lock.lock().await;
      if let Some(aggregator) =
        self.session_events.aggregators.lock().expect("aggregator map mutex is not poisoned").get(session_id).cloned()
      {
        return Ok(aggregator);
      }
      // Python's `_load_aggregator_init_inputs` runs the full history read in `asyncio.to_thread`.
      let events = self.load_chat_events_shared_async(session_id).await?;
      let archive_offset = self.read_archive_offset(session_id)?;
      let mut aggregator = self.new_message_aggregator(archive_offset, false);
      let mut dropped = false;
      let mut start = 0;
      while start < events.len() {
        let end = (start + AGGREGATOR_INIT_SLICE_EVENTS).min(events.len());
        for event in &events[start..end] {
          aggregator.feed(&event.to_value())?;
        }
        start = end;
        tokio::task::yield_now().await;
        if self.epoch(session_id) != epoch {
          dropped = true;
          break;
        }
      }
      if dropped || self.epoch(session_id) != epoch {
        self.clear_chat_events_cache(session_id);
        continue;
      }
      aggregator.set_emit_stream_deltas(true);
      let aggregator = Arc::new(Mutex::new(aggregator));
      self
        .session_events
        .aggregators
        .lock()
        .expect("aggregator map mutex is not poisoned")
        .insert(session_id.to_string(), Arc::clone(&aggregator));
      return Ok(aggregator);
    }
  }

  fn stamp_event_index(&self, session_id: &str, event: &mut ChatEvent) -> StoreResult<()> {
    let count = self.get_chat_event_count_sync(session_id)?;
    let index = count.checked_sub(1).ok_or_else(|| StoreError::new("cannot index an empty chat event log"))?;
    if !event.is_object() {
      return Err(StoreError::new("chat event must be a JSON object"));
    }
    event.insert("event_index", json!(index));
    Ok(())
  }

  fn epoch(&self, session_id: &str) -> u64 {
    *self.session_events.epochs.lock().expect("aggregator epoch mutex is not poisoned").get(session_id).unwrap_or(&0)
  }

  async fn event_lock(&self, session_id: &str) -> OwnedMutexGuard<()> {
    let lock = async_lock_for(&self.session_events.event_locks, session_id);
    lock.lock_owned().await
  }
}

fn async_lock_for(locks: &StdMutex<HashMap<String, Arc<Mutex<()>>>>, session_id: &str) -> Arc<Mutex<()>> {
  Arc::clone(
    locks
      .lock()
      .expect("session event lock map mutex is not poisoned")
      .entry(session_id.to_string())
      .or_insert_with(|| Arc::new(Mutex::new(()))),
  )
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::{Arc, Mutex as StdMutex, mpsc};

  use charliebot_core::config::CharlieBotConfig;

  use super::*;
  use crate::hooks::{AfterTurnWork, EmptyRuntimeEventHooks, EventRenderer, RuntimeEventHooks};
  use crate::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
  use crate::store::test_support::{
    assistant_event, assistant_text_event, make_test_store, make_test_store_with_runtime, open_test_store,
  };
  use crate::streaming::Subscription;

  #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
  async fn aggregator_seed_history_load_runs_on_the_blocking_pool() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    store.append_chat_event("synthetic", json!({"type": "user", "content": "seed"})).await.unwrap();
    let runtime_thread = std::thread::current().id();
    let (thread_tx, thread_rx) = mpsc::channel();
    *store.event_read_hook.lock().unwrap() = Some(Arc::new(move || {
      thread_tx.send(std::thread::current().id()).unwrap();
    }));

    let reads = store.history_work().file_reads;
    store.prime_aggregator("synthetic").await.unwrap();
    let read_thread = thread_rx.recv().unwrap();
    assert_ne!(read_thread, runtime_thread);
    assert_eq!(store.history_work().file_reads, reads + 1, "aggregator seeding reads the live history once");
    store.prime_aggregator("synthetic").await.unwrap();
    assert_eq!(store.history_work().file_reads, reads + 1, "a cached aggregator does not read history again");
  }

  /// Renderers under the given event types, each rendering nothing; the other hooks stay empty.
  struct RendererHook(Vec<&'static str>);

  impl RuntimeEventHooks for RendererHook {
    fn copy_on_fork(&self, _parent_dir: &std::path::Path, _child_dir: &std::path::Path) -> StoreResult<()> {
      Ok(())
    }

    fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>> {
      Ok(
        self
          .0
          .iter()
          .map(|name| (name.to_string(), Arc::new(|_: &Value| Ok(None)) as Arc<dyn EventRenderer>))
          .collect(),
      )
    }

    fn after_turn(&self, meta: SessionMetadata, event: Value, cfg: CharlieBotConfig) -> Vec<AfterTurnWork> {
      EmptyRuntimeEventHooks.after_turn(meta, event, cfg)
    }

    fn mark_sidebar_dirty(&self, session_id: &str) {
      EmptyRuntimeEventHooks.mark_sidebar_dirty(session_id)
    }

    fn drop_session_runtime_state(&self, session_id: &str) {
      EmptyRuntimeEventHooks.drop_session_runtime_state(session_id)
    }
  }

  /// Drain every frame already queued on a subscription; a frame is queued before its send returns.
  async fn drain_frames(subscription: &mut Subscription) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(std::time::Duration::from_millis(1), subscription.recv()).await {
      frames.push(serde_json::from_str(&frame).unwrap());
    }
    frames
  }

  /// The stored event without the fields the append and the feed add.
  fn strip_stamps(event: ChatEvent) -> Value {
    let mut event = event.to_value();
    let object = event.as_object_mut().unwrap();
    for key in ["id", "timestamp", "event_index"] {
      object.remove(key);
    }
    event
  }

  async fn seed_session(store: &Store) {
    write_test_session(store, "synthetic");
    store
      .append_chat_event(
        "synthetic",
        json!({
          "type": "user",
          "message": {"content": [{"type": "text", "text": "seed question"}]},
          "timestamp": "2026-09-07T00:00:00Z",
        }),
      )
      .await
      .unwrap();
    let mut answer = assistant_text_event("seed answer");
    answer["timestamp"] = json!("2026-09-07T00:00:01Z");
    store.append_chat_event("synthetic", answer).await.unwrap();
  }

  struct AfterTurnHook {
    sender: StdMutex<Option<tokio::sync::oneshot::Sender<Value>>>,
  }

  struct FeatureEventHook;

  impl RuntimeEventHooks for FeatureEventHook {
    fn copy_on_fork(&self, _parent_dir: &std::path::Path, _child_dir: &std::path::Path) -> StoreResult<()> {
      Ok(())
    }

    fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn crate::hooks::EventRenderer>)>> {
      let renderer: Arc<dyn crate::hooks::EventRenderer> =
        Arc::new(|event: &Value| Ok(Some(json!({"role": "feature", "content": event["content"].clone()}))));
      Ok(vec![("feature_event".to_string(), renderer)])
    }

    fn after_turn(
      &self,
      _meta: SessionMetadata,
      _event: Value,
      _cfg: charliebot_core::config::CharlieBotConfig,
    ) -> Vec<AfterTurnWork> {
      Vec::new()
    }

    fn mark_sidebar_dirty(&self, _session_id: &str) {}

    fn drop_session_runtime_state(&self, _session_id: &str) {}
  }

  impl RuntimeEventHooks for AfterTurnHook {
    fn copy_on_fork(&self, _parent_dir: &std::path::Path, _child_dir: &std::path::Path) -> StoreResult<()> {
      Ok(())
    }

    fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn crate::hooks::EventRenderer>)>> {
      Ok(Vec::new())
    }

    fn after_turn(
      &self,
      _meta: SessionMetadata,
      event: Value,
      _cfg: charliebot_core::config::CharlieBotConfig,
    ) -> Vec<AfterTurnWork> {
      let sender = self.sender.lock().unwrap().take().unwrap();
      vec![AfterTurnWork {
        name: "test-contribution".to_string(),
        future: Box::pin(
          async move { sender.send(event).map_err(|_| StoreError::new("after-turn receiver was dropped")) },
        ),
      }]
    }

    fn mark_sidebar_dirty(&self, _session_id: &str) {}

    fn drop_session_runtime_state(&self, _session_id: &str) {}
  }

  fn write_test_session(store: &Store, session_id: &str) {
    let metadata = SessionMetadata::new(session_id, "synthetic", TaskProfile::Manager);
    let path = store.metadata_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, metadata.to_json_string(&[], true)).unwrap();
  }

  #[tokio::test]
  async fn persist_broadcast_sends_message_deltas_and_suppresses_replaced_raw_events() {
    let (_home, store) = make_test_store();
    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    let event = store
      .persist_and_broadcast(
        "synthetic",
        json!({
          "type": "assistant",
          "id": "assistant-1",
          "message": {"content": [{"type": "text", "text": "hello"}]},
          "timestamp": "2026-01-01T00:00:00Z",
        }),
      )
      .await
      .unwrap();
    assert_eq!(event["event_index"].as_i64(), Some(0));
    let frame: Value = serde_json::from_str(&subscription.recv().await.unwrap()).unwrap();
    assert_eq!(frame["type"], "stream");
    assert_eq!(frame["message"]["content"], "hello");
  }

  #[tokio::test]
  async fn sidebar_frames_are_public_and_task_tree_changes_include_the_fact_type() {
    let (_home, store) = make_test_store();
    let mut subscription = store.subscribe(streaming::SIDEBAR_CHANNEL);
    store.broadcast_task_tree_changed("synthetic", Some("run_finished")).await.unwrap();
    let frame: Value = serde_json::from_str(&subscription.recv().await.unwrap()).unwrap();
    assert_eq!(
      frame,
      json!({
        "type": "task_tree_changed",
        "session_id": "synthetic",
        "fact_type": "run_finished",
      })
    );
  }

  #[tokio::test]
  async fn after_turn_contribution_starts_for_master_done_after_persistence() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let hook = Arc::new(AfterTurnHook { sender: StdMutex::new(Some(sender)) });
    let (_home, store) = make_test_store_with_runtime(hook);
    write_test_session(&store, "synthetic");

    let event = store.persist_and_broadcast("synthetic", json!({"type": "master_done"})).await.unwrap();
    assert_eq!(event["event_index"].as_i64(), Some(0));
    let contribution_event = tokio::time::timeout(std::time::Duration::from_secs(1), receiver).await.unwrap().unwrap();
    assert_eq!(contribution_event["type"], "master_done");
    assert_eq!(contribution_event["event_index"], 0);
    assert_eq!(store.load_chat_events_sync("synthetic").unwrap().len(), 1);
  }

  /// Counts the after-turn requests the store makes; it starts no work.
  #[derive(Default)]
  struct CountingAfterTurnHook {
    requests: AtomicUsize,
  }

  impl RuntimeEventHooks for CountingAfterTurnHook {
    fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn crate::hooks::EventRenderer>)>> {
      Ok(Vec::new())
    }

    fn after_turn(&self, _meta: SessionMetadata, _event: Value, _cfg: CharlieBotConfig) -> Vec<AfterTurnWork> {
      self.requests.fetch_add(1, Ordering::SeqCst);
      Vec::new()
    }

    fn copy_on_fork(&self, _parent_dir: &std::path::Path, _child_dir: &std::path::Path) -> StoreResult<()> {
      Ok(())
    }

    fn mark_sidebar_dirty(&self, _session_id: &str) {}

    fn drop_session_runtime_state(&self, _session_id: &str) {}
  }

  #[tokio::test]
  async fn after_turn_is_requested_only_for_a_master_done_of_an_existing_session() {
    let hook = Arc::new(CountingAfterTurnHook::default());
    let (_home, store) = make_test_store_with_runtime(hook.clone());
    write_test_session(&store, "synthetic");

    store
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "hi", "timestamp": "t1"}))
      .await
      .unwrap();
    store.persist_and_broadcast("missing", json!({"type": "master_done"})).await.unwrap();
    assert_eq!(hook.requests.load(Ordering::SeqCst), 0);

    store.persist_and_broadcast("synthetic", json!({"type": "master_done"})).await.unwrap();
    assert_eq!(hook.requests.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn construction_supplies_feature_renderers_to_message_helpers() {
    let (_home, store) = make_test_store_with_runtime(Arc::new(FeatureEventHook));
    let messages =
      store.events_to_messages(&[json!({"type": "feature_event", "content": "feature text"}).into()]).unwrap();
    assert_eq!(
      messages,
      vec![json!({
        "role": "feature",
        "content": "feature text",
        "timestamp": null,
        "event_index": 0,
        "id": "legacy:0",
      })]
    );
  }

  #[tokio::test]
  async fn projection_advances_immutably_and_runtime_drop_bumps_the_epoch() {
    let (_home, store) = make_test_store();
    store.append_chat_event("synthetic", json!({"type": "user", "content": "first"})).await.unwrap();
    let first = store.get_message_projection("synthetic").unwrap().unwrap();
    assert_eq!(first.event_count(), 1);
    assert!(store.projection_memo_hit("synthetic").is_some());

    store
      .append_chat_event(
        "synthetic",
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "draft"}]}}),
      )
      .await
      .unwrap();
    assert!(store.projection_memo_hit("synthetic").is_none());
    let advanced = store.get_message_projection("synthetic").unwrap().unwrap();
    assert_eq!(first.event_count(), 1);
    assert_eq!(advanced.event_count(), 2);
    assert_eq!(advanced.pending_draft().unwrap()["content"], "draft");

    let epoch = store.prime_aggregator("synthetic").await.unwrap();
    store.drop_session_runtime_state("synthetic");
    assert_eq!(store.prime_aggregator("synthetic").await.unwrap(), epoch + 1);
    assert!(store.projection_memo_hit("synthetic").is_none());
  }

  #[tokio::test]
  async fn an_old_announce_is_skipped_after_the_aggregator_epoch_changes() {
    let (_home, store) = make_test_store();
    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    let epoch = store.prime_aggregator("synthetic").await.unwrap();
    let event = store
      .append_chat_event(
        "synthetic",
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "once"}]}}),
      )
      .await
      .unwrap();
    store.drop_session_runtime_state("synthetic");
    store.announce_appended_event("synthetic", &event, epoch).await.unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_millis(1), subscription.recv()).await.is_err());
  }

  #[tokio::test]
  async fn compaction_inputs_persist_the_python_synthesized_event_shapes() {
    let (_home, store) = make_test_store();
    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    assert!(store.handle_compaction_event("synthetic", &json!({"type": "user"})).await.unwrap().is_none());

    let boundary = store
      .handle_compaction_event(
        "synthetic",
        &json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": null}}),
      )
      .await
      .unwrap()
      .unwrap();
    assert_eq!(boundary["type"].as_str(), Some("context_compacted"));
    assert!(boundary["trigger"].is_null());
    assert_eq!(boundary["compact_metadata"].to_value(), json!({"trigger": null}));
    let rendered: Value = serde_json::from_str(&subscription.recv().await.unwrap()).unwrap();
    assert_eq!(rendered["type"], "message");
    assert_eq!(rendered["message"]["content"], "Context compacted");
    let raw: Value = serde_json::from_str(&subscription.recv().await.unwrap()).unwrap();
    assert_eq!(raw["type"], "context_compacted");

    let failed = store
      .handle_compaction_event("synthetic", &json!({"type": "system", "subtype": "status", "compact_result": "failed"}))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(failed["type"].as_str(), Some("context_compact_failed"));
    assert!(failed["error"].is_null());
    assert_eq!(failed["event_index"].as_i64(), Some(1));
    assert!(failed["id"].as_str().is_some());
    assert!(failed["timestamp"].as_str().is_some());
  }

  #[tokio::test]
  async fn first_paint_surfaces_are_disjoint() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    store
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "q1", "timestamp": "t1"}))
      .await
      .unwrap();
    store.persist_and_broadcast("synthetic", assistant_event("reply1", "a1")).await.unwrap();
    store
      .persist_and_broadcast("synthetic", json!({"type": "master_done", "thinking_seconds": 1, "timestamp": "t2"}))
      .await
      .unwrap();
    store
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "q2", "timestamp": "t3"}))
      .await
      .unwrap();
    store.persist_and_broadcast("synthetic", assistant_event("IN PROGRESS", "a2")).await.unwrap();

    let projection = store.get_message_projection("synthetic").unwrap().unwrap();
    let draft = projection.pending_draft().unwrap().clone();
    assert_eq!(draft["content"], "IN PROGRESS");
    let (page, _, _) = projection.tail(40);
    assert!(!page.contains(&draft));
    assert!(!page.iter().any(|message| message["content"] == "IN PROGRESS"));
    let (older, _, _) = projection.slice_before(projection.committed().len(), 40);
    assert!(!older.iter().any(|message| message["content"] == "IN PROGRESS"));
    assert_eq!(projection.history().last(), Some(&draft));
  }

  #[tokio::test]
  async fn projection_memo_hit_archived_session_is_always_miss() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    store
      .append_chat_event("synthetic", json!({"type": "user", "content": "old", "timestamp": "2020-01-01T00:00:00Z"}))
      .await
      .unwrap();
    store
      .append_chat_event("synthetic", json!({"type": "user", "content": "new", "timestamp": "2030-01-01T00:00:00Z"}))
      .await
      .unwrap();
    assert!(store.get_message_projection("synthetic").unwrap().is_some());
    let cutoff = UtcDateTime::parse("2025-01-01T00:00:00Z").unwrap();
    assert!(store.archive_events_before("synthetic", cutoff).await.unwrap().is_some());
    assert!(store.get_message_projection("synthetic").unwrap().is_none());
    assert!(store.projection_memo_hit("synthetic").is_none());
  }

  #[tokio::test]
  async fn persist_user_event_broadcasts_message_delta_only() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    store
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "hi", "timestamp": "ts"}))
      .await
      .unwrap();
    let frames = drain_frames(&mut subscription).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "message");
    assert_eq!(frames[0]["message"]["role"], "user");
    assert_eq!(frames[0]["message"]["content"], "hi");
  }

  #[tokio::test]
  async fn persist_handler_result_broadcasts_message_delta_and_raw_event() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    store
      .persist_and_broadcast(
        "synthetic",
        json!({"type": "handler_result", "task": "Lint", "status": "ok", "message": "Done", "timestamp": "ts"}),
      )
      .await
      .unwrap();
    let frames = drain_frames(&mut subscription).await;
    assert_eq!(
      frames.iter().map(|frame| frame["type"].as_str().unwrap()).collect::<Vec<_>>(),
      ["message", "handler_result"]
    );
    assert_eq!(frames[0]["message"]["role"], "system");
    assert_eq!(frames[0]["message"]["content"], "✓ Lint: Done");
  }

  #[tokio::test]
  async fn lazy_init_aggregator_after_restart_does_not_replay_history() {
    let (home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    // Root-session creation writes this fact before the three chat events in
    // the Python fixture, so the new user event has global index 4.
    store.append_chat_event("synthetic", json!({"type": "task_created"})).await.unwrap();
    store
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "hi", "timestamp": "t1"}))
      .await
      .unwrap();
    let mut answer = assistant_text_event("ok");
    answer["timestamp"] = json!("t2");
    store.persist_and_broadcast("synthetic", answer).await.unwrap();
    store
      .persist_and_broadcast("synthetic", json!({"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"}))
      .await
      .unwrap();

    // A restarted process: a fresh store over the same home, with no live aggregator.
    let restarted = open_test_store(home.path(), Arc::new(EmptyRuntimeEventHooks)).unwrap();
    let mut subscription = restarted.subscribe(&streaming::session_channel("synthetic"));
    restarted
      .persist_and_broadcast("synthetic", json!({"type": "user", "content": "next", "timestamp": "t4"}))
      .await
      .unwrap();
    let frames = drain_frames(&mut subscription).await;
    // Only the new event's delta is broadcast; the four events persisted above stay silent.
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "message");
    assert_eq!(frames[0]["message"]["role"], "user");
    assert_eq!(frames[0]["message"]["content"], "next");
    assert_eq!(frames[0]["message"]["event_index"], 4);
  }

  #[tokio::test]
  async fn catchup_restores_stream_deltas_and_live_feed_broadcasts() {
    let (_home, store) = make_test_store();
    seed_session(&store).await;

    let aggregator = store.get_or_init_aggregator("synthetic").await.unwrap();
    assert!(aggregator.lock().await.emits_stream_deltas());
    let stored = store.session_events.aggregators.lock().unwrap().get("synthetic").cloned().unwrap();
    assert!(Arc::ptr_eq(&aggregator, &stored));

    let mut subscription = store.subscribe(&streaming::session_channel("synthetic"));
    let mut live = assistant_text_event("live tail");
    live["timestamp"] = json!("2026-09-07T00:00:02Z");
    store.persist_and_broadcast("synthetic", live).await.unwrap();
    let frames = drain_frames(&mut subscription).await;
    assert!(frames.iter().any(|frame| frame["type"] == "stream"));
  }

  #[tokio::test]
  async fn concurrent_first_persists_catch_up_once() {
    let (_home, store) = make_test_store();
    seed_session(&store).await;

    let (first, second) =
      tokio::join!(store.get_or_init_aggregator("synthetic"), store.get_or_init_aggregator("synthetic"));
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.lock().await.pending_draft_message().unwrap()["content"], "seed answer");
  }

  #[tokio::test]
  async fn drop_mid_feed_discards_and_reruns() {
    let (_home, store) = make_test_store();
    seed_session(&store).await;
    let epoch = store.epoch("synthetic");

    let (aggregator, ()) = tokio::join!(store.get_or_init_aggregator("synthetic"), async {
      store.drop_session_runtime_state("synthetic");
    });
    let aggregator = aggregator.unwrap();
    assert_eq!(store.epoch("synthetic"), epoch + 1);
    let stored = store.session_events.aggregators.lock().unwrap().get("synthetic").cloned().unwrap();
    assert!(Arc::ptr_eq(&aggregator, &stored));
    assert!(aggregator.lock().await.emits_stream_deltas());
    assert_eq!(aggregator.lock().await.pending_draft_message().unwrap()["content"], "seed answer");
  }

  #[tokio::test]
  async fn compact_boundary_emits_context_compacted_with_the_payload_whole() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    let event = store
      .handle_compaction_event(
        "synthetic",
        &json!({
          "type": "system",
          "subtype": "compact_boundary",
          "compact_metadata": {"trigger": "manual", "pre_tokens": 239708},
        }),
      )
      .await
      .unwrap()
      .unwrap();
    assert_eq!(
      strip_stamps(event),
      json!({
        "type": "context_compacted",
        "trigger": "manual",
        "compact_metadata": {"trigger": "manual", "pre_tokens": 239708},
      })
    );
  }

  #[tokio::test]
  async fn status_failed_emits_context_compact_failed() {
    let (_home, store) = make_test_store();
    write_test_session(&store, "synthetic");
    let rows = [
      (
        json!({"type": "system", "subtype": "status", "compact_result": "failed", "compact_error": "context too large"}),
        json!("context too large"),
      ),
      (json!({"type": "system", "subtype": "status", "compact_result": "failed"}), Value::Null),
    ];
    for (input, expected_error) in rows {
      let event = store.handle_compaction_event("synthetic", &input).await.unwrap().unwrap();
      assert_eq!(strip_stamps(event), json!({"type": "context_compact_failed", "error": expected_error}));
    }
  }

  #[test]
  fn an_event_type_rendered_twice_fails_store_construction() {
    let home = tempfile::tempdir().unwrap();
    let error = open_test_store(home.path(), Arc::new(RendererHook(vec!["first_event", "first_event"])))
      .err()
      .expect("two renderers for one event type must fail");
    assert!(error.to_string().contains("first_event"));
  }

  #[test]
  fn a_feature_renderer_for_a_builtin_event_type_fails_store_construction() {
    let home = tempfile::tempdir().unwrap();
    let error = open_test_store(home.path(), Arc::new(RendererHook(vec!["master_done"])))
      .err()
      .expect("a feature renderer for a built-in event type must fail");
    assert!(error.to_string().contains("master_done"));
  }
}
