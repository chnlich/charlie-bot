//! Task facts and cache records shared by Store and task-tree logic.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Index;
use std::sync::Arc;
use std::time::Instant;

use crate::compact::CompactValue;
use crate::event_types::{ChatEvent, FileIdentity, SharedChatEvent};
use crate::metadata::SessionMetadata;
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
  pub delivered_reports: HashSet<(String, String)>,
  pub events_by_id: EventIdIndex,
  pub covered: usize,
}

/// Insertion-ordered references to events, indexed by a content hash so the index does not duplicate each event id.
#[derive(Clone, Debug, Default)]
pub struct EventIdIndex {
  pub(crate) by_hash: HashMap<u64, EventIdBucket>,
  ordered: Vec<SharedChatEvent>,
}

#[derive(Clone, Debug)]
pub(crate) struct EventIdBucket {
  first: usize,
  collisions: Option<Vec<usize>>,
}

impl EventIdIndex {
  pub fn len(&self) -> usize {
    self.ordered.len()
  }

  pub fn is_empty(&self) -> bool {
    self.ordered.is_empty()
  }

  pub fn get(&self, id: &str) -> Option<&SharedChatEvent> {
    self.get_hashed(id, event_id_hash)
  }

  pub(crate) fn get_hashed(&self, id: &str, hash_id: impl FnOnce(&str) -> u64) -> Option<&SharedChatEvent> {
    let bucket = self.by_hash.get(&hash_id(id))?;
    let first = &self.ordered[bucket.first];
    if event_id(first) == id {
      return Some(first);
    }
    bucket.collisions.as_ref()?.iter().map(|index| &self.ordered[*index]).find(|event| event_id(event) == id)
  }

  pub fn contains_key(&self, id: &str) -> bool {
    self.get(id).is_some()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &SharedChatEvent)> {
    self.ordered.iter().map(|event| (event_id(event), event))
  }

  pub fn values(&self) -> impl Iterator<Item = &SharedChatEvent> {
    self.ordered.iter()
  }

  pub(crate) fn insert(&mut self, event: SharedChatEvent) -> bool {
    self.insert_hashed(event, event_id_hash)
  }

  pub(crate) fn insert_hashed(&mut self, event: SharedChatEvent, hash_id: impl FnOnce(&str) -> u64) -> bool {
    let id = event_id(&event);
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

impl Index<&str> for EventIdIndex {
  type Output = ChatEvent;

  fn index(&self, id: &str) -> &Self::Output {
    self.get(id).expect("indexed event id is present").as_ref()
  }
}

fn event_id(event: &ChatEvent) -> &str {
  event.get("id").and_then(CompactValue::as_str).expect("event id index only stores events with string ids")
}

fn event_id_hash(id: &str) -> u64 {
  let mut hash = DefaultHasher::new();
  id.hash(&mut hash);
  hash.finish()
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
/// Python's `_activity_memo` holds the same key. A verdict that read `/proc` is never stored.
#[derive(Clone)]
pub(crate) struct CachedActivity {
  pub(crate) records_generation: u64,
  pub(crate) identity: Option<FileIdentity>,
  pub(crate) activity: TaskTreeActivity,
}

/// A node's run facts folded from its history at one identity. Python's `_outcomes_memo` holds the same key.
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

pub(crate) struct TreeIndex {
  pub(crate) metas: HashMap<String, Arc<SessionMetadata>>,
  pub(crate) children: HashMap<Option<String>, Vec<String>>,
  pub(crate) revision: String,
  pub(crate) derived_archived_ids: Vec<String>,
  pub(crate) archive_by_id: HashMap<String, bool>,
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
