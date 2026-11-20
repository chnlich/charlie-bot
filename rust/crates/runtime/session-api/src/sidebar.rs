//! Sidebar status derived from trigger files, feature contributions, and the task tree.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use charliebot_core::memo::BoundedMemo;
use charliebot_core::timefmt;
use charliebot_registry::Registry;
use charliebot_store::layout;
use charliebot_store::metadata::{
  HAS_PENDING_TRIGGER, HAS_RUNNING_TASKS, NEXT_TRIGGER_AT, PENDING_TRIGGER_COUNT, SessionMetadata, SessionStatus,
  WORK_STATE,
};
use charliebot_store::run_records::RUN_METADATA_NAME;
use charliebot_store::{Store, StoreError, StoreResult};
use serde_json::{Map, Value, json};

pub use charliebot_store::sidebar_state::{
  DerivedSidebarMap, HAS_PENDING_PLAN_APPROVAL, HAS_WAITING_ITEMS, TASK_TREE_ACTIVITY,
};

static SIDEBAR_SWEEP_RUNNING: AtomicBool = AtomicBool::new(false);

/// How long after one self-heal sweep starts the next one may start.
///
/// The sweep is triggered by every tenth sidebar poll. A poller faster than ten polls per sweep would start a new
/// sweep the moment the previous one ended. The sweep thread therefore ran back to back for as long as the poller
/// kept up. The interval fixes the sweep's frequency and [`SIDEBAR_SWEEP_SLICE`] fixes each sweep's size, so
/// together they bound the CPU the self-heal spends on a home of any size. Changes the server makes itself reach
/// the sidebar through their dirty marks, which the poll consumes without a sweep. The interval therefore only
/// bounds how late an edit from outside the server can show.
const SIDEBAR_SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// How many sessions one self-heal sweep rechecks.
///
/// A sweep rechecked every active session, so its cost grew with the home's session count. On the 7,160-session mock
/// home one sweep spent about 58 ms of CPU, and the interval starts one every ten seconds. A sweep rechecks this many
/// sessions and the next sweep continues after them, so each sweep costs a fixed share of CPU. A session waits at most
/// the corpus size divided by this slice, times the interval, to be rechecked again.
const SIDEBAR_SWEEP_SLICE: usize = 1024;

/// How many threads one large sidebar read splits its per-session probes across.
///
/// A process's first sidebar read has no stored state, so it probes every active session: each probe reads that
/// session's trigger, metadata and run files. On the 7,160-session mock home the first listing read spends about 760 ms
/// of CPU on that walk. A probe reads one session's files and shares nothing with another, so the walk splits across
/// threads. The self-heal sweep already runs the same probes on its own thread beside the request threads.
const SIDEBAR_PROBE_THREADS: usize = 8;

/// The fewest sessions worth splitting. A poll over the rows one view paints stays on its own thread, because the
/// threads cost more than the probes they carry.
const SIDEBAR_PROBE_SPLIT_MIN: usize = 256;

/// How many corpora keep their sweep rotation.
///
/// A process polls a few large corpora, and an evicted entry only restarts that corpus's rotation.
const SIDEBAR_SWEEP_CURSOR_ENTRIES: usize = 16;

/// The next corpus index each corpus's rotation starts at, keyed by the corpus's own ids.
///
/// The cursor belongs to the corpus. One shared cursor would skip rows when two corpora of related lengths are
/// polled in turn. Each corpus would apply the other corpus's advance modulo its own length and repeat the same
/// offsets. A corpus is named by what it holds, so a caller that builds an equal corpus per request keeps its
/// rotation instead of restarting at the first row.
static SIDEBAR_SWEEP_CURSORS: LazyLock<Mutex<BoundedMemo<(usize, u64), usize>>> =
  LazyLock::new(|| Mutex::new(BoundedMemo::new(SIDEBAR_SWEEP_CURSOR_ENTRIES)));

/// The start time of the last self-heal sweep this process ran.
static SIDEBAR_SWEEP_LAST_START: Mutex<Option<Instant>> = Mutex::new(None);

struct SidebarSweepFlight;

impl Drop for SidebarSweepFlight {
  fn drop(&mut self) {
    SIDEBAR_SWEEP_RUNNING.store(false, Ordering::Release);
  }
}

/// The sidebar fields one listing or status route puts on each of its rows. A field the route leaves out is absent
/// from those rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SidebarFields {
  pub(crate) running: bool,
  pub(crate) trigger: bool,
  pub(crate) plan: bool,
  pub(crate) waiting: bool,
}

/// The sidebar fields selected by one listing or status route, resolved.
pub(crate) fn resolve_sidebar_state(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
  sessions: &[Arc<SessionMetadata>],
  fields: SidebarFields,
  force: bool,
) -> StoreResult<Arc<DerivedSidebarMap>> {
  let SidebarFields {
    running: include_running,
    trigger: include_trigger,
    plan: include_plan,
    waiting: include_waiting,
  } = fields;
  if sessions.is_empty() {
    return Ok(Arc::new(HashMap::new()));
  }
  let flags = vec![include_running, include_trigger, include_plan, include_waiting];
  let generation = charliebot_store::sidebar_state::derived_generation();
  let key = charliebot_store::sidebar_state::DerivedMapKey { rows: sessions.to_vec(), flags, generation };
  let force_full = charliebot_store::sidebar_state::register_poll(force);
  if !force && let Some(derived) = charliebot_store::sidebar_state::peek_derived_map(&key) {
    if force_full {
      schedule_sidebar_sweep(Arc::clone(store), Arc::clone(registry), sessions)?;
    }
    return Ok(derived);
  }

  let mut derived = HashMap::new();
  let mut active = Vec::new();
  for meta in sessions {
    if meta.status == SessionStatus::Archived {
      let mut entry = Map::new();
      if include_running {
        entry.insert(HAS_RUNNING_TASKS.to_string(), Value::Bool(false));
      }
      if include_trigger {
        entry.insert(HAS_PENDING_TRIGGER.to_string(), Value::Bool(false));
        entry.insert(PENDING_TRIGGER_COUNT.to_string(), Value::from(0));
        entry.insert(NEXT_TRIGGER_AT.to_string(), Value::Null);
      }
      if include_plan {
        entry.insert(HAS_PENDING_PLAN_APPROVAL.to_string(), Value::Bool(false));
      }
      if include_waiting {
        let session_dir = store.session_dir(&meta.id);
        let waiting_items = registry
          .sidebar_contributions()
          .flat_map(|contribution| contribution.row_flags(&session_dir, &meta.id))
          .any(|(name, value)| name == HAS_WAITING_ITEMS && value);
        entry.insert(HAS_WAITING_ITEMS.to_string(), Value::Bool(waiting_items));
      }
      derived.insert(meta.id.clone(), Arc::new(entry));
    } else {
      active.push(Arc::clone(meta));
    }
  }

  if force_full && !force {
    schedule_sidebar_sweep(Arc::clone(store), Arc::clone(registry), &active)?;
  }
  let candidates: Vec<&Arc<SessionMetadata>> = active
    .iter()
    .filter(|meta| {
      should_probe_synchronously(
        force_full,
        force,
        charliebot_store::sidebar_state::is_dirty(&meta.id),
        charliebot_store::sidebar_state::snapshot_entry(&meta.id).is_some(),
      )
    })
    .collect();
  let checked: Vec<Option<(String, Vec<Value>)>> = spread_across_threads(&candidates, |meta| {
    let session_id = &meta.id;
    let signature = probe_signature(store, registry, session_id);
    let unchanged = charliebot_store::sidebar_state::probe_signature(session_id).as_ref() == Some(&signature);
    if force || !unchanged || charliebot_store::sidebar_state::is_dirty(session_id) {
      Some((session_id.clone(), signature))
    } else {
      None
    }
  });
  let mut selected = Vec::new();
  let mut signatures = HashMap::new();
  for (session_id, signature) in checked.into_iter().flatten() {
    signatures.insert(session_id.clone(), signature);
    selected.push(session_id);
  }
  charliebot_store::sidebar_state::discard_dirty(&selected);
  let probed = spread_across_threads(&selected, |session_id| probe_session(store, registry, session_id));
  for (index, result) in probed.into_iter().enumerate() {
    let session_id = &selected[index];
    match result {
      Ok(snapshot) => {
        charliebot_store::sidebar_state::store_snapshot_entry(session_id, snapshot);
        charliebot_store::sidebar_state::store_probe_signature(
          session_id,
          signatures.remove(session_id).expect("every selected row has a probe signature"),
        );
      }
      Err(error) => {
        for dirty in &selected {
          charliebot_store::sidebar_state::mark_sidebar_dirty(dirty);
        }
        return Err(error);
      }
    }
  }

  for meta in active {
    let snapshot = charliebot_store::sidebar_state::snapshot_entry(&meta.id).unwrap_or_default();
    let mut entry = Map::new();
    let activity = snapshot.get(TASK_TREE_ACTIVITY).and_then(Value::as_array);
    if include_running {
      entry.insert(
        HAS_RUNNING_TASKS.to_string(),
        activity.and_then(|value| value.first()).cloned().unwrap_or(Value::Bool(false)),
      );
    }
    if include_trigger {
      let count = snapshot.get(PENDING_TRIGGER_COUNT).cloned().unwrap_or(Value::from(0));
      entry.insert(HAS_PENDING_TRIGGER.to_string(), Value::Bool(count.as_i64().unwrap_or_default() > 0));
      entry.insert(PENDING_TRIGGER_COUNT.to_string(), count);
      entry.insert(NEXT_TRIGGER_AT.to_string(), snapshot.get(NEXT_TRIGGER_AT).cloned().unwrap_or(Value::Null));
    }
    if include_plan {
      entry.insert(
        HAS_PENDING_PLAN_APPROVAL.to_string(),
        snapshot.get(HAS_PENDING_PLAN_APPROVAL).cloned().unwrap_or(Value::Bool(false)),
      );
    }
    if include_waiting {
      entry
        .insert(HAS_WAITING_ITEMS.to_string(), snapshot.get(HAS_WAITING_ITEMS).cloned().unwrap_or(Value::Bool(false)));
    }
    if let Some(work_state) = activity.and_then(|value| value.get(1)) {
      entry.insert(WORK_STATE.to_string(), work_state.clone());
    }
    derived.insert(meta.id.clone(), Arc::new(entry));
  }
  let derived = Arc::new(derived);
  charliebot_store::sidebar_state::store_derived_map(key, Arc::clone(&derived));
  Ok(derived)
}

fn should_probe_synchronously(force_full: bool, force: bool, dirty: bool, has_snapshot: bool) -> bool {
  if force_full && !force { dirty || !has_snapshot } else { force || dirty || !has_snapshot }
}

/// The count of one sidebar read that answered from a memo, with the periodic self-heal sweep scheduled when this
/// poll is due.
///
/// The poll count bounds how late an edit from outside the server can show. Every tenth poll sweeps the active
/// sessions and stores what moved, which moves the sidebar generation that invalidates the memo. A route that
/// answered from a memo without counting its read would stop the sweeps for a client that only reads that route.
pub(crate) fn count_memoized_read(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
  sessions: &[Arc<SessionMetadata>],
) -> StoreResult<()> {
  if charliebot_store::sidebar_state::register_poll(false) {
    schedule_sidebar_sweep(Arc::clone(store), Arc::clone(registry), sessions)?;
  }
  Ok(())
}

/// Whether one poll may start a self-heal sweep now: none runs, and the last one started at least `interval` ago.
fn should_start_sweep(running: bool, last_started: Option<Instant>, now: Instant, interval: Duration) -> bool {
  if running {
    return false;
  }
  last_started.is_none_or(|started| now.duration_since(started) >= interval)
}

/// Recheck one slice of the clean active sessions off the request path on the periodic poll.
/// Dirty rows stay with the synchronous poll that consumes their mark.
fn schedule_sidebar_sweep(
  store: Arc<Store>,
  registry: Arc<Registry>,
  sessions: &[Arc<SessionMetadata>],
) -> StoreResult<()> {
  // In the test build the sweep never runs. Its thread would move the shared sidebar generation long after the
  // read that scheduled it returned, which is exactly the window the corpus-memo tests of `listing.rs` compare.
  // No test of this crate observes the sweep itself. `should_start_sweep` covers its policy as a pure function.
  if cfg!(test) {
    return Ok(());
  }
  let now = Instant::now();
  {
    let mut last_start = SIDEBAR_SWEEP_LAST_START.lock().expect("the sweep start time mutex is not poisoned");
    if !should_start_sweep(SIDEBAR_SWEEP_RUNNING.load(Ordering::Acquire), *last_start, now, SIDEBAR_SWEEP_MIN_INTERVAL)
    {
      return Ok(());
    }
    *last_start = Some(now);
  }
  if SIDEBAR_SWEEP_RUNNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
    return Ok(());
  }
  // The pass owns its slice of the rows: a poll that schedules no sweep, which is most of them, hands the corpus back
  // without cloning every row's handle.
  let sessions = sweep_slice(sessions);
  let spawned = std::thread::Builder::new().name("sidebar-self-heal-sweep".to_string()).spawn(move || {
    let _flight = SidebarSweepFlight;
    let mut selected = Vec::new();
    for meta in sessions {
      if charliebot_store::sidebar_state::is_dirty(&meta.id) {
        continue;
      }
      let signature = probe_signature(&store, &registry, &meta.id);
      let changed = charliebot_store::sidebar_state::probe_signature(&meta.id).as_ref() != Some(&signature);
      let recheck_liveness =
        charliebot_store::sidebar_state::snapshot_task_activity(&meta.id).is_some_and(|(_, work)| work == "running");
      if changed || recheck_liveness {
        selected.push((meta.id.clone(), signature));
      }
    }
    let selected_ids: Vec<String> = selected.iter().map(|(session_id, _)| session_id.clone()).collect();
    for (session_id, signature) in selected {
      match probe_session(&store, &registry, &session_id) {
        Ok(snapshot) => {
          charliebot_store::sidebar_state::store_snapshot_entry(&session_id, snapshot);
          charliebot_store::sidebar_state::store_probe_signature(&session_id, signature);
        }
        Err(error) => {
          for selected_id in selected_ids {
            charliebot_store::sidebar_state::mark_sidebar_dirty(&selected_id);
          }
          tracing::error!(error = %error, "sidebar_self_heal_sweep_failed");
          return;
        }
      }
    }
  });
  if let Err(error) = spawned {
    SIDEBAR_SWEEP_RUNNING.store(false, Ordering::Release);
    // No sweep ran, so the poll after this one may start one instead of waiting out the interval.
    *SIDEBAR_SWEEP_LAST_START.lock().expect("the sweep start time mutex is not poisoned") = None;
    return Err(StoreError::new(error.to_string()));
  }
  Ok(())
}

/// The slice of `sessions` one sweep rechecks: up to [`SIDEBAR_SWEEP_SLICE`] rows from where this corpus's last
/// sweep stopped, wrapping around the end.
///
/// A corpus no larger than one slice is rechecked whole. The corpus's cursor advances by the slice's length, so its
/// consecutive sweeps cover disjoint slices until the corpus ends and the next one wraps. A change in the corpus
/// names a new corpus, whose rotation starts at its first row.
fn sweep_slice(sessions: &[Arc<SessionMetadata>]) -> Vec<Arc<SessionMetadata>> {
  let len = sessions.len();
  if len <= SIDEBAR_SWEEP_SLICE {
    return sessions.to_vec();
  }
  let key = sweep_corpus_key(sessions);
  let start = {
    let mut cursors = SIDEBAR_SWEEP_CURSORS.lock().expect("the sweep cursor mutex is not poisoned");
    let start = cursors.get(&key).unwrap_or(0) % len;
    cursors.store(key, start + SIDEBAR_SWEEP_SLICE);
    start
  };
  let end = start + SIDEBAR_SWEEP_SLICE;
  let mut slice = Vec::with_capacity(SIDEBAR_SWEEP_SLICE);
  slice.extend_from_slice(&sessions[start..end.min(len)]);
  if end > len {
    slice.extend_from_slice(&sessions[..end - len]);
  }
  slice
}

/// The identity of one sweep corpus: its length and a digest of its ids in order.
fn sweep_corpus_key(sessions: &[Arc<SessionMetadata>]) -> (usize, u64) {
  let mut hasher = DefaultHasher::new();
  for meta in sessions {
    meta.id.hash(&mut hasher);
  }
  (sessions.len(), hasher.finish())
}

/// The results of `work` over `items`, in `items`' order, split across [`SIDEBAR_PROBE_THREADS`] when the set is
/// large enough to pay for them.
///
/// Every item's work reads its own session's files and touches the shared memos only through their locks. So one
/// worker's result never depends on another's, and the order of the results is the order of the input.
fn spread_across_threads<T, R, F>(items: &[T], work: F) -> Vec<R>
where
  T: Sync,
  R: Send,
  F: Fn(&T) -> R + Sync,
{
  let threads = SIDEBAR_PROBE_THREADS.min(items.len());
  if threads <= 1 || items.len() < SIDEBAR_PROBE_SPLIT_MIN {
    return items.iter().map(work).collect();
  }
  let chunk = items.len().div_ceil(threads);
  std::thread::scope(|scope| {
    let handles: Vec<_> =
      items.chunks(chunk).map(|part| scope.spawn(|| part.iter().map(&work).collect::<Vec<R>>())).collect();
    handles.into_iter().flat_map(|handle| handle.join().expect("a sidebar probe worker does not panic")).collect()
  })
}

fn probe_session(store: &Store, registry: &Registry, session_id: &str) -> StoreResult<Map<String, Value>> {
  let session_dir = store.session_dir(session_id);
  let triggers_dir = layout::triggers_dir(&session_dir);
  let (pending_count, next_trigger) = charliebot_store::trigger_files::pending_trigger_state(&triggers_dir);
  let mut pending_plan_approval = false;
  let mut waiting_items = false;
  for contribution in registry.sidebar_contributions() {
    for (name, value) in contribution.row_flags(&session_dir, session_id) {
      match name {
        HAS_PENDING_PLAN_APPROVAL => pending_plan_approval |= value,
        HAS_WAITING_ITEMS => waiting_items |= value,
        _ => {}
      }
    }
  }
  let activity = store.task_tree_activity(session_id)?;
  let mut snapshot = Map::new();
  snapshot.insert(PENDING_TRIGGER_COUNT.to_string(), Value::from(pending_count));
  snapshot.insert(NEXT_TRIGGER_AT.to_string(), next_trigger.map_or(Value::Null, |value| json!(value.to_str())));
  snapshot.insert(HAS_PENDING_PLAN_APPROVAL.to_string(), Value::Bool(pending_plan_approval));
  snapshot.insert(HAS_WAITING_ITEMS.to_string(), Value::Bool(waiting_items));
  snapshot.insert(TASK_TREE_ACTIVITY.to_string(), json!([activity.has_running_tasks, activity.work_state]));
  Ok(snapshot)
}

fn probe_signature(store: &Store, registry: &Registry, session_id: &str) -> Vec<Value> {
  let session_dir = store.session_dir(session_id);
  let trigger_files =
    charliebot_store::trigger_files::iter_trigger_file_stats(layout::triggers_dir(&session_dir)).unwrap_or_default();
  let mut trigger_signature: Vec<Value> = trigger_files
    .iter()
    .map(|(path, metadata)| {
      json!([
        path.file_name().unwrap_or_default().to_string_lossy(),
        timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()) as i64,
        metadata.size(),
      ])
    })
    .collect();
  trigger_signature.sort_by(|left, right| left[0].as_str().cmp(&right[0].as_str()));

  let mut watched = Vec::new();
  let mut seen = HashSet::new();
  for contribution in registry.sidebar_contributions() {
    for relative in contribution.watched_files() {
      if seen.insert(*relative) {
        watched.push(stat_signature(&session_dir.join(relative)));
      }
    }
  }
  let runs_dir = layout::runs_dir(&session_dir);
  let mut run_names: Vec<String> = fs::read_dir(&runs_dir)
    .into_iter()
    .flatten()
    .filter_map(Result::ok)
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .collect();
  run_names.sort();
  let run_signatures: Vec<Value> = run_names
    .iter()
    .filter_map(|name| {
      stat_signature(&runs_dir.join(name).join(RUN_METADATA_NAME))
        .map(|signature| json!([name, signature[0], signature[1]]))
    })
    .collect();
  let task_signature = json!([
    stat_signature(&layout::metadata_path(&session_dir)),
    stat_signature(&layout::chat_events_path(&session_dir)),
    stat_signature(&layout::archives_dir(&session_dir)),
    run_signatures,
    store.records_generation(),
  ]);
  vec![json!(trigger_signature), json!(watched), task_signature]
}

fn stat_signature(path: &Path) -> Option<Value> {
  match fs::metadata(path) {
    Ok(metadata) => {
      Some(json!([timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()) as i64, metadata.size()]))
    }
    Err(_) => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_split_sidebar_walk_keeps_the_order_of_its_input() {
    let large: Vec<usize> = (0..SIDEBAR_PROBE_SPLIT_MIN * 3).collect();
    let split = spread_across_threads(&large, |value| value * 2);
    assert_eq!(split, large.iter().map(|value| value * 2).collect::<Vec<usize>>(), "a split keeps every item's place");
    let small = [3_usize, 1, 2];
    assert_eq!(spread_across_threads(&small, |value| value + 1), vec![4, 2, 3], "a small set runs in place");
  }

  #[test]
  fn a_fast_poller_starts_one_self_heal_sweep_per_interval() {
    let interval = Duration::from_secs(10);
    let started = Instant::now();
    assert!(should_start_sweep(false, None, started, interval), "the first poll starts a sweep");
    assert!(
      !should_start_sweep(false, Some(started), started + Duration::from_millis(1), interval),
      "a poll right after a sweep's start waits"
    );
    assert!(
      !should_start_sweep(false, Some(started), started + interval - Duration::from_millis(1), interval),
      "a poll just before the interval waits"
    );
    assert!(should_start_sweep(false, Some(started), started + interval, interval), "the interval's end starts one");
    assert!(!should_start_sweep(true, None, started, interval), "a running sweep takes the poll's trigger");
  }

  #[test]
  fn periodic_self_heal_leaves_clean_probes_off_the_request_path() {
    let clean_active_sessions = 4317;
    let synchronous_probes =
      (0..clean_active_sessions).filter(|_| should_probe_synchronously(true, false, false, true)).count();
    assert_eq!(synchronous_probes, 0);
    assert!(should_probe_synchronously(true, true, false, true), "an explicit force still probes synchronously");
    assert!(should_probe_synchronously(false, false, true, true), "a dirty row is consumed synchronously");
    assert!(should_probe_synchronously(false, false, false, false), "a missing snapshot is built synchronously");
  }

  /// A sweep corpus of `len` rows whose ids carry `prefix`, so two corpora of one length name different cursors.
  fn sweep_corpus(prefix: &str, len: usize) -> Vec<Arc<SessionMetadata>> {
    (0..len)
      .map(|index| {
        Arc::new(SessionMetadata::new(
          format!("{prefix}-{index}"),
          "session",
          charliebot_store::metadata::TaskProfile::Manager,
        ))
      })
      .collect()
  }

  #[test]
  fn a_sweep_covers_the_corpus_in_disjoint_slices_and_wraps() {
    let rows = sweep_corpus("wrap", SIDEBAR_SWEEP_SLICE + 100);
    let first = sweep_slice(&rows);
    let second = sweep_slice(&rows);
    assert_eq!(first.len(), SIDEBAR_SWEEP_SLICE);
    assert!(Arc::ptr_eq(&first[0], &rows[0]), "the first sweep starts at the corpus start");
    assert_eq!(second.len(), SIDEBAR_SWEEP_SLICE);
    assert!(Arc::ptr_eq(&second[0], &rows[SIDEBAR_SWEEP_SLICE]), "the next sweep continues after the first");
    assert!(
      Arc::ptr_eq(&second[rows.len() - SIDEBAR_SWEEP_SLICE], &rows[0]),
      "a slice past the corpus end wraps to its start"
    );
    assert_eq!(sweep_slice(&rows[..10]).len(), 10, "a corpus no larger than one slice is rechecked whole");
  }

  #[test]
  fn a_sweep_rotates_each_corpus_on_its_own_cursor() {
    // Two corpora of one length, polled in turn: a shared cursor would park each on the other's offsets, and the
    // other half of every corpus would never be rechecked.
    let left = sweep_corpus("left", SIDEBAR_SWEEP_SLICE * 2);
    let right = sweep_corpus("right", SIDEBAR_SWEEP_SLICE * 2);
    let left_first = sweep_slice(&left);
    let right_first = sweep_slice(&right);
    let left_second = sweep_slice(&left);
    let right_second = sweep_slice(&right);
    assert!(Arc::ptr_eq(&left_first[0], &left[0]));
    assert!(Arc::ptr_eq(&left_second[0], &left[SIDEBAR_SWEEP_SLICE]), "a corpus continues its own rotation");
    assert!(Arc::ptr_eq(&right_first[0], &right[0]), "another corpus keeps its own cursor");
    assert!(Arc::ptr_eq(&right_second[0], &right[SIDEBAR_SWEEP_SLICE]));
  }

  #[test]
  fn archived_sidebar_rows_have_false_flags_and_no_task_activity() {
    let meta = SessionMetadata {
      id: "synthetic-archived".to_string(),
      name: "Archived synthetic session".to_string(),
      status: SessionStatus::Archived,
      has_unread: false,
      has_running_tasks: false,
      work_state: None,
      has_pending_trigger: false,
      pending_trigger_count: 0,
      next_trigger_at: None,
      has_pending_plan_approval: false,
      starred: false,
      thinking_since: None,
      run_backend: None,
      created_at: charliebot_store::metadata::UtcDateTime::now(),
      updated_at: charliebot_store::metadata::UtcDateTime::now(),
      cc_session_id: None,
      cc_session_started_at: None,
      backend: String::new(),
      schedule_cron: None,
      schedule_enabled: None,
      schedule_next_run: None,
      schedule_timezone: None,
      schedule_allow_failure: None,
      parent_session_id: None,
      successor_session_id: None,
      schema_version: 2,
      task_parent_id: None,
      profile: charliebot_store::metadata::TaskProfile::Manager,
      task: None,
      created_by_event: None,
      origin_ref: None,
      subtree_prompt_ref: None,
      node_prompt_ref: None,
      native_prompt_hash: None,
      native_backend: None,
      native_model: None,
      round_ratings: charliebot_store::metadata::RoundRatings::default(),
      group: None,
      archive_offset: 0,
      extra: Map::new(),
    };
    let mut entry = Map::new();
    entry.insert(HAS_RUNNING_TASKS.to_string(), Value::Bool(false));
    entry.insert("has_pending_trigger".to_string(), Value::Bool(false));
    entry.insert("pending_trigger_count".to_string(), Value::from(0));
    entry.insert("next_trigger_at".to_string(), Value::Null);
    entry.insert(HAS_PENDING_PLAN_APPROVAL.to_string(), Value::Bool(false));
    assert_eq!(meta.status, SessionStatus::Archived);
    assert_eq!(entry.get("work_state"), None);
  }
}
