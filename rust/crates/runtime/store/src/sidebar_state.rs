//! Process-memory sidebar snapshots and invalidation state.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use serde_json::{Map, Value};

use crate::lock::lock;
use crate::metadata::SessionMetadata;

pub const HAS_PENDING_PLAN_APPROVAL: &str = "has_pending_plan_approval";
pub const TASK_TREE_ACTIVITY: &str = "task_tree_activity";
const FORCE_FULL_EVERY: u64 = 10;
const DERIVED_MAP_MEMO_LIMIT: usize = 4;
/// A memoized sidebar row per session.
///
/// Each row is shared, so a memo hit hands out pointers rather than copying every row's fields.
pub type DerivedSidebarMap = HashMap<String, Arc<Map<String, Value>>>;

/// The inputs one memoized derived map was built from: the session rows, the route's fields, and the generation of
/// the sidebar state their overlays derive from.
///
/// The rows compare and hash by pointer. The store hands out one shared, immutable row while its content holds, so
/// the same pointer means the same row and another pointer means a row that moved. Keying on the ids instead clones
/// and hashes every id string on every request of the sidebar listing and the chat page.
#[derive(Clone, Debug)]
pub struct DerivedMapKey {
  pub rows: Vec<Arc<SessionMetadata>>,
  pub flags: Vec<bool>,
  pub generation: u64,
}

impl PartialEq for DerivedMapKey {
  fn eq(&self, other: &Self) -> bool {
    self.generation == other.generation
      && self.flags == other.flags
      && self.rows.len() == other.rows.len()
      && self.rows.iter().zip(other.rows.iter()).all(|(left, right)| Arc::ptr_eq(left, right))
  }
}

impl Eq for DerivedMapKey {}

impl Hash for DerivedMapKey {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.generation.hash(state);
    self.flags.hash(state);
    self.rows.len().hash(state);
    for row in &self.rows {
      Arc::as_ptr(row).hash(state);
    }
  }
}

struct State {
  dirty: HashSet<String>,
  snapshot: std::collections::HashMap<String, Map<String, Value>>,
  probe_signatures: std::collections::HashMap<String, Vec<Value>>,
  poll_count: u64,
  derived_generation: u64,
  derived_maps: LruCache<DerivedMapKey, Arc<DerivedSidebarMap>>,
}

impl Default for State {
  fn default() -> Self {
    Self {
      dirty: HashSet::new(),
      snapshot: HashMap::new(),
      probe_signatures: HashMap::new(),
      poll_count: 0,
      derived_generation: 0,
      derived_maps: LruCache::new(NonZeroUsize::new(DERIVED_MAP_MEMO_LIMIT).expect("the memo limit is nonzero")),
    }
  }
}

#[cfg(not(test))]
fn state() -> &'static Mutex<State> {
  static STATE: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
  STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Under test, each test thread owns one state, so a test's marks, generation and poll count never reach another
/// test. libtest runs each test on a thread of its own (`run_test_inner` in `library/test/src/lib.rs`), and no test
/// takes a lock, so a panic in one test cannot poison another. The leaked box is one small allocation per test thread.
#[cfg(test)]
fn state() -> &'static Mutex<State> {
  thread_local! {
    static THREAD_STATE: &'static Mutex<State> = Box::leak(Box::new(Mutex::new(State::default())));
  }
  THREAD_STATE.with(|state| *state)
}

/// Flag one session for a fresh sidebar probe.
pub fn mark_sidebar_dirty(session_id: &str) {
  let mut state = lock(state(), "sidebar state mutex");
  state.dirty.insert(session_id.to_string());
  state.derived_generation += 1;
}

/// Return whether one session needs a fresh sidebar probe.
pub fn is_dirty(session_id: &str) -> bool {
  lock(state(), "sidebar state mutex").dirty.contains(session_id)
}

/// Discard the selected dirty marks.
pub fn discard_dirty(session_ids: &[String]) {
  let mut state = lock(state(), "sidebar state mutex");
  for session_id in session_ids {
    state.dirty.remove(session_id);
  }
}

/// Return a copy of one session's probe snapshot, or `None` when it is cold.
pub fn snapshot_entry(session_id: &str) -> Option<Map<String, Value>> {
  lock(state(), "sidebar state mutex").snapshot.get(session_id).cloned()
}

/// Store a probe snapshot and bump the derived-state generation.
pub fn store_snapshot_entry(session_id: &str, entry: Map<String, Value>) {
  let mut state = lock(state(), "sidebar state mutex");
  state.derived_generation += 1;
  state.snapshot.insert(session_id.to_string(), entry);
}

/// Return the current generation of sidebar inputs.
pub fn derived_generation() -> u64 {
  lock(state(), "sidebar state mutex").derived_generation
}

/// Read a memoized derived map and mark it most recently used.
pub fn peek_derived_map(key: &DerivedMapKey) -> Option<Arc<DerivedSidebarMap>> {
  lock(state(), "sidebar state mutex").derived_maps.get(key).cloned()
}

/// Store one derived map in the four-entry memo.
pub fn store_derived_map(key: DerivedMapKey, value: Arc<DerivedSidebarMap>) {
  lock(state(), "sidebar state mutex").derived_maps.put(key, value);
}

/// Return the task-activity pair in a session snapshot, when present.
pub fn snapshot_task_activity(session_id: &str) -> Option<(bool, String)> {
  let state = lock(state(), "sidebar state mutex");
  let activity = state.snapshot.get(session_id)?.get(TASK_TREE_ACTIVITY)?;
  let active = activity.get(0)?.as_bool()?;
  let work_state = activity.get(1)?.as_str()?.to_string();
  Some((active, work_state))
}

/// Return the last deep-probe signature for one session.
pub fn probe_signature(session_id: &str) -> Option<Vec<Value>> {
  lock(state(), "sidebar state mutex").probe_signatures.get(session_id).cloned()
}

/// Store the latest deep-probe signature for one session.
pub fn store_probe_signature(session_id: &str, signature: Vec<Value>) {
  lock(state(), "sidebar state mutex").probe_signatures.insert(session_id.to_string(), signature);
}

/// Count a sidebar poll and return whether it must probe every active session.
pub fn register_poll(force: bool) -> bool {
  let mut state = lock(state(), "sidebar state mutex");
  state.poll_count += 1;
  force || state.poll_count.is_multiple_of(FORCE_FULL_EVERY)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::metadata::TaskProfile;

  #[test]
  fn dirty_marks_bump_generation_and_poll_reprobe_is_bounded() {
    let initial = derived_generation();
    mark_sidebar_dirty("session-a");
    assert!(is_dirty("session-a"));
    assert_eq!(derived_generation(), initial + 1);
    discard_dirty(&["session-a".to_string()]);
    assert!(!is_dirty("session-a"));
    for _ in 0..9 {
      assert!(!register_poll(false));
    }
    assert!(register_poll(false));
  }

  #[test]
  fn snapshot_updates_and_derived_memo_have_caps() {
    let mut entry = Map::new();
    entry.insert(TASK_TREE_ACTIVITY.to_string(), serde_json::json!([true, "running"]));
    store_snapshot_entry("session-a", entry);
    assert_eq!(snapshot_task_activity("session-a"), Some((true, "running".to_string())));
    let row = |session_id: &str| Arc::new(SessionMetadata::new(session_id, session_id, TaskProfile::Manager));
    for generation in 0..5 {
      let key = DerivedMapKey { rows: vec![row(&format!("session-{generation}"))], flags: Vec::new(), generation };
      store_derived_map(key, Arc::new(HashMap::new()));
    }
    let first = DerivedMapKey { rows: vec![row("session-0")], flags: Vec::new(), generation: 0 };
    assert_eq!(peek_derived_map(&first), None);
  }

  #[test]
  fn a_key_matches_a_row_by_pointer_and_not_by_content() {
    let row = Arc::new(SessionMetadata::new("session-a", "Session A", TaskProfile::Manager));
    let key = DerivedMapKey { rows: vec![Arc::clone(&row)], flags: vec![true], generation: 3 };
    store_derived_map(key.clone(), Arc::new(HashMap::new()));
    assert!(peek_derived_map(&key).is_some());
    let same_content = Arc::new(SessionMetadata::new("session-a", "Session A", TaskProfile::Manager));
    let other_pointer = DerivedMapKey { rows: vec![same_content], flags: vec![true], generation: 3 };
    assert!(peek_derived_map(&other_pointer).is_none());
  }
}
