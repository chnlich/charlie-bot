//! Sidebar status derived from trigger files, feature contributions, and the task tree.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use charliebot_registry::Registry;
use charliebot_store::layout;
use charliebot_store::metadata::{SessionMetadata, SessionStatus};
use charliebot_store::run_records::RUN_METADATA_NAME;
use charliebot_store::{Store, StoreError, StoreResult};
use serde_json::{Map, Value, json};

pub use charliebot_store::sidebar_state::DerivedSidebarMap;

static SIDEBAR_SWEEP_RUNNING: AtomicBool = AtomicBool::new(false);

/// How long after one self-heal sweep starts the next one may start.
///
/// The sweep is triggered by every tenth sidebar poll, and a poller faster than ten polls per sweep would start a new
/// sweep the moment the previous one ended, so the sweep thread ran back to back for as long as the poller kept up. One
/// sweep rechecks the files of every active session, so an interval keeps the self-heal inside a fixed CPU budget.
/// Changes the server makes itself reach the sidebar through their dirty marks, which the poll consumes without a
/// sweep, so the interval only bounds how late an edit from outside the server can show.
const SIDEBAR_SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// The start time of the last self-heal sweep this process ran.
static SIDEBAR_SWEEP_LAST_START: Mutex<Option<Instant>> = Mutex::new(None);

struct SidebarSweepFlight;

impl Drop for SidebarSweepFlight {
  fn drop(&mut self) {
    SIDEBAR_SWEEP_RUNNING.store(false, Ordering::Release);
  }
}

/// Resolve only the sidebar fields selected by one listing or status route.
pub(crate) fn resolve_sidebar_state(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
  sessions: &[Arc<SessionMetadata>],
  include_running: bool,
  include_trigger: bool,
  include_plan: bool,
  force: bool,
) -> StoreResult<Arc<DerivedSidebarMap>> {
  if sessions.is_empty() {
    return Ok(Arc::new(HashMap::new()));
  }
  let flags = vec![include_running, include_trigger, include_plan];
  let ids: Vec<String> = sessions.iter().map(|meta| meta.id.clone()).collect();
  let generation = charliebot_store::sidebar_state::derived_generation();
  let key = charliebot_store::sidebar_state::DerivedMapKey { session_ids: ids.clone(), flags, generation };
  let force_full = charliebot_store::sidebar_state::register_poll(force);
  if !force && let Some(derived) = charliebot_store::sidebar_state::peek_derived_map(&key) {
    if force_full {
      schedule_sidebar_sweep(Arc::clone(store), Arc::clone(registry), sessions.to_vec())?;
    }
    return Ok(derived);
  }

  let mut derived = HashMap::new();
  let mut active = Vec::new();
  for meta in sessions {
    if meta.status == SessionStatus::Archived {
      let mut entry = Map::new();
      if include_running {
        entry.insert("has_running_tasks".to_string(), Value::Bool(false));
      }
      if include_trigger {
        entry.insert("has_pending_trigger".to_string(), Value::Bool(false));
        entry.insert("pending_trigger_count".to_string(), Value::from(0));
        entry.insert("next_trigger_at".to_string(), Value::Null);
      }
      if include_plan {
        entry.insert("has_pending_plan_approval".to_string(), Value::Bool(false));
      }
      derived.insert(meta.id.clone(), Arc::new(entry));
    } else {
      active.push(Arc::clone(meta));
    }
  }

  let mut selected = Vec::new();
  let mut signatures = HashMap::new();
  if force_full && !force {
    schedule_sidebar_sweep(
      Arc::clone(store),
      Arc::clone(registry),
      active.iter().map(|meta| (*meta).clone()).collect(),
    )?;
  }
  for meta in &active {
    let should_check = should_probe_synchronously(
      force_full,
      force,
      charliebot_store::sidebar_state::is_dirty(&meta.id),
      charliebot_store::sidebar_state::snapshot_entry(&meta.id).is_some(),
    );
    if !should_check {
      continue;
    }
    let signature = probe_signature(store, registry, &meta.id);
    let unchanged = charliebot_store::sidebar_state::probe_signature(&meta.id).as_ref() == Some(&signature);
    signatures.insert(meta.id.clone(), signature);
    if force || !unchanged || charliebot_store::sidebar_state::is_dirty(&meta.id) {
      selected.push(meta.id.clone());
    }
  }
  charliebot_store::sidebar_state::discard_dirty(&selected);
  for session_id in &selected {
    match probe_session(store, registry, session_id) {
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
    let activity = snapshot.get("task_tree_activity").and_then(Value::as_array);
    if include_running {
      entry.insert(
        "has_running_tasks".to_string(),
        activity.and_then(|value| value.first()).cloned().unwrap_or(Value::Bool(false)),
      );
    }
    if include_trigger {
      let count = snapshot.get("pending_trigger_count").cloned().unwrap_or(Value::from(0));
      entry.insert("has_pending_trigger".to_string(), Value::Bool(count.as_i64().unwrap_or_default() > 0));
      entry.insert("pending_trigger_count".to_string(), count);
      entry.insert("next_trigger_at".to_string(), snapshot.get("next_trigger_at").cloned().unwrap_or(Value::Null));
    }
    if include_plan {
      entry.insert(
        "has_pending_plan_approval".to_string(),
        snapshot.get("has_pending_plan_approval").cloned().unwrap_or(Value::Bool(false)),
      );
    }
    if let Some(work_state) = activity.and_then(|value| value.get(1)) {
      entry.insert("work_state".to_string(), work_state.clone());
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

/// Whether one poll may start a self-heal sweep now: none runs, and the last one started at least `interval` ago.
fn should_start_sweep(running: bool, last_started: Option<Instant>, now: Instant, interval: Duration) -> bool {
  if running {
    return false;
  }
  last_started.is_none_or(|started| now.duration_since(started) >= interval)
}

/// Recheck every clean active session off the request path on the periodic poll, as Python's
/// `schedule_sidebar_sweep` does. Dirty rows stay with the synchronous poll that consumes their mark.
fn schedule_sidebar_sweep(
  store: Arc<Store>,
  registry: Arc<Registry>,
  sessions: Vec<Arc<SessionMetadata>>,
) -> StoreResult<()> {
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
    return Err(StoreError::new(error.to_string()));
  }
  Ok(())
}

fn probe_session(store: &Store, registry: &Registry, session_id: &str) -> StoreResult<Map<String, Value>> {
  let session_dir = store.session_dir(session_id);
  let triggers_dir = layout::triggers_dir(&session_dir);
  let (pending_count, next_trigger) = charliebot_store::trigger_files::pending_trigger_state(&triggers_dir);
  let mut pending_plan_approval = false;
  for contribution in registry.sidebar_contributions() {
    for (name, value) in contribution.row_flags(&session_dir, session_id) {
      if name == "has_pending_plan_approval" {
        pending_plan_approval |= value;
      }
    }
  }
  let activity = store.task_tree_activity(session_id)?;
  let mut snapshot = Map::new();
  snapshot.insert("pending_trigger_count".to_string(), Value::from(pending_count));
  snapshot.insert("next_trigger_at".to_string(), next_trigger.map_or(Value::Null, |value| json!(value.to_str())));
  snapshot.insert("has_pending_plan_approval".to_string(), Value::Bool(pending_plan_approval));
  snapshot.insert("task_tree_activity".to_string(), json!([activity.has_running_tasks, activity.work_state]));
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
        metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000,
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
    Ok(metadata) => Some(json!([metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000, metadata.size()])),
    Err(_) => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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

  #[test]
  fn archived_sidebar_rows_have_false_flags_and_no_task_activity() {
    let meta = SessionMetadata {
      id: "synthetic-archived".to_string(),
      name: "Archived synthetic session".to_string(),
      status: SessionStatus::Archived,
      ..SessionMetadata::new("unused", "unused", charliebot_store::metadata::TaskProfile::Manager)
    };
    let mut entry = Map::new();
    entry.insert("has_running_tasks".to_string(), Value::Bool(false));
    entry.insert("has_pending_trigger".to_string(), Value::Bool(false));
    entry.insert("pending_trigger_count".to_string(), Value::from(0));
    entry.insert("next_trigger_at".to_string(), Value::Null);
    entry.insert("has_pending_plan_approval".to_string(), Value::Bool(false));
    assert_eq!(meta.status, SessionStatus::Archived);
    assert_eq!(entry.get("work_state"), None);
  }
}
