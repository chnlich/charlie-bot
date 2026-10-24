//! Durable task nodes, fact-derived tree state, and delegation authorization.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Deref;
use std::ops::Index;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use charliebot_core::{event_types, fsutil};
use serde_json::{Map, Value, json};

use crate::ChatEvent;
use crate::compact::CompactValue;

use crate::control_events::{self, ACTOR_AGENT, ACTOR_SYSTEM, ACTOR_USER};
use crate::events::{FileIdentity, LiveHistory, SharedChatEvent};
use crate::metadata::{EventRef, SessionMetadata, SessionStatus, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use crate::run_records::{CachedRunFacts, HostBoot, RunDisplayState, RunFacts, RunOutcomeKind};
use crate::store::{CachedNodeFacts, NodeFactsKey};
use crate::{ControlGuard, Store, StoreError, StoreResult};

const TREE_INDEX_TTL: Duration = Duration::from_secs(2);
const LIFECYCLE_TAIL_LINES: usize = 1000;
const LIFECYCLE_TAIL_CHUNK_BYTES: usize = 1 << 16;
const LIFECYCLE_HEAD_PROOF_BYTES: usize = 256;
const TASK_ANCESTOR_HOP_LIMIT: usize = 1000;
// Bound on the non-archived descendants one subtree walk may hold; archived ones do not count.
const TASK_DESCENDANT_NODE_LIMIT: usize = 1000;
const SUCCESSOR_HOP_LIMIT: usize = 100;
const TASK_FACTS_STOP_CHECK_INTERVAL: usize = 256;
/// The refusal for an agent create outside its own open manager task (`task_sessions.AGENT_CREATE_SCOPE_REFUSAL`).
pub const AGENT_CREATE_SCOPE_REFUSAL: &str = "an agent may only create a task directly under its own open manager task";
/// The refusal for a task create without a request id (`task_sessions.TASK_CREATE_REQUEST_ID_REQUIRED`).
pub const TASK_CREATE_REQUEST_ID_REQUIRED: &str = "request_id is required for task creation";

/// One create request, including its stable identity inputs.
#[derive(Clone, Debug)]
pub struct CreateTask {
  pub request_id: String,
  pub task_parent_id: Option<String>,
  pub profile: TaskProfile,
  pub task: Option<TaskSpec>,
  pub name: Option<String>,
  pub backend: Option<String>,
  pub group: Option<String>,
  pub session_id: Option<String>,
}

/// The verified source of a task create request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCaller {
  Operator,
  System,
  Agent { session_id: String },
}

/// The task fields of one PATCH body. A `None` field is absent from the body and changes nothing; `Some(None)` is an
/// explicit null, which clears the field where the Python model allows it. `name` has no explicit null: a `None` name
/// changes nothing.
#[derive(Clone, Debug, Default)]
pub struct PatchTask {
  pub name: Option<String>,
  pub task_parent_id: Option<Option<String>>,
  pub profile: Option<Option<TaskProfile>>,
  pub task: Option<Option<TaskSpec>>,
  pub subtree_prompt: Option<Option<String>>,
  pub node_prompt: Option<Option<String>>,
}

/// The structural guards that the task dispatcher answers for a node (`TaskTreeManager.pending_input_blockers` and
/// `TaskInputDispatcher.undelivered_report_blockers`). The store does not own them: the dispatcher reads the input
/// boundary and the report deliveries through the store, so the dispatcher implements this trait and the caller passes
/// it in.
pub trait StructuralBlockers: Send + Sync {
  /// The blockers of the node's unprocessed task inputs; empty when it has none.
  fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>>;
  /// One blocker for each close report that the node still owes its fixed recipient.
  fn undelivered_report_blockers(&self, session_id: &str) -> StoreResult<Vec<String>>;
}

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
  by_hash: HashMap<u64, EventIdBucket>,
  ordered: Vec<SharedChatEvent>,
}

#[derive(Clone, Debug)]
struct EventIdBucket {
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

  fn get_hashed(&self, id: &str, hash_id: impl FnOnce(&str) -> u64) -> Option<&SharedChatEvent> {
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

  fn insert(&mut self, event: SharedChatEvent) -> bool {
    self.insert_hashed(event, event_id_hash)
  }

  fn insert_hashed(&mut self, event: SharedChatEvent, hash_id: impl FnOnce(&str) -> u64) -> bool {
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
  task: Result<TaskFacts, String>,
}

impl NodeFacts {
  /// The task fold, or the error that [`Store::facts_of`] returns for the same history.
  pub fn task(&self) -> StoreResult<&TaskFacts> {
    self.task.as_ref().map_err(|message| StoreError::new(message.clone()))
  }
}

impl Default for NodeFacts {
  fn default() -> Self {
    Self { task: Ok(TaskFacts::default()) }
  }
}

/// A read-path view of the facts already retained for this node.
///
/// Keeping a second Run-facts fold here duplicated the task facts' outcomes and stop requests for every read node.
/// Sharing the fact snapshot preserves one fold, one invalidation key, and one owner for the event history.
#[derive(Clone, Debug)]
pub struct NodeState {
  facts: Arc<NodeFacts>,
  pub runs: Arc<RunFacts>,
  malformed_task_events: Option<Arc<Vec<SharedChatEvent>>>,
}

impl NodeState {
  /// The task state, or the error that [`Store::facts_of`] returns for the same history.
  pub fn task_state(&self) -> StoreResult<&str> {
    Ok(self.facts.task()?.task_state.as_str())
  }

  fn has_user_request(&self) -> bool {
    self.facts.task.as_ref().is_ok_and(|task| task.has_user_request)
  }

  pub fn display_state(
    &self,
    run: &crate::run_records::RunRecord,
    boot: &mut HostBoot,
  ) -> StoreResult<RunDisplayState> {
    let withheld = match (self.facts.task.as_ref(), self.malformed_task_events.as_deref()) {
      (Ok(task), None) => has_withheld_reason(task.events_by_id.values().map(|event| event.as_ref()), &run.id),
      (Err(_), Some(events)) => has_withheld_reason(events.iter().map(|event| event.as_ref()), &run.id),
      (Err(_), None) => unreachable!("an invalid task fold retains the source events for Run display-state reads"),
      (Ok(_), Some(_)) => unreachable!("valid task facts do not retain a fallback event view"),
    };
    self.runs.display_state(run, boot, withheld)
  }
}

fn has_withheld_reason<'a>(events: impl Iterator<Item = &'a ChatEvent>, run_id: &str) -> bool {
  let mut withheld = None;
  for event in events {
    if event.get("type").and_then(CompactValue::as_str) == Some("run_launch_withheld")
      && event.get("run_id").and_then(CompactValue::as_str) == Some(run_id)
    {
      withheld = event.get("reason").and_then(CompactValue::as_str).filter(|reason| !reason.is_empty());
    }
  }
  withheld.is_some()
}

impl Deref for NodeState {
  type Target = NodeFacts;

  fn deref(&self) -> &Self::Target {
    &self.facts
  }
}

/// A node's [`NodeState`], the history it covers and its version. A new version marks every change of the state.
#[derive(Clone)]
pub(crate) struct CachedNodeState {
  key: NodeStateKey,
  archived: usize,
  version: u64,
}

/// The live history a [`CachedNodeState`] covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeStateKey {
  /// The first `covered` events of the live cache at `generation`, keyed as [`CachedNodeFacts`] is. A read after
  /// appends extends the fold by the appended suffix.
  Cached { generation: u64, covered: usize },
  /// The log file with this identity, after an eviction dropped the cached list. The state is sealed: a read that finds
  /// another file, or a warm cache, folds the history again.
  File(FileIdentity),
}

/// A node's memoized [`TaskTreeActivity`] and the inputs it was derived from: the run-record generation and, for a
/// node with runs, the version of the [`NodeState`] it read.
#[derive(Clone)]
pub(crate) struct CachedActivity {
  records_generation: u64,
  state_version: Option<u64>,
  activity: TaskTreeActivity,
}

impl Default for TaskFacts {
  fn default() -> Self {
    Self {
      task_state: "open".to_string(),
      has_user_request: false,
      run_outcomes: HashMap::new(),
      run_outcome_string_none_ids: HashSet::new(),
      run_finish_positions: HashMap::new(),
      confirmed_input_ids: HashSet::new(),
      input_candidates: Vec::new(),
      imported_pending_ids: HashSet::new(),
      boundary_index: None,
      boundary_open: true,
      close_events: Vec::new(),
      close_requests: Vec::new(),
      delivered_reports: HashSet::new(),
      events_by_id: EventIdIndex::default(),
      covered: 0,
    }
  }
}

impl TaskFacts {
  /// The terminal outcome represented by this task-facts snapshot.
  pub fn terminal_run_outcome(&self, run_id: &str) -> StoreResult<Option<RunOutcomeKind>> {
    let Some(outcome) = self.run_outcomes.get(run_id) else {
      return Ok(None);
    };
    if outcome == "None" && !self.run_outcome_string_none_ids.contains(run_id) {
      return Ok(None);
    }
    match outcome.as_str() {
      "success" => Ok(Some(RunOutcomeKind::Success)),
      "failed" => Ok(Some(RunOutcomeKind::Failed)),
      "interrupted" => Ok(Some(RunOutcomeKind::Interrupted)),
      value => Err(StoreError::new(format!("run {run_id} has unknown terminal outcome {value:?}"))),
    }
  }
}

/// The node's current running and queued work verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTreeActivity {
  pub has_running_tasks: bool,
  pub work_state: String,
}

/// The prompt rule scope whose fact is being reconciled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptScope {
  Subtree,
  Node,
}

impl PromptScope {
  fn as_str(self) -> &'static str {
    match self {
      Self::Subtree => "subtree",
      Self::Node => "node",
    }
  }

  fn metadata_ref(self, meta: &SessionMetadata) -> Option<&str> {
    match self {
      Self::Subtree => meta.subtree_prompt_ref.as_deref(),
      Self::Node => meta.node_prompt_ref.as_deref(),
    }
  }

  fn set_metadata_ref(self, meta: &mut SessionMetadata, value: Option<String>) {
    match self {
      Self::Subtree => meta.subtree_prompt_ref = value,
      Self::Node => meta.node_prompt_ref = value,
    }
  }
}

struct TreeIndex {
  metas: HashMap<String, Arc<SessionMetadata>>,
  children: HashMap<Option<String>, Vec<String>>,
  task_state_by_id: HashMap<String, String>,
  revision: String,
  derived_archived_ids: Vec<String>,
  archive_by_id: HashMap<String, bool>,
}

pub(crate) struct CachedTreeIndex {
  generation: u64,
  built_at: Instant,
  index: Arc<TreeIndex>,
}

pub(crate) struct CachedTaskStateTail {
  pub(crate) identity: Option<FileIdentity>,
  pub(crate) state: String,
}

/// A shared task-tree index snapshot for read-only projections.
#[derive(Clone)]
pub struct TaskTreeSnapshot {
  index: Arc<TreeIndex>,
}

impl TaskTreeSnapshot {
  /// The revision that binds task-tree page cursors to this snapshot.
  pub fn revision(&self) -> &str {
    &self.index.revision
  }

  /// Return one task's metadata from the snapshot.
  pub fn meta(&self, session_id: &str) -> StoreResult<&SessionMetadata> {
    index_meta(&self.index, session_id)
  }

  /// Return direct children in the snapshot's creation order.
  pub fn child_ids(&self, parent_id: Option<&str>) -> &[String] {
    let key = parent_id.map(str::to_string);
    self.index.children.get(&key).map_or(&[], Vec::as_slice)
  }

  /// Return every descendant in the snapshot's depth-first order.
  pub fn descendant_ids(&self, store: &Store, session_id: &str) -> StoreResult<Vec<String>> {
    descendant_ids(store, &self.index, session_id)
  }

  /// Return the task lifecycle state recorded in the snapshot.
  pub fn task_state(&self, session_id: &str) -> StoreResult<&str> {
    self
      .index
      .task_state_by_id
      .get(session_id)
      .map(String::as_str)
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }

  /// Return the effective archive state recorded in the snapshot.
  pub fn archived(&self, session_id: &str) -> StoreResult<bool> {
    self
      .index
      .archive_by_id
      .get(session_id)
      .copied()
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }
}

impl Store {
  /// Read one session through the metadata cache.
  pub async fn load_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.get_session(session_id).await
  }

  /// Read one required task node through the metadata cache.
  pub async fn load_task_meta(&self, session_id: &str) -> StoreResult<SessionMetadata> {
    self.load_meta(session_id).await?.ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }

  /// Return the selected home’s immutable prompt body directory.
  pub fn prompt_bodies_dir(&self) -> PathBuf {
    self.cfg.charliebot_home.join("prompt_bodies")
  }

  /// Read all durable facts and fold the task lifecycle and input boundary.
  pub fn facts_of(&self, session_id: &str) -> StoreResult<TaskFacts> {
    Ok(self.node_facts(session_id)?.task()?.clone())
  }

  /// The node's folded facts, shared. A read after appends folds only the events appended since the last read.
  ///
  /// The memo answers what a fold of [`Store::load_all_facts`] answers because its key covers every input of that
  /// read: the live-cache generation and the archived extent. This follows `src/runtime/task_sessions.py:650,710-746`'s
  /// list-identity key and suffix cursor; `node_facts_cache` stays resident across recovery eviction. A load from disk
  /// takes a new generation. Within one
  /// generation the cached list changes only by `append_chat_event` pushes, which the suffix fold extends.
  pub fn node_facts(&self, session_id: &str) -> StoreResult<Arc<NodeFacts>> {
    let mut never_stop = || false;
    Ok(self.node_facts_until(session_id, &mut never_stop)?.expect("the no-stop fold completes"))
  }

  /// The current run facts from the 64-history memo.
  pub fn run_facts(&self, session_id: &str) -> StoreResult<Arc<RunFacts>> {
    let mut never_stop = || false;
    Ok(self.run_facts_until(session_id, &mut never_stop)?.expect("the no-stop run-facts read completes"))
  }

  /// Read run facts with the Python memo bound and check the stop flag between event blocks.
  pub fn run_facts_until(
    &self,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Arc<RunFacts>>> {
    if self.node_facts_until(session_id, should_stop)?.is_none() {
      return Ok(None);
    }
    let node_entry = self
      .node_facts_cache
      .lock()
      .expect("node facts cache mutex is not poisoned")
      .get(session_id)
      .cloned()
      .expect("a node facts read stores its covered history");
    let archived = node_entry.archived;
    let (identity, live) = match self.live_history(session_id)? {
      LiveHistory::Cached { identity, events, .. } => (identity, events),
      LiveHistory::Cold(_) => {
        let Some((_generation, identity, events)) = self.live_events_with_generation_until(session_id, should_stop)?
        else {
          return Ok(None);
        };
        (identity, events)
      }
    };
    let cached = self.run_facts_memo.lock().expect("run facts memo mutex is not poisoned").get(&identity);
    if let Some(cached) = cached.as_ref().filter(|cached| cached.covered == live.len()) {
      return Ok(Some(Arc::clone(&cached.facts)));
    }
    let mut facts = if let Some(cached) = &cached {
      if cached.covered > live.len() {
        return Err(StoreError::new("run facts memo covers more events than the current history"));
      }
      cached.facts.as_ref().clone()
    } else {
      let mut facts = RunFacts::default();
      if archived > 0 {
        let Some(archive) = self.load_archive_range_until(session_id, 0, archived, should_stop)? else {
          return Ok(None);
        };
        if !observe_run_facts(&mut facts, &archive, should_stop)? {
          return Ok(None);
        }
      }
      facts
    };
    let covered = cached.as_ref().map_or(0, |cached| cached.covered);
    if !observe_run_facts(&mut facts, &live[covered..], should_stop)? {
      return Ok(None);
    }
    let facts = Arc::new(facts);
    self.run_facts_memo.lock().expect("run facts memo mutex is not poisoned").insert(CachedRunFacts {
      identity,
      events: Arc::clone(&live),
      covered: live.len(),
      facts: Arc::clone(&facts),
    });
    Ok(Some(facts))
  }

  /// Fold a node's task and run facts, checking `should_stop` every bounded block of history events.
  ///
  /// A stopped fold returns `None` and leaves the previous memo intact.
  pub fn node_facts_until(
    &self,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Arc<NodeFacts>>> {
    if should_stop() {
      return Ok(None);
    }
    let (generation, list_identity, live) = match self.live_history(session_id)? {
      LiveHistory::Cached { generation, identity, events } => (generation, identity, events),
      LiveHistory::Cold(identity) => {
        let archived = self.read_archive_offset(session_id)?;
        if let Some(identity) = identity
          && let Some(entry) =
            self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").get(session_id)
          && entry.key == NodeFactsKey::File(identity)
          && entry.archived == archived
        {
          return Ok(Some(Arc::clone(&entry.facts)));
        }
        let Some(events) = self.live_events_with_generation_until(session_id, should_stop)? else {
          return Ok(None);
        };
        events
      }
    };
    self.node_facts_of(session_id, generation, list_identity, &live, should_stop)
  }

  fn node_facts_of(
    &self,
    session_id: &str,
    generation: u64,
    list_identity: Arc<()>,
    live: &Arc<Vec<SharedChatEvent>>,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Arc<NodeFacts>>> {
    if should_stop() {
      return Ok(None);
    }
    let archived = self.read_archive_offset(session_id)?;
    let run_facts_identity = Arc::clone(&list_identity);
    let cached_runs =
      self.run_facts_memo.lock().expect("run facts memo mutex is not poisoned").get(&run_facts_identity);
    let cached = self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").get(session_id).cloned();
    let mut folded_runs = None;
    let mut entry = match cached {
      Some(entry)
        if entry.key == (NodeFactsKey::Cached { generation, covered: entry.covered })
          && entry.archived == archived
          && entry.covered <= live.len() =>
      {
        entry
      }
      _ => {
        let mut facts = NodeFacts::default();
        let mut runs = RunFacts::default();
        if archived > 0 {
          let Some(archive) = self.load_archive_range_until(session_id, 0, archived, should_stop)? else {
            return Ok(None);
          };
          if !fold_node_facts_from(&mut facts, &archive, 0, should_stop, Some(&mut runs))? {
            return Ok(None);
          }
          self.history_work.folded_events.fetch_add(archive.len() as u64, Ordering::Relaxed);
        }
        folded_runs = Some(runs);
        self.history_work.full_folds.fetch_add(1, Ordering::Relaxed);
        CachedNodeFacts {
          key: NodeFactsKey::Cached { generation, covered: 0 },
          list_identity: Arc::clone(&list_identity),
          archived,
          covered: 0,
          facts: Arc::new(facts),
        }
      }
    };
    if entry.covered < live.len() {
      let suffix = &live[entry.covered..];
      let mut runs = if let Some(runs) = folded_runs.take() {
        Some(runs)
      } else {
        cached_runs
          .as_ref()
          .filter(|cached| cached.covered == entry.covered)
          .map(|cached| cached.facts.as_ref().clone())
      };
      if !fold_node_facts_from(
        Arc::make_mut(&mut entry.facts),
        suffix,
        archived + entry.covered,
        should_stop,
        runs.as_mut(),
      )? {
        return Ok(None);
      }
      self.history_work.folded_events.fetch_add(suffix.len() as u64, Ordering::Relaxed);
      entry.covered = live.len();
      folded_runs = runs;
    }
    if let Some(runs) = folded_runs {
      self.run_facts_memo.lock().expect("run facts memo mutex is not poisoned").insert(CachedRunFacts {
        identity: Arc::clone(&run_facts_identity),
        events: Arc::clone(live),
        covered: live.len(),
        facts: Arc::new(runs),
      });
    } else if !cached_runs.as_ref().is_some_and(|cached| cached.covered == live.len()) {
      let mut runs = cached_runs.map(|cached| (cached.covered, cached.facts.as_ref().clone()));
      if let Some((covered, facts)) = &mut runs {
        if *covered < live.len() && !observe_run_facts(facts, &live[*covered..], should_stop)? {
          return Ok(None);
        }
      } else {
        let mut facts = RunFacts::default();
        if archived > 0 {
          let Some(archive) = self.load_archive_range_until(session_id, 0, archived, should_stop)? else {
            return Ok(None);
          };
          if !observe_run_facts(&mut facts, &archive, should_stop)? {
            return Ok(None);
          }
        }
        if !observe_run_facts(&mut facts, live.as_slice(), should_stop)? {
          return Ok(None);
        }
        runs = Some((live.len(), facts));
      }
      if let Some((covered, facts)) = runs {
        self.run_facts_memo.lock().expect("run facts memo mutex is not poisoned").insert(CachedRunFacts {
          identity: Arc::clone(&run_facts_identity),
          events: Arc::clone(live),
          covered: live.len().max(covered),
          facts: Arc::new(facts),
        });
      }
    }
    let facts = Arc::clone(&entry.facts);
    entry.list_identity = list_identity;
    entry.key = NodeFactsKey::Cached { generation, covered: entry.covered };
    self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").insert(session_id.to_string(), entry);
    Ok(Some(facts))
  }

  /// Keep a node's folded task facts past recovery eviction, keyed by the file identity they cover.
  pub(crate) fn seal_node_facts(&self, session_id: &str, evicted: Option<(u64, usize, FileIdentity)>) {
    let Some((generation, covered, identity)) = evicted else { return };
    let mut cache = self.node_facts_cache.lock().expect("node facts mutex is not poisoned");
    if let Some(entry) = cache.get_mut(session_id)
      && entry.key == (NodeFactsKey::Cached { generation, covered })
    {
      entry.key = NodeFactsKey::File(identity);
    }
  }

  /// A view of the node's shared facts, keyed and invalidated by the same history as [`Store::node_facts`].
  pub fn node_state(&self, session_id: &str) -> StoreResult<Arc<NodeState>> {
    Ok(self.node_state_versioned(session_id)?.1)
  }

  /// [`Store::node_state`] and the version of the memo entry that answered it.
  ///
  /// While the event cache is cold, a state sealed by [`Store::evict_recovery_history`] answers when the log file and
  /// the archived extent are the ones it covers; the read parses no history. Any other cold read loads the history.
  fn node_state_versioned(&self, session_id: &str) -> StoreResult<(u64, Arc<NodeState>)> {
    let (generation, list_identity, live) = match self.live_history(session_id)? {
      LiveHistory::Cached { generation, identity, events } => (generation, identity, events),
      LiveHistory::Cold(identity) => {
        let archived = self.read_archive_offset(session_id)?;
        let version = {
          let states = self.node_state_cache.lock().expect("node-state cache mutex is not poisoned");
          identity
            .and_then(|identity| {
              states
                .get(session_id)
                .filter(|entry| entry.key == NodeStateKey::File(identity) && entry.archived == archived)
            })
            .map(|entry| entry.version)
        };
        if let Some(version) = version {
          let Some(facts) = self.node_facts_until(session_id, &mut || false)? else {
            unreachable!("the no-stop facts lookup completes")
          };
          return Ok((version, self.node_state_view(session_id, facts)?));
        }
        self.live_events_with_generation(session_id)?
      }
    };
    self.node_state_of(session_id, generation, list_identity, &live)
  }

  /// [`Store::node_state_versioned`] for a caller that holds the live events and their cache generation.
  fn node_state_of(
    &self,
    session_id: &str,
    generation: u64,
    list_identity: Arc<()>,
    live: &Arc<Vec<SharedChatEvent>>,
  ) -> StoreResult<(u64, Arc<NodeState>)> {
    let archived = self.read_archive_offset(session_id)?;
    let current = NodeStateKey::Cached { generation, covered: live.len() };
    let previous =
      self.node_state_cache.lock().expect("node-state cache mutex is not poisoned").get(session_id).cloned();
    let (version, state_fold_count) = match previous {
      Some(entry) if entry.key == current && entry.archived == archived => {
        let mut never_stop = || false;
        let Some(facts) =
          self.node_facts_of(session_id, generation, Arc::clone(&list_identity), live, &mut never_stop)?
        else {
          unreachable!("the no-stop facts lookup completes")
        };
        return Ok((entry.version, self.node_state_view(session_id, facts)?));
      }
      Some(CachedNodeState {
        key: NodeStateKey::Cached { generation: cached_generation, covered },
        archived: cached_archived,
        ..
      }) if cached_generation == generation && cached_archived == archived && covered <= live.len() => {
        (self.next_node_state_version.fetch_add(1, Ordering::Relaxed), Some((live.len() - covered) as u64))
      }
      _ => (self.next_node_state_version.fetch_add(1, Ordering::Relaxed), None),
    };
    let mut never_stop = || false;
    let Some(facts) = self.node_facts_of(session_id, generation, list_identity, live, &mut never_stop)? else {
      unreachable!("the no-stop facts lookup completes")
    };
    if let Some(folded_events) = state_fold_count {
      self.history_work.node_state_folded_events.fetch_add(folded_events, Ordering::Relaxed);
    } else {
      self.history_work.node_state_full_folds.fetch_add(1, Ordering::Relaxed);
      let folded_events = (archived + live.len()) as u64;
      self.history_work.node_state_folded_events.fetch_add(folded_events, Ordering::Relaxed);
    }
    let state = self.node_state_view(session_id, facts)?;
    let entry = CachedNodeState { key: current, archived, version };
    self.node_state_cache.lock().expect("node-state cache mutex is not poisoned").insert(session_id.to_string(), entry);
    Ok((version, state))
  }

  fn node_state_view(&self, session_id: &str, facts: Arc<NodeFacts>) -> StoreResult<Arc<NodeState>> {
    let runs = match self.cached_run_facts(session_id) {
      Some(runs) => runs,
      None => self.run_facts(session_id)?,
    };
    let malformed_task_events =
      if facts.task.is_err() { Some(self.load_all_facts_shared_sync(session_id)?) } else { None };
    Ok(Arc::new(NodeState { facts, runs, malformed_task_events }))
  }

  fn cached_run_facts(&self, session_id: &str) -> Option<Arc<RunFacts>> {
    let node_entry =
      self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").get(session_id).cloned()?;
    self
      .run_facts_memo
      .lock()
      .expect("run facts memo mutex is not poisoned")
      .get(&node_entry.list_identity)
      .filter(|cached| cached.covered == node_entry.covered)
      .map(|cached| Arc::clone(&cached.facts))
  }

  /// Keep a node's state past the eviction of its cached events, keyed by the log file's identity.
  ///
  /// `evicted` holds the dropped list's generation and length and the identity of the file read under the same session
  /// lock, so the file holds exactly the dropped list. A state memo that covers that whole list keeps its version; one
  /// that does not gives way to the node's [`NodeFacts`] memo when that one covers the whole list. A state keyed by a
  /// file outlives an eviction that finds the cache already cold. Anything else leaves the memo.
  pub(crate) fn seal_node_state(&self, session_id: &str, evicted: Option<(u64, usize, FileIdentity, Arc<()>)>) {
    let mut states = self.node_state_cache.lock().expect("node state cache mutex is not poisoned");
    let entry = states.remove(session_id);
    let sealed = match (evicted, entry) {
      (Some((generation, len, identity, _)), Some(entry))
        if entry.key == (NodeStateKey::Cached { generation, covered: len }) =>
      {
        Some(CachedNodeState { key: NodeStateKey::File(identity), archived: entry.archived, version: entry.version })
      }
      (Some((generation, len, identity, _)), _) => {
        let node_facts = self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned");
        node_facts
          .get(session_id)
          .filter(|entry| entry.key == (NodeFactsKey::Cached { generation, covered: len }) && entry.covered == len)
          .map(|entry| CachedNodeState {
            key: NodeStateKey::File(identity),
            archived: entry.archived,
            version: self.next_node_state_version.fetch_add(1, Ordering::Relaxed),
          })
      }
      (None, Some(entry)) if matches!(entry.key, NodeStateKey::File(_)) => Some(entry),
      (None, _) => None,
    };
    if let Some(sealed) = sealed {
      states.insert(session_id.to_string(), sealed);
    }
  }

  /// Read the node’s fact-derived lifecycle state.
  pub fn task_state(&self, session_id: &str) -> StoreResult<TaskState> {
    Ok(self.node_state(session_id)?.task_state()?.to_string())
  }

  /// Read task lifecycle state from the last 1000 history lines before falling back to the full fold.
  ///
  /// This uses Python's `_task_state_fast` key `(inode, size, mtime)` and its 1000-line tail bound.
  pub fn task_state_fast(&self, session_id: &str) -> StoreResult<TaskState> {
    self.task_state_from_file_tail(session_id, false)
  }

  /// State for a task-tree index, using the sealed recovery fold before opening the bounded history tail.
  ///
  /// This avoids re-reading a node's tail when task recovery just folded the same `(inode, size, mtime)` file.
  pub fn task_state_for_tree_index(&self, session_id: &str) -> StoreResult<TaskState> {
    self.task_state_from_file_tail(session_id, true)
  }

  fn task_state_from_file_tail(&self, session_id: &str, use_sealed_recovery_state: bool) -> StoreResult<TaskState> {
    let path = self.chat_events_path(session_id);
    let identity = FileIdentity::read(&path)?;
    if let Some(cached) =
      self.task_state_tail_cache.lock().expect("task-state tail cache mutex is not poisoned").get(session_id)
      && cached.identity == identity
    {
      return Ok(cached.state.clone());
    }

    if use_sealed_recovery_state {
      let archived = self.read_archive_offset(session_id)?;
      let sealed = {
        let facts = self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned");
        identity.and_then(|identity| match facts.get(session_id) {
          Some(entry) if entry.key == NodeFactsKey::File(identity) && entry.archived == archived => {
            Some(entry.facts.task().map(|task| task.task_state.clone()))
          }
          _ => None,
        })
      };
      if let Some(state) = sealed.transpose()? {
        // Recovery already folded this file; use the same (inode, size, mtime) verdict before opening its tail again.
        self
          .task_state_tail_cache
          .lock()
          .expect("task-state tail cache mutex is not poisoned")
          .insert(session_id.to_string(), CachedTaskStateTail { identity, state: state.clone() });
        return Ok(state);
      }
    }

    self.task_state_tail_reads.fetch_add(1, Ordering::Relaxed);
    let state = match lifecycle_state_from_file(&path)? {
      Some(state) => state,
      None => self.task_state(session_id)?,
    };
    self
      .task_state_tail_cache
      .lock()
      .expect("task-state tail cache mutex is not poisoned")
      .insert(session_id.to_string(), CachedTaskStateTail { identity, state: state.clone() });
    Ok(state)
  }

  /// Count actual lifecycle-tail file scans since the store opened.
  pub fn task_state_tail_read_count(&self) -> u64 {
    self.task_state_tail_reads.load(Ordering::Relaxed)
  }

  /// Rebuild the cached task index after a node metadata write.
  pub fn invalidate_tree_index(&self) {
    self.task_tree_generation.fetch_add(1, Ordering::AcqRel);
    self.task_tree_index.lock().expect("task tree index mutex is not poisoned").take();
  }

  /// Return a node’s task ancestors, nearest first.
  pub fn ancestor_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    ancestor_ids(&index, session_id)
  }

  /// Return one node’s direct task children in creation order.
  pub fn child_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    if !index.metas.contains_key(session_id) {
      return Err(StoreError::not_found(format!("task {session_id} not found")));
    }
    Ok(index.children.get(&Some(session_id.to_string())).cloned().unwrap_or_default())
  }

  /// Return every descendant in the task index’s depth-first order.
  pub fn descendant_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let mut never_stop = || false;
    Ok(self.descendant_ids_until(session_id, &mut never_stop)?.expect("the no-stop descendant walk completes"))
  }

  /// Return every descendant in depth-first order, or `None` if startup recovery asks the walk to stop.
  pub fn descendant_ids_until(
    &self,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Vec<String>>> {
    let Some(index) = self.tree_index_until(should_stop)? else {
      return Ok(None);
    };
    descendant_ids_until(self, &index, session_id, should_stop)
  }

  /// Return every task node whose effective archive state is inherited or fact-derived.
  pub fn derived_archived_ids(&self) -> StoreResult<Vec<String>> {
    Ok(self.tree_index()?.derived_archived_ids.clone())
  }

  /// Return the revision used by task-tree pagination from its cached two-second index.
  pub fn task_tree_revision(&self) -> StoreResult<String> {
    Ok(self.tree_index()?.revision.clone())
  }

  /// Return a shared snapshot for a task-tree read page.
  pub fn task_tree_snapshot(&self) -> StoreResult<TaskTreeSnapshot> {
    Ok(TaskTreeSnapshot { index: self.tree_index()? })
  }

  /// Count full task-tree index builds since the store opened.
  pub fn task_tree_index_build_count(&self) -> u64 {
    self.task_tree_index_builds.load(Ordering::Relaxed)
  }

  /// Return a task’s effective archive state, including archived ancestors.
  pub fn archived_of(&self, session_id: &str) -> StoreResult<bool> {
    archived_of(self, &*self.tree_index()?, session_id, &mut HashMap::new())
  }

  /// Require every ancestor of a node to remain open.
  pub fn require_open_ancestry(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    let ancestors = ancestor_ids(&index, session_id)?;
    let mut closed = Vec::new();
    for ancestor_id in &ancestors {
      if self.task_state(ancestor_id)? != "open" {
        closed.push(ancestor_id.clone());
      }
    }
    if !closed.is_empty() {
      return Err(StoreError::conflict(vec![closed_ancestors_blocker(&closed)]));
    }
    Ok(ancestors)
  }

  /// Return the nearest node on the open manager chain that holds a user request: a real user message or a thread
  /// platform summon. A refusal is `forbidden`: `takeoff_gate.DelegationBlockedError` is a 403 on every route.
  pub fn check_task_authorization(&self, session_id: &str) -> StoreResult<String> {
    let index = self.tree_index()?;
    let mut current = session_id.to_string();
    let mut user_node = None;
    for _ in 0..TASK_ANCESTOR_HOP_LIMIT {
      let metadata = index.metas.get(&current);
      let (parent_id, profile) =
        metadata.map_or((None, None), |meta| (meta.task_parent_id.as_deref(), Some(meta.profile)));
      let is_start = current == session_id;
      if is_start && profile != Some(TaskProfile::Manager) {
        return Err(StoreError::forbidden(format!(
          "task {current} is not a manager; agent calls run only under a manager task"
        )));
      }
      let state = if metadata.is_some() { self.task_state(&current)? } else { "open".to_string() };
      if state != "open" {
        let kind = if is_start { "task" } else { "ancestor task" };
        return Err(StoreError::forbidden(format!("Delegation blocked: {kind} {current} is {state}; open it first")));
      }
      if user_node.is_none() && self.node_state(&current)?.has_user_request() {
        user_node = Some(current.clone());
      }
      let Some(parent_id) = parent_id else {
        return user_node.ok_or_else(|| no_user_request_error(session_id));
      };
      current = parent_id.to_string();
    }
    Err(StoreError::forbidden(format!(
      "Delegation blocked: the task chain from task {session_id} exceeds {TASK_ANCESTOR_HOP_LIMIT} hops without reaching a root"
    )))
  }

  /// Check the caller scope and task gate for an agent create request.
  pub fn runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()> {
    let TaskCaller::Agent { session_id } = caller else { return Ok(()) };
    if request.session_id.is_some() {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    if request.task_parent_id.as_deref() != Some(session_id.as_str()) {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    let parent = self.read_metadata_fresh(session_id)?;
    if parent.as_ref().is_none_or(|meta| meta.profile != TaskProfile::Manager) {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    if request.profile == TaskProfile::Worker && !is_read_only_exempt(request.task.as_ref()) {
      self.check_task_authorization(session_id)?;
    }
    Ok(())
  }

  /// Create one task node and replay the original product for a repeated request id.
  pub async fn create_task(
    &self,
    request: CreateTask,
    caller: TaskCaller,
    slot_values: Map<String, Value>,
  ) -> StoreResult<SessionMetadata> {
    if request.request_id.is_empty() {
      return Err(StoreError::invalid(TASK_CREATE_REQUEST_ID_REQUIRED));
    }
    if !matches!(request.profile, TaskProfile::Manager | TaskProfile::Worker) {
      return Err(StoreError::invalid("profile must be 'manager' or 'worker'"));
    }
    let task_id = request
      .session_id
      .clone()
      .unwrap_or_else(|| control_events::stable_task_id(request.task_parent_id.as_deref(), &request.request_id));
    let guard = self.control_lock().await;
    self.require_control_guard(&guard)?;

    if let Some(existing) = self.read_metadata_fresh(&task_id)? {
      if matches!(caller, TaskCaller::Agent { .. }) {
        let replay = CreateTask { profile: existing.profile, task: existing.task.clone(), ..request };
        self.runtime_gate(&replay, &caller)?;
      }
      return Ok(existing);
    }
    self.runtime_gate(&request, &caller)?;

    let parent_meta = match request.task_parent_id.as_deref() {
      None => None,
      Some(parent_id) => {
        let parent = self
          .read_metadata_fresh(parent_id)?
          .ok_or_else(|| StoreError::not_found(format!("task {parent_id} not found")))?;
        if parent.profile == TaskProfile::Worker {
          return Err(StoreError::invalid(format!("parent task {parent_id} is not a manager")));
        }
        let parent_state = self.task_state(parent_id)?;
        if parent_state != "open" {
          return Err(StoreError::conflict(vec![format!("parent task {parent_id} is {parent_state}")]));
        }
        self.require_open_ancestry(parent_id)?;
        Some(parent)
      }
    };

    let mut metadata = SessionMetadata::new(task_id.clone(), "".to_string(), request.profile);
    metadata.name = match request.name.as_deref().filter(|name| !name.is_empty()) {
      Some(name) => name.to_string(),
      None => match request.task.as_ref().and_then(|task| default_task_name(task, request.profile)) {
        Some(name) => name,
        None => self.next_session_name()?,
      },
    };
    metadata.task_parent_id = request.task_parent_id.clone();
    metadata.task = request.task.clone();
    metadata.group = request.group.clone();
    metadata.backend = request
      .backend
      .as_deref()
      .filter(|backend| !backend.is_empty())
      .or_else(|| parent_meta.as_ref().map(|parent| parent.backend.as_str()).filter(|backend| !backend.is_empty()))
      .map(str::to_string)
      .or_else(|| self.cfg.backends.options.first().map(|backend| backend.id.clone()))
      .ok_or_else(|| StoreError::new("task create requires a configured backend"))?;
    set_registered_slot_values(&self.slots, &mut metadata, slot_values)?;

    let created_event_id = uuid::Uuid::new_v4().to_string();
    let created_event = control_events::build_task_created_event(
      caller_actor(&caller),
      &task_id,
      &request.request_id,
      request.task_parent_id.as_deref(),
      request.task.as_ref().map(task_spec_hash).transpose()?.as_deref(),
      Some(&created_event_id),
      None,
    );
    metadata.created_by_event = Some(EventRef { session_id: task_id.clone(), event_id: Some(created_event_id) });

    self.publish_task_node(&metadata, &created_event)?;
    self.invalidate_cache(&task_id);
    self.invalidate_tree_index();
    let fresh = self
      .read_metadata_fresh(&task_id)?
      .ok_or_else(|| StoreError::new(format!("created task {task_id} is missing")))?;
    // The creation fact is published by the rename above, which appends no control fact. The signal follows the
    // publication and the index invalidation, so a receiver can read the node at once (`task_sessions.create_task`).
    self.notify_task_tree_changed(&task_id, Some(event_types::TASK_CREATED)).await;
    Ok(fresh)
  }

  /// Change registered slot values while the caller holds the tree control lock.
  pub async fn update_task_slot_fields_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<SessionMetadata> {
    let metadata = self
      .update_slot_fields_locked(guard, session_id, owner, values)
      .await?
      .ok_or_else(|| StoreError::not_found(format!("session {session_id} not found")))?;
    self.invalidate_tree_index();
    Ok(metadata)
  }

  /// Change a node’s backend under the tree control lock without changing `updated_at`.
  pub async fn set_node_backend_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    backend: &str,
  ) -> StoreResult<()> {
    self.require_control_guard(guard)?;
    let backend = backend.to_string();
    let updated = self
      .mutate_fresh(session_id, false, true, move |meta| {
        meta.backend = backend;
        Ok(true)
      })
      .await?;
    if updated.is_none() {
      return Err(StoreError::not_found(format!("session {session_id} not found")));
    }
    self.invalidate_tree_index();
    Ok(())
  }

  /// Copy registered metadata fields from `old` into a fresh target metadata read.
  pub async fn adopt_metadata_slot_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    old: &SessionMetadata,
    owner: &str,
    fields: Option<&[&str]>,
  ) -> StoreResult<SessionMetadata> {
    self.require_control_guard(guard)?;
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == owner && slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .ok_or_else(|| StoreError::new(format!("{owner:?} registered no fields on session metadata")))?;
    let selected: Vec<_> = match fields {
      Some(names) => names
        .iter()
        .map(|name| {
          slot
            .fields
            .iter()
            .find(|field| field.name == *name)
            .ok_or_else(|| StoreError::new(format!("{owner:?} has no field {name:?}")))
        })
        .collect::<StoreResult<Vec<_>>>()?,
      None => slot.fields.iter().collect(),
    };
    let old = old.clone();
    let mut copied = Vec::new();
    for field in selected {
      let value = old.extra.get(field.name).unwrap_or(&field.default).clone();
      copied.push((field.name, value));
    }
    let result = self
      .mutate_fresh(session_id, false, true, move |meta| {
        for (name, value) in copied {
          meta.set_extra(name, value);
        }
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("session {session_id} not found")))?;
    self.invalidate_tree_index();
    Ok(result)
  }

  /// Persist the native anchor provenance under the tree control lock.
  pub async fn record_native_anchor(
    &self,
    session_id: &str,
    prompt_hash: &str,
    backend: &str,
    model: Option<&str>,
  ) -> StoreResult<()> {
    self.record_native_anchor_with_reset(session_id, prompt_hash, backend, model, false).await
  }

  /// Persist native anchor provenance and optionally clear a superseded native conversation.
  pub async fn record_native_anchor_with_reset(
    &self,
    session_id: &str,
    prompt_hash: &str,
    backend: &str,
    model: Option<&str>,
    reset_anchor: bool,
  ) -> StoreResult<()> {
    let _guard = self.control_lock().await;
    self.persist_native_anchor_provenance(session_id, prompt_hash, backend, model).await?;
    if reset_anchor {
      let fresh = self
        .read_metadata_fresh(session_id)?
        .ok_or_else(|| StoreError::new(format!("session {session_id} not found")))?;
      if fresh.cc_session_id.is_some() {
        self.clear_cc_session_anchor(session_id).await?;
      }
    }
    self.invalidate_tree_index();
    Ok(())
  }

  /// Store a prompt body once by its SHA-256 reference.
  pub fn store_prompt_body(&self, body: &str) -> StoreResult<String> {
    let digest = control_events::sha256_hex(body);
    let path = self.prompt_bodies_dir().join(format!("{digest}.md"));
    if path.exists() {
      let existing = fs::read_to_string(&path).map_err(|error| path_error(&path, error))?;
      if existing != body {
        return Err(StoreError::new(format!(
          "prompt body store corrupted at {}: fingerprint content mismatch",
          path.display()
        )));
      }
      return Ok(digest);
    }
    let parent = path.parent().expect("prompt body path has a parent");
    fs::create_dir_all(parent).map_err(|error| path_error(parent, error))?;
    fsutil::atomic_write_text(&path, body).map_err(|error| path_error(&path, error))?;
    Ok(digest)
  }

  /// Reconcile the prompt-change fact implied by one metadata snapshot.
  pub async fn ensure_prompt_changed_fact(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    scope: PromptScope,
  ) -> StoreResult<()> {
    let guard = self.control_lock().await;
    self.ensure_prompt_changed_fact_locked(&guard, session_id, meta, scope).await
  }

  /// Reconcile a prompt-change fact while the caller owns the control lock.
  pub async fn ensure_prompt_changed_fact_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    meta: &SessionMetadata,
    scope: PromptScope,
  ) -> StoreResult<()> {
    self.require_control_guard(guard)?;
    let events = self.load_chat_events_shared_sync(session_id)?;
    let scope_name = scope.as_str();
    let last = events.iter().rev().find(|event| {
      event.get("type").and_then(CompactValue::as_str) == Some("prompt_changed")
        && event.get("scope").and_then(CompactValue::as_str) == Some(scope_name)
    });
    let current_ref = scope.metadata_ref(meta);
    if last.is_some_and(|event| event.get("new_ref").and_then(CompactValue::as_str) == current_ref) {
      return Ok(());
    }
    if last.is_none() && current_ref.is_none() {
      return Ok(());
    }
    let previous_ref = last.and_then(|event| event.get("new_ref")).map(CompactValue::to_value).unwrap_or(Value::Null);
    let event = control_events::build_control_event(
      "prompt_changed",
      ACTOR_USER,
      session_id,
      None,
      None,
      [
        ("scope".to_string(), Value::String(scope_name.to_string())),
        ("previous_ref".to_string(), previous_ref),
        ("new_ref".to_string(), current_ref.map_or(Value::Null, |value| Value::String(value.to_string()))),
      ],
    );
    self.append_control_fact(guard, session_id, event).await?;
    Ok(())
  }

  /// Run the sequence binding's archive duty for one node (`sequence_controllers.binding_for(sid).on_archive`).
  pub async fn sequence_archive_on_archive(&self, session_id: &str) -> StoreResult<()> {
    self.sequence_archive.on_archive(session_id).await
  }

  /// Permanently delete an empty, unreferenced task while holding the tree control lock.
  pub async fn delete_permanently(&self, session_id: &str, caller: TaskCaller) -> StoreResult<bool> {
    if caller != TaskCaller::Operator {
      return Err(StoreError::forbidden("permanent delete requires operator credentials"));
    }
    let _guard = self.control_lock().await;
    self.invalidate_tree_index();
    let index = self.tree_index()?;
    if !index.metas.contains_key(session_id) {
      return Err(StoreError::not_found(format!("task {session_id} not found")));
    }

    let mut blockers = Vec::new();
    let children = index.children.get(&Some(session_id.to_string())).cloned().unwrap_or_default();
    if !children.is_empty() {
      blockers.push(format!("has child task(s): {}", children.join(", ")));
    }
    let runs = self.list_run_records_sync(session_id)?;
    if !runs.is_empty() {
      blockers
        .push(format!("has run record(s): {}", runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let triggers_dir = self.session_dir(session_id).join("triggers");
    if triggers_dir.is_dir() {
      let mut has_trigger = false;
      for entry in fs::read_dir(&triggers_dir).map_err(|error| path_error(&triggers_dir, error))? {
        let entry = entry.map_err(|error| path_error(&triggers_dir, error))?;
        if entry.file_name().to_string_lossy().ends_with(".json") {
          has_trigger = true;
          break;
        }
      }
      if has_trigger {
        blockers.push("has saved trigger reference(s)".to_string());
      }
    }

    let facts = self.node_facts(session_id)?;
    let facts = facts.task()?;
    let mut substance = facts
      .events_by_id
      .values()
      .filter(|event| event.get("type").and_then(CompactValue::as_str) != Some(event_types::TASK_CREATED))
      .map(|event| event.get("type").and_then(CompactValue::as_str).unwrap_or("None").to_string())
      .collect::<Vec<_>>();
    substance.sort();
    substance.dedup();
    if !substance.is_empty() {
      blockers.push(format!("has preserved conversation/evidence: {}", substance.join(", ")));
    }

    for (other_id, other) in &index.metas {
      if other_id == session_id {
        continue;
      }
      let mut refs = Vec::new();
      if other.parent_session_id.as_deref() == Some(session_id) {
        refs.push("parent_session_id");
      }
      if other.successor_session_id.as_deref() == Some(session_id) {
        refs.push("successor_session_id");
      }
      if other.origin_ref.as_ref().is_some_and(|reference| reference.session_id == session_id) {
        refs.push("origin_ref");
      }
      if other.created_by_event.as_ref().is_some_and(|reference| reference.session_id == session_id) {
        refs.push("created_by_event");
      }
      if !refs.is_empty() {
        blockers.push(format!("referenced by task {other_id} ({})", refs.join(", ")));
        continue;
      }
      let other_facts = self.node_facts(other_id)?;
      if other_facts.task()?.delivered_reports.iter().any(|(child_id, _)| child_id == session_id) {
        blockers.push(format!("referenced by a child report in task {other_id}"));
      }
    }

    blockers.sort();
    blockers.dedup();
    if !blockers.is_empty() {
      return Err(StoreError::conflict(blockers));
    }
    let deleted = self.delete_session_permanently(session_id).await?;
    if deleted {
      self.invalidate_tree_index();
    }
    Ok(deleted)
  }

  /// Apply one operator PATCH of a task node with its structural guards (`TaskTreeManager.patch_task`).
  ///
  /// Operator scope only. Under the control lock the guards of every node that the change moves or restructures are
  /// collected before any write, so a refusal writes nothing. The field write is a fresh read-modify-write, so a
  /// rename that landed meanwhile is kept. Each prompt scope then swaps its reference and lands its fact.
  pub async fn patch_task(
    &self,
    session_id: &str,
    patch: PatchTask,
    caller: TaskCaller,
    guards: &dyn StructuralBlockers,
  ) -> StoreResult<SessionMetadata> {
    if caller != TaskCaller::Operator {
      return Err(StoreError::forbidden("task metadata mutations require operator credentials"));
    }
    let guard = self.control_lock().await;
    // Rebuild from disk under the lock: the cache can predate writes that another process made.
    self.invalidate_tree_index();
    let index = self.tree_index()?;
    let meta = self.load_task_meta(session_id).await?;
    let PatchTask { name, task_parent_id, profile, task, subtree_prompt, node_prompt } = patch;
    let prompt_changed = subtree_prompt.is_some() || node_prompt.is_some();
    let structural = task.is_some() || profile.is_some() || task_parent_id.is_some();
    let mut blockers = if structural { self.structural_blockers(session_id, guards)? } else { Vec::new() };
    let children = index.children.get(&Some(session_id.to_string())).map_or(0, Vec::len);
    if profile == Some(Some(TaskProfile::Worker)) && meta.profile != TaskProfile::Worker && children > 0 {
      blockers.push("demotion to worker requires a task with no child tasks".to_string());
    }
    if let Some(parent) = task_parent_id.as_ref().filter(|parent| parent.as_deref() != meta.task_parent_id.as_deref()) {
      blockers.extend(self.reparent_blockers(&index, session_id, parent.as_deref(), guards)?);
    }
    if !blockers.is_empty() {
      blockers.sort();
      blockers.dedup();
      return Err(StoreError::conflict(blockers));
    }

    self
      .mutate_fresh(session_id, false, true, move |meta| {
        if let Some(name) = name {
          meta.name = name;
        }
        if let Some(Some(profile)) = profile {
          meta.profile = profile;
        }
        if let Some(task) = task {
          meta.task = task;
        }
        if let Some(parent) = task_parent_id {
          meta.task_parent_id = parent;
        }
        meta.schema_version = 2;
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    self.invalidate_tree_index();

    if let Some(body) = subtree_prompt {
      self.apply_prompt_change(&guard, session_id, PromptScope::Subtree, body.as_deref()).await?;
    }
    if let Some(body) = node_prompt {
      self.apply_prompt_change(&guard, session_id, PromptScope::Node, body.as_deref()).await?;
    }
    self.invalidate_tree_index();
    let event_type = if prompt_changed { event_types::PROMPT_CHANGED } else { "task_updated" };
    self.notify_task_tree_changed(session_id, Some(event_type)).await;
    self.read_metadata_fresh(session_id)?.ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }

  /// Swap one prompt scope's reference and land its fact. An interrupted earlier edit (metadata ahead of its fact) is
  /// landed first, so a retry or a later edit never folds two transitions into one fact. Bodies are content-addressed,
  /// so a retried PATCH stores the same bytes under the same reference.
  async fn apply_prompt_change(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    scope: PromptScope,
    body: Option<&str>,
  ) -> StoreResult<()> {
    let current = self.load_task_meta(session_id).await?;
    self.ensure_prompt_changed_fact_locked(guard, session_id, &current, scope).await?;
    let new_ref = body.map(|text| self.store_prompt_body(text)).transpose()?;
    if new_ref.as_deref() == scope.metadata_ref(&current) {
      return Ok(());
    }
    let fresh = self
      .mutate_fresh(session_id, false, true, move |meta| {
        scope.set_metadata_ref(meta, new_ref);
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    self.invalidate_tree_index();
    self.ensure_prompt_changed_fact_locked(guard, session_id, &fresh, scope).await
  }

  /// The structural blockers of one node: its unprocessed task inputs, then its unfinished runs.
  fn structural_blockers(&self, session_id: &str, guards: &dyn StructuralBlockers) -> StoreResult<Vec<String>> {
    let mut blockers = guards.pending_input_blockers(session_id)?;
    blockers.extend(self.run_blockers(session_id)?.into_iter().map(|blocker| blocker.reason));
    Ok(blockers)
  }

  /// The refusals for moving a node under `new_parent`: the target is an open manager, the move does not enter the
  /// node's own subtree, and the target's ancestors are open. Every node in the moving subtree must also be idle, with
  /// no undelivered close report.
  fn reparent_blockers(
    &self,
    index: &TreeIndex,
    session_id: &str,
    new_parent: Option<&str>,
    guards: &dyn StructuralBlockers,
  ) -> StoreResult<Vec<String>> {
    let mut blockers = Vec::new();
    if let Some(parent_id) = new_parent {
      let target = index_meta(index, parent_id)?;
      if target.profile != TaskProfile::Manager {
        blockers.push(format!("reparent target {parent_id} is not a manager task"));
      }
      let target_state = self.task_state(parent_id)?;
      if target_state != "open" {
        blockers.push(format!("reparent target {parent_id} is {target_state}"));
      }
      let chain = ancestor_ids(index, parent_id)?;
      if session_id == parent_id || chain.iter().any(|id| id == session_id) {
        blockers.push(format!("reparent target {parent_id} is inside {session_id}'s own subtree"));
      } else {
        let mut closed = Vec::new();
        for ancestor in &chain {
          if self.task_state(ancestor)? != "open" {
            closed.push(ancestor.clone());
          }
        }
        if !closed.is_empty() {
          blockers.push(closed_ancestors_blocker(&closed));
        }
      }
    }
    let subtree = std::iter::once(session_id.to_string()).chain(descendant_ids(self, index, session_id)?);
    for node in subtree {
      for blocker in self.structural_blockers(&node, guards)? {
        blockers.push(format!("{node}: {blocker}"));
      }
      for blocker in guards.undelivered_report_blockers(&node)? {
        blockers.push(format!("{node}: {blocker}"));
      }
    }
    Ok(blockers)
  }

  /// Move sequence bindings from an elone source and copy its scheduled-run bookkeeping.
  pub async fn rebind_elone_bindings_locked(
    &self,
    guard: &ControlGuard,
    source_session_id: &str,
    new_session_id: &str,
  ) -> StoreResult<bool> {
    self.require_control_guard(guard)?;
    let moved = self.sequence_binding_mover.move_bindings(source_session_id, new_session_id).await?;
    if !moved {
      return Ok(false);
    }
    let source = self
      .read_metadata_fresh(source_session_id)?
      .ok_or_else(|| StoreError::not_found(format!("session {source_session_id} not found")))?;
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == "cron" && slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .ok_or_else(|| StoreError::new("cron registered no fields on session metadata"))?;
    let names = ["last_scheduled_run", "last_scheduled_cron", "last_run_status"];
    let selected = names
      .iter()
      .map(|name| {
        let field = slot
          .fields
          .iter()
          .find(|field| field.name == *name)
          .ok_or_else(|| StoreError::new(format!("cron has no field {name:?}")))?;
        Ok((field.name, source.extra.get(field.name).unwrap_or(&field.default).clone()))
      })
      .collect::<StoreResult<Vec<_>>>()?;
    let updated = self
      .mutate_fresh(new_session_id, false, true, move |meta| {
        for (name, value) in selected {
          meta.set_extra(name, value);
        }
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?;
    if updated.is_none() {
      return Err(StoreError::not_found(format!("session {new_session_id} not found")));
    }
    self.invalidate_tree_index();
    Ok(true)
  }

  /// Resolve every existing metadata row from a session through its successor chain.
  pub fn resolve_successor_chain(&self, session_id: &str) -> StoreResult<Vec<SessionMetadata>> {
    let mut chain = Vec::new();
    let Some(mut current) = self.read_metadata_fresh(session_id)? else { return Ok(chain) };
    chain.push(current.clone());
    for _ in 0..SUCCESSOR_HOP_LIMIT {
      let Some(successor_id) = current.successor_session_id.as_deref() else { return Ok(chain) };
      let Some(successor) = self.read_metadata_fresh(successor_id)? else {
        tracing::error!(session_id, missing_session_id = successor_id, "successor_chain_broken");
        return Ok(chain);
      };
      chain.push(successor.clone());
      current = successor;
    }
    if current.successor_session_id.is_some() {
      return Err(StoreError::new(format!(
        "successor chain from {session_id} exceeds {SUCCESSOR_HOP_LIMIT} hops; aborting to avoid a cycle"
      )));
    }
    Ok(chain)
  }

  /// Append an event to the current successor-chain tail while holding that session’s lock.
  pub async fn deliver_to_successor(&self, session_id: &str, mut event: Value) -> StoreResult<Option<String>> {
    for _ in 0..=SUCCESSOR_HOP_LIMIT {
      let Some(tail) = self.resolve_successor_chain(session_id)?.last().cloned() else {
        tracing::error!(session_id, "deliver_to_successor_origin_missing");
        return Ok(None);
      };
      let _session_guard = self.lock_for(&tail.id).await;
      let Some(fresh_tail) = self.read_metadata_fresh(&tail.id)? else {
        tracing::error!(session_id, tail_id = %tail.id, "deliver_to_successor_tail_missing");
        return Ok(None);
      };
      if fresh_tail.successor_session_id.is_some() {
        continue;
      }
      if !self.metadata_path(&fresh_tail.id).is_file() {
        tracing::error!(session_id, tail_id = %fresh_tail.id, "deliver_to_successor_metadata_missing");
        return Ok(None);
      }
      if fresh_tail.id != session_id {
        event
          .as_object_mut()
          .ok_or_else(|| StoreError::new("successor event must be a JSON object"))?
          .insert("origin_session_id".to_string(), Value::String(session_id.to_string()));
      }
      self.append_chat_event(&fresh_tail.id, event).await?;
      return Ok(Some(fresh_tail.id));
    }
    Err(StoreError::new(format!(
      "deliver_to_successor from {session_id} retried {SUCCESSOR_HOP_LIMIT} times; aborting to avoid a loop"
    )))
  }

  /// Derive live and waiting work for one task node, as `TaskSessions.activity_of` does.
  ///
  /// A node without runs is idle, and its derivation reads no history. Otherwise one [`Store::node_state`] read answers
  /// the task state and the facts of every run. The verdict is memoized on the run-record generation and, for a node
  /// with runs, on the version of that state. A verdict that judged a process through `/proc` is not stored: a process
  /// exit writes no file that would move the key.
  pub fn task_tree_activity(&self, session_id: &str) -> StoreResult<TaskTreeActivity> {
    self.task_tree_activity_calls.fetch_add(1, Ordering::Relaxed);
    let records_generation = self.records_generation();
    let cached = self.activity_cache.lock().expect("activity cache mutex is not poisoned").get(session_id).cloned();
    let mut state = None;
    if let Some(cached) = cached.filter(|cached| cached.records_generation == records_generation) {
      let Some(version) = cached.state_version else { return Ok(cached.activity) };
      let current = self.node_state_versioned(session_id)?;
      if current.0 == version {
        return Ok(cached.activity);
      }
      state = Some(current);
    }
    let runs = self.list_run_records_sync(session_id)?;
    if runs.is_empty() {
      let activity = TaskTreeActivity { has_running_tasks: false, work_state: "idle".to_string() };
      let entry = CachedActivity { records_generation, state_version: None, activity: activity.clone() };
      self.activity_cache.lock().expect("activity cache mutex is not poisoned").insert(session_id.to_string(), entry);
      return Ok(activity);
    }
    let (version, node) = match state {
      Some(state) => state,
      None => self.node_state_versioned(session_id)?,
    };
    let task_open = node.task_state()? == "open";
    let mut boot = HostBoot::default();
    let mut running = false;
    let mut waiting = false;
    for run in &runs {
      match node.display_state(run, &mut boot)? {
        RunDisplayState::Running => running = true,
        RunDisplayState::Queued | RunDisplayState::Withheld if task_open => waiting = true,
        RunDisplayState::Queued
        | RunDisplayState::Withheld
        | RunDisplayState::Attention
        | RunDisplayState::Stopped
        | RunDisplayState::Success
        | RunDisplayState::Failed
        | RunDisplayState::Interrupted => {}
      }
    }
    let activity = TaskTreeActivity {
      has_running_tasks: running,
      work_state: if running {
        "running"
      } else if waiting {
        "waiting"
      } else {
        "idle"
      }
      .to_string(),
    };
    let mut cache = self.activity_cache.lock().expect("activity cache mutex is not poisoned");
    if boot.was_read() {
      cache.remove(session_id);
    } else {
      let entry = CachedActivity { records_generation, state_version: Some(version), activity: activity.clone() };
      cache.insert(session_id.to_string(), entry);
    }
    Ok(activity)
  }

  /// Count task-tree activity lookups since the store opened.
  pub fn task_tree_activity_call_count(&self) -> u64 {
    self.task_tree_activity_calls.load(Ordering::Relaxed)
  }

  /// Build the session detail projection used by the session read and completion routes.
  pub fn session_detail(&self, session_id: &str) -> StoreResult<Value> {
    let index = self.tree_index()?;
    let meta =
      index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    let mut detail = self.session_metadata_value(meta.as_ref());
    let object = detail.as_object_mut().expect("session metadata serializes to an object");
    object.insert("task_state".to_string(), Value::String(self.task_state(session_id)?));
    object.insert("work_state".to_string(), Value::String(self.task_tree_activity(session_id)?.work_state));
    object.insert("archived".to_string(), Value::Bool(archived_of(self, &index, session_id, &mut HashMap::new())?));

    let mut ancestors = Vec::new();
    for ancestor_id in ancestor_ids(&index, session_id)? {
      let ancestor =
        index.metas.get(&ancestor_id).ok_or_else(|| StoreError::not_found(format!("task {ancestor_id} not found")))?;
      ancestors.push(json!({"id": ancestor.id, "name": ancestor.name}));
    }
    object.insert("ancestors".to_string(), Value::Array(ancestors));

    let descendants = descendant_ids(self, &index, session_id)?.len();
    let prompt_rules = json!({
      "subtree": self.prompt_rule_summary(meta.subtree_prompt_ref.as_deref())?,
      "node": self.prompt_rule_summary(meta.node_prompt_ref.as_deref())?,
      "affected_descendants": descendants,
    });
    object.insert("prompt_rules".to_string(), prompt_rules);
    Ok(detail)
  }

  fn prompt_rule_summary(&self, prompt_ref: Option<&str>) -> StoreResult<Value> {
    let Some(prompt_ref) = prompt_ref else {
      return Ok(json!({"ref": null, "source": null, "chars": 0, "text": null}));
    };
    let path = self.prompt_bodies_dir().join(format!("{prompt_ref}.md"));
    let body = fs::read_to_string(&path).map_err(|error| path_error(&path, error))?;
    Ok(json!({
      "ref": prompt_ref,
      "source": path.to_string_lossy(),
      "chars": body.chars().count(),
      "text": body,
    }))
  }

  /// The task index, shared: every reader within one build's lifetime reads the same index, as `_get_index` hands out.
  fn tree_index(&self) -> StoreResult<Arc<TreeIndex>> {
    let mut never_stop = || false;
    Ok(self.tree_index_until(&mut never_stop)?.expect("the no-stop task index build completes"))
  }

  fn tree_index_until(&self, should_stop: &mut impl FnMut() -> bool) -> StoreResult<Option<Arc<TreeIndex>>> {
    if should_stop() {
      return Ok(None);
    }
    let generation = self.task_tree_generation.load(Ordering::Acquire);
    if let Some(cached) = self.task_tree_index.lock().expect("task tree index mutex is not poisoned").as_ref()
      && cached.generation == generation
      && cached.built_at.elapsed() < TREE_INDEX_TTL
    {
      return Ok(Some(Arc::clone(&cached.index)));
    }
    let Some(index) = build_tree_index_until(self, should_stop)? else {
      return Ok(None);
    };
    if should_stop() {
      return Ok(None);
    }
    let index = Arc::new(index);
    self.task_tree_index_builds.fetch_add(1, Ordering::Relaxed);
    if self.task_tree_generation.load(Ordering::Acquire) == generation {
      *self.task_tree_index.lock().expect("task tree index mutex is not poisoned") =
        Some(CachedTreeIndex { generation, built_at: Instant::now(), index: Arc::clone(&index) });
    }
    Ok(Some(index))
  }

  fn publish_task_node(&self, meta: &SessionMetadata, event: &Value) -> StoreResult<()> {
    let sessions_dir = self.cfg.sessions_dir();
    fs::create_dir_all(&sessions_dir).map_err(|error| path_error(&sessions_dir, error))?;
    let final_dir = sessions_dir.join(&meta.id);
    let temp_dir = sessions_dir.join(staging_dir_name(&meta.id));
    fs::create_dir(&temp_dir).map_err(|error| path_error(&temp_dir, error))?;
    let operation = (|| {
      let data_dir = temp_dir.join("data");
      fs::create_dir(&data_dir).map_err(|error| path_error(&data_dir, error))?;
      let events_path = data_dir.join("chat_events.jsonl");
      #[cfg(any(test, feature = "write-faults"))]
      self.fail_chat_append(&meta.id, event, &events_path)?;
      crate::ndjson::append(&events_path, event)?;
      let metadata_path = temp_dir.join("metadata.json");
      fsutil::atomic_write_text(&metadata_path, &meta.to_json_string(&self.slots, true))
        .map_err(|error| path_error(&metadata_path, error))?;
      match fs::rename(&temp_dir, &final_dir) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
          if let Some(existing) = self.read_metadata_path(&final_dir.join("metadata.json"))? {
            return if existing.id == meta.id {
              Ok(())
            } else {
              Err(StoreError::new(format!("task create target {} contains another id", final_dir.display())))
            };
          }
          Err(path_error(&final_dir, rename_error))
        }
      }
    })();
    if temp_dir.exists() {
      fs::remove_dir_all(&temp_dir).map_err(|error| path_error(&temp_dir, error))?;
    }
    operation
  }

  /// Name the next session from `sessions/.counter`. An unreadable or unparsable counter takes the
  /// session-directory count, as Python's `_next_session_name` does, and the counter is rewritten after it.
  fn next_session_name(&self) -> StoreResult<String> {
    let sessions_dir = self.cfg.sessions_dir();
    fs::create_dir_all(&sessions_dir).map_err(|error| path_error(&sessions_dir, error))?;
    let counter = sessions_dir.join(".counter");
    let number = match fs::read_to_string(&counter) {
      Ok(value) => match value.trim().parse::<i64>() {
        Ok(number) => number,
        Err(error) => {
          tracing::warn!(path = %counter.display(), %error, "session_counter_unparsable");
          count_session_dirs(&sessions_dir)?
        }
      },
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => count_session_dirs(&sessions_dir)?,
      Err(error) => {
        tracing::warn!(path = %counter.display(), %error, "session_counter_unreadable");
        count_session_dirs(&sessions_dir)?
      }
    };
    fs::write(&counter, (number + 1).to_string()).map_err(|error| path_error(&counter, error))?;
    Ok(format!("Session {number}"))
  }
}

fn build_tree_index_until(store: &Store, should_stop: &mut impl FnMut() -> bool) -> StoreResult<Option<TreeIndex>> {
  if should_stop() {
    return Ok(None);
  }
  let sessions_dir = store.cfg.sessions_dir();
  let entries = match fs::read_dir(&sessions_dir) {
    Ok(entries) => entries,
    Err(error) => return Err(path_error(&sessions_dir, error)),
  };
  let mut names = Vec::new();
  for entry in entries {
    if should_stop() {
      return Ok(None);
    }
    let entry = entry.map_err(|error| path_error(&sessions_dir, error))?;
    if !entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_dir() {
      continue;
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if name.starts_with(".task-") && name.ends_with(".tmp") {
      continue;
    }
    names.push(name);
  }
  names.sort();
  // As `_build_index_sync` (`src/runtime/task_sessions.py:495`): `fresh_cached_metas` uses the same per-entry stat
  // check as `src/runtime/session_store.py:241` and returns shared metadata. The index owns Arcs, so cached metadata
  // incurs no deep clone while building or serving the index.
  let Some(mut cached) = store.fresh_cached_metas_until(should_stop)? else {
    return Ok(None);
  };
  let mut metas = HashMap::new();
  for name in names {
    if should_stop() {
      return Ok(None);
    }
    if let Some(meta) = cached.remove(&name) {
      metas.insert(name, meta);
    } else if let Some(meta) = store.read_metadata_fresh(&name)? {
      #[cfg(test)]
      store.tree_index_metadata_reads.fetch_add(1, Ordering::Relaxed);
      metas.insert(name, Arc::new(meta));
    }
  }
  let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
  for (session_id, meta) in &metas {
    if should_stop() {
      return Ok(None);
    }
    children.entry(meta.task_parent_id.clone()).or_default().push(session_id.clone());
  }
  for child_ids in children.values_mut() {
    if should_stop() {
      return Ok(None);
    }
    child_ids.sort_by(|left, right| {
      let left_meta = &metas[left];
      let right_meta = &metas[right];
      left_meta.created_at.cmp(&right_meta.created_at).then_with(|| left.cmp(right))
    });
  }
  let mut index = TreeIndex {
    metas,
    children,
    task_state_by_id: HashMap::new(),
    revision: String::new(),
    derived_archived_ids: Vec::new(),
    archive_by_id: HashMap::new(),
  };
  let mut memo = HashMap::new();
  let mut structural = Vec::with_capacity(index.metas.len());
  let mut derived_archived_ids = Vec::new();
  for meta in index.metas.values() {
    if should_stop() {
      return Ok(None);
    }
    let task_state = store.task_state_for_tree_index(&meta.id)?;
    let archived = archived_of(store, &index, &meta.id, &mut memo)?;
    index.task_state_by_id.insert(meta.id.clone(), task_state.clone());
    index.archive_by_id.insert(meta.id.clone(), archived);
    if meta.status != SessionStatus::Archived && archived {
      derived_archived_ids.push(meta.id.clone());
    }
    let profile = serde_json::to_value(meta.profile).expect("task profile serializes to JSON");
    structural.push(format!(
      "{}|{}|{}|{}|{}|{}",
      meta.id,
      meta.task_parent_id.as_deref().unwrap_or_default(),
      profile.as_str().expect("task profile serializes as a string"),
      match meta.status {
        SessionStatus::Active => "active",
        SessionStatus::Archived => "archived",
      },
      task_state,
      if archived { "True" } else { "False" },
    ));
  }
  if should_stop() {
    return Ok(None);
  }
  structural.sort();
  derived_archived_ids.sort();
  if should_stop() {
    return Ok(None);
  }
  index.revision = control_events::sha256_hex(&structural.join("\n"));
  index.derived_archived_ids = derived_archived_ids;
  Ok(Some(index))
}

/// Read the task lifecycle fact in the last 1000 non-empty lines, matching `_task_state_fast`.
fn lifecycle_state_from_file(path: &Path) -> StoreResult<Option<String>> {
  let mut file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(path_error(path, error)),
  };
  let size = file.metadata().map_err(|error| path_error(path, error))?.len();
  let ranges = lifecycle_tail_ranges(&mut file, size, path)?;
  for range in ranges {
    let length = usize::try_from(range.end - range.start).expect("one event line fits in memory");
    let mut head = vec![0; length.min(LIFECYCLE_HEAD_PROOF_BYTES)];
    file.seek(SeekFrom::Start(range.start)).map_err(|error| path_error(path, error))?;
    file.read_exact(&mut head).map_err(|error| path_error(path, error))?;
    if lifecycle_head_verdict(&head) == Some(false) {
      continue;
    }
    let raw = if head.len() == length {
      head
    } else {
      let mut line = vec![0; length];
      file.seek(SeekFrom::Start(range.start)).map_err(|error| path_error(path, error))?;
      file.read_exact(&mut line).map_err(|error| path_error(path, error))?;
      line
    };
    let Some(event) = crate::ndjson::parse_line_compact(&raw) else { continue };
    let Some(event_type) = event.get("type").and_then(CompactValue::as_str) else { continue };
    match event_type {
      "clone_start" | "task_reopened" => return Ok(Some("open".to_string())),
      "task_closed" => {
        let state = event
          .get("outcome")
          .and_then(CompactValue::as_str)
          .filter(|value| !value.is_empty())
          .unwrap_or("completed")
          .to_string();
        return Ok(Some(state));
      }
      _ => {}
    }
  }
  Ok(None)
}

/// Find the last 1000 non-empty physical lines without retaining their contents.
fn lifecycle_tail_ranges(file: &mut File, size: u64, path: &Path) -> StoreResult<Vec<std::ops::Range<u64>>> {
  let mut ranges = Vec::new();
  if size == 0 {
    return Ok(ranges);
  }
  let mut cursor = size;
  let mut line_end = size;
  let mut last = [0_u8; 1];
  file.seek(SeekFrom::Start(size - 1)).map_err(|error| path_error(path, error))?;
  file.read_exact(&mut last).map_err(|error| path_error(path, error))?;
  if last[0] == b'\n' {
    cursor -= 1;
    line_end -= 1;
  }

  let mut block = vec![0_u8; LIFECYCLE_TAIL_CHUNK_BYTES];
  while cursor > 0 && ranges.len() < LIFECYCLE_TAIL_LINES {
    let block_start = cursor.saturating_sub(LIFECYCLE_TAIL_CHUNK_BYTES as u64);
    let block_length = usize::try_from(cursor - block_start).expect("a tail block fits in memory");
    file.seek(SeekFrom::Start(block_start)).map_err(|error| path_error(path, error))?;
    file.read_exact(&mut block[..block_length]).map_err(|error| path_error(path, error))?;
    for (index, byte) in block[..block_length].iter().enumerate().rev() {
      if *byte == b'\n' {
        let newline = block_start + index as u64;
        let line_start = newline + 1;
        if line_start < line_end {
          ranges.push(line_start..line_end);
          if ranges.len() == LIFECYCLE_TAIL_LINES {
            break;
          }
        }
        line_end = newline;
      }
    }
    cursor = block_start;
  }
  if ranges.len() < LIFECYCLE_TAIL_LINES && line_end > 0 {
    ranges.push(0..line_end);
  }
  Ok(ranges)
}

/// Return a proven lifecycle verdict for the two serialized event headers used by the Python reader.
fn lifecycle_head_verdict(head: &[u8]) -> Option<bool> {
  let (event_type, _) = if head.starts_with(b"{\"type\"") {
    string_value_after_key(head, b"{\"type\"".len())?
  } else if head.starts_with(b"{\"id\"") {
    let (_, after_id) = string_value_after_key(head, b"{\"id\"".len())?;
    if head.get(after_id) != Some(&b',') {
      return None;
    }
    let mut type_key = after_id + 1;
    while head.get(type_key).is_some_and(u8::is_ascii_whitespace) {
      type_key += 1;
    }
    if !head[type_key..].starts_with(b"\"type\"") {
      return None;
    }
    string_value_after_key(head, type_key + b"\"type\"".len())?
  } else {
    return None;
  };
  Some(event_type == b"clone_start" || event_type == b"task_closed" || event_type == b"task_reopened")
}

fn string_value_after_key(head: &[u8], mut cursor: usize) -> Option<(&[u8], usize)> {
  while head.get(cursor).is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
    cursor += 1;
  }
  if head.get(cursor) != Some(&b':') {
    return None;
  }
  cursor += 1;
  while head.get(cursor).is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
    cursor += 1;
  }
  if head.get(cursor) != Some(&b'"') {
    return None;
  }
  let start = cursor + 1;
  let tail = head.get(start..)?;
  let end = tail.iter().position(|byte| *byte == b'"')?;
  let value = &tail[..end];
  if value.contains(&b'\\') {
    return None;
  }
  Some((value, start + end + 1))
}

fn index_meta<'a>(index: &'a TreeIndex, session_id: &str) -> StoreResult<&'a SessionMetadata> {
  index
    .metas
    .get(session_id)
    .map(Arc::as_ref)
    .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
}

/// The 409 blocker sentence naming a node's closed ancestor tasks.
fn closed_ancestors_blocker(closed: &[String]) -> String {
  format!("closed ancestor task(s): {}", closed.join(", "))
}

fn ancestor_ids(index: &TreeIndex, session_id: &str) -> StoreResult<Vec<String>> {
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

/// Every transitive descendant in depth-first order. Only non-archived nodes count toward the cap, so a closed
/// subtree of any size stays walkable.
fn descendant_ids(store: &Store, index: &TreeIndex, session_id: &str) -> StoreResult<Vec<String>> {
  let mut never_stop = || false;
  Ok(descendant_ids_until(store, index, session_id, &mut never_stop)?.expect("the no-stop descendant walk completes"))
}

fn descendant_ids_until(
  store: &Store,
  index: &TreeIndex,
  session_id: &str,
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<Option<Vec<String>>> {
  if should_stop() {
    return Ok(None);
  }
  if !index.metas.contains_key(session_id) {
    return Err(StoreError::not_found(format!("task {session_id} not found")));
  }
  let children_of = |parent: &str| index.children.get(&Some(parent.to_string())).cloned().unwrap_or_default();
  let mut output = Vec::new();
  let mut stack = children_of(session_id);
  let mut seen = HashSet::from([session_id.to_string()]);
  let mut archived = HashMap::new();
  let mut open_count = 0;
  while let Some(next) = stack.pop() {
    if should_stop() {
      return Ok(None);
    }
    if !seen.insert(next.clone()) {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {next}")]));
    }
    output.push(next.clone());
    stack.extend(children_of(&next));
    let meta = index_meta(index, &next)?;
    if meta.status != SessionStatus::Archived && !archived_of(store, index, &next, &mut archived)? {
      open_count += 1;
      if open_count > TASK_DESCENDANT_NODE_LIMIT {
        return Err(StoreError::conflict(vec![format!(
          "subtree of {session_id} exceeds {TASK_DESCENDANT_NODE_LIMIT} non-archived nodes"
        )]));
      }
    }
  }
  if should_stop() {
    return Ok(None);
  }
  Ok(Some(output))
}

/// Whether a node is archived, counting an archived ancestor: a node is archived when its own task state or
/// any ancestor's is not open. The memo keeps each resolved node, so one pass over many nodes stays linear.
fn archived_of(
  store: &Store,
  index: &TreeIndex,
  session_id: &str,
  memo: &mut HashMap<String, bool>,
) -> StoreResult<bool> {
  if let Some(archived) = index.archive_by_id.get(session_id) {
    return Ok(*archived);
  }
  let mut chain = Vec::new();
  let mut seen = HashSet::new();
  let mut current_id = session_id;
  let mut inherited = loop {
    if let Some(&known) = memo.get(current_id) {
      break known;
    }
    if !seen.insert(current_id) || chain.len() > TASK_ANCESTOR_HOP_LIMIT {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {current_id}")]));
    }
    chain.push(current_id);
    match index_meta(index, current_id)?.task_parent_id.as_deref() {
      Some(parent_id) => current_id = parent_id,
      None => break false,
    }
  };
  for node_id in chain.into_iter().rev() {
    if !inherited {
      inherited = store.task_state_fast(node_id)? != "open";
    }
    memo.insert(node_id.to_string(), inherited);
  }
  Ok(inherited)
}

/// Fold `events`, which start at absolute history position `offset`, into `facts` holding the history before them.
#[cfg(test)]
fn fold_task_facts(events: &[Value]) -> StoreResult<TaskFacts> {
  let events = events.iter().map(|event| Arc::new(ChatEvent::from_value(event))).collect::<Vec<_>>();
  let mut facts = TaskFacts::default();
  let mut never_stop = || false;
  assert!(fold_task_facts_from(&mut facts, &events, 0, &mut never_stop, None)?);
  Ok(facts)
}

fn fold_task_facts_from(
  facts: &mut TaskFacts,
  events: &[SharedChatEvent],
  offset: usize,
  should_stop: &mut impl FnMut() -> bool,
  run_facts: Option<&mut RunFacts>,
) -> StoreResult<bool> {
  let mut run_facts = run_facts;
  for (index, event) in events.iter().enumerate() {
    if index % TASK_FACTS_STOP_CHECK_INTERVAL == 0 && should_stop() {
      return Ok(false);
    }
    let event = event.as_ref();
    if let Some(run_facts) = run_facts.as_deref_mut() {
      run_facts.observe(event);
    }
    let position = offset + index;
    match event.get("type").and_then(CompactValue::as_str) {
      Some("clone_start") => facts.has_user_request = false,
      _ if event_types::is_real_user_message(event) || event_types::is_platform_summon(event) => {
        facts.has_user_request = true;
      }
      _ => {}
    }
    if event.get("id").and_then(CompactValue::as_str).is_some()
      && !facts.events_by_id.insert(Arc::clone(&events[index]))
    {
      continue;
    }
    let event_type = event.get("type").and_then(CompactValue::as_str).unwrap_or_default();
    match event_type {
      "clone_start" => {
        facts.task_state = "open".to_string();
        facts.boundary_index = None;
        facts.boundary_open = true;
        facts.input_candidates.clear();
        facts.imported_pending_ids.clear();
        facts.close_events.clear();
        facts.close_requests.clear();
        facts.delivered_reports.clear();
      }
      "task_created" => {
        if facts.boundary_index.is_none() {
          facts.boundary_index = Some(position);
          facts.input_candidates.clear();
        }
      }
      "task_imported" => {
        facts.boundary_index = Some(position);
        facts.imported_pending_ids.clear();
        for input_id in imported_pending_input_ids(event)? {
          facts.imported_pending_ids.insert(input_id.to_string());
        }
        facts.input_candidates.clear();
        for (prior_index, prior) in facts.events_by_id.values().enumerate() {
          if prior_index % TASK_FACTS_STOP_CHECK_INTERVAL == 0 && should_stop() {
            return Ok(false);
          }
          if admits_input(prior)
            && prior.get("id").and_then(CompactValue::as_str).is_some_and(|id| facts.imported_pending_ids.contains(id))
          {
            facts.input_candidates.push(Arc::clone(prior));
          }
        }
      }
      "task_closed" => {
        facts.task_state = event
          .get("outcome")
          .and_then(CompactValue::as_str)
          .filter(|value| !value.is_empty())
          .unwrap_or("completed")
          .to_string();
        facts.close_events.push(Arc::clone(&events[index]));
        facts.boundary_index = Some(position);
        facts.boundary_open = false;
        facts.imported_pending_ids.clear();
        facts.input_candidates.clear();
      }
      "task_reopened" => {
        facts.task_state = "open".to_string();
        facts.boundary_index = Some(position);
        facts.boundary_open = true;
      }
      "task_close_requested" => {
        facts.close_requests.push(Arc::clone(&events[index]));
      }
      "run_finished" => {
        if let Some(run_id) = event.get("run_id").and_then(CompactValue::as_str) {
          let outcome = event.get("outcome").and_then(CompactValue::as_str).unwrap_or("None").to_string();
          if event.get("outcome").and_then(CompactValue::as_str) == Some("None") {
            facts.run_outcome_string_none_ids.insert(run_id.to_string());
          } else {
            facts.run_outcome_string_none_ids.remove(run_id);
          }
          facts.run_outcomes.insert(run_id.to_string(), outcome.clone());
          facts.run_finish_positions.entry(run_id.to_string()).or_insert(position);
          if outcome == "success"
            && let Some(input_ids) = event.get("input_event_ids").and_then(CompactValue::as_array)
          {
            facts.confirmed_input_ids.extend(input_ids.iter().filter_map(CompactValue::as_str).map(str::to_string));
          }
        }
      }
      "task_input_acknowledged" => {
        if let Some(input_ids) = event.get("input_ids").and_then(CompactValue::as_array) {
          facts.confirmed_input_ids.extend(input_ids.iter().filter_map(CompactValue::as_str).map(str::to_string));
        }
      }
      "child_report" => {
        if let (Some(child_id), Some(child_event_id)) = (
          event.get("child_session_id").and_then(CompactValue::as_str),
          event.get("child_event_id").and_then(CompactValue::as_str),
        ) {
          facts.delivered_reports.insert((child_id.to_string(), child_event_id.to_string()));
        }
      }
      _ => {}
    }
    let imported = event
      .get("id")
      .and_then(CompactValue::as_str)
      .is_some_and(|event_id| facts.imported_pending_ids.contains(event_id));
    if admits_input(event)
      && facts.boundary_open
      && (facts.boundary_index.is_none_or(|boundary| position > boundary) || imported)
    {
      facts.input_candidates.push(Arc::clone(&events[index]));
    }
  }
  facts.covered = offset + events.len();
  Ok(!should_stop())
}

/// The stable input ids that a `task_imported` fact lists as pending. A list that is not an array, or an entry without
/// a non-empty `input_id`, is the task fold's error.
fn imported_pending_input_ids(event: &ChatEvent) -> StoreResult<Vec<&str>> {
  let pending = match event.get("pending_inputs") {
    None | Some(CompactValue::Null) => return Ok(Vec::new()),
    Some(CompactValue::Array(pending)) => pending,
    Some(_) => return Err(StoreError::new("task_imported pending_inputs must be an array")),
  };
  pending
    .iter()
    .map(|entry| {
      entry.get("input_id").and_then(CompactValue::as_str).filter(|value| !value.is_empty()).ok_or_else(|| {
        StoreError::new(format!(
          "task_imported event {:?} lists a pending input without a stable input_id",
          event.get("id")
        ))
      })
    })
    .collect()
}

/// Fold `events`, which start at absolute history position `offset`, into the node's facts. A task fold error stays:
/// the rejected fact remains in the history for as long as the memo covers it.
fn fold_node_facts_from(
  facts: &mut NodeFacts,
  events: &[SharedChatEvent],
  offset: usize,
  should_stop: &mut impl FnMut() -> bool,
  run_facts: Option<&mut RunFacts>,
) -> StoreResult<bool> {
  if should_stop() {
    return Ok(false);
  }
  let mut run_facts = run_facts;
  let mut task_error = None;
  if let Ok(task) = &mut facts.task {
    match fold_task_facts_from(task, events, offset, should_stop, run_facts.as_deref_mut()) {
      Ok(true) => return Ok(true),
      Ok(false) => return Ok(false),
      Err(error) => task_error = Some(error.to_string()),
    }
  }
  if let Some(error) = task_error {
    facts.task = Err(error);
  }
  // A malformed task fold still has complete Run facts, like Python's independent Run-facts memo.
  // Valid histories collect both sets during the task fold above.
  for (index, event) in events.iter().enumerate() {
    if index % TASK_FACTS_STOP_CHECK_INTERVAL == 0 && should_stop() {
      return Ok(false);
    }
    if let Some(run_facts) = run_facts.as_deref_mut() {
      run_facts.observe(event.as_ref());
    }
  }
  Ok(!should_stop())
}

fn observe_run_facts(
  facts: &mut RunFacts,
  events: &[SharedChatEvent],
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<bool> {
  for (index, event) in events.iter().enumerate() {
    if index % TASK_FACTS_STOP_CHECK_INTERVAL == 0 && should_stop() {
      return Ok(false);
    }
    facts.observe(event.as_ref());
  }
  Ok(!should_stop())
}

fn admits_input(event: &ChatEvent) -> bool {
  match event.get("type").and_then(CompactValue::as_str) {
    Some("user") => event.get("content").is_some_and(CompactValue::is_string),
    Some("agent_message" | "scheduled_trigger" | "child_report") => true,
    _ => false,
  }
}

fn no_user_request_error(session_id: &str) -> StoreError {
  StoreError::forbidden(format!(
    "Delegation blocked: no user request on the task chain from task {session_id} to its root. A delegation serves a line that a real user message or a thread platform summon started; other agent messages, scheduled triggers and child reports do not count."
  ))
}

/// Whether a node's task carries the read-only exemption: verify and cold-read are read-only and repo-less, so the
/// runtime gate does not need a user message for them. The one owner of the judgment
/// (`takeoff_gate.is_read_only_exempt`); no task is not exempt.
pub fn is_read_only_exempt(task: Option<&TaskSpec>) -> bool {
  task
    .and_then(|task| task.task_type)
    .is_some_and(|task_type| matches!(task_type, TaskType::Verify | TaskType::ColdRead))
}

fn task_spec_hash(task: &TaskSpec) -> StoreResult<String> {
  let serialized = serde_json::to_string(task).map_err(|error| StoreError::new(error.to_string()))?;
  Ok(control_events::sha256_hex(&serialized))
}

/// One archive-format close fact: outcome `archived` and no report target, so an archive wakes nobody.
pub(crate) fn archived_close_event(
  session_id: &str,
  request_id: &str,
  actor: &str,
  summary: &str,
  run_ids: &[&str],
  archived_with: Option<&str>,
) -> Value {
  let mut payload = vec![
    ("request_id".to_string(), Value::String(request_id.to_string())),
    ("outcome".to_string(), Value::String("archived".to_string())),
    ("summary".to_string(), Value::String(summary.to_string())),
    ("result_refs".to_string(), json!([])),
    ("run_ids".to_string(), json!(run_ids)),
    ("report_to".to_string(), Value::Null),
  ];
  if let Some(archived_with) = archived_with {
    payload.push(("archived_with".to_string(), Value::String(archived_with.to_string())));
  }
  control_events::build_control_event(
    "task_closed",
    actor,
    session_id,
    Some(&control_events::stable_close_event_id(session_id, request_id)),
    None,
    payload,
  )
}

fn caller_actor(caller: &TaskCaller) -> &'static str {
  match caller {
    TaskCaller::Operator => ACTOR_USER,
    TaskCaller::System => ACTOR_SYSTEM,
    TaskCaller::Agent { .. } => ACTOR_AGENT,
  }
}

fn set_registered_slot_values(
  slots: &[charliebot_core::metadata_slot::MetadataSlot],
  meta: &mut SessionMetadata,
  values: Map<String, Value>,
) -> StoreResult<()> {
  for (name, value) in values {
    let matches = slots
      .iter()
      .filter(|slot| slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .flat_map(|slot| slot.fields.iter())
      .filter(|field| field.name == name)
      .count();
    if matches == 0 {
      return Err(StoreError::new(format!("no owner registered the key {name:?}")));
    }
    if matches > 1 {
      return Err(StoreError::new(format!("metadata slot key {name:?} is registered more than once")));
    }
    meta.set_extra(name, value);
  }
  Ok(())
}

fn default_task_name(task: &TaskSpec, profile: TaskProfile) -> Option<String> {
  let lines: Vec<&str> = task.goal.lines().filter(|line| !line.trim().is_empty()).collect();
  let content = lines.iter().copied().find(|line| !is_markdown_heading(line));
  let selected = content.or_else(|| lines.first().copied().map(strip_heading))?;
  let selected = selected.trim();
  if selected.is_empty() {
    return Some(format!("New {} task", profile_name(profile)));
  }
  Some(selected.chars().take(80).collect())
}

fn is_markdown_heading(line: &str) -> bool {
  let leading_spaces = line.chars().take_while(|value| *value == ' ').count();
  if leading_spaces > 3 {
    return false;
  }
  let suffix = &line[leading_spaces..];
  let marks = suffix.chars().take_while(|value| *value == '#').count();
  (1..=6).contains(&marks) && suffix[marks..].chars().next().is_none_or(char::is_whitespace)
}

fn strip_heading(line: &str) -> &str {
  let trimmed = line.trim_start();
  trimmed.trim_start_matches('#').trim_start()
}

fn profile_name(profile: TaskProfile) -> &'static str {
  match profile {
    TaskProfile::Manager => "manager",
    TaskProfile::Worker => "worker",
  }
}

fn count_session_dirs(sessions_dir: &Path) -> StoreResult<i64> {
  let mut count = 0;
  for entry in fs::read_dir(sessions_dir).map_err(|error| path_error(sessions_dir, error))? {
    // Path::is_dir follows symlinks, as Python's Path.is_dir does; DirEntry::file_type would not.
    if entry.map_err(|error| path_error(sessions_dir, error))?.path().is_dir() {
      count += 1;
    }
  }
  Ok(count)
}

fn path_error(path: &Path, error: impl std::fmt::Display) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

/// The unpublished create's staging directory name, matching Python's
/// `.task-{task_id}-{os.getpid()}-{uuid4().hex}.tmp` in src/runtime/task_sessions.py.
fn staging_dir_name(task_id: &str) -> String {
  format!(".task-{}-{}-{}.tmp", task_id, std::process::id(), uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
pub(crate) mod tests {
  use std::collections::HashMap;
  use std::sync::Arc;

  use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
  use serde_json::{Map, Value, json};

  use crate::ChatEvent;
  use crate::compact::CompactValue;

  use super::*;
  use crate::metadata::{TaskProfile, TaskSpec};
  use crate::run_records::{RunKind, RunOutcomeKind, RunRecord};
  use crate::store::{
    AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields,
    StoreHookFuture, StoreHooks,
  };
  use crate::streaming::{SIDEBAR_CHANNEL, Subscription};

  struct TestAccountLabels;

  impl AccountLabelKeeper for TestAccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |value| Value::String(value.to_string()));
      if meta.extra("account_label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("account_label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("account_label").and_then(Value::as_str).map(str::to_string)
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

    fn listing_fields(&self, _ids: &[String], _now: UtcDateTime) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  struct TestSequenceBindingMover;

  impl SequenceBindingMover for TestSequenceBindingMover {
    fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(true) })
    }
  }

  fn slots() -> Vec<MetadataSlot> {
    vec![
      MetadataSlot {
        owner: "cron",
        on: MetadataFile::Session,
        after: Some("backend"),
        fields: vec![
          SlotField { name: "last_scheduled_run", default: Value::Null },
          SlotField { name: "last_scheduled_cron", default: Value::Null },
          SlotField { name: "last_run_status", default: Value::Null },
        ],
      },
      MetadataSlot {
        owner: "slack",
        on: MetadataFile::Session,
        after: Some("successor_session_id"),
        fields: vec![SlotField { name: "slack_watermark_ts", default: Value::Null }],
      },
    ]
  }

  pub(crate) fn make_store(home: &Path) -> Store {
    make_store_with_archive(home, Arc::new(crate::store::test_support::SequenceArchive))
  }

  fn make_store_with_archive(home: &Path, sequence_archive: Arc<dyn SequenceArchiveHook>) -> Store {
    fs::create_dir_all(home).unwrap();
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      slots(),
      StoreHooks {
        account_labels: Some(Arc::new(TestAccountLabels)),
        anchor_reconciler: Some(Arc::new(TestAnchorReconciler)),
        sequence_listing: Some(Arc::new(TestSequenceListing)),
        runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
        sequence_archive: Some(sequence_archive),
        sequence_binding_mover: Some(Arc::new(TestSequenceBindingMover)),
      },
    )
    .unwrap()
  }

  pub(crate) async fn create_node(
    store: &Store,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
  ) -> SessionMetadata {
    store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: None,
          name: Some(request_id.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  async fn create_unnamed_node(store: &Store, request_id: &str) -> SessionMetadata {
    store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: None,
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  fn user_event(content: Value) -> Value {
    json!({"type": "user", "content": content})
  }

  async fn append(store: &Store, session_id: &str, event: Value) {
    store.append_chat_event(session_id, event).await.unwrap();
  }

  #[tokio::test]
  async fn create_replay_and_slot_updates_preserve_the_original_node() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root-request", None, TaskProfile::Manager).await;
    let mut values = Map::new();
    values.insert("slack_watermark_ts".to_string(), json!("before"));
    let child_request = CreateTask {
      request_id: "child-request".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: Some(TaskSpec { goal: "child goal".to_string(), ..TaskSpec::default() }),
      name: Some("Child".to_string()),
      backend: Some("synthetic".to_string()),
      group: Some("group-a".to_string()),
      session_id: None,
    };
    let child = store.create_task(child_request.clone(), TaskCaller::System, values).await.unwrap();
    let event_count = store.load_chat_events_sync(&child.id).unwrap().len();
    let replay = store.create_task(child_request, TaskCaller::System, Map::new()).await.unwrap();
    assert_eq!(replay.created_by_event, child.created_by_event);
    assert_eq!(store.load_chat_events_sync(&child.id).unwrap().len(), event_count);

    let guard = store.control_lock().await;
    let mut update = Map::new();
    update.insert("slack_watermark_ts".to_string(), json!("after"));
    let updated = store.update_task_slot_fields_locked(&guard, &child.id, "slack", update).await.unwrap();
    assert_eq!(updated.extra("slack_watermark_ts"), Some(&json!("after")));
    assert_eq!(updated.backend, "synthetic");
    assert_eq!(updated.task_parent_id.as_deref(), Some(root.id.as_str()));
  }

  #[tokio::test]
  async fn record_native_anchor_reset_clears_only_the_held_conversation() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "native-anchor", None, TaskProfile::Manager).await;
    store.persist_cc_session_id(&node.id, "previous-conversation", None).await.unwrap();

    store.record_native_anchor_with_reset(&node.id, "prompt-hash", "synthetic", Some("model-a"), true).await.unwrap();

    let updated = store.read_metadata_fresh(&node.id).unwrap().unwrap();
    assert_eq!(updated.native_prompt_hash.as_deref(), Some("prompt-hash"));
    assert_eq!(updated.native_backend.as_deref(), Some("synthetic"));
    assert_eq!(updated.native_model.as_deref(), Some("model-a"));
    assert_eq!(updated.cc_session_id, None);
    assert_eq!(updated.cc_session_started_at, None);
  }

  #[tokio::test]
  async fn gate_matches_manager_chain_user_and_open_state_rules() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let grandchild = create_node(&store, "grandchild", Some(&child.id), TaskProfile::Manager).await;

    append(&store, &root.id, user_event(json!("Can you inspect this?"))).await;
    append(&store, &child.id, json!({"type": "agent_message", "content": "progress"})).await;
    assert_eq!(store.check_task_authorization(&grandchild.id).unwrap(), root.id);
    append(&store, &child.id, user_event(json!("Which file?"))).await;
    assert_eq!(store.check_task_authorization(&grandchild.id).unwrap(), child.id);

    for state in ["completed", "cancelled", "archived"] {
      let guard = store.control_lock().await;
      let event = control_events::build_control_event(
        "task_closed",
        ACTOR_SYSTEM,
        &root.id,
        None,
        None,
        [("outcome".to_string(), json!(state))],
      );
      store.append_control_fact(&guard, &root.id, event).await.unwrap();
      drop(guard);
      let error = store.check_task_authorization(&grandchild.id).unwrap_err();
      assert_eq!(error.to_string(), format!("Delegation blocked: ancestor task {} is {state}; open it first", root.id));
      let guard = store.control_lock().await;
      let reopen = control_events::build_control_event("task_reopened", ACTOR_SYSTEM, &root.id, None, None, []);
      store.append_control_fact(&guard, &root.id, reopen).await.unwrap();
    }

    let worker = create_node(&store, "worker", Some(&child.id), TaskProfile::Worker).await;
    let error = store.check_task_authorization(&worker.id).unwrap_err();
    assert_eq!(
      error.to_string(),
      format!("task {} is not a manager; agent calls run only under a manager task", worker.id)
    );
  }

  #[tokio::test]
  async fn gate_ignores_machine_inputs_and_resets_at_clone_markers() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let grandchild = create_node(&store, "grandchild", Some(&child.id), TaskProfile::Manager).await;
    for session_id in [&root.id, &child.id, &grandchild.id] {
      append(&store, session_id, json!({"type": "agent_message", "content": "take off"})).await;
      append(&store, session_id, json!({"type": "scheduled_trigger", "content": "take off"})).await;
      append(&store, session_id, json!({"type": "child_report", "content": "take off"})).await;
      append(&store, session_id, user_event(json!([{"type": "tool_result", "content": "take off"}]))).await;
    }
    let error = store.check_task_authorization(&grandchild.id).unwrap_err();
    assert!(error.to_string().contains("no user request on the task chain"));

    let clone = create_node(&store, "clone", Some(&root.id), TaskProfile::Manager).await;
    append(&store, &clone.id, user_event(json!("source request"))).await;
    append(&store, &clone.id, json!({"type": "clone_start"})).await;
    assert!(store.check_task_authorization(&clone.id).unwrap_err().to_string().contains("no user request"));
    append(&store, &clone.id, user_event(json!("new request"))).await;
    assert_eq!(store.check_task_authorization(&clone.id).unwrap(), clone.id);
    append(&store, &clone.id, json!({"type": "clone_start"})).await;
    assert!(store.check_task_authorization(&clone.id).unwrap_err().to_string().contains("no user request"));
  }

  #[tokio::test]
  async fn agent_create_runs_the_gate_except_for_coordination_and_read_only_work() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "agent-root", None, TaskProfile::Manager).await;
    let manager_request = CreateTask {
      request_id: "agent-manager-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Manager,
      task: None,
      name: Some("coordination child".to_string()),
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    assert!(
      store.create_task(manager_request, TaskCaller::Agent { session_id: root.id.clone() }, Map::new()).await.is_ok()
    );

    let worker_request = CreateTask {
      request_id: "agent-worker-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: Some("implementation child".to_string()),
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    assert!(
      store
        .create_task(worker_request.clone(), TaskCaller::Agent { session_id: root.id.clone() }, Map::new())
        .await
        .unwrap_err()
        .to_string()
        .contains("no user request on the task chain")
    );

    let verify_request = CreateTask {
      task: Some(TaskSpec { task_type: Some(TaskType::Verify), ..TaskSpec::default() }),
      request_id: "agent-verify-child".to_string(),
      ..worker_request.clone()
    };
    assert!(
      store.create_task(verify_request, TaskCaller::Agent { session_id: root.id.clone() }, Map::new()).await.is_ok()
    );
    append(&store, &root.id, user_event(json!("Please make the change."))).await;
    assert!(
      store
        .create_task(
          CreateTask { request_id: "agent-worker-after-user".to_string(), ..worker_request },
          TaskCaller::Agent { session_id: root.id },
          Map::new(),
        )
        .await
        .is_ok()
    );
  }

  #[tokio::test]
  async fn task_index_archive_activity_and_successor_reads_follow_durable_state() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "leaf", Some(&child.id), TaskProfile::Worker).await;
    assert_eq!(store.ancestor_ids(&leaf.id).unwrap(), [child.id.clone(), root.id.clone()]);
    let child_ids = store.child_ids(&root.id).unwrap();
    assert_eq!(child_ids.len(), 1);
    assert_eq!(child_ids[0], child.id);
    assert_eq!(store.descendant_ids(&root.id).unwrap(), [child.id.clone(), leaf.id.clone()]);

    let run = RunRecord {
      id: "queued-run".to_string(),
      session_id: leaf.id.clone(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic".to_string()),
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
    };
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &leaf.id, run, None).await.unwrap();
    assert_eq!(store.task_tree_activity(&leaf.id).unwrap().work_state, "waiting");
    let stopped = control_events::build_control_event(
      "run_stop_requested",
      ACTOR_SYSTEM,
      &leaf.id,
      None,
      None,
      [("request_id".to_string(), json!("cancel-queued")), ("run_id".to_string(), json!("queued-run"))],
    );
    store.append_control_fact(&guard, &leaf.id, stopped).await.unwrap();
    let close = control_events::build_control_event(
      "task_closed",
      ACTOR_SYSTEM,
      &root.id,
      None,
      None,
      [("outcome".to_string(), json!("archived"))],
    );
    store.append_control_fact(&guard, &root.id, close).await.unwrap();
    drop(guard);
    store.invalidate_tree_index();
    assert!(store.archived_of(&leaf.id).unwrap());
    let mut archived = store.derived_archived_ids().unwrap();
    archived.sort();
    let mut expected_archived = vec![root.id.clone(), child.id.clone(), leaf.id.clone()];
    expected_archived.sort();
    assert_eq!(archived, expected_archived);
    let active: Vec<String> = store
      .list_sessions(Some(SessionStatus::Active), None, None)
      .unwrap()
      .rows
      .into_iter()
      .map(|meta| meta.id)
      .collect();
    assert!(!active.contains(&leaf.id));
    let archived_page = store.list_archived_page(None, 10, None, None).unwrap();
    let archived_page_ids: Vec<String> = archived_page.sessions.into_iter().map(|meta| meta.id).collect();
    assert!(archived_page_ids.contains(&root.id));
    assert!(archived_page_ids.contains(&child.id));
    assert!(archived_page_ids.contains(&leaf.id));
    assert_eq!(store.read_metadata_fresh(&leaf.id).unwrap().unwrap().status, SessionStatus::Active);
    assert_eq!(store.task_tree_activity(&leaf.id).unwrap().work_state, "idle");

    let successor = create_node(&store, "successor", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store
      .mutate_fresh(&root.id, false, true, |meta| {
        meta.successor_session_id = Some(successor.id.clone());
        Ok(true)
      })
      .await
      .unwrap();
    drop(guard);
    store.invalidate_tree_index();
    let chain = store.resolve_successor_chain(&root.id).unwrap();
    assert_eq!(
      chain.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(),
      [root.id.as_str(), successor.id.as_str()]
    );
    assert_eq!(
      store.deliver_to_successor(&root.id, json!({"type": "agent_message", "content": "forwarded"})).await.unwrap(),
      Some(successor.id.clone())
    );
    let event = store.load_chat_events_sync(&successor.id).unwrap().pop().unwrap();
    assert_eq!(event["origin_session_id"].as_str(), Some(root.id.as_str()));
  }

  #[tokio::test]
  async fn tree_index_reuses_cached_metadata_and_shares_its_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "index-root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "index-child", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "index-leaf", Some(&child.id), TaskProfile::Worker).await;
    for id in [&root.id, &child.id, &leaf.id] {
      store.get_session(id).await.unwrap().unwrap();
    }

    store.invalidate_tree_index();
    store.tree_index_metadata_reads.store(0, Ordering::Relaxed);
    let first = store.tree_index().unwrap();
    let cached = store.fresh_cached_meta(&root.id).unwrap().unwrap();
    let second = store.tree_index().unwrap();

    assert_eq!(store.tree_index_metadata_reads.load(Ordering::Relaxed), 0);
    assert!(Arc::ptr_eq(&cached, first.metas.get(&root.id).unwrap()));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(store.child_ids(&root.id).unwrap(), [child.id]);
  }

  #[tokio::test]
  async fn tree_index_build_stops_between_nodes_without_publishing_a_partial_index() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "stop-root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "stop-child", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "stop-leaf", Some(&child.id), TaskProfile::Worker).await;
    for id in [&root.id, &child.id, &leaf.id] {
      store.get_session(id).await.unwrap().unwrap();
    }

    store.invalidate_tree_index();
    let builds_before = store.task_tree_index_build_count();
    let mut checks = 0;
    let stopped = store
      .descendant_ids_until(&root.id, &mut || {
        checks += 1;
        checks == 10
      })
      .unwrap();

    assert!(stopped.is_none(), "a stopped index build returns no partial descendant list");
    assert_eq!(checks, 10, "the metadata index checks the stop between node reads");
    assert_eq!(store.task_tree_index_build_count(), builds_before, "a stopped build is not published");
    assert_eq!(store.descendant_ids(&root.id).unwrap(), [child.id, leaf.id]);
    assert_eq!(store.task_tree_index_build_count(), builds_before + 1, "the retry publishes one complete index");
  }

  #[tokio::test]
  async fn descendant_walk_checks_the_stop_for_each_node() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "walk-root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "walk-child", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "walk-leaf", Some(&child.id), TaskProfile::Worker).await;
    assert_eq!(store.descendant_ids(&root.id).unwrap(), [child.id.clone(), leaf.id]);

    let mut checks = 0;
    let stopped = store
      .descendant_ids_until(&root.id, &mut || {
        checks += 1;
        checks == 4
      })
      .unwrap();

    assert!(stopped.is_none(), "a stopped walk discards its partial descendant list");
    assert_eq!(checks, 4, "the cached index lookup and each visited descendant check the stop");
  }

  #[tokio::test]
  async fn prompt_facts_are_idempotent_and_elone_rebind_copies_cron_fields() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let source = create_node(&store, "source", None, TaskProfile::Manager).await;
    let new = create_node(&store, "new", None, TaskProfile::Manager).await;
    let ref_a = store.store_prompt_body("synthetic rule\n").unwrap();
    let ref_b = store.store_prompt_body("replacement rule\n").unwrap();
    let mut metadata = source.clone();
    metadata.node_prompt_ref = Some(ref_a.clone());
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    let first_len = store.load_chat_events_sync(&source.id).unwrap().len();
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    assert_eq!(store.load_chat_events_sync(&source.id).unwrap().len(), first_len);
    metadata.node_prompt_ref = Some(ref_b.clone());
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    let prompts: Vec<_> = store
      .load_chat_events_sync(&source.id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"].as_str() == Some("prompt_changed"))
      .collect();
    assert_eq!(prompts[1]["previous_ref"].to_value(), ref_a);
    assert_eq!(prompts[1]["new_ref"].to_value(), ref_b);

    let mut source_slots = Map::new();
    source_slots.insert("last_scheduled_run".to_string(), json!("2025-02-03T04:05:06+00:00"));
    source_slots.insert("last_scheduled_cron".to_string(), json!("0 0 * * *"));
    source_slots.insert("last_run_status".to_string(), json!("success"));
    let source = {
      let guard = store.control_lock().await;
      store.update_slot_fields_locked(&guard, &source.id, "cron", source_slots).await.unwrap().unwrap()
    };
    let guard = store.control_lock().await;
    assert!(store.rebind_elone_bindings_locked(&guard, &source.id, &new.id).await.unwrap());
    let updated = store.read_metadata_fresh(&new.id).unwrap().unwrap();
    assert_eq!(updated.extra("last_scheduled_cron"), Some(&json!("0 0 * * *")));
    assert_eq!(updated.extra("last_run_status"), Some(&json!("success")));
  }

  pub(crate) fn queued_run(session_id: &str, run_id: &str) -> RunRecord {
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
      backend: Some("synthetic".to_string()),
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

  /// Structural guards keyed by node: the dispatcher answers these in production.
  #[derive(Default)]
  struct StubGuards {
    pending: HashMap<String, Vec<String>>,
  }

  impl StructuralBlockers for StubGuards {
    fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
      Ok(self.pending.get(session_id).cloned().unwrap_or_default())
    }

    fn undelivered_report_blockers(&self, _session_id: &str) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }
  }

  fn prompt_facts(store: &Store, session_id: &str) -> Vec<ChatEvent> {
    store
      .load_chat_events_sync(session_id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_types::PROMPT_CHANGED))
      .collect()
  }

  #[tokio::test]
  async fn patch_task_refuses_structural_changes_under_guards_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let other = create_node(&store, "other", None, TaskProfile::Manager).await;
    let worker = create_node(&store, "worker", Some(&root.id), TaskProfile::Worker).await;
    let pending = format!("{} has an unprocessed input", worker.id);
    let guards = StubGuards { pending: HashMap::from([(worker.id.clone(), vec![pending.clone()])]) };

    let demotion = PatchTask { profile: Some(Some(TaskProfile::Worker)), ..Default::default() };
    let error = store.patch_task(&root.id, demotion, TaskCaller::Operator, &guards).await.unwrap_err();
    let blocker = "demotion to worker requires a task with no child tasks".to_string();
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&blocker)));

    // The node's own guard counts once for the structural change and once for the moving subtree, as Python's does.
    let reparent = PatchTask { task_parent_id: Some(Some(other.id.clone())), ..Default::default() };
    let error = store.patch_task(&worker.id, reparent, TaskCaller::Operator, &guards).await.unwrap_err();
    let mut expected = vec![pending.clone(), format!("{}: {pending}", worker.id)];
    expected.sort();
    assert_eq!(error.blockers(), Some(expected.as_slice()));

    let ghost = PatchTask { task_parent_id: Some(Some("ghost".to_string())), ..Default::default() };
    let error = store.patch_task(&worker.id, ghost, TaskCaller::Operator, &guards).await.unwrap_err();
    assert_eq!(error.status_code(), Some(404));
    assert_eq!(error.to_string(), "task ghost not found");

    let renamed = PatchTask { name: Some("renamed".to_string()), ..Default::default() };
    let error = store.patch_task(&root.id, renamed, TaskCaller::System, &guards).await.unwrap_err();
    assert_eq!(error.to_string(), "task metadata mutations require operator credentials");
    assert_eq!(error.status_code(), Some(403));

    let root_after = store.read_metadata_fresh(&root.id).unwrap().unwrap();
    let worker_after = store.read_metadata_fresh(&worker.id).unwrap().unwrap();
    assert_eq!(root_after.profile, TaskProfile::Manager);
    assert_eq!(root_after.name, root.name);
    assert_eq!(worker_after.task_parent_id.as_deref(), Some(root.id.as_str()));
    assert!(prompt_facts(&store, &root.id).is_empty());
  }

  #[tokio::test]
  async fn patch_task_lands_one_prompt_fact_per_scope_edit_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let subtree_ref = store.store_prompt_body("subtree rule\n").unwrap();
    let node_ref = store.store_prompt_body("node rule\n").unwrap();
    let edit = || PatchTask {
      name: Some("renamed".to_string()),
      subtree_prompt: Some(Some("subtree rule\n".to_string())),
      node_prompt: Some(Some("node rule\n".to_string())),
      ..Default::default()
    };

    let guards = StubGuards::default();
    let patched = store.patch_task(&root.id, edit(), TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(patched.name, "renamed");
    assert_eq!(patched.subtree_prompt_ref.as_deref(), Some(subtree_ref.as_str()));
    assert_eq!(patched.node_prompt_ref.as_deref(), Some(node_ref.as_str()));
    assert_eq!(prompt_facts(&store, &root.id).len(), 2);

    store.patch_task(&root.id, edit(), TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(prompt_facts(&store, &root.id).len(), 2);

    let cleared = PatchTask { node_prompt: Some(None), ..Default::default() };
    let patched = store.patch_task(&root.id, cleared, TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(patched.node_prompt_ref, None);
    let facts = prompt_facts(&store, &root.id);
    assert_eq!(facts.len(), 3);
    assert_eq!(facts[2]["scope"].as_str(), Some("node"));
    assert_eq!(facts[2]["previous_ref"].to_value(), node_ref);
    assert_eq!(facts[2]["new_ref"].to_value(), Value::Null);
  }

  #[tokio::test]
  async fn patch_task_lands_an_interrupted_prompt_edit_before_the_next_one() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let interrupted_ref = store.store_prompt_body("interrupted rule\n").unwrap();
    // A crash after the metadata swap and before its fact: the metadata is ahead of the fact stream.
    store
      .mutate_fresh(&root.id, false, true, |meta| {
        meta.node_prompt_ref = Some(interrupted_ref.clone());
        Ok(true)
      })
      .await
      .unwrap();
    assert!(prompt_facts(&store, &root.id).is_empty());

    let next = PatchTask { node_prompt: Some(Some("next rule\n".to_string())), ..Default::default() };
    store.patch_task(&root.id, next, TaskCaller::Operator, &StubGuards::default()).await.unwrap();

    let facts = prompt_facts(&store, &root.id);
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0]["previous_ref"].to_value(), Value::Null);
    assert_eq!(facts[0]["new_ref"].to_value(), interrupted_ref);
    assert_eq!(facts[1]["previous_ref"].to_value(), interrupted_ref);
  }

  #[tokio::test]
  async fn delete_permanently_refuses_every_saved_task_reference_and_evidence() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());

    let parent = create_node(&store, "delete-parent", None, TaskProfile::Manager).await;
    let child = create_node(&store, "delete-child", Some(&parent.id), TaskProfile::Worker).await;
    let error = store.delete_permanently(&parent.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), format!("has child task(s): {}", child.id));

    let with_run = create_node(&store, "delete-with-run", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &with_run.id, queued_run(&with_run.id, "delete-run"), None).await.unwrap();
    drop(guard);
    let error = store.delete_permanently(&with_run.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has run record(s): delete-run");

    let with_trigger = create_node(&store, "delete-with-trigger", None, TaskProfile::Manager).await;
    let triggers_dir = store.session_dir(&with_trigger.id).join("triggers");
    fs::create_dir_all(&triggers_dir).unwrap();
    fs::write(triggers_dir.join("trigger-a.json"), "{}\n").unwrap();
    let error = store.delete_permanently(&with_trigger.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has saved trigger reference(s)");

    let with_evidence = create_node(&store, "delete-with-evidence", None, TaskProfile::Manager).await;
    append(&store, &with_evidence.id, json!({"type": "user", "content": "preserve this"})).await;
    let error = store.delete_permanently(&with_evidence.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has preserved conversation/evidence: user");

    for (request_id, reference_kind) in [
      ("delete-parent-reference", "parent_session_id"),
      ("delete-successor-reference", "successor_session_id"),
      ("delete-origin-reference", "origin_ref"),
      ("delete-created-reference", "created_by_event"),
    ] {
      let target = create_node(&store, request_id, None, TaskProfile::Manager).await;
      let reference = create_node(&store, &format!("{request_id}-other"), None, TaskProfile::Manager).await;
      let _guard = store.lock_for(&reference.id).await;
      let mut meta = store.read_metadata_fresh(&reference.id).unwrap().unwrap();
      match reference_kind {
        "parent_session_id" => meta.parent_session_id = Some(target.id.clone()),
        "successor_session_id" => meta.successor_session_id = Some(target.id.clone()),
        "origin_ref" => {
          meta.origin_ref =
            Some(EventRef { session_id: target.id.clone(), event_id: Some("origin-event".to_string()) });
        }
        "created_by_event" => {
          meta.created_by_event =
            Some(EventRef { session_id: target.id.clone(), event_id: Some("created-event".to_string()) });
        }
        _ => unreachable!(),
      }
      store.save_metadata_locked(&mut meta, false).unwrap();
      drop(_guard);

      let error = store.delete_permanently(&target.id, TaskCaller::Operator).await.unwrap_err();
      assert_eq!(error.to_string(), format!("referenced by task {} ({reference_kind})", reference.id));
    }

    let report_target = create_node(&store, "delete-report-target", None, TaskProfile::Manager).await;
    let reporter = create_node(&store, "delete-report-source", None, TaskProfile::Manager).await;
    append(
      &store,
      &reporter.id,
      json!({
        "type": "child_report",
        "child_session_id": report_target.id,
        "child_event_id": "reported-event",
      }),
    )
    .await;
    let error = store.delete_permanently(&report_target.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), format!("referenced by a child report in task {}", reporter.id));
  }

  #[tokio::test]
  async fn delete_permanently_is_operator_only_reports_missing_tasks_and_removes_an_empty_task() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "delete-empty", None, TaskProfile::Manager).await;
    let other_a = create_node(&store, "delete-other-a", None, TaskProfile::Manager).await;
    let other_b = create_node(&store, "delete-other-b", None, TaskProfile::Manager).await;

    let error =
      store.delete_permanently(&root.id, TaskCaller::Agent { session_id: root.id.clone() }).await.unwrap_err();
    assert_eq!(error.to_string(), "permanent delete requires operator credentials");
    assert_eq!(error.status_code(), Some(403));
    let error = store.delete_permanently("missing-task", TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "task missing-task not found");
    assert_eq!(error.status_code(), Some(404));

    for id in [&root.id, &other_a.id, &other_b.id] {
      store.clear_chat_events_cache(id);
    }
    let before = store.history_work();
    assert!(store.delete_permanently(&root.id, TaskCaller::Operator).await.unwrap());
    let after = store.history_work();
    assert_eq!(after.full_folds - before.full_folds, 3, "each node history is folded once for deletion checks");
    assert!(!store.session_dir(&root.id).exists());
    assert!(store.read_metadata_fresh(&root.id).unwrap().is_none());
  }

  #[tokio::test]
  async fn agent_create_refusals_are_forbidden_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "agent-root", None, TaskProfile::Manager).await;
    let other = create_node(&store, "other-root", None, TaskProfile::Manager).await;
    let caller = TaskCaller::Agent { session_id: root.id.clone() };

    // `task_sessions.AGENT_CREATE_SCOPE_REFUSAL`: a create outside the caller's own manager task.
    let foreign = CreateTask {
      request_id: "foreign-child".to_string(),
      task_parent_id: Some(other.id.clone()),
      profile: TaskProfile::Manager,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(foreign, caller.clone(), Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "an agent may only create a task directly under its own open manager task");
    assert_eq!(error.status_code(), Some(403));

    // `takeoff_gate.DelegationBlockedError`: an implementation worker with no user request on its chain.
    let gated = CreateTask {
      request_id: "gated-worker".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(gated, caller, Map::new()).await.unwrap_err();
    assert!(error.to_string().starts_with("Delegation blocked: no user request on the task chain"));
    assert_eq!(error.status_code(), Some(403));
  }

  /// Write one archive close fact directly, as the completion owner's archive writes it.
  async fn archive_node(store: &Store, session_id: &str) {
    let guard = store.control_lock().await;
    let event = archived_close_event(session_id, "archive-test", ACTOR_USER, "archived by the user", &[], None);
    store.append_control_fact(&guard, session_id, event).await.unwrap();
    drop(guard);
    store.invalidate_tree_index();
  }

  #[tokio::test]
  async fn create_under_an_archived_parent_is_a_conflict_naming_the_parent() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    archive_node(&store, &root.id).await;

    // `task_sessions.create_task`: a closed parent is a `TaskConflictError` whose one blocker names the state.
    let late = CreateTask {
      request_id: "late-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(late, TaskCaller::System, Map::new()).await.unwrap_err();

    let blocker = format!("parent task {} is archived", root.id);
    assert_eq!(error.to_string(), blocker);
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&blocker)));
  }

  #[test]
  fn archived_refusal_is_python_s_plain_sentence_without_blockers() {
    // `task_errors.TaskArchivedError`, raised for machine input to an archived node such as a report to an
    // archived parent: the 409 detail is this sentence alone.
    let error = StoreError::archived("node-a");

    assert_eq!(error.to_string(), "task node-a is archived");
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), None);
  }

  #[tokio::test]
  async fn invalid_create_requests_are_400_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let worker = create_node(&store, "worker", None, TaskProfile::Worker).await;

    let empty_id = CreateTask {
      request_id: String::new(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(empty_id, TaskCaller::System, Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "request_id is required for task creation");
    assert_eq!(error.status_code(), Some(400));

    // `TaskInvalidError("parent task <id> is not a manager")`: a worker cannot parent a task.
    let under_worker = CreateTask {
      request_id: "under-worker".to_string(),
      task_parent_id: Some(worker.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(under_worker, TaskCaller::System, Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), format!("parent task {} is not a manager", worker.id));
    assert_eq!(error.status_code(), Some(400));
  }

  #[tokio::test]
  async fn absent_task_is_a_404_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());

    let error = store.load_task_meta("missing").await.unwrap_err();

    assert_eq!(error.to_string(), "task missing not found");
    assert_eq!(error.status_code(), Some(404));
  }

  async fn next_sidebar_frame(sidebar: &mut Subscription) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), sidebar.recv())
      .await
      .expect("a sidebar frame arrives")
      .expect("the sidebar channel stays open");
    serde_json::from_str(&frame).unwrap()
  }

  #[tokio::test]
  async fn control_facts_and_task_creation_notify_the_sidebar_with_the_fact_type() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let mut sidebar = store.subscribe(SIDEBAR_CHANNEL);
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Worker).await;
    for node in [&root.id, &child.id] {
      assert_eq!(
        next_sidebar_frame(&mut sidebar).await,
        json!({"type": "task_tree_changed", "session_id": node, "fact_type": "task_created"})
      );
    }

    archive_node(&store, &root.id).await;
    archive_node(&store, &child.id).await;

    for node in [&root.id, &child.id] {
      assert_eq!(
        next_sidebar_frame(&mut sidebar).await,
        json!({"type": "task_tree_changed", "session_id": node, "fact_type": "task_closed"})
      );
    }
  }

  #[tokio::test]
  async fn unnamed_sessions_follow_the_counter_and_recover_from_the_directory_count() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    create_node(&store, "named-a", None, TaskProfile::Manager).await;
    create_node(&store, "named-b", None, TaskProfile::Manager).await;
    let counter = home.path().join("sessions").join(".counter");

    fs::write(&counter, "not-a-number").unwrap();
    assert_eq!(create_unnamed_node(&store, "unnamed-a").await.name, "Session 2");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "3");

    fs::write(&counter, " 7\n").unwrap();
    assert_eq!(create_unnamed_node(&store, "unnamed-b").await.name, "Session 7");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "8");
  }

  /// Write one task node's metadata file directly, since `create_node` rebuilds the index per call and the cap
  /// tests need more nodes than that path makes quickly.
  fn write_task_node(store: &Store, session_id: &str, parent: Option<&str>, status: SessionStatus) {
    let mut meta = SessionMetadata::new(session_id, session_id, TaskProfile::Worker);
    meta.task_parent_id = parent.map(str::to_string);
    meta.status = status;
    let path = store.metadata_path(session_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, meta.to_json_string(&store.slots, true)).unwrap();
  }

  // Python: src/runtime/task_sessions.py:581
  #[test]
  fn subtree_walk_holds_more_than_a_thousand_stored_archived_descendants() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    write_task_node(&store, "root", None, SessionStatus::Active);
    for index in 0..1001 {
      write_task_node(&store, &format!("archived-{index:04}"), Some("root"), SessionStatus::Archived);
    }
    write_task_node(&store, "open-a", Some("root"), SessionStatus::Active);
    write_task_node(&store, "open-b", Some("root"), SessionStatus::Active);
    store.invalidate_tree_index();
    let walked = store.descendant_ids("root");
    assert!(walked.as_ref().is_ok_and(|ids| ids.len() == 1003), "{walked:?}");
  }

  // Python: src/runtime/task_sessions.py:585
  #[test]
  fn subtree_walk_refuses_more_than_a_thousand_non_archived_descendants() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    write_task_node(&store, "root", None, SessionStatus::Active);
    for index in 0..1001 {
      write_task_node(&store, &format!("open-{index:04}"), Some("root"), SessionStatus::Active);
    }
    store.invalidate_tree_index();
    let refusal = store.descendant_ids("root").err();
    assert!(
      matches!(&refusal, Some(StoreError::Conflict(blockers))
        if blockers == &["subtree of root exceeds 1000 non-archived nodes"]),
      "{refusal:?}"
    );
  }

  // Python: src/runtime/task_sessions.py:581
  #[tokio::test]
  async fn subtree_cap_does_not_count_a_node_only_the_derived_archive_marks() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    write_task_node(&store, "root", None, SessionStatus::Active);
    write_task_node(&store, "closed", Some("root"), SessionStatus::Active);
    for index in 0..1001 {
      write_task_node(&store, &format!("under-closed-{index:04}"), Some("closed"), SessionStatus::Active);
    }
    let close = control_events::build_control_event(
      "task_closed",
      ACTOR_SYSTEM,
      "closed",
      None,
      None,
      [("outcome".to_string(), json!("archived"))],
    );
    let guard = store.control_lock().await;
    store.append_control_fact(&guard, "closed", close).await.unwrap();
    drop(guard);
    store.invalidate_tree_index();
    assert!(store.archived_of("under-closed-0000").unwrap());
    let walked = store.descendant_ids("root");
    assert!(walked.as_ref().is_ok_and(|ids| ids.len() == 1002), "{walked:?}");
  }

  #[test]
  fn fact_fold_tracks_boundaries_closes_reports_and_run_positions() {
    let events = vec![
      json!({"id": "created", "type": "task_created"}),
      json!({"id": "input-a", "type": "user", "content": "first"}),
      json!({"id": "imported", "type": "task_imported", "pending_inputs": [{"input_id": "input-a"}]}),
      json!({"id": "closed", "type": "task_closed", "outcome": "cancelled"}),
      json!({"id": "input-b", "type": "user", "content": "during close"}),
      json!({"id": "reopened", "type": "task_reopened"}),
      json!({"id": "input-c", "type": "user", "content": "after reopen"}),
      json!({"id": "finished", "type": "run_finished", "run_id": "run-a", "outcome": "success", "input_event_ids": ["input-c"]}),
      json!({"id": "close-request", "type": "task_close_requested"}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child-a", "child_event_id": "event-a"}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child-a", "child_event_id": "duplicate"}),
    ];
    let facts = fold_task_facts(&events).unwrap();
    assert_eq!(facts.task_state, "open");
    assert_eq!(
      facts.input_candidates.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(),
      ["input-c", "report"]
    );
    assert_eq!(facts.confirmed_input_ids, HashSet::from(["input-c".to_string()]));
    assert_eq!(facts.run_outcomes["run-a"], "success");
    assert_eq!(facts.terminal_run_outcome("run-a").unwrap(), Some(RunOutcomeKind::Success));
    assert_eq!(facts.run_finish_positions["run-a"], 7);
    assert_eq!(facts.close_events.len(), 1);
    assert_eq!(facts.close_requests.len(), 1);
    assert_eq!(facts.delivered_reports, HashSet::from([("child-a".to_string(), "event-a".to_string())]));
    assert_eq!(facts.covered, events.len());
  }

  #[test]
  fn task_fact_outcomes_preserve_run_facts_null_and_unknown_rules() {
    let events = vec![
      json!({"type": "run_finished", "run_id": "null", "outcome": null}),
      json!({"type": "run_finished", "run_id": "number", "outcome": 7}),
      json!({"type": "run_finished", "run_id": "literal-none", "outcome": "None"}),
      json!({"type": "run_finished", "run_id": "unknown", "outcome": "unexpected"}),
    ];
    let facts = fold_task_facts(&events).unwrap();

    assert_eq!(facts.terminal_run_outcome("null").unwrap(), None);
    assert_eq!(facts.terminal_run_outcome("number").unwrap(), None);
    assert_eq!(
      facts.terminal_run_outcome("literal-none").unwrap_err().to_string(),
      "run literal-none has unknown terminal outcome \"None\""
    );
    assert_eq!(
      facts.terminal_run_outcome("unknown").unwrap_err().to_string(),
      "run unknown has unknown terminal outcome \"unexpected\""
    );
  }

  #[tokio::test]
  async fn run_record_recovery_walk_checks_each_entry_before_reading_it() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "run-record-stop-walk", None, TaskProfile::Manager).await;
    for index in 0..3 {
      let run_id = format!("run-{index}");
      let path = store.run_metadata_path(&node.id, &run_id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, serde_json::to_string(&queued_run(&node.id, &run_id)).unwrap()).unwrap();
    }

    let reads_before = store.run_record_read_count();
    let mut checks = 0;
    let records = store
      .list_run_records_until(&node.id, &mut || {
        checks += 1;
        checks == 3
      })
      .unwrap();

    assert!(records.is_none(), "a stopped run-directory walk returns no partial record list");
    assert_eq!(checks, 3, "the recovery checks before each run directory entry");
    assert_eq!(store.run_record_read_count() - reads_before, 2, "the third record is not read after stop");
  }

  #[test]
  fn task_fact_event_index_keeps_unique_order_without_copying_id_keys() {
    let facts = fold_task_facts(&[
      json!({"id": "event-a", "type": "user", "content": "first"}),
      json!({"id": "event-a", "type": "user", "content": "duplicate"}),
      json!({"id": "event-b", "type": "agent_message", "content": "second"}),
    ])
    .unwrap();

    assert_eq!(facts.events_by_id.len(), 2, "the ordered references include each unique event id once");
    assert_eq!(facts.events_by_id.by_hash.len(), 2, "the index keeps one fixed-width hash key per unique id");
    assert!(facts.events_by_id.by_hash.keys().all(|key| std::mem::size_of_val(key) == std::mem::size_of::<u64>()));
    for (id, event) in facts.events_by_id.iter() {
      assert_eq!(event["id"].as_str().unwrap(), id);
      assert!(Arc::ptr_eq(event, facts.events_by_id.get(id).unwrap()));
    }
    assert_eq!(facts.events_by_id.iter().next().unwrap().0, "event-a");
    assert_eq!(facts.events_by_id.iter().nth(1).unwrap().0, "event-b");
  }

  #[test]
  fn task_fact_event_index_checks_hash_collisions_by_event_id() {
    let first = Arc::new(json!({"id": "event-a", "type": "user"}).into());
    let second = Arc::new(json!({"id": "event-b", "type": "agent_message"}).into());
    let duplicate = Arc::new(json!({"id": "event-b", "type": "duplicate"}).into());
    let mut index = EventIdIndex::default();

    assert!(index.insert_hashed(first, |_| 7));
    assert!(index.insert_hashed(second, |_| 7));
    assert!(!index.insert_hashed(duplicate, |_| 7));

    assert_eq!(index.by_hash.len(), 1, "colliding ids share one fixed-width hash key");
    assert_eq!(index.len(), 2, "the collision bucket retains only distinct event references");
    assert_eq!(index.get_hashed("event-a", |_| 7).unwrap()["type"].as_str(), Some("user"));
    assert_eq!(index.get_hashed("event-b", |_| 7).unwrap()["type"].as_str(), Some("agent_message"));
  }

  #[test]
  fn node_state_view_retains_one_shared_node_facts_owner() {
    let facts = Arc::new(NodeFacts::default());
    let state =
      NodeState { facts: Arc::clone(&facts), runs: Arc::new(RunFacts::default()), malformed_task_events: None };

    assert!(Arc::ptr_eq(&facts, &state.facts));
    assert_eq!(Arc::strong_count(&facts), 2, "the view adds one Arc owner and no RunFacts copy");
  }

  #[test]
  fn withheld_reason_scans_events_in_order_without_retaining_run_entries() {
    let events = [
      json!({"type": "run_launch_withheld", "run_id": "run-a", "reason": "closed task"}),
      json!({"type": "run_launch_withheld", "run_id": "run-b", "reason": "other task"}),
      json!({"type": "run_launch_withheld", "run_id": "run-a", "reason": ""}),
    ];
    let mut visited = 0;
    let compact: Vec<ChatEvent> = events.iter().map(ChatEvent::from_value).collect();
    let withheld = has_withheld_reason(compact.iter().inspect(|_| visited += 1), "run-a");

    assert_eq!(visited, 3, "the scan visits each event once");
    assert!(!withheld, "the latest empty reason clears the earlier withheld state");
  }

  #[test]
  fn imported_pending_inputs_require_stable_input_ids() {
    let error =
      fold_task_facts(&[json!({"id": "bad-import", "type": "task_imported", "pending_inputs": [{}]})]).unwrap_err();
    assert!(error.to_string().contains("without a stable input_id"));
  }

  #[tokio::test]
  async fn node_facts_fold_each_appended_event_once() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "node-facts-suffix", None, TaskProfile::Manager).await;

    let before = store.history_work();
    assert_eq!(store.node_facts(&node.id).unwrap().task().unwrap().task_state, "open");
    append(&store, &node.id, json!({"id": "user-a", "type": "user", "content": "first"})).await;
    append(&store, &node.id, json!({"type": "task_closed", "outcome": "completed"})).await;
    append(&store, &node.id, json!({"id": "user-b", "type": "user", "content": "after close"})).await;
    let folded = store.node_facts(&node.id).unwrap();
    let folded = folded.task().unwrap();
    let full = store.facts_of(&node.id).unwrap();
    assert_eq!(folded.task_state, full.task_state);
    assert_eq!(folded.covered, full.covered);
    assert_eq!(folded.boundary_index, full.boundary_index);
    assert_eq!(folded.input_candidates, full.input_candidates);
    let work = store.history_work();
    assert_eq!(work.full_folds - before.full_folds, 1);
    assert_eq!(work.folded_events - before.folded_events, full.covered as u64);
  }

  #[tokio::test]
  async fn task_facts_and_authorization_reuse_the_node_memo_after_recovery_eviction() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "facts-memo", None, TaskProfile::Manager).await;
    append(&store, &node.id, json!({"id": "user-request", "type": "user", "content": "work"})).await;

    let before = store.history_work();
    let facts = store.facts_of(&node.id).unwrap();
    assert!(facts.has_user_request);
    let after_fold = store.history_work();
    assert_eq!(after_fold.full_folds - before.full_folds, 1);
    assert_eq!(after_fold.folded_events - before.folded_events, facts.covered as u64);
    store.evict_recovery_history(&node.id);

    let before_read = store.history_work();
    assert!(store.facts_of(&node.id).unwrap().has_user_request);
    assert_eq!(store.check_task_authorization(&node.id).unwrap(), node.id);
    let after_read = store.history_work();
    assert_eq!(after_read.file_reads, before_read.file_reads);
    assert_eq!(after_read.full_folds, before_read.full_folds);
    assert_eq!(after_read.folded_events, before_read.folded_events);
  }

  #[tokio::test]
  async fn many_run_blockers_fold_one_node_history_once() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "run-facts-memo", None, TaskProfile::Manager).await;
    let mut event_count = 1;
    for index in 0..50 {
      let run_id = format!("run-{index:02}");
      let run_dir = store.run_dir(&node.id, &run_id);
      fs::create_dir_all(&run_dir).unwrap();
      fs::write(
        run_dir.join(crate::run_records::RUN_METADATA_NAME),
        serde_json::to_string(&queued_run(&node.id, &run_id)).unwrap(),
      )
      .unwrap();
      let fact = match index {
        0..30 => Some(json!({"type": "run_finished", "run_id": run_id, "outcome": "success"})),
        30..40 => Some(json!({"type": "run_stop_requested", "run_id": run_id, "request_id": "stop"})),
        _ => None,
      };
      if let Some(fact) = fact {
        append(&store, &node.id, fact).await;
        event_count += 1;
      }
    }

    let before = store.history_work();
    let blockers = store.run_blockers(&node.id).unwrap();
    assert_eq!(blockers.len(), 10);
    let folded = store.history_work();
    assert_eq!(folded.full_folds - before.full_folds, 1);
    assert_eq!(folded.folded_events - before.folded_events, event_count);
    assert_eq!(store.run_blockers(&node.id).unwrap().len(), 10);
    assert_eq!(store.history_work().folded_events, folded.folded_events);
  }

  #[tokio::test]
  async fn request_stop_folds_once_and_extends_the_run_facts_suffix() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "stop-facts-memo", None, TaskProfile::Manager).await;
    let run = queued_run(&node.id, "queued-stop");
    let run_dir = store.run_dir(&node.id, &run.id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join(crate::run_records::RUN_METADATA_NAME), serde_json::to_string(&run).unwrap()).unwrap();
    for index in 0..20 {
      append(&store, &node.id, json!({"type": "ordinary_event", "index": index})).await;
    }
    store.clear_chat_events_cache(&node.id);

    let before = store.history_work();
    let result = store.request_stop(&node.id, &run.id, "stop-once").await.unwrap();
    assert!(result.stop_requested);
    let folded = store.history_work();
    assert_eq!(folded.full_folds - before.full_folds, 1);
    assert_eq!(folded.folded_events - before.folded_events, 21);

    store.request_stop(&node.id, &run.id, "stop-once").await.unwrap();
    let extended = store.history_work();
    assert_eq!(extended.full_folds, folded.full_folds);
    assert_eq!(extended.folded_events - folded.folded_events, 1, "the existing stop fact is only the appended suffix");
  }

  #[test]
  fn node_facts_memo_keeps_more_than_sixty_four_sessions() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let ids: Vec<String> = (0..65).map(|index| format!("memo-node-{index:02}")).collect();
    for session_id in &ids {
      let metadata_path = store.metadata_path(session_id);
      let events_path = store.chat_events_path(session_id);
      fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
      fs::create_dir_all(events_path.parent().unwrap()).unwrap();
      let meta = SessionMetadata::new(session_id, session_id, TaskProfile::Manager);
      fs::write(&metadata_path, meta.to_json_string(&store.slots, true)).unwrap();
      fs::write(&events_path, format!("{{\"id\": \"{session_id}-event\", \"type\": \"task_created\"}}\n")).unwrap();
    }

    for session_id in &ids {
      store.node_facts(session_id).unwrap();
    }
    assert_eq!(store.node_facts_cache.lock().unwrap().len(), ids.len());
    let full_folds = store.history_work().full_folds;
    for session_id in &ids {
      store.node_facts(session_id).unwrap();
    }
    assert_eq!(store.history_work().full_folds, full_folds);
  }

  #[tokio::test]
  async fn task_state_tail_reads_reuse_file_identity_and_move_after_append() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "task-state-tail", None, TaskProfile::Manager).await;
    let reads_before = store.task_state_tail_read_count();
    let folds_before = store.history_work().full_folds;
    append(&store, &node.id, json!({"type": "task_closed", "outcome": "completed"})).await;

    assert_eq!(store.task_state_fast(&node.id).unwrap(), "completed");
    let reads_after_close = store.task_state_tail_read_count();
    assert_eq!(reads_after_close - reads_before, 1);
    assert_eq!(store.history_work().full_folds, folds_before, "a tail lifecycle fact does not load the full history");
    assert_eq!(store.task_state_fast(&node.id).unwrap(), "completed");
    assert_eq!(store.task_state_tail_read_count(), reads_after_close);

    append(&store, &node.id, json!({"type": "task_reopened"})).await;
    assert_eq!(store.task_state_fast(&node.id).unwrap(), "open");
    assert_eq!(store.task_state_tail_read_count(), reads_after_close + 1);
  }

  #[tokio::test]
  async fn task_state_for_tree_index_reuses_the_sealed_recovery_fold() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "sealed-task-state", None, TaskProfile::Manager).await;
    append(&store, &node.id, json!({"type": "task_closed", "outcome": "completed"})).await;
    store.node_facts(&node.id).unwrap();
    store.evict_recovery_history(&node.id);
    let tails_before = store.task_state_tail_read_count();
    let files_before = store.history_work().file_reads;

    assert_eq!(store.task_state_for_tree_index(&node.id).unwrap(), "completed");

    assert_eq!(store.task_state_tail_read_count(), tails_before, "a sealed recovery fold avoids a tail-file read");
    assert_eq!(store.history_work().file_reads, files_before, "the sealed state avoids loading the chat history");
  }

  fn history_reads(store: &Store) -> u64 {
    store.history_reads.load(Ordering::Relaxed)
  }

  #[tokio::test]
  async fn activity_of_a_node_with_fifty_runs_reads_its_history_once() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "fifty-runs", None, TaskProfile::Manager).await;
    // The records and facts land as files: 30 finished runs, 10 stopped queued runs and 10 queued runs.
    let mut facts = String::new();
    for index in 0..50 {
      let run_id = format!("run-{index:02}");
      let run_dir = store.run_dir(&node.id, &run_id);
      fs::create_dir_all(&run_dir).unwrap();
      let record = serde_json::to_string(&queued_run(&node.id, &run_id)).unwrap();
      fs::write(run_dir.join(crate::run_records::RUN_METADATA_NAME), record).unwrap();
      let fact = match index {
        0..30 => json!({"type": "run_finished", "run_id": run_id, "outcome": "success"}),
        30..40 => json!({"type": "run_stop_requested", "run_id": run_id, "request_id": "stop"}),
        _ => continue,
      };
      facts.push_str(&format!("{fact}\n"));
    }
    let mut log = fs::OpenOptions::new().append(true).open(store.chat_events_path(&node.id)).unwrap();
    std::io::Write::write_all(&mut log, facts.as_bytes()).unwrap();
    // The facts landed behind the cache, so the derivation starts cold and parses the log.
    store.clear_chat_events_cache(&node.id);

    let before = history_reads(&store);
    let activity = store.task_tree_activity(&node.id).unwrap();
    assert_eq!(history_reads(&store) - before, 1, "one derivation reads the node's history once, not once per run");
    assert_eq!(activity, TaskTreeActivity { has_running_tasks: false, work_state: "waiting".to_string() });

    // The memoized verdict: its key check is the one read.
    let before = history_reads(&store);
    assert_eq!(store.task_tree_activity(&node.id).unwrap(), activity);
    assert_eq!(history_reads(&store) - before, 1);

    // A new fact moves the key: the queued runs stop, and the node is idle.
    for index in 40..50 {
      append(&store, &node.id, json!({"type": "run_stop_requested", "run_id": format!("run-{index:02}")})).await;
    }
    let before = history_reads(&store);
    assert_eq!(store.task_tree_activity(&node.id).unwrap().work_state, "idle");
    assert_eq!(history_reads(&store) - before, 1);
  }

  #[tokio::test]
  async fn a_boot_pass_eviction_keeps_the_node_state_for_the_first_sidebar_read() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "sealed", None, TaskProfile::Manager).await;
    for run_id in ["done", "queued-a", "queued-b"] {
      let run_dir = store.run_dir(&node.id, run_id);
      fs::create_dir_all(&run_dir).unwrap();
      let record = serde_json::to_string(&queued_run(&node.id, run_id)).unwrap();
      fs::write(run_dir.join(crate::run_records::RUN_METADATA_NAME), record).unwrap();
    }
    append(&store, &node.id, json!({"type": "run_finished", "run_id": "done", "outcome": "success"})).await;
    // The boot pass reads the node's facts, then drops its events.
    store.node_facts(&node.id).unwrap();
    store.evict_recovery_history(&node.id);

    let reads = store.history_work().file_reads;
    let waiting = TaskTreeActivity { has_running_tasks: false, work_state: "waiting".to_string() };
    assert_eq!(store.task_tree_activity(&node.id).unwrap(), waiting);
    assert_eq!(store.task_state(&node.id).unwrap(), "open");
    assert_eq!(store.history_work().file_reads, reads, "the sealed state answers without parsing the log");

    // An append to the cold log moves the file's identity, so the next derivation parses the log.
    for run_id in ["queued-a", "queued-b"] {
      append(&store, &node.id, json!({"type": "run_stop_requested", "run_id": run_id})).await;
    }
    assert_eq!(store.task_tree_activity(&node.id).unwrap().work_state, "idle");
    assert_eq!(store.history_work().file_reads, reads + 1);

    // A fold that misses the last append is not sealed: the read after the eviction parses the log.
    store.node_facts(&node.id).unwrap();
    append(&store, &node.id, json!({"type": "task_closed", "outcome": "cancelled"})).await;
    store.evict_recovery_history(&node.id);
    let reads = store.history_work().file_reads;
    assert_eq!(store.task_state(&node.id).unwrap(), "cancelled");
    assert_eq!(store.history_work().file_reads, reads + 1);
  }

  #[tokio::test]
  async fn a_runless_node_is_idle_without_a_history_read() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "runless", None, TaskProfile::Manager).await;
    // `activity_of` answers a runless node before its task fold, so a fold error does not reach the verdict.
    append(&store, &node.id, json!({"id": "import", "type": "task_imported", "pending_inputs": "malformed"})).await;
    store.evict_recovery_history(&node.id);

    let before = history_reads(&store);
    let activity = store.task_tree_activity(&node.id).unwrap();
    assert_eq!(activity, TaskTreeActivity { has_running_tasks: false, work_state: "idle".to_string() });
    assert_eq!(store.task_tree_activity(&node.id).unwrap(), activity);
    assert_eq!(history_reads(&store), before);
  }

  #[tokio::test]
  async fn an_activity_verdict_that_read_proc_is_not_memoized() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "live-run", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &node.id, queued_run(&node.id, "live"), None).await.unwrap();
    drop(guard);
    let pid = std::process::id();
    let pid_start = charliebot_core::proc_identity::read_pid_stat(pid).unwrap().pid_start;
    store.record_launch(&node.id, "live", pid, &pid_start).await.unwrap();

    let running = TaskTreeActivity { has_running_tasks: true, work_state: "running".to_string() };
    assert_eq!(store.task_tree_activity(&node.id).unwrap(), running);
    assert!(!store.activity_cache.lock().unwrap().contains_key(&node.id), "a /proc verdict is not stored");

    append(&store, &node.id, json!({"type": "run_finished", "run_id": "live", "outcome": "success"})).await;
    assert_eq!(store.task_tree_activity(&node.id).unwrap().work_state, "idle");
    assert!(store.activity_cache.lock().unwrap().contains_key(&node.id), "a fact-only verdict is stored");
  }

  #[tokio::test]
  async fn the_task_state_memo_answers_the_full_fold() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "state-memo", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &node.id, queued_run(&node.id, "queued"), None).await.unwrap();
    drop(guard);
    let created_id = store.load_chat_events_sync(&node.id).unwrap()[0]["id"].as_str().unwrap().to_string();
    let steps = [
      (json!({"id": "close-1", "type": "task_closed", "outcome": "cancelled"}), "cancelled"),
      (json!({"id": "reopen-1", "type": "task_reopened"}), "open"),
      // A fact whose id repeats an earlier event's id is skipped by the fold.
      (json!({"id": created_id, "type": "task_closed", "outcome": "completed"}), "open"),
      (json!({"id": "close-2", "type": "task_closed", "outcome": ""}), "completed"),
      (json!({"id": "clone", "type": "clone_start"}), "open"),
    ];
    for (event, expected) in steps {
      append(&store, &node.id, event).await;
      assert_eq!(store.task_state(&node.id).unwrap(), expected);
      assert_eq!(store.facts_of(&node.id).unwrap().task_state, expected);
    }

    let import = json!({"id": "import", "type": "task_imported", "pending_inputs": [{"input_id": ""}]});
    append(&store, &node.id, import).await;
    let fold_error = store.facts_of(&node.id).unwrap_err().to_string();
    assert_eq!(store.task_state(&node.id).unwrap_err().to_string(), fold_error);
    assert_eq!(store.task_tree_activity(&node.id).unwrap_err().to_string(), fold_error);
    // The Run facts still fold past the rejected fact.
    append(&store, &node.id, json!({"type": "run_finished", "run_id": "queued", "outcome": "failed"})).await;
    assert_eq!(store.run_display_state(&node.id, "queued").unwrap(), RunDisplayState::Failed);
  }

  #[tokio::test]
  async fn repeated_task_state_reads_fold_a_node_once() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "state-repeat", None, TaskProfile::Manager).await;
    let event_count = store.load_chat_events_shared_sync(&node.id).unwrap().len() as u64;
    let before = store.history_work();
    for _ in 0..100 {
      assert_eq!(store.task_state(&node.id).unwrap(), "open");
    }
    let after = store.history_work();
    assert_eq!(after.node_state_full_folds - before.node_state_full_folds, 1);
    assert_eq!(after.node_state_folded_events - before.node_state_folded_events, event_count);
  }

  #[tokio::test]
  async fn task_facts_share_cached_event_payloads_until_cache_invalidation() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "shared-task-events", None, TaskProfile::Manager).await;
    let event =
      store.append_chat_event(&node.id, json!({"type": "user", "content": "synthetic shared payload"})).await.unwrap();
    let id = event.get("id").unwrap().as_str().unwrap();

    let before = store.history_work();
    let history = store.load_chat_events_shared_sync(&node.id).unwrap();
    let facts = store.node_facts(&node.id).unwrap();
    let identity = match store.live_history(&node.id).unwrap() {
      LiveHistory::Cached { identity, .. } => identity,
      LiveHistory::Cold(_) => panic!("the task event list remains cached"),
    };
    let cached =
      history.iter().find(|candidate| candidate.get("id").and_then(CompactValue::as_str) == Some(id)).unwrap();
    let fact = facts.task().unwrap().events_by_id.get(id).unwrap();
    assert!(Arc::ptr_eq(cached, fact), "task facts point at the one cached parsed payload");
    assert!(Arc::ptr_eq(cached, &facts.task().unwrap().input_candidates[0]));
    assert_eq!(Arc::strong_count(cached), 3, "the history, ordered id map, and candidate list share one payload");
    assert_eq!(store.history_work().owned_event_clones, before.owned_event_clones);
    assert_eq!(
      store.run_facts_memo.lock().unwrap().get(&identity).unwrap().events.len(),
      history.len(),
      "the run-facts memo retains the list whose identity it keys"
    );

    store.append_chat_event(&node.id, json!({"type": "agent_message", "content": "append"})).await.unwrap();
    let appended = store.load_chat_events_shared_sync(&node.id).unwrap();
    let appended_identity = match store.live_history(&node.id).unwrap() {
      LiveHistory::Cached { identity, .. } => identity,
      LiveHistory::Cold(_) => panic!("the appended event list remains cached"),
    };
    assert!(Arc::ptr_eq(&identity, &appended_identity), "append keeps the Python list identity");
    assert_eq!(
      store.run_facts_memo.lock().unwrap().get(&identity).unwrap().events.len(),
      appended.len(),
      "append refreshes the retained list without adding a memo entry"
    );
    let still_cached =
      appended.iter().find(|candidate| candidate.get("id").and_then(CompactValue::as_str) == Some(id)).unwrap();
    assert!(Arc::ptr_eq(cached, still_cached), "an append shares every existing parsed payload");

    store.clear_chat_events_cache(&node.id);
    let reloaded = store.load_chat_events_shared_sync(&node.id).unwrap();
    let replacement_identity = match store.live_history(&node.id).unwrap() {
      LiveHistory::Cached { identity, .. } => identity,
      LiveHistory::Cold(_) => panic!("the reloaded event list remains cached"),
    };
    assert!(!Arc::ptr_eq(&identity, &replacement_identity), "clear and reload replaces the Python list identity");
    let replaced =
      reloaded.iter().find(|candidate| candidate.get("id").and_then(CompactValue::as_str) == Some(id)).unwrap();
    assert!(!Arc::ptr_eq(cached, replaced), "cache invalidation reparses the event and replaces its identity");
    let refreshed_facts = store.node_facts(&node.id).unwrap();
    assert!(Arc::ptr_eq(replaced, refreshed_facts.task().unwrap().events_by_id.get(id).unwrap()));
    let memo = store.run_facts_memo.lock().unwrap();
    assert_eq!(memo.len(), 2, "replacement creates a miss while the bounded memo retains the prior list");
    assert!(memo.get(&identity).is_some(), "the old list remains strongly retained until eviction");
    assert!(memo.get(&replacement_identity).is_some(), "the replacement list gets its own entry");
  }

  #[test]
  fn node_fact_fold_checks_stop_at_each_256_event_boundary() {
    let events = (0..1024)
      .map(|index| {
        Arc::new(
          json!({
            "id": format!("finish-{index}"),
            "type": "run_finished",
            "run_id": format!("run-{index}"),
            "outcome": "success"
          })
          .into(),
        )
      })
      .collect::<Vec<_>>();
    let mut facts = NodeFacts::default();
    let mut runs = RunFacts::default();
    let mut checks = 0;
    let folded = fold_node_facts_from(
      &mut facts,
      &events,
      0,
      &mut || {
        checks += 1;
        checks == 3
      },
      Some(&mut runs),
    )
    .unwrap();

    assert!(!folded);
    assert_eq!(checks, 3, "the fold checks once initially and then at the 256 event boundary");
    assert_eq!(runs.terminal_outcome("run-255").unwrap(), Some(RunOutcomeKind::Success));
    assert_eq!(runs.terminal_outcome("run-256").unwrap(), None);
  }

  #[test]
  fn node_fact_fold_visits_each_event_once_for_task_and_run_facts() {
    let events = [
      Arc::new(json!({"id": "finish-a", "type": "run_finished", "run_id": "run-a", "outcome": "success"}).into()),
      Arc::new(json!({"id": "stop-b", "type": "run_stop_requested", "run_id": "run-b"}).into()),
      Arc::new(json!({"id": "user-c", "type": "user", "content": "synthetic"}).into()),
    ];
    let mut facts = NodeFacts::default();
    let mut runs = RunFacts::default();
    let mut stop_checks = 0;
    let completed = fold_node_facts_from(
      &mut facts,
      &events,
      0,
      &mut || {
        stop_checks += 1;
        false
      },
      Some(&mut runs),
    )
    .unwrap();

    assert!(completed);
    assert_eq!(runs.observed_events, events.len(), "Run facts observe one event visit per history entry");
    assert_eq!(facts.task().unwrap().covered, events.len(), "task facts cover the same events in that pass");
    assert_eq!(stop_checks, 3, "one block needs an initial, block-boundary, and completion stop check");
  }

  #[tokio::test]
  async fn node_facts_cold_read_checks_stop_between_event_lines_without_caching_a_prefix() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "stop-cold-event-read", None, TaskProfile::Manager).await;
    for index in 0..5 {
      append(&store, &node.id, json!({"type": "user", "content": format!("synthetic-{index}")})).await;
    }
    let expected_events = store.load_chat_events_shared_sync(&node.id).unwrap().len();
    store.clear_chat_events_cache(&node.id);

    let mut checks = 0;
    let facts = store
      .node_facts_until(&node.id, &mut || {
        checks += 1;
        checks == 5
      })
      .unwrap();

    assert!(facts.is_none(), "the stopped recovery returns no partial node facts");
    assert_eq!(checks, 5, "the recovery stop check reached the cold event reader before its third line");
    assert!(store.cached_chat_events(&node.id).is_none(), "a cancelled read does not publish its parsed prefix");
    assert_eq!(store.load_chat_events_shared_sync(&node.id).unwrap().len(), expected_events);
  }

  #[tokio::test]
  async fn task_state_fast_falls_back_after_one_thousand_lines_without_a_lifecycle_fact() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "state-fast-bound", None, TaskProfile::Manager).await;
    let path = store.chat_events_path(&node.id);
    let mut raw = String::from("{\"id\":\"close-a\",\"type\":\"task_closed\",\"outcome\":\"cancelled\"}\n");
    for index in 0..1000 {
      raw.push_str(&format!("{{\"id\":\"message-{index}\",\"type\":\"user\"}}\n"));
    }
    fs::write(&path, raw).unwrap();
    store.clear_chat_events_cache(&node.id);

    let before = store.history_work();
    assert_eq!(store.task_state_fast(&node.id).unwrap(), "cancelled");
    let after = store.history_work();
    assert_eq!(after.full_folds - before.full_folds, 1, "the full memoized fold is the tail-miss fallback");
    assert_eq!(after.folded_events - before.folded_events, 1001);
  }

  #[test]
  fn task_spec_hash_matches_python_canonical_text() {
    let implement = TaskSpec {
      goal: "Ship the é change".to_string(),
      acceptance: vec!["tests pass".to_string()],
      context_refs: vec!["docs/a.md".to_string()],
      repo_path: Some("/home/synthetic/repo".to_string()),
      base_branch: Some("main".to_string()),
      task_type: Some(TaskType::Implement),
      keep_worktree: true,
      cold_read: None,
    };
    let text = serde_json::to_string(&implement).unwrap();
    assert_eq!(
      text,
      r#"{"goal":"Ship the é change","acceptance":["tests pass"],"context_refs":["docs/a.md"],"repo_path":"/home/synthetic/repo","base_branch":"main","task_type":"implement","keep_worktree":true,"cold_read":null}"#
    );
    assert_eq!(task_spec_hash(&implement).unwrap(), "c5ddb0177e12fd9b6e51707f3c41a0285c830bff33280ac66ebd0121cdd3a7c1");

    let verify = TaskSpec { goal: "check".to_string(), task_type: Some(TaskType::Verify), ..TaskSpec::default() };
    assert_eq!(
      serde_json::to_string(&verify).unwrap(),
      r#"{"goal":"check","acceptance":[],"context_refs":[],"repo_path":null,"base_branch":null,"task_type":"verify","keep_worktree":false,"cold_read":null}"#
    );
    assert_eq!(task_spec_hash(&verify).unwrap(), "9e765c73c0cf10b2fbaef99cb808d77a04b4451cc5924cdaa17b7ddf4c6f106b");
  }
}

#[test]
fn staging_dir_name_carries_task_id_process_id_and_uuid() {
  let name = staging_dir_name("synthetic-task-id");
  // The tree walkers skip staging dirs on the `.task-` prefix and `.tmp` suffix alone
  // (task_tree.rs build_tree_index, charliebot-core v1_sessions count), so both ends must hold.
  assert!(name.starts_with(".task-synthetic-task-id-"), "unexpected staging name prefix: {name}");
  assert!(name.ends_with(".tmp"), "unexpected staging name suffix: {name}");
  let body = name.trim_start_matches(".task-").trim_end_matches(".tmp");
  let mut parts = body.rsplitn(3, '-');
  let uuid_part = parts.next().expect("staging name has no uuid segment");
  let pid_part = parts.next().expect("staging name has no pid segment");
  let id_part = parts.next().expect("staging name has no task id segment");
  assert_eq!(id_part, "synthetic-task-id");
  assert_eq!(pid_part.parse::<u32>().unwrap(), std::process::id());
  assert_eq!(uuid_part.len(), 32);
  assert!(uuid_part.chars().all(|c| c.is_ascii_hexdigit()));
}
