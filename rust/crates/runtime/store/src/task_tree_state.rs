//! Task facts and cache records shared by Store and task-tree logic.

use std::collections::{HashMap, HashSet};
use std::ops::Index;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use charliebot_core::fsutil::FileBytes;

use crate::compact::CompactValue;
use crate::event_types::{ChatEvent, FileIdentity, SharedChatEvent};
use crate::metadata::{SessionMetadata, UtcDateTime};
use crate::run_facts::RunFacts;
use crate::{StoreError, StoreResult};

/// The task lifecycle folded from a node's durable event history.
pub type TaskState = String;

/// A fact snapshot used by tree, gate, and pending-input readers.
#[derive(Clone, Debug)]
pub struct TaskFacts {
  pub task_state: TaskState,
  pub has_user_request: bool,
  pub run_outcomes: HashMap<String, String>,
  #[doc(hidden)]
  pub run_outcome_string_none_ids: HashSet<String>,
  pub run_finish_positions: HashMap<String, usize>,
  pub confirmed_input_ids: HashSet<String>,
  pub input_candidates: Vec<SharedChatEvent>,
  pub imported_pending_ids: HashSet<String>,
  pub boundary_index: Option<usize>,
  pub boundary_open: bool,
  pub close_events: Vec<SharedChatEvent>,
  pub close_requests: Vec<SharedChatEvent>,
  /// The request id of every `task_close_requested` line. Unlike `close_requests`, it keeps a line whose id repeats and a
  /// line before a clone start, as a scan of the raw history does.
  pub close_request_ids: HashSet<String>,
  /// The first `task_closed` line of each request id, over the same lines as `close_request_ids`.
  pub first_close_by_request: HashMap<String, SharedChatEvent>,
  pub delivered_reports: HashSet<(String, String)>,
  pub events_by_id: EventIdIndex,
  pub covered: usize,
}

/// Insertion-ordered references to events, indexed by a content hash so the index does not duplicate each event id.
///
/// A checkpoint answers most of an index without parsing it. [`EventIdIndex::from_prefix`] keeps the checkpoint's
/// event lines and the hash table that points into them, so a membership test costs a binary search and one line's
/// parse. The whole prefix is parsed only when a reader needs a reference to each event
/// ([`EventIdIndex::values`], [`EventIdIndex::iter`], [`EventIdIndex::get`] and the `Index` form). A boot that folds a
/// checkpoint plus a small appended tail never pays for the covered history.
#[derive(Clone, Debug, Default)]
pub struct EventIdIndex {
  pub(crate) by_hash: HashMap<u64, EventIdBucket>,
  ordered: Vec<SharedChatEvent>,
  /// The checkpoint's covered events, unparsed until a reader asks for them by reference.
  prefix: Option<Arc<PrefixIndex>>,
  /// The prefix's events, parsed on the first reference-based read.
  materialized: OnceLock<Vec<SharedChatEvent>>,
}

#[derive(Clone, Debug)]
pub(crate) struct EventIdBucket {
  first: usize,
  collisions: Option<Vec<usize>>,
}

/// One checkpoint line decoded to the value the fold keeps. The checkpoint writer owns the line format, so it hands
/// its decoder to the readers instead of either reader depending on the other.
pub(crate) type LineDecoder = fn(&[u8], &crate::compact::ParseKeys) -> Option<CompactValue>;

/// One checkpoint's event lines and the sorted `(id hash, line offset)` table that points into them.
#[derive(Debug)]
pub(crate) struct PrefixIndex {
  pub(crate) bytes: Arc<FileBytes>,
  /// The event lines, one per kept event in insertion order.
  pub(crate) lines_start: usize,
  pub(crate) lines_end: usize,
  /// The table: `events` entries of a little-endian `u64` hash and `u32` line offset, sorted by hash then offset.
  pub(crate) table_start: usize,
  pub(crate) events: usize,
  /// The checkpoint line format's decoder, supplied by the scan that read the file.
  pub(crate) decode: LineDecoder,
}

impl PrefixIndex {
  /// The line a table entry at `entry` points at.
  fn line_at(&self, entry: usize) -> &[u8] {
    let at = self.table_start + entry * ENTRY_BYTES;
    let offset =
      u32::from_le_bytes(self.bytes[at + 8..at + 12].try_into().expect("a table entry holds four offset bytes"));
    let start = self.lines_start + offset as usize;
    let end = self.bytes[start..].iter().position(|byte| *byte == b'\n').map_or(self.lines_end, |at| start + at);
    &self.bytes[start..end]
  }

  /// The table entry at `at`, or `None` past the last one.
  fn entry(&self, at: usize) -> Option<(u64, usize)> {
    if at >= self.events {
      return None;
    }
    let offset = self.table_start + at * ENTRY_BYTES;
    let hash = u64::from_le_bytes(self.bytes[offset..offset + 8].try_into().expect("a table entry holds a hash"));
    Some((hash, at))
  }

  /// The first table entry whose hash is at least `hash`.
  fn lower_bound(&self, hash: u64) -> usize {
    let (mut low, mut high) = (0, self.events);
    while low < high {
      let middle = (low + high) / 2;
      match self.entry(middle) {
        Some((value, _)) if value < hash => low = middle + 1,
        _ => high = middle,
      }
    }
    low
  }

  /// The event whose id is `id`, found through the table without parsing the other lines.
  fn find(&self, id: &str) -> Option<SharedChatEvent> {
    let hash = event_id_hash(id);
    let mut at = self.lower_bound(hash);
    while let Some((value, entry)) = self.entry(at) {
      if value != hash {
        return None;
      }
      let event = (self.decode)(self.line_at(entry), &crate::compact::ParseKeys::default())?;
      if event.get("id").and_then(CompactValue::as_str) == Some(id) {
        return Some(Arc::new(event));
      }
      at += 1;
    }
    None
  }
}

/// One table entry's byte length.
const ENTRY_BYTES: usize = 12;

impl EventIdIndex {
  pub fn len(&self) -> usize {
    self.prefix.as_ref().map_or(0, |prefix| prefix.events) + self.ordered.len()
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// The index a checkpoint's covered events make, unparsed until a reference-based read asks for them.
  pub(crate) fn from_prefix(index: PrefixIndex) -> Self {
    Self { prefix: Some(Arc::new(index)), ..Self::default() }
  }

  pub fn get(&self, id: &str) -> Option<&SharedChatEvent> {
    self.get_hashed(id, event_id_hash)
  }

  pub(crate) fn get_hashed(&self, id: &str, hash_id: impl FnOnce(&str) -> u64) -> Option<&SharedChatEvent> {
    if let Some(bucket) = self.by_hash.get(&hash_id(id)) {
      let first = &self.ordered[bucket.first];
      if event_id(first) == id {
        return Some(first);
      }
      if let Some(collisions) = bucket.collisions.as_ref()
        && let Some(found) = collisions.iter().map(|index| &self.ordered[*index]).find(|event| event_id(event) == id)
      {
        return Some(found);
      }
    }
    self.prefix_events().iter().find(|event| event_id(event) == id)
  }

  /// Whether an event with this id is in the index. A checkpoint's prefix answers from its table, without parsing.
  pub fn contains_key(&self, id: &str) -> bool {
    if let Some(prefix) = self.prefix.as_ref()
      && prefix.find(id).is_some()
    {
      return true;
    }
    self.get(id).is_some()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &SharedChatEvent)> {
    self.values().map(|event| (event_id(event), event))
  }

  pub fn values(&self) -> impl Iterator<Item = &SharedChatEvent> {
    self.prefix_events().iter().chain(self.ordered.iter())
  }

  /// The checkpoint's covered events in insertion order, parsed on the first reference-based read.
  fn prefix_events(&self) -> &[SharedChatEvent] {
    self.materialized.get_or_init(|| match self.prefix.as_ref() {
      Some(prefix) => IndexSeed::parse(prefix),
      None => Vec::new(),
    })
  }

  pub(crate) fn insert(&mut self, event: SharedChatEvent) -> bool {
    self.insert_hashed(event, event_id_hash)
  }

  pub(crate) fn insert_hashed(&mut self, event: SharedChatEvent, hash_id: impl FnOnce(&str) -> u64) -> bool {
    let id = event_id(&event);
    // The checkpoint's covered events came first, so an id they hold is a repeat.
    if let Some(prefix) = self.prefix.as_ref()
      && prefix.find(id).is_some()
    {
      return false;
    }
    let hash = hash_id(id);
    let index = self.ordered.len();
    match self.by_hash.get_mut(&hash) {
      Some(bucket) => {
        if event_id(&self.ordered[bucket.first]) == id {
          return false;
        }
        if let Some(collisions) = bucket.collisions.as_ref()
          && collisions.iter().any(|index| event_id(&self.ordered[*index]) == id)
        {
          return false;
        }
        self.ordered.push(event);
        match &mut bucket.collisions {
          Some(collisions) => collisions.push(index),
          None => bucket.collisions = Some(vec![index]),
        }
      }
      None => {
        self.ordered.push(event);
        self.by_hash.insert(hash, EventIdBucket { first: index, collisions: None });
      }
    }
    true
  }
}

/// A prefix's events parsed into the index form, once: each kept line with a string id, first occurrence of an id only,
/// exactly as the whole-log fold inserts them.
struct IndexSeed;

impl IndexSeed {
  fn parse(prefix: &PrefixIndex) -> Vec<SharedChatEvent> {
    let lines = crate::ndjson::split_lines_of(&prefix.bytes[prefix.lines_start..prefix.lines_end], &prefix.decode);
    let mut seen = HashSet::with_capacity(lines.len());
    let mut events = Vec::with_capacity(prefix.events);
    for event in lines {
      let Some(id) = event.get("id").and_then(CompactValue::as_str) else { continue };
      if seen.insert(id.to_string()) {
        events.push(Arc::new(event));
      }
    }
    events
  }
}

impl Index<&str> for EventIdIndex {
  type Output = ChatEvent;

  fn index(&self, id: &str) -> &Self::Output {
    self.get(id).expect("indexed event id is present").as_ref()
  }
}

fn event_id(event: &ChatEvent) -> &str {
  event.get("id").and_then(CompactValue::as_str).expect("event id index only stores events with string ids")
}

/// A stable 64-bit FNV-1a hash of an event id.
///
/// The checkpoint's sorted table stores these values, so the function must not change between builds. A table written
/// by one build is read by the next, and a hash that moved would make every lookup miss. That is why the index does
/// not use `DefaultHasher`, whose algorithm Rust does not promise to keep.
pub(crate) fn event_id_hash(id: &str) -> u64 {
  let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
  for byte in id.bytes() {
    hash ^= u64::from(byte);
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  hash
}

/// A node's task facts, folded from the history covered by the store memo.
#[derive(Clone, Debug)]
pub struct NodeFacts {
  pub(crate) task: Result<TaskFacts, String>,
}

impl NodeFacts {
  /// The task fold, or the error returned for the same history.
  pub fn task(&self) -> StoreResult<&TaskFacts> {
    self.task.as_ref().map_err(|message| StoreError::new(message.clone()))
  }
}

impl Default for NodeFacts {
  fn default() -> Self {
    Self { task: Ok(TaskFacts::default()) }
  }
}

/// The live history identity covered by a node-facts memo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeFactsKey {
  Cached { generation: u64, covered: usize },
  File(FileIdentity),
}

/// A node's folded facts and the history they cover.
#[derive(Clone)]
pub(crate) struct CachedNodeFacts {
  pub(crate) key: NodeFactsKey,
  pub(crate) list_identity: Arc<()>,
  pub(crate) archived: usize,
  pub(crate) covered: usize,
  pub(crate) facts: Arc<NodeFacts>,
}

/// A node's [`NodeState`], the history it covers and its version. A new version marks every change of the state.
#[derive(Clone)]
pub(crate) struct CachedNodeState {
  pub(crate) key: NodeStateKey,
  pub(crate) archived: usize,
  pub(crate) version: u64,
}

/// The live history a [`CachedNodeState`] covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeStateKey {
  /// The first `covered` events of the live cache at `generation`, keyed as [`CachedNodeFacts`] is. A read after
  /// appends extends the fold by the appended suffix.
  Cached { generation: u64, covered: usize },
  /// The log file with this identity, after an eviction dropped the cached list. The state is sealed: a read that finds
  /// another file, or a warm cache, folds the history again.
  File(FileIdentity),
}

/// A node's memoized [`TaskTreeActivity`]: the run-record generation and the history identity it was derived at.
///
/// The activity memo holds the same key. A verdict that read `/proc` is never stored.
#[derive(Clone)]
pub(crate) struct CachedActivity {
  pub(crate) records_generation: u64,
  pub(crate) identity: Option<FileIdentity>,
  pub(crate) activity: TaskTreeActivity,
}

/// A node's run facts folded from its history at one identity. The outcomes memo holds the same key.
#[derive(Clone)]
pub(crate) struct CachedRunFactsAt {
  pub(crate) identity: Option<FileIdentity>,
  pub(crate) facts: Arc<RunFacts>,
}

/// The node's current running and queued work verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTreeActivity {
  pub has_running_tasks: bool,
  pub work_state: String,
}

/// One child row of a task-tree page: the node's derived facts, read in one pass over the history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTreeRow {
  pub session_id: String,
  pub task_state: String,
  pub work_state: String,
  pub archived: bool,
  pub child_count: usize,
  pub open_descendant_count: usize,
  pub running_descendant_count: usize,
}

/// One computed task-tree page kept for the repeat requests whose inputs still hold.
///
/// The rows change when the index build, a run record or any history the row pass read changes. The stored identities
/// name exactly the histories that pass read, so a repeat request re-reads those and serves the rows when every
/// identity still matches.
pub(crate) struct CachedTreePage {
  /// The index build whose metadata and structure the rows were rendered from. A later build may carry the same
  /// revision while a rename or an unread flip changed a row's fields, so the identity is the build, not the revision.
  pub(crate) build_id: u64,
  pub(crate) records_generation: u64,
  pub(crate) parent_id: Option<String>,
  pub(crate) include_archived: bool,
  pub(crate) after: Option<(UtcDateTime, String)>,
  pub(crate) limit: usize,
  /// The histories the row pass read, in read order, shared so a hit re-checks them without copying the list.
  pub(crate) ids: Arc<Vec<String>>,
  pub(crate) identity_values: Vec<Option<FileIdentity>>,
  pub(crate) rows: Vec<TaskTreeRow>,
  pub(crate) has_more: bool,
}

pub(crate) struct TreeIndex {
  pub(crate) metas: HashMap<String, Arc<SessionMetadata>>,
  pub(crate) children: HashMap<Option<String>, Vec<String>>,
  pub(crate) revision: String,
  pub(crate) derived_archived_ids: Arc<Vec<String>>,
  pub(crate) archive_by_id: HashMap<String, bool>,
  /// Identifies this build, so a reader can tell one index apart from a later build that carries the same revision.
  pub(crate) build_id: u64,
  /// Every input this build read, sorted by session id: the session directories, their metadata files and their
  /// live histories. A refresh that reads the same inputs serves this index instead of building it again.
  pub(crate) inputs: Vec<TreeIndexInput>,
}

/// What one session contributes to the tree index: the two files the index's metadata and task state come from.
///
/// The metadata file carries the parent, the profile, the status and the archive offset; the live history's identity
/// carries the newest lifecycle fact, and the archive segments the state walk reads behind it. The archive segments
/// move only in the write that replaces the live log and raises the archive offset, so both of them are read here. Equal
/// inputs on every session mean an equal index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TreeIndexInput {
  pub(crate) session_id: String,
  pub(crate) metadata: Option<(i64, i64)>,
  pub(crate) history: Option<FileIdentity>,
}

/// The most ancestors one chain walk follows before it reports the chain as overlong.
pub(crate) const TASK_ANCESTOR_HOP_LIMIT: usize = 1000;

/// The metadata of one indexed node; a node the index lacks answers its 404.
pub(crate) fn index_meta<'a>(index: &'a TreeIndex, session_id: &str) -> StoreResult<&'a SessionMetadata> {
  index
    .metas
    .get(session_id)
    .map(Arc::as_ref)
    .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
}

/// The ancestors of a node, nearest first.
pub(crate) fn ancestor_ids(index: &TreeIndex, session_id: &str) -> StoreResult<Vec<String>> {
  let mut chain = Vec::new();
  let mut seen = HashSet::from([session_id.to_string()]);
  let mut current =
    index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
  while let Some(parent_id) = current.task_parent_id.as_deref() {
    if !seen.insert(parent_id.to_string()) {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {parent_id}")]));
    }
    current = index.metas.get(parent_id).ok_or_else(|| StoreError::not_found(format!("task {parent_id} not found")))?;
    chain.push(parent_id.to_string());
    if chain.len() > TASK_ANCESTOR_HOP_LIMIT {
      return Err(StoreError::conflict(vec![format!(
        "ancestor chain of {session_id} exceeds {TASK_ANCESTOR_HOP_LIMIT} hops"
      )]));
    }
  }
  Ok(chain)
}

/// Whether the task sits strictly below the caller's task: the agent subtree test that reparent and archive share.
pub(crate) fn task_below_caller(index: &TreeIndex, caller_task: &str, session_id: &str) -> StoreResult<bool> {
  Ok(ancestor_ids(index, session_id)?.iter().any(|id| id == caller_task))
}

/// Whether the node is the caller's task or sits below it: the subtree test that reparent scope and cron binding share.
pub(crate) fn within_caller_subtree(index: &TreeIndex, caller_task: &str, session_id: &str) -> StoreResult<bool> {
  Ok(session_id == caller_task || ancestor_ids(index, session_id)?.iter().any(|id| id == caller_task))
}

pub(crate) struct CachedTreeIndex {
  pub(crate) generation: u64,
  pub(crate) built_at: Instant,
  pub(crate) index: Arc<TreeIndex>,
}

pub(crate) struct CachedTaskStateTail {
  pub(crate) identity: Option<FileIdentity>,
  pub(crate) state: String,
}

/// Which side of the history identity's stat a test hook observes.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityStage {
  BeforeStat,
  AfterStat,
}

/// A test hook that observes each history identity read of a session.
#[cfg(test)]
pub(crate) type IdentityHook = Arc<dyn Fn(&str, IdentityStage) + Send + Sync>;

/// The newest `prompt_changed` fact of each scope in a live log.
#[derive(Clone, Default)]
pub(crate) struct LivePromptChanges {
  pub(crate) subtree: Option<SharedChatEvent>,
  pub(crate) node: Option<SharedChatEvent>,
}

/// A sealed boot scan's prompt facts and the log identity they cover.
pub(crate) struct SealedPromptChanges {
  pub(crate) identity: FileIdentity,
  pub(crate) changes: LivePromptChanges,
}
