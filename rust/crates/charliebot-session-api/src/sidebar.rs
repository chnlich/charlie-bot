//! Sidebar status derived from trigger files, feature contributions, and the task tree.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use charliebot_registry::Registry;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, StoreResult};
use serde_json::{Map, Value, json};

pub type DerivedSidebarMap = HashMap<String, Map<String, Value>>;

/// Resolve only the sidebar fields selected by one listing or status route.
pub(crate) fn resolve_sidebar_state(
  store: &Store,
  registry: &Registry,
  sessions: &[SessionMetadata],
  include_running: bool,
  include_trigger: bool,
  include_plan: bool,
  force: bool,
) -> StoreResult<DerivedSidebarMap> {
  if sessions.is_empty() {
    return Ok(HashMap::new());
  }
  let flags = vec![include_running, include_trigger, include_plan];
  let ids: Vec<String> = sessions.iter().map(|meta| meta.id.clone()).collect();
  let generation = charliebot_store::sidebar_state::derived_generation();
  let key = charliebot_store::sidebar_state::DerivedMapKey { session_ids: ids.clone(), flags, generation };
  let force_full = charliebot_store::sidebar_state::register_poll(force);
  if !force && let Some(derived) = charliebot_store::sidebar_state::peek_derived_map(&key) {
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
      derived.insert(meta.id.clone(), entry);
    } else {
      active.push(meta);
    }
  }

  let mut selected = Vec::new();
  let mut signatures = HashMap::new();
  for meta in &active {
    let should_check = force_full
      || force
      || charliebot_store::sidebar_state::is_dirty(&meta.id)
      || charliebot_store::sidebar_state::snapshot_entry(&meta.id).is_none();
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
    derived.insert(meta.id.clone(), entry);
  }
  charliebot_store::sidebar_state::store_derived_map(key, derived.clone());
  Ok(derived)
}

fn probe_session(store: &Store, registry: &Registry, session_id: &str) -> StoreResult<Map<String, Value>> {
  let session_dir = store.session_dir(session_id);
  let triggers_dir = session_dir.join("triggers");
  let mut pending_count = 0_i64;
  let mut next_trigger = None;
  let trigger_files = match charliebot_store::trigger_files::iter_trigger_file_stats(&triggers_dir) {
    Ok(files) => files,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => {
      tracing::debug!(session_id, error = %error, "trigger_directory_read_failed");
      Vec::new()
    }
  };
  for (path, _) in trigger_files {
    let raw = match fs::read_to_string(&path) {
      Ok(raw) => raw,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "trigger_meta_read_failed");
        continue;
      }
    };
    let trigger: Value = match serde_json::from_str(&raw) {
      Ok(trigger) => trigger,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "trigger_meta_read_failed");
        continue;
      }
    };
    if trigger.get("status").and_then(Value::as_str) != Some("pending") {
      continue;
    }
    pending_count += 1;
    if let Some(fire_at) = trigger.get("fire_at").and_then(Value::as_str)
      && let Ok(parsed) = UtcDateTime::parse(fire_at)
    {
      next_trigger = Some(next_trigger.map_or(parsed, |current: UtcDateTime| current.min(parsed)));
    }
  }

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
  snapshot
    .insert("next_trigger_at".to_string(), next_trigger.map_or(Value::Null, |value| json!(value.to_python_string())));
  snapshot.insert("has_pending_plan_approval".to_string(), Value::Bool(pending_plan_approval));
  snapshot.insert("task_tree_activity".to_string(), json!([activity.has_running_tasks, activity.work_state]));
  Ok(snapshot)
}

fn probe_signature(store: &Store, registry: &Registry, session_id: &str) -> Vec<Value> {
  let session_dir = store.session_dir(session_id);
  let trigger_files =
    charliebot_store::trigger_files::iter_trigger_file_stats(session_dir.join("triggers")).unwrap_or_default();
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
  let runs_dir = session_dir.join("data/runs");
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
      stat_signature(&runs_dir.join(name).join("metadata.json"))
        .map(|signature| json!([name, signature[0], signature[1]]))
    })
    .collect();
  let task_signature = json!([
    stat_signature(&session_dir.join("metadata.json")),
    stat_signature(&session_dir.join("data/chat_events.jsonl")),
    stat_signature(&session_dir.join("data/archives")),
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
