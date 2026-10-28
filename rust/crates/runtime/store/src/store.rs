use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::Future;
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::fsutil;
use charliebot_core::metadata_slot::{METADATA_FILE_NAME, MetadataFile, MetadataSlot};
use lru::LruCache;
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::aggregator::{MessageAggregator, builtin_event_types};
use crate::control::ControlLock;
use crate::event_state::EventReadCaches;
use crate::event_types::SharedChatEvent;
use crate::hooks::{EventRenderer, RuntimeEventHooks};
use crate::layout;
use crate::metadata::{RoundRating, SessionMetadata, SessionStatus, UtcDateTime};
use crate::projection::MessageProjection;
use crate::run_facts::RunFactsMemo;
use crate::streaming::StreamingManager;
#[cfg(test)]
use crate::task_tree_state::IdentityHook;
use crate::task_tree_state::{
  CachedActivity, CachedNodeFacts, CachedNodeState, CachedRunFactsAt, CachedTaskStateTail, CachedTreeIndex,
};
use crate::{ControlGuard, StoreError, StoreResult};

pub(crate) const METADATA_CACHE_TTL: Duration = Duration::from_secs(30);

/// Backend lifecycle owner for the account label held by a session.
pub trait AccountLabelKeeper: Send + Sync {
  /// Record a label on the metadata fields this lifecycle owns.
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool>;
  /// Read the label back from its owning fields.
  fn account_label(&self, meta: &SessionMetadata) -> Option<String>;
}

/// Reconcile resume-anchor fields against the on-disk metadata before ordinary saves.
pub trait AnchorFieldReconciler: Send + Sync {
  /// Restore protected anchor values from `disk` onto `attempted`.
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()>;
}

/// Runtime sequence contributions used by session listings.
pub trait SequenceListingFields: Send + Sync {
  /// Whether a sequence controller owns this session.
  fn owns_session(&self, meta: &SessionMetadata) -> bool;
  /// Return the derived sequence fields for the requested ids.
  fn listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>>;
  /// Return sequence subtree roots over a metadata snapshot.
  fn sequence_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>>;
  /// Return view subtree roots over a metadata snapshot.
  fn view_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>>;
}

/// The sequence binding archive callback for one task-tree node.
pub trait SequenceArchiveHook: Send + Sync {
  /// Run the registered binding's archive duty when it exists.
  fn on_archive<'a>(&'a self, session_id: &'a str) -> StoreHookFuture<'a, ()>;
}

/// Rebind sequence-owned state when an elone replaces a node.
pub trait SequenceBindingMover: Send + Sync {
  /// Move bindings from `source_session_id` to `new_session_id`.
  fn move_bindings<'a>(&'a self, source_session_id: &'a str, new_session_id: &'a str) -> StoreHookFuture<'a, bool>;
}

/// A future returned by a store hook.
pub type StoreHookFuture<'a, T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send + 'a>>;

/// Required runtime hooks. Missing implementations fail store construction.
#[derive(Default)]
pub struct StoreHooks {
  /// The backend lifecycle that owns account labels.
  pub account_labels: Option<Arc<dyn AccountLabelKeeper>>,
  /// Backend lifecycle anchor reconciliation.
  pub anchor_reconciler: Option<Arc<dyn AnchorFieldReconciler>>,
  /// Sequence-controller listing contributions.
  pub sequence_listing: Option<Arc<dyn SequenceListingFields>>,
  /// Feature event renderers, after-turn contributions, and runtime-state callbacks.
  pub runtime_events: Option<Arc<dyn RuntimeEventHooks>>,
  /// Sequence-binding archive callback.
  pub sequence_archive: Option<Arc<dyn SequenceArchiveHook>>,
  /// Sequence-binding elone rebind callback.
  pub sequence_binding_mover: Option<Arc<dyn SequenceBindingMover>>,
}

#[derive(Clone)]
pub(crate) struct CachedMetadata {
  pub(crate) value: Arc<SessionMetadata>,
  pub(crate) at: Instant,
  pub(crate) signature: Option<(i64, i64)>,
  /// Changes only when a cache entry is replaced.
  pub(crate) identity: Arc<()>,
}

pub(crate) struct CachedSubtreeRoots {
  pub(crate) metadata_identity: Vec<(String, Arc<()>)>,
  pub(crate) sequence: Arc<HashMap<String, String>>,
  pub(crate) views: Arc<HashMap<String, HashMap<String, String>>>,
}

#[cfg(test)]
pub(crate) type MetadataStatHook = Arc<dyn Fn(&Path) + Send + Sync>;

pub(crate) struct CachedChatEvents {
  pub(crate) generation: u64,
  pub(crate) identity: Arc<()>,
  pub(crate) events: Arc<Vec<SharedChatEvent>>,
}

/// Work done on node histories since the store opened, read through [`Store::history_work`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryWork {
  /// Live-log reads from disk, each a cache miss.
  pub file_reads: u64,
  /// Folds that start from a node's first event.
  pub full_folds: u64,
  /// Events folded, counting each event once for every fold that covers it.
  pub folded_events: u64,
  /// Parsed events deep-cloned into owned history vectors.
  pub owned_event_clones: u64,
  /// Calls that load an archived-plus-live history view.
  pub event_history_views: u64,
  /// Full node-state folds, distinct from the suffix extensions and memo hits.
  pub node_state_full_folds: u64,
  /// Events visited by node-state folds and suffix extensions.
  pub node_state_folded_events: u64,
  /// Run event-log bytes parsed for the worker transcript rebuilds.
  pub run_event_bytes_read: u64,
}

#[derive(Default)]
pub(crate) struct HistoryWorkCounters {
  pub(crate) file_reads: AtomicU64,
  pub(crate) full_folds: AtomicU64,
  pub(crate) folded_events: AtomicU64,
  pub(crate) owned_event_clones: AtomicU64,
  pub(crate) event_history_views: AtomicU64,
  pub(crate) node_state_full_folds: AtomicU64,
  pub(crate) node_state_folded_events: AtomicU64,
  pub(crate) run_event_bytes_read: AtomicU64,
}

/// The sessions directory's entry names at the root signature that produced them (`_dir_names_memo`).
pub(crate) struct DirNames {
  pub(crate) signature: (i64, i64),
  pub(crate) names: Arc<Vec<String>>,
}

/// One status filter's listing and what it was walked at: the listings revision, the time, and the sessions root's
/// signature.
pub(crate) struct CachedListing {
  pub(crate) revision: u64,
  pub(crate) generation: u64,
  pub(crate) at: Instant,
  pub(crate) root: (i64, i64),
  pub(crate) rows: Arc<Vec<Arc<SessionMetadata>>>,
}

const PROJECTION_LRU_LIMIT: usize = 64;

/// Per-session aggregator and message-projection state.
pub(crate) struct SessionEventState {
  pub(crate) aggregators: StdMutex<HashMap<String, Arc<Mutex<MessageAggregator>>>>,
  pub(crate) init_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  pub(crate) event_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  pub(crate) epochs: StdMutex<HashMap<String, u64>>,
  pub(crate) projections: StdMutex<ProjectionCache>,
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

pub(crate) struct ProjectionCache {
  values: LruCache<String, Arc<MessageProjection>>,
}

impl Default for ProjectionCache {
  fn default() -> Self {
    Self { values: LruCache::new(NonZeroUsize::new(PROJECTION_LRU_LIMIT).expect("the memo limit is nonzero")) }
  }
}

impl ProjectionCache {
  pub(crate) fn get(&mut self, session_id: &str) -> Option<Arc<MessageProjection>> {
    self.values.get(session_id).cloned()
  }

  pub(crate) fn store(&mut self, session_id: &str, projection: Arc<MessageProjection>) {
    self.values.put(session_id.to_string(), projection);
  }

  pub(crate) fn remove(&mut self, session_id: &str) {
    self.values.pop(session_id);
  }
}

#[derive(Default)]
struct ThinkingMaps {
  busy_since: HashMap<String, UtcDateTime>,
  run_busy: HashMap<String, String>,
  run_backends: HashMap<String, String>,
}

/// In-memory busy intervals and worker display backends.
#[derive(Default)]
pub(crate) struct ThinkingState {
  maps: StdMutex<ThinkingMaps>,
}

impl Store {
  /// Open a busy interval, keeping the earlier start when the session is already busy.
  pub fn mark_busy(&self, session_id: &str, since: UtcDateTime) -> StoreResult<(UtcDateTime, bool)> {
    Ok(self.mark_busy_inner(session_id, since))
  }

  fn mark_busy_inner(&self, session_id: &str, since: UtcDateTime) -> (UtcDateTime, bool) {
    let mut maps = self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned");
    let result = match maps.busy_since.get(session_id) {
      Some(existing) => (*existing, false),
      None => {
        maps.busy_since.insert(session_id.to_string(), since);
        (since, true)
      }
    };
    drop(maps);
    if result.1 {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
    result
  }

  /// Clear a busy interval when one is present.
  pub fn clear_busy(&self, session_id: &str) {
    let removed =
      self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").busy_since.remove(session_id);
    if removed.is_some() {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
  }

  /// Return the current busy interval start.
  pub fn busy_since(&self, session_id: &str) -> Option<UtcDateTime> {
    self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").busy_since.get(session_id).copied()
  }

  /// Open the busy interval owned by one worker run.
  pub fn mark_run_busy(&self, session_id: &str, run_id: &str, since: UtcDateTime) {
    self
      .thinking_state
      .maps
      .lock()
      .expect("thinking state mutex is not poisoned")
      .run_busy
      .insert(session_id.to_string(), run_id.to_string());
    self.mark_busy_inner(session_id, since);
  }

  /// Clear a worker busy interval only when the run id owns it.
  pub fn clear_run_busy(&self, session_id: &str, run_id: &str) {
    let cleared = {
      let mut maps = self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned");
      if maps.run_busy.get(session_id).map(String::as_str) != Some(run_id) {
        false
      } else {
        maps.run_busy.remove(session_id);
        maps.busy_since.remove(session_id).is_some()
      }
    };
    if cleared {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
  }

  /// Record the newest worker run backend for display.
  pub fn note_run_backend(&self, session_id: &str, backend: &str) {
    if !backend.is_empty() {
      self
        .thinking_state
        .maps
        .lock()
        .expect("thinking state mutex is not poisoned")
        .run_backends
        .insert(session_id.to_string(), backend.to_string());
    }
  }

  /// Return the newest worker run backend known for display.
  pub fn run_backend(&self, session_id: &str) -> Option<String> {
    self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").run_backends.get(session_id).cloned()
  }
}

/// Session metadata and event-log persistence for one configured CharlieBot home.
pub struct Store {
  pub(crate) cfg: CharlieBotConfig,
  pub(crate) slots: Vec<MetadataSlot>,
  pub(crate) account_labels: Arc<dyn AccountLabelKeeper>,
  pub(crate) anchor_reconciler: Arc<dyn AnchorFieldReconciler>,
  pub(crate) sequence_listing: Arc<dyn SequenceListingFields>,
  pub(crate) runtime_hooks: Arc<dyn RuntimeEventHooks>,
  pub(crate) event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
  pub(crate) sequence_archive: Arc<dyn SequenceArchiveHook>,
  pub(crate) sequence_binding_mover: Arc<dyn SequenceBindingMover>,
  pub(crate) metadata_cache: StdMutex<HashMap<String, CachedMetadata>>,
  /// Moves on every metadata write and every eviction of a cache entry, as `_listings_revision` does.
  pub(crate) listings_revision: AtomicU64,
  /// Identifies each status-filtered metadata corpus refresh for search memo invalidation.
  pub(crate) next_listing_generation: AtomicU64,
  /// The last listing walked for each status filter (see [`Store::load_session_metas`]).
  pub(crate) listings_memo: StdMutex<HashMap<Option<SessionStatus>, CachedListing>>,
  /// The sessions directory's entry names at the root signature that produced them (`_dir_names_memo`).
  pub(crate) dir_names_memo: StdMutex<Option<DirNames>>,
  #[cfg(test)]
  pub(crate) directory_scans: AtomicU64,
  pub(crate) subtree_roots_cache: StdMutex<Option<CachedSubtreeRoots>>,
  pub(crate) metadata_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  pub(crate) events_cache: StdMutex<HashMap<String, Arc<StdMutex<Option<CachedChatEvents>>>>>,
  pub(crate) node_facts_cache: StdMutex<HashMap<String, CachedNodeFacts>>,
  pub(crate) run_facts_memo: StdMutex<RunFactsMemo>,
  pub(crate) event_read_caches: StdMutex<EventReadCaches>,
  pub(crate) node_state_cache: StdMutex<HashMap<String, CachedNodeState>>,
  pub(crate) task_state_tail_cache: StdMutex<HashMap<String, CachedTaskStateTail>>,
  pub(crate) task_state_tail_reads: AtomicU64,
  /// Python's `_activity_memo`: each node's verdict keyed by the records generation and the history identity.
  pub(crate) activity_cache: StdMutex<HashMap<String, CachedActivity>>,
  /// Python's `_outcomes_memo`: each node's run facts keyed by the history identity they were folded from.
  pub(crate) run_facts_by_session: StdMutex<HashMap<String, CachedRunFactsAt>>,
  /// Chat-history walks that read a node's facts from its files without caching them.
  pub(crate) history_fact_walks: AtomicU64,
  pub(crate) run_record_reads: AtomicU64,
  pub(crate) task_tree_activity_calls: AtomicU64,
  /// Python's `_tree_rows_lock`: one tree page's row computation runs at a time, so concurrent pages never walk the
  /// same histories in parallel.
  pub(crate) tree_rows_lock: StdMutex<()>,
  pub(crate) history_work: HistoryWorkCounters,
  /// Reads of a node's history, each a call that hands out the live event list, cached or not.
  #[cfg(test)]
  pub(crate) history_reads: AtomicU64,
  pub(crate) next_events_cache_generation: AtomicU64,
  /// The next version a node-state memo entry takes when its state changes.
  pub(crate) next_node_state_version: AtomicU64,
  #[cfg(test)]
  pub(crate) metadata_stat_hook: StdMutex<Option<MetadataStatHook>>,
  #[cfg(test)]
  pub(crate) event_read_hook: StdMutex<Option<Arc<dyn Fn() + Send + Sync>>>,
  #[cfg(test)]
  pub(crate) event_append_hook: StdMutex<Option<Arc<dyn Fn() + Send + Sync>>>,
  #[cfg(test)]
  pub(crate) identity_hook: StdMutex<Option<IdentityHook>>,
  #[cfg(test)]
  pub(crate) tree_rows_hook: StdMutex<Option<Arc<dyn Fn() + Send + Sync>>>,
  #[cfg(test)]
  pub(crate) tree_index_metadata_reads: AtomicU64,
  pub(crate) control: ControlLock,
  pub(crate) records_generation: AtomicU64,
  pub(crate) control_event_ids: StdMutex<Option<VecDeque<String>>>,
  pub(crate) session_events: SessionEventState,
  pub(crate) streaming_manager: StreamingManager,
  pub(crate) thinking_state: ThinkingState,
  pub(crate) task_tree_index: StdMutex<Option<CachedTreeIndex>>,
  pub(crate) task_tree_index_builds: AtomicU64,
  pub(crate) task_tree_generation: AtomicU64,
  #[cfg(any(test, feature = "write-faults"))]
  pub(crate) write_faults: Arc<crate::write_fault_types::WriteFaults>,
}

impl Store {
  /// Build a store after package registration has supplied slots and every required hook.
  pub fn new(cfg: CharlieBotConfig, slots: Vec<MetadataSlot>, hooks: StoreHooks) -> StoreResult<Self> {
    let account_labels =
      hooks.account_labels.ok_or_else(|| StoreError::new("store requires an account label keeper"))?;
    let anchor_reconciler =
      hooks.anchor_reconciler.ok_or_else(|| StoreError::new("store requires an anchor field reconciler"))?;
    let sequence_listing =
      hooks.sequence_listing.ok_or_else(|| StoreError::new("store requires sequence listing fields"))?;
    let runtime_hooks = hooks.runtime_events.ok_or_else(|| StoreError::new("store requires runtime event hooks"))?;
    let mut event_renderers = HashMap::new();
    for (event_type, renderer) in runtime_hooks.event_renderers()? {
      if builtin_event_types().contains(&event_type.as_str()) {
        return Err(StoreError::new(format!("event type {event_type:?} already has a built-in renderer")));
      }
      if event_renderers.insert(event_type.clone(), renderer).is_some() {
        return Err(StoreError::new(format!("event type {event_type:?} already has a renderer")));
      }
    }
    let sequence_archive =
      hooks.sequence_archive.ok_or_else(|| StoreError::new("store requires sequence archive hooks"))?;
    let sequence_binding_mover =
      hooks.sequence_binding_mover.ok_or_else(|| StoreError::new("store requires sequence binding mover"))?;
    validate_slots(&slots)?;
    let store_id = Uuid::new_v4();
    Ok(Self {
      cfg,
      slots,
      account_labels,
      anchor_reconciler,
      sequence_listing,
      runtime_hooks,
      event_renderers,
      sequence_archive,
      sequence_binding_mover,
      metadata_cache: StdMutex::new(HashMap::new()),
      listings_revision: AtomicU64::new(0),
      next_listing_generation: AtomicU64::new(1),
      listings_memo: StdMutex::new(HashMap::new()),
      dir_names_memo: StdMutex::new(None),
      #[cfg(test)]
      directory_scans: AtomicU64::new(0),
      subtree_roots_cache: StdMutex::new(None),
      metadata_locks: StdMutex::new(HashMap::new()),
      events_cache: StdMutex::new(HashMap::new()),
      node_facts_cache: StdMutex::new(HashMap::new()),
      run_facts_memo: StdMutex::new(RunFactsMemo::default()),
      event_read_caches: StdMutex::new(EventReadCaches::default()),
      node_state_cache: StdMutex::new(HashMap::new()),
      task_state_tail_cache: StdMutex::new(HashMap::new()),
      task_state_tail_reads: AtomicU64::new(0),
      activity_cache: StdMutex::new(HashMap::new()),
      run_facts_by_session: StdMutex::new(HashMap::new()),
      history_fact_walks: AtomicU64::new(0),
      run_record_reads: AtomicU64::new(0),
      task_tree_activity_calls: AtomicU64::new(0),
      tree_rows_lock: StdMutex::new(()),
      history_work: HistoryWorkCounters::default(),
      #[cfg(test)]
      history_reads: AtomicU64::new(0),
      next_events_cache_generation: AtomicU64::new(1),
      next_node_state_version: AtomicU64::new(1),
      #[cfg(test)]
      metadata_stat_hook: StdMutex::new(None),
      #[cfg(test)]
      event_read_hook: StdMutex::new(None),
      #[cfg(test)]
      event_append_hook: StdMutex::new(None),
      #[cfg(test)]
      identity_hook: StdMutex::new(None),
      #[cfg(test)]
      tree_rows_hook: StdMutex::new(None),
      #[cfg(test)]
      tree_index_metadata_reads: AtomicU64::new(0),
      control: ControlLock::new(store_id),
      records_generation: AtomicU64::new(0),
      control_event_ids: StdMutex::new(None),
      session_events: SessionEventState::default(),
      streaming_manager: StreamingManager::new(),
      thinking_state: ThinkingState::default(),
      task_tree_index: StdMutex::new(None),
      task_tree_index_builds: AtomicU64::new(0),
      task_tree_generation: AtomicU64::new(0),
      #[cfg(any(test, feature = "write-faults"))]
      write_faults: Arc::default(),
    })
  }

  /// The session directory for `session_id`.
  pub fn session_dir(&self, session_id: &str) -> PathBuf {
    self.cfg.sessions_dir().join(session_id)
  }

  /// The typed configuration this store was built from.
  pub fn config(&self) -> &CharlieBotConfig {
    &self.cfg
  }

  pub(crate) fn next_events_cache_generation(&self) -> u64 {
    self.next_events_cache_generation.fetch_add(1, Ordering::Relaxed)
  }

  pub(crate) fn events_cache_entry(&self, session_id: &str) -> Arc<StdMutex<Option<CachedChatEvents>>> {
    let mut cache = self.events_cache.lock().expect("event cache mutex is not poisoned");
    Arc::clone(cache.entry(session_id.to_string()).or_insert_with(|| Arc::new(StdMutex::new(None))))
  }

  /// The metadata file for `session_id`.
  pub fn metadata_path(&self, session_id: &str) -> PathBuf {
    layout::metadata_path(&self.session_dir(session_id))
  }

  /// Serialize one session with every registered field's Python model default.
  pub fn session_metadata_value(&self, metadata: &SessionMetadata) -> Value {
    metadata.to_response_value(&self.slots)
  }

  /// Return cached session metadata, revalidating expired active entries by `(mtime_ns, size)`.
  pub async fn get_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    if let Some(meta) = self.fresh_cached_meta(session_id)? {
      return Ok(Some(meta.as_ref().clone()));
    }
    let path = self.metadata_path(session_id);
    let signature = stat_signature(&path);
    let meta = self.read_metadata_path(&path)?;
    let mut cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
    if let Some(entry) = cache.get(session_id) {
      return Ok(Some(entry.value.as_ref().clone()));
    }
    let Some(meta) = meta else { return Ok(None) };
    let meta = Arc::new(meta);
    cache.insert(
      session_id.to_string(),
      CachedMetadata { value: Arc::clone(&meta), at: Instant::now(), signature, identity: Arc::new(()) },
    );
    Ok(Some(meta.as_ref().clone()))
  }

  /// Read `metadata.json` from disk without consulting or populating the cache.
  pub fn read_metadata_fresh(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.read_metadata_path(&self.metadata_path(session_id))
  }

  /// Return cached metadata when fresh, reading and populating the cache only on a miss.
  pub fn get_session_cached_or_read_sync(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    if let Some(meta) = self.fresh_cached_meta(session_id)? {
      return Ok(Some(meta.as_ref().clone()));
    }
    let path = self.metadata_path(session_id);
    let signature = stat_signature(&path);
    let meta = self.read_metadata_path(&path)?;
    let mut cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
    if let Some(entry) = cache.get(session_id) {
      return Ok(Some(entry.value.as_ref().clone()));
    }
    let Some(meta) = meta else { return Ok(None) };
    let meta = Arc::new(meta);
    cache.insert(
      session_id.to_string(),
      CachedMetadata { value: Arc::clone(&meta), at: Instant::now(), signature, identity: Arc::new(()) },
    );
    Ok(Some(meta.as_ref().clone()))
  }

  /// The generation that changes when the session listing's metadata corpus changes.
  pub fn listings_revision(&self) -> u64 {
    self.listings_revision.load(Ordering::Acquire)
  }

  /// Return the directory where the live event log is stored.
  pub fn chat_events_path(&self, session_id: &str) -> PathBuf {
    layout::chat_events_path(&self.session_dir(session_id))
  }

  /// Path to a legacy thread's metadata file.
  pub fn thread_metadata_path(&self, session_id: &str, thread_id: &str) -> PathBuf {
    layout::threads_dir(&self.session_dir(session_id)).join(thread_id).join(METADATA_FILE_NAME)
  }

  /// Read one thread metadata file from disk.
  pub fn read_thread_metadata(&self, path: &Path) -> StoreResult<Option<crate::metadata::ThreadMetadata>> {
    let raw = match fs::read_to_string(path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
      return Ok(None);
    }
    serde_json::from_str(&raw)
      .map(Some)
      .map_err(|error| StoreError::new(format!("{}: invalid thread metadata: {error}", path.display())))
  }

  /// Atomically write a thread metadata file using Python model field and slot order.
  pub fn write_thread_metadata(&self, path: &Path, metadata: &crate::metadata::ThreadMetadata) -> StoreResult<()> {
    let parent = path.parent().expect("thread metadata path has a parent");
    fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
    fsutil::atomic_write_text(path, &metadata.to_json_string(&self.slots))
      .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
  }

  /// Invalidate cached metadata and listing snapshots after an external write or deletion.
  pub fn invalidate_cache(&self, session_id: &str) {
    self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").remove(session_id);
    self.listings_revision.fetch_add(1, Ordering::AcqRel);
  }

  pub(crate) async fn lock_for(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
      let mut locks = self.metadata_locks.lock().expect("metadata lock map mutex is not poisoned");
      Arc::clone(locks.entry(session_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    lock.lock_owned().await
  }

  pub(crate) fn fresh_cached_meta(&self, session_id: &str) -> StoreResult<Option<Arc<SessionMetadata>>> {
    let (identity, signature) = {
      let cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
      let Some(entry) = cache.get(session_id) else { return Ok(None) };
      if entry.value.status == SessionStatus::Archived || entry.at.elapsed() < METADATA_CACHE_TTL {
        return Ok(Some(Arc::clone(&entry.value)));
      }
      (Arc::clone(&entry.identity), entry.signature)
    };
    let path = self.metadata_path(session_id);
    #[cfg(test)]
    {
      let hook = self.metadata_stat_hook.lock().expect("metadata stat hook mutex is not poisoned").clone();
      if let Some(hook) = hook {
        hook(&path);
      }
    }
    let current_signature = stat_signature(&path);
    let mut cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
    let Some(current) = cache.get_mut(session_id) else { return Ok(None) };
    if !Arc::ptr_eq(&current.identity, &identity) {
      return Ok(Some(Arc::clone(&current.value)));
    }
    if signature.is_some() && current_signature == signature {
      current.at = Instant::now();
      return Ok(Some(Arc::clone(&current.value)));
    }
    cache.remove(session_id);
    // The file moved without a write through this store: the next listing walks again instead of serving the rows
    // this eviction proved stale.
    self.listings_revision.fetch_add(1, Ordering::AcqRel);
    Ok(None)
  }

  pub(crate) fn fresh_cached_metas(&self) -> StoreResult<HashMap<String, Arc<SessionMetadata>>> {
    let mut never_stop = || false;
    Ok(self.fresh_cached_metas_until(&mut never_stop)?.expect("the no-stop metadata freshness check completes"))
  }

  pub(crate) fn fresh_cached_metas_until(
    &self,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<HashMap<String, Arc<SessionMetadata>>>> {
    if should_stop() {
      return Ok(None);
    }
    let ids: Vec<String> =
      self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").keys().cloned().collect();
    let mut fresh = HashMap::new();
    for id in ids {
      if should_stop() {
        return Ok(None);
      }
      if let Some(meta) = self.fresh_cached_meta(&id)? {
        fresh.insert(id, meta);
      }
    }
    if should_stop() {
      return Ok(None);
    }
    Ok(Some(fresh))
  }

  pub(crate) fn read_metadata_path(&self, path: &Path) -> StoreResult<Option<SessionMetadata>> {
    let raw = match fs::read_to_string(path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
      return Ok(None);
    }
    SessionMetadata::from_json(&raw, Some(&path.display().to_string())).map(Some)
  }

  pub(crate) async fn mutate_fresh<F>(
    &self,
    session_id: &str,
    anchor_write: bool,
    always_write: bool,
    mutate: F,
  ) -> StoreResult<Option<SessionMetadata>>
  where
    F: FnOnce(&mut SessionMetadata) -> StoreResult<bool>,
  {
    let _guard = self.lock_for(session_id).await;
    let Some(mut meta) = self.read_metadata_fresh(session_id)? else { return Ok(None) };
    let changed = mutate(&mut meta)?;
    if changed || always_write {
      self.save_metadata_locked(&mut meta, anchor_write)?;
    }
    self.read_metadata_fresh(session_id)
  }

  pub(crate) fn save_metadata_locked(&self, meta: &mut SessionMetadata, anchor_write: bool) -> StoreResult<()> {
    if !anchor_write && let Some(disk) = self.read_metadata_fresh(&meta.id)? {
      self.anchor_reconciler.reconcile(&disk, meta)?;
    }
    let path = self.metadata_path(&meta.id);
    let parent = path.parent().expect("metadata file path has a parent");
    fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
    let serialized = meta.to_json_string(&self.slots, true);
    fsutil::atomic_write_text(&path, &serialized)
      .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    let parsed = Arc::new(SessionMetadata::from_json(&serialized, Some(&path.display().to_string()))?);
    let signature = stat_signature(&path);
    self
      .metadata_cache
      .lock()
      .expect("metadata cache mutex is not poisoned")
      .insert(meta.id.clone(), CachedMetadata { value: parsed, at: Instant::now(), signature, identity: Arc::new(()) });
    crate::sidebar_state::mark_sidebar_dirty(&meta.id);
    self.listings_revision.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }

  /// Persist the native session id and return the value read back from disk.
  pub async fn persist_cc_session_id(
    &self,
    session_id: &str,
    cc_session_id: &str,
    native_backend: Option<&str>,
  ) -> StoreResult<Option<String>> {
    self.persist_cc_session_id_at(session_id, cc_session_id, native_backend, UtcDateTime::now()).await
  }

  /// Persist the native session id with an explicit clock value for deterministic replay.
  pub async fn persist_cc_session_id_at(
    &self,
    session_id: &str,
    cc_session_id: &str,
    native_backend: Option<&str>,
    now: UtcDateTime,
  ) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, true, |meta| {
        if meta.cc_session_id.as_deref() != Some(cc_session_id) {
          meta.cc_session_id = Some(cc_session_id.to_string());
          meta.cc_session_started_at = Some(now);
        }
        if let Some(backend) = native_backend {
          meta.native_backend = Some(backend.to_string());
        }
        Ok(true)
      })
      .await?;
    Ok(saved.and_then(|meta| meta.cc_session_id))
  }

  /// Persist the backend that produced the native conversation.
  pub async fn persist_native_backend(&self, session_id: &str, native_backend: &str) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, false, |meta| {
        if meta.native_backend.as_deref() == Some(native_backend) {
          return Ok(false);
        }
        meta.native_backend = Some(native_backend.to_string());
        Ok(true)
      })
      .await?;
    Ok(saved.and_then(|meta| meta.native_backend))
  }

  /// Persist provenance for the held native conversation.
  pub async fn persist_native_anchor_provenance(
    &self,
    session_id: &str,
    prompt_hash: &str,
    native_backend: &str,
    model: Option<&str>,
  ) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, true, true, |meta| {
        meta.native_prompt_hash = Some(prompt_hash.to_string());
        meta.native_backend = Some(native_backend.to_string());
        meta.native_model = model.map(str::to_string);
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Clear the held conversation id and its start timestamp.
  pub async fn clear_cc_session_anchor(&self, session_id: &str) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, true, true, |meta| {
        meta.cc_session_id = None;
        meta.cc_session_started_at = None;
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Persist an account label through its registered lifecycle and read it back.
  pub async fn persist_account_label(&self, session_id: &str, label: &str) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, false, |meta| self.account_labels.record_account_label(meta, Some(label)))
      .await?;
    Ok(saved.and_then(|meta| self.account_labels.account_label(&meta)))
  }

  /// Update `updated_at` without saving a cached metadata object.
  pub async fn update_thinking_state(&self, session_id: &str, updated_at: UtcDateTime) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, false, true, |meta| {
        meta.updated_at = updated_at;
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Change one registered slot's fields and read the metadata back from disk.
  pub async fn update_slot_fields(
    &self,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<Option<SessionMetadata>> {
    self.update_slot_fields_fresh(session_id, owner, values, None).await
  }

  /// Change one registered slot's fields and activity timestamp in one fresh metadata write.
  pub async fn update_slot_fields_at(
    &self,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self.update_slot_fields_fresh(session_id, owner, values, Some(updated_at)).await
  }

  /// Change one registered slot's fields under the tree-wide control lock and session lock.
  pub async fn update_slot_fields_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<Option<SessionMetadata>> {
    self.require_control_guard(guard)?;
    self.update_slot_fields_fresh(session_id, owner, values, None).await
  }

  async fn update_slot_fields_fresh(
    &self,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
    updated_at: Option<UtcDateTime>,
  ) -> StoreResult<Option<SessionMetadata>> {
    if values.is_empty() {
      return Err(StoreError::new("update_slot_fields requires at least one field"));
    }
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == owner && slot.on == MetadataFile::Session)
      .ok_or_else(|| StoreError::new(format!("{owner:?} registered no session metadata fields")))?;
    for key in values.keys() {
      if !slot.fields.iter().any(|field| field.name == key) {
        return Err(StoreError::new(format!("{owner:?} has no field {key:?}")));
      }
    }
    self
      .mutate_fresh(session_id, false, true, move |meta| {
        for (key, value) in values {
          meta.set_extra(key, value);
        }
        if let Some(updated_at) = updated_at {
          meta.updated_at = updated_at;
        }
        Ok(true)
      })
      .await
  }

  /// Rename a session and update its activity timestamp.
  pub async fn rename_session(&self, session_id: &str, name: &str) -> StoreResult<Option<SessionMetadata>> {
    self.rename_session_at(session_id, name, UtcDateTime::now()).await
  }

  /// Rename a session with an explicit activity timestamp for deterministic replay.
  pub async fn rename_session_at(
    &self,
    session_id: &str,
    name: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.name = name.to_string();
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Assign the session to a group and update its activity timestamp.
  pub async fn set_group(&self, session_id: &str, group: Option<&str>) -> StoreResult<Option<SessionMetadata>> {
    let meta = self.set_group_at(session_id, group, UtcDateTime::now()).await?;
    if meta.is_some() {
      tracing::info!(session_id, group = ?group, "session_group_set");
    }
    Ok(meta)
  }

  /// Set a group with an explicit activity timestamp for deterministic replay.
  pub async fn set_group_at(
    &self,
    session_id: &str,
    group: Option<&str>,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.group = group.map(str::to_string);
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Star a session and update its activity timestamp.
  pub async fn star_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    let meta = self
      .update_field(
        session_id,
        |meta| {
          meta.starred = true;
          Ok(())
        },
        UtcDateTime::now(),
      )
      .await?;
    if meta.is_some() {
      tracing::info!(session_id, "session_starred");
    }
    Ok(meta)
  }

  /// Remove a session star and update its activity timestamp.
  pub async fn unstar_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    let meta = self
      .update_field(
        session_id,
        |meta| {
          meta.starred = false;
          Ok(())
        },
        UtcDateTime::now(),
      )
      .await?;
    if meta.is_some() {
      tracing::info!(session_id, "session_unstarred");
    }
    Ok(meta)
  }

  /// Set or clear one round's rating and update the session's activity timestamp.
  pub async fn rate_round(
    &self,
    session_id: &str,
    round_id: &str,
    rating: Option<RoundRating>,
  ) -> StoreResult<Option<SessionMetadata>> {
    let meta = self
      .update_field(
        session_id,
        |meta| {
          meta.round_ratings.set(round_id.to_string(), rating);
          Ok(())
        },
        UtcDateTime::now(),
      )
      .await?;
    if meta.is_some() {
      let rating_name = rating.map(|rating| match rating {
        RoundRating::ThumbsUp => "thumbs_up",
        RoundRating::ThumbsDown => "thumbs_down",
      });
      tracing::info!(session_id, round_id, rating = ?rating_name, "round_rated");
    }
    Ok(meta)
  }

  /// Rename one group across all matching sessions.
  pub async fn rename_group(&self, old_name: &str, new_name: &str) -> StoreResult<usize> {
    let count = self.rewrite_group(old_name, Some(new_name), None).await?;
    if count > 0 {
      tracing::info!(old_name, new_name, count, "group_renamed");
    }
    Ok(count)
  }

  /// Rename one group with an explicit activity timestamp for deterministic replay.
  pub async fn rename_group_at(&self, old_name: &str, new_name: &str, updated_at: UtcDateTime) -> StoreResult<usize> {
    self.rewrite_group(old_name, Some(new_name), Some(updated_at)).await
  }

  /// Clear one group from every matching session.
  pub async fn delete_group(&self, group: &str) -> StoreResult<usize> {
    let count = self.rewrite_group(group, None, None).await?;
    if count > 0 {
      tracing::info!(group, count, "group_deleted");
    }
    Ok(count)
  }

  /// Clear one group with an explicit activity timestamp for deterministic replay.
  pub async fn delete_group_at(&self, group: &str, updated_at: UtcDateTime) -> StoreResult<usize> {
    self.rewrite_group(group, None, Some(updated_at)).await
  }

  async fn rewrite_group(
    &self,
    old_name: &str,
    new_name: Option<&str>,
    updated_at: Option<UtcDateTime>,
  ) -> StoreResult<usize> {
    let sessions = self.load_session_metas(None)?;
    let mut count = 0;
    for snapshot in sessions {
      if snapshot.group.as_deref() != Some(old_name) {
        continue;
      }
      let _guard = self.lock_for(&snapshot.id).await;
      let Some(mut fresh) = self.read_metadata_fresh(&snapshot.id)? else { continue };
      if fresh.group.as_deref() != Some(old_name) {
        continue;
      }
      fresh.group = new_name.map(str::to_string);
      fresh.updated_at = updated_at.unwrap_or_else(UtcDateTime::now);
      self.save_metadata_locked(&mut fresh, false)?;
      count += 1;
    }
    Ok(count)
  }

  /// Clear the unread flag without changing activity time and notify the sidebar only after a flip.
  pub async fn mark_read(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    let meta = {
      let _guard = self.lock_for(session_id).await;
      let Some(mut meta) = self.read_metadata_fresh(session_id)? else { return Ok(None) };
      if !meta.has_unread {
        return Ok(Some(meta));
      }
      meta.has_unread = false;
      self.save_metadata_locked(&mut meta, false)?;
      self.read_metadata_fresh(session_id)?
    };
    let Some(meta) = meta else { return Ok(None) };
    let mut fields = Map::new();
    fields.insert("has_unread".to_string(), Value::Bool(false));
    self.broadcast_sidebar(session_id, charliebot_core::event_types::UNREAD_CHANGED, fields).await?;
    Ok(Some(meta))
  }

  /// Remove a session directory and its empty cgroup, then clear its runtime state.
  pub async fn delete_session_permanently(&self, session_id: &str) -> StoreResult<bool> {
    let _guard = self.lock_for(session_id).await;
    let session_dir = self.session_dir(session_id);
    if !session_dir.exists() {
      return Ok(false);
    }
    let remove_dir = session_dir.clone();
    tokio::task::spawn_blocking(move || fs::remove_dir_all(&remove_dir))
      .await
      .map_err(|error| StoreError::new(format!("{}: {error}", session_dir.display())))?
      .map_err(|error| StoreError::new(format!("{}: {error}", session_dir.display())))?;
    let base_dir = charliebot_core::cgroup::system_cgroup_base_dir();
    charliebot_core::cgroup::cleanup_session_cgroup(&base_dir, session_id);
    self.drop_session_runtime_state(session_id);
    self.invalidate_cache(session_id);
    crate::sidebar_state::mark_sidebar_dirty(session_id);
    self.metadata_locks.lock().expect("metadata lock map mutex is not poisoned").remove(session_id);
    tracing::info!(session_id, "session_deleted_permanently");
    Ok(true)
  }

  /// Switch the session's selected backend and update its activity timestamp.
  pub async fn switch_backend(&self, session_id: &str, backend: &str) -> StoreResult<Option<SessionMetadata>> {
    self.switch_backend_at(session_id, backend, UtcDateTime::now()).await
  }

  /// Switch backend with an explicit activity timestamp for deterministic replay.
  pub async fn switch_backend_at(
    &self,
    session_id: &str,
    backend: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.backend = backend.to_string();
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Archive a session without removing its files.
  pub async fn archive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.archive_session_at(session_id, UtcDateTime::now()).await
  }

  /// Archive a session with an explicit activity timestamp for deterministic replay.
  pub async fn archive_session_at(
    &self,
    session_id: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.status = SessionStatus::Archived;
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Restore an archived session.
  pub async fn unarchive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.unarchive_session_at(session_id, UtcDateTime::now()).await
  }

  /// Restore a session with an explicit activity timestamp for deterministic replay.
  pub async fn unarchive_session_at(
    &self,
    session_id: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.status = SessionStatus::Active;
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Mark a session unread without changing its activity timestamp, and notify the sidebar only when the flag flips.
  pub async fn mark_unread(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    let meta = {
      let _guard = self.lock_for(session_id).await;
      let Some(mut meta) = self.read_metadata_fresh(session_id)? else { return Ok(None) };
      if meta.has_unread {
        return Ok(Some(meta));
      }
      meta.has_unread = true;
      self.save_metadata_locked(&mut meta, false)?;
      self.read_metadata_fresh(session_id)?
    };
    let Some(meta) = meta else { return Ok(None) };
    let mut fields = Map::new();
    fields.insert("has_unread".to_string(), Value::Bool(true));
    self.broadcast_sidebar(session_id, charliebot_core::event_types::UNREAD_CHANGED, fields).await?;
    Ok(Some(meta))
  }

  /// Acquire the process-wide, non-reentrant task-tree control lock.
  pub async fn control_lock(&self) -> ControlGuard {
    self.control.lock().await
  }

  /// Check that `guard` proves ownership of this store's control lock.
  pub(crate) fn require_control_guard(&self, guard: &ControlGuard) -> StoreResult<()> {
    self.control.validate(guard).map_err(StoreError::new)
  }

  async fn update_field<F>(
    &self,
    session_id: &str,
    mutate: F,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>>
  where
    F: FnOnce(&mut SessionMetadata) -> StoreResult<()>,
  {
    self
      .mutate_fresh(session_id, false, true, |meta| {
        mutate(meta)?;
        meta.updated_at = updated_at;
        Ok(true)
      })
      .await
  }
}

fn stat_signature(path: &Path) -> Option<(i64, i64)> {
  let metadata = fs::metadata(path).ok()?;
  Some((metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000, metadata.size() as i64))
}

fn validate_slots(slots: &[MetadataSlot]) -> StoreResult<()> {
  let mut owners = Vec::new();
  let mut names = Vec::new();
  for slot in slots {
    if owners.contains(&(slot.owner, slot.on)) {
      return Err(StoreError::new(format!("metadata owner {:?} is already registered for {:?}", slot.owner, slot.on)));
    }
    owners.push((slot.owner, slot.on));
    let declared: &[&str] = match slot.on {
      MetadataFile::Session => &[
        "id",
        "name",
        "status",
        "has_unread",
        "has_running_tasks",
        "work_state",
        "has_pending_trigger",
        "pending_trigger_count",
        "next_trigger_at",
        "has_pending_plan_approval",
        "starred",
        "thinking_since",
        "run_backend",
        "created_at",
        "updated_at",
        "cc_session_id",
        "cc_session_started_at",
        "backend",
        "schedule_cron",
        "schedule_enabled",
        "schedule_next_run",
        "schedule_timezone",
        "schedule_project",
        "schedule_allow_failure",
        "parent_session_id",
        "successor_session_id",
        "schema_version",
        "task_parent_id",
        "profile",
        "task",
        "created_by_event",
        "origin_ref",
        "subtree_prompt_ref",
        "node_prompt_ref",
        "native_prompt_hash",
        "native_backend",
        "native_model",
        "round_ratings",
        "group",
        "archive_offset",
      ],
      MetadataFile::Thread => &[
        "id",
        "session_id",
        "description",
        "status",
        "created_at",
        "started_at",
        "completed_at",
        "pid",
        "pid_start",
        "exit_code",
        "branch_name",
        "base_branch",
        "repo_path",
        "worktree_path",
        "context",
        "backend",
        "model",
        "keep_worktree",
        "tried_backends",
        "task_type",
      ],
    };
    if let Some(after) = slot.after
      && !declared.contains(&after)
    {
      return Err(StoreError::new(format!("{after:?} is not a declared {:?} metadata field", slot.on)));
    }
    for field in &slot.fields {
      if declared.contains(&field.name) || names.contains(&(slot.on, field.name)) {
        return Err(StoreError::new(format!("metadata slot field {:?} collides with another field", field.name)));
      }
      names.push((slot.on, field.name));
    }
  }
  Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
  use super::*;
  use serde_json::json;

  use crate::hooks::EmptyRuntimeEventHooks;

  /// Convert built JSON events to the compact parsed form the event readers take.
  pub(crate) fn compact_events(values: &[Value]) -> Vec<crate::events::ChatEvent> {
    values.iter().map(crate::events::ChatEvent::from_value).collect()
  }

  /// An assistant text event with an explicit id (`conftest.assistant_event`).
  pub(crate) fn assistant_event(text: &str, event_id: &str) -> Value {
    json!({"id": event_id, "type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
  }

  /// An assistant text event without id or timestamp (`conftest.assistant_text_event`).
  pub(crate) fn assistant_text_event(text: &str) -> Value {
    json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
  }

  /// A text block then a tool_use block, stamped (`conftest.assistant_text_tool_use_event`).
  pub(crate) fn assistant_text_tool_use_event(
    text: &str,
    tool_name: &str,
    tool_input: Value,
    timestamp: &str,
  ) -> Value {
    json!({
      "type": "assistant",
      "message": {"content": [
        {"type": "text", "text": text},
        {"type": "tool_use", "name": tool_name, "input": tool_input},
      ]},
      "timestamp": timestamp,
    })
  }

  /// Two runs on one session with a queued user inside the first (`conftest.queued_user_reorder_events`).
  pub(crate) fn queued_user_reorder_events() -> Vec<Value> {
    vec![
      json!({"session_id": "opencode-session"}),
      json!({"id": "thinking-1", "type": "thinking", "content": "final thought"}),
      json!({"id": "tool-1", "type": "tool_use", "name": "Read", "input": {"file_path": "report.txt"}}),
      json!({"id": "queued-user", "type": "user", "content": "second question"}),
      json!({"id": "tool-result-1", "type": "tool_result", "content": "report contents"}),
      assistant_event("first conclusion", "assistant-1"),
      json!({"id": "done-1", "type": "master_done", "thinking_seconds": 4}),
      json!({"session_id": "opencode-session"}),
      assistant_event("second answer", "assistant-2"),
      json!({"id": "done-2", "type": "master_done", "thinking_seconds": 2}),
    ]
  }

  struct AccountLabels;

  impl AccountLabelKeeper for AccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("label").and_then(Value::as_str).map(str::to_string)
    }
  }

  struct AnchorReconciler;

  impl AnchorFieldReconciler for AnchorReconciler {
    fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
      attempted.native_backend.clone_from(&disk.native_backend);
      Ok(())
    }
  }

  struct SequenceListing;

  impl SequenceListingFields for SequenceListing {
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

  pub(crate) struct SequenceArchive;

  impl SequenceArchiveHook for SequenceArchive {
    fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  pub(crate) struct BindingMover;

  impl SequenceBindingMover for BindingMover {
    fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(false) })
    }
  }

  pub(crate) fn make_test_store() -> (tempfile::TempDir, Store) {
    make_test_store_with_runtime(Arc::new(EmptyRuntimeEventHooks))
  }

  pub(crate) fn make_test_store_with_runtime(runtime_events: Arc<dyn RuntimeEventHooks>) -> (tempfile::TempDir, Store) {
    let home = tempfile::tempdir().unwrap();
    let store = open_test_store(home.path(), runtime_events).unwrap();
    (home, store)
  }

  /// Open a store on an existing test home, the way a restarted process does.
  pub(crate) fn open_test_store(
    home: &std::path::Path,
    runtime_events: Arc<dyn RuntimeEventHooks>,
  ) -> StoreResult<Store> {
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      Vec::new(),
      StoreHooks {
        account_labels: Some(Arc::new(AccountLabels)),
        anchor_reconciler: Some(Arc::new(AnchorReconciler)),
        sequence_listing: Some(Arc::new(SequenceListing)),
        runtime_events: Some(runtime_events),
        sequence_archive: Some(Arc::new(SequenceArchive)),
        sequence_binding_mover: Some(Arc::new(BindingMover)),
      },
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  struct TestAccountLabels;

  impl AccountLabelKeeper for TestAccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("label").and_then(Value::as_str).map(str::to_string)
    }
  }

  struct TestAnchorReconciler;

  impl AnchorFieldReconciler for TestAnchorReconciler {
    fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
      attempted.native_backend.clone_from(&disk.native_backend);
      Ok(())
    }
  }

  struct TestSequenceListing;

  impl SequenceListingFields for TestSequenceListing {
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

  struct TestSequenceArchive;

  impl SequenceArchiveHook for TestSequenceArchive {
    fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  struct TestSequenceBindingMover;

  impl SequenceBindingMover for TestSequenceBindingMover {
    fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(false) })
    }
  }

  fn explicit_test_hooks() -> StoreHooks {
    StoreHooks {
      account_labels: Some(Arc::new(TestAccountLabels)),
      anchor_reconciler: Some(Arc::new(TestAnchorReconciler)),
      sequence_listing: Some(Arc::new(TestSequenceListing)),
      runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
      sequence_archive: Some(Arc::new(TestSequenceArchive)),
      sequence_binding_mover: Some(Arc::new(TestSequenceBindingMover)),
    }
  }

  #[test]
  fn construction_fails_when_required_hooks_are_missing() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let error = Store::new(cfg, Vec::new(), StoreHooks::default()).err().unwrap();
    assert!(error.to_string().contains("account label keeper"));
  }

  #[tokio::test]
  async fn slot_fields_at_writes_fields_and_timestamp_together_and_keeps_a_concurrent_field() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let slots = vec![MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: None,
      fields: vec![
        charliebot_core::metadata_slot::SlotField { name: "slack_origin", default: Value::Null },
        charliebot_core::metadata_slot::SlotField { name: "slack_watermark_ts", default: Value::Null },
      ],
    }];
    let store = Store::new(cfg, slots.clone(), explicit_test_hooks()).unwrap();
    let meta = SessionMetadata::new("synthetic-thread", "Synthetic thread", crate::metadata::TaskProfile::Manager);
    let path = store.metadata_path(&meta.id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, meta.to_json_string(&slots, true)).unwrap();

    let updated_at = UtcDateTime::parse("2026-10-19T12:34:56Z").unwrap();
    let (watermark_update, origin_update) = tokio::join!(
      store.update_slot_fields_at(
        "synthetic-thread",
        "slack",
        Map::from_iter([("slack_watermark_ts".to_string(), json!("123.456"))]),
        updated_at,
      ),
      store.update_slot_fields(
        "synthetic-thread",
        "slack",
        Map::from_iter([("slack_origin".to_string(), json!({"channel_id": "C0SYNTHETIC"}))]),
      ),
    );
    watermark_update.unwrap().unwrap();
    origin_update.unwrap().unwrap();

    let saved = store.read_metadata_fresh("synthetic-thread").unwrap().unwrap();
    assert_eq!(saved.extra("slack_watermark_ts"), Some(&json!("123.456")));
    assert_eq!(saved.extra("slack_origin"), Some(&json!({"channel_id": "C0SYNTHETIC"})));
    assert_eq!(saved.updated_at, updated_at);
  }

  #[tokio::test]
  async fn expired_cache_entries_revalidate_the_disk_signature() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let store = Store::new(cfg, Vec::new(), explicit_test_hooks()).unwrap();
    let mut meta = SessionMetadata::new("synthetic-id", "seed", crate::metadata::TaskProfile::Manager);
    let path = store.metadata_path(&meta.id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, meta.to_json_string(&[], true)).unwrap();
    let signature = stat_signature(&path).unwrap();
    store.metadata_cache.lock().unwrap().insert(
      meta.id.clone(),
      CachedMetadata {
        value: Arc::new(meta.clone()),
        at: Instant::now() - METADATA_CACHE_TTL - Duration::from_secs(1),
        signature: Some(signature),
        identity: Arc::new(()),
      },
    );
    assert_eq!(store.get_session(&meta.id).await.unwrap().unwrap().name, "seed");

    meta.name = "fresh metadata".to_string();
    fs::write(&path, meta.to_json_string(&[], true)).unwrap();
    store.metadata_cache.lock().unwrap().get_mut(&meta.id).unwrap().at =
      Instant::now() - METADATA_CACHE_TTL - Duration::from_secs(1);
    assert_eq!(store.get_session(&meta.id).await.unwrap().unwrap().name, "fresh metadata");
  }

  #[test]
  fn stalled_metadata_stat_does_not_block_other_sessions_and_keeps_a_concurrent_write() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let store = Arc::new(Store::new(cfg, Vec::new(), explicit_test_hooks()).unwrap());
    for (id, name) in [("synthetic-a", "Session A"), ("synthetic-b", "Session B")] {
      let meta = SessionMetadata::new(id, name, crate::metadata::TaskProfile::Manager);
      let path = store.metadata_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, meta.to_json_string(&[], true)).unwrap();
    }

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(store.get_session("synthetic-a")).unwrap().unwrap();
    runtime.block_on(store.get_session("synthetic-b")).unwrap().unwrap();
    let expired_at = Instant::now() - METADATA_CACHE_TTL - Duration::from_secs(1);
    for id in ["synthetic-a", "synthetic-b"] {
      store.metadata_cache.lock().unwrap().get_mut(id).unwrap().at = expired_at;
    }

    let stat_path = store.metadata_path("synthetic-a");
    let stat_entered = Arc::new(std::sync::Barrier::new(2));
    let release_stat = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&stat_entered);
    let hook_release = Arc::clone(&release_stat);
    *store.metadata_stat_hook.lock().unwrap() = Some(Arc::new(move |path| {
      if path == stat_path.as_path() {
        hook_entered.wait();
        hook_release.wait();
      }
    }));

    let reader_store = Arc::clone(&store);
    let reader_a = std::thread::spawn(move || {
      tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(reader_store.get_session("synthetic-a"))
    });
    stat_entered.wait();

    let (reader_b_tx, reader_b_rx) = std::sync::mpsc::channel();
    let reader_store = Arc::clone(&store);
    let reader_b = std::thread::spawn(move || {
      let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
      reader_b_tx.send(runtime.block_on(reader_store.get_session("synthetic-b"))).unwrap();
    });
    let result_b = match reader_b_rx.recv_timeout(Duration::from_secs(1)) {
      Ok(result) => result,
      Err(error) => {
        release_stat.wait();
        let _reader_a_result = reader_a.join().unwrap().unwrap();
        reader_b.join().unwrap();
        panic!("session B metadata read waited for session A stat: {error}");
      }
    };
    assert_eq!(result_b.unwrap().unwrap().name, "Session B");
    reader_b.join().unwrap();

    let updated_at = UtcDateTime::parse("2026-01-06T00:00:00Z").unwrap();
    let updated = runtime.block_on(store.rename_session_at("synthetic-a", "Updated A", updated_at)).unwrap().unwrap();
    release_stat.wait();
    let raced = reader_a.join().unwrap().unwrap().unwrap();
    assert_eq!(raced.name, updated.name);
    assert_eq!(raced.updated_at, updated.updated_at);

    let next = runtime.block_on(store.get_session("synthetic-a")).unwrap().unwrap();
    assert_eq!(next.name, updated.name);
    assert_eq!(next.updated_at, updated.updated_at);
  }
}
