//! Read-only worker transcript projection over task-run event logs.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use charliebot_core::event_types;
use charliebot_core::pyfmt;
use charliebot_store::control_events;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::projection::MessageProjection;
use charliebot_store::run_records::RunRecord;
use charliebot_store::{Store, StoreError, StoreResult};
use serde_json::{Value, json};

const WORKER_PROJECTION_MEMO_LIMIT: usize = 32;
const FAILED_STATES: [&str; 3] = ["failed", "interrupted", "attention"];
const SIGNALLABLE_STATES: [&str; 2] = ["running", "queued"];

type Signature = Vec<Value>;
type TranscriptMemo = Mutex<VecDeque<(String, Signature, Arc<TranscriptEntry>)>>;

static WORKER_MEMO: LazyLock<TranscriptMemo> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// The synthesized worker history and the values shared by transcript readers.
pub struct TranscriptEntry {
  pub revision: String,
  pub events: Vec<Value>,
  pub projection: Arc<MessageProjection>,
  pub active_run_id: Option<String>,
  states: BTreeMap<String, String>,
}

/// Load a memoized worker transcript, rebuilding after a run-file signature or task-state change.
pub fn load_worker_transcript(store: &Store, session_id: &str) -> StoreResult<Arc<TranscriptEntry>> {
  let signature = worker_signature(store, session_id)?;
  let task_state = store.task_state(session_id)?;
  {
    let mut memo = WORKER_MEMO.lock().expect("worker transcript memo mutex is not poisoned");
    if let Some(index) = memo.iter().position(|(id, _, _)| id == session_id) {
      let (id, cached_signature, cached) = memo.remove(index).expect("memo index names an entry");
      if cached.revision == revision(&signature, &cached.states, &task_state)? {
        memo.push_back((id, cached_signature, Arc::clone(&cached)));
        return Ok(cached);
      }
      memo.push_back((id, cached_signature, cached));
    }
  }
  let entry = Arc::new(build_worker_transcript(store, session_id, signature)?);
  let mut memo = WORKER_MEMO.lock().expect("worker transcript memo mutex is not poisoned");
  memo.retain(|(id, _, _)| id != session_id);
  memo.push_back((session_id.to_string(), worker_signature(store, session_id)?, Arc::clone(&entry)));
  while memo.len() > WORKER_PROJECTION_MEMO_LIMIT {
    memo.pop_front();
  }
  Ok(entry)
}

fn build_worker_transcript(store: &Store, session_id: &str, signature: Signature) -> StoreResult<TranscriptEntry> {
  let run_records = store.list_run_records_sync(session_id)?;
  let facts_events = store.load_all_facts(session_id)?;
  let mut states = BTreeMap::new();
  for run in &run_records {
    states.insert(run.id.clone(), store.run_display_state(session_id, &run.id)?.as_str().to_string());
  }

  let cfg = store.config();
  let mut transcript = Vec::new();
  for run in &run_records {
    let state = states.get(&run.id).expect("every listed run has a display state");
    let run_dir = store.run_dir(session_id, &run.id);
    let mut run_events = read_run_events(&run_dir.join("events.jsonl"));
    let (error, error_index) = header_error(state, &run_events);
    if let Some(index) = error_index {
      run_events.remove(index);
    }
    let launch_prompt_path = run_dir.join("launch_prompt.md");
    let launch_prompt_ref =
      if launch_prompt_path.is_file() { launch_prompt_path.to_string_lossy().into_owned() } else { String::new() };
    let backend = run.backend.as_deref().unwrap_or("");
    let backend_label = cfg.get_backend_option(backend).map_or(backend, |option| option.label.as_str());
    transcript.push(run_header_event(
      run,
      state,
      &error,
      backend_label,
      &launch_prompt_ref,
      if state == "withheld" { withheld_reason(&facts_events, &run.id) } else { None },
    ));
    transcript.extend(run_events);
  }

  let task_state = store.task_state(session_id)?;
  if let Some(delivery) = delivery_event(&task_state, &facts_events, &run_records) {
    transcript.push(delivery);
  }
  let active_run_id = run_records.iter().rev().find_map(|run| {
    states.get(&run.id).filter(|state| SIGNALLABLE_STATES.contains(&state.as_str())).map(|_| run.id.clone())
  });
  let revision = revision(&signature, &states, &task_state)?;
  let projection = store.build_message_projection(&transcript, 0)?;
  Ok(TranscriptEntry { revision, events: transcript, projection: Arc::new(projection), active_run_id, states })
}

fn worker_signature(store: &Store, session_id: &str) -> StoreResult<Signature> {
  let root = store.runs_root(session_id);
  let mut parts = Vec::new();
  match fs::read_dir(&root) {
    Ok(entries) => {
      let mut directories = Vec::new();
      for entry in entries {
        let entry = entry.map_err(|error| path_error(&root, error))?;
        if entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_dir() {
          directories.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
        }
      }
      directories.sort_by(|left, right| left.0.cmp(&right.0));
      for (name, path) in directories {
        parts.push(json!([
          name,
          stat_signature(&path.join("metadata.json"))?,
          stat_signature(&path.join("events.jsonl"))?
        ]));
      }
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(path_error(&root, error)),
  }
  parts.push(json!(["chat", stat_signature(&store.chat_events_path(session_id))?]));
  Ok(parts)
}

fn stat_signature(path: &Path) -> StoreResult<Value> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(json!([metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000, metadata.size()])),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
    Err(error) => Err(path_error(path, error)),
  }
}

fn read_run_events(path: &Path) -> Vec<Value> {
  let raw = match fs::read(path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "worker transcript run log could not be read");
      return Vec::new();
    }
  };
  if raw.is_empty() {
    return Vec::new();
  }
  raw
    .split(|byte| *byte == b'\n')
    .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    .filter_map(|line| {
      serde_json::from_slice::<Value>(line)
        .or_else(|_| serde_json::from_slice::<Value>(String::from_utf8_lossy(line).as_bytes()))
        .ok()
    })
    .filter(|event| event.get("type").and_then(Value::as_str) != Some(event_types::SESSION_ATTACHED))
    .collect()
}

fn header_error(state: &str, events: &[Value]) -> (String, Option<usize>) {
  if !FAILED_STATES.contains(&state) {
    return (String::new(), None);
  }
  for (index, event) in events.iter().enumerate().rev() {
    if event.get("type").and_then(Value::as_str) != Some(event_types::ERROR) {
      continue;
    }
    for key in ["message", "content"] {
      if let Some(value) = event.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()) {
        return (value.trim().to_string(), Some(index));
      }
    }
  }
  (String::new(), None)
}

fn run_header_event(
  run: &RunRecord,
  state: &str,
  error: &str,
  backend_label: &str,
  launch_prompt_ref: &str,
  withheld: Option<String>,
) -> Value {
  let started = run.started_at.map(UtcDateTime::to_python_isoformat);
  let terminal = run.ended_at.map(UtcDateTime::to_python_isoformat);
  let kind = serde_json::to_value(run.kind).expect("run kind serializes");
  json!({
    "type": event_types::RUN_HEADER,
    "run_id": run.id,
    "kind": kind,
    "backend": run.backend.as_deref().unwrap_or(""),
    "backend_label": backend_label,
    "state": state,
    "error": error,
    "withheld": withheld,
    "started_at": started,
    "timestamp": started.or(terminal),
    "launched": run.started_at.is_some(),
    "launch_failed": FAILED_STATES.contains(&state) && run.started_at.is_none(),
    "task_spec_ref": run.task_spec_ref.as_deref().unwrap_or(""),
    "launch_prompt_ref": launch_prompt_ref,
  })
}

fn withheld_reason(events: &[Value], run_id: &str) -> Option<String> {
  let mut reason = None;
  for event in events {
    if event.get("type").and_then(Value::as_str) != Some(event_types::RUN_LAUNCH_WITHHELD)
      || event.get("run_id").and_then(Value::as_str) != Some(run_id)
    {
      continue;
    }
    reason = event.get("reason").and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);
  }
  reason
}

fn delivery_event(task_state: &str, facts_events: &[Value], run_records: &[RunRecord]) -> Option<Value> {
  if task_state == "open" || run_records.is_empty() {
    return None;
  }
  let mut summary = String::new();
  let mut close_refs = Vec::new();
  let mut outcomes = BTreeMap::new();
  for event in facts_events {
    match event.get("type").and_then(Value::as_str) {
      Some(event_types::TASK_CLOSED) => {
        summary =
          event.get("summary").filter(|value| !value.is_null()).map_or_else(String::new, value_to_python_string);
        close_refs = event
          .get("result_refs")
          .and_then(Value::as_array)
          .into_iter()
          .flatten()
          .map(value_to_python_string)
          .collect();
      }
      Some(event_types::RUN_FINISHED) => {
        if let Some(run_id) = event.get("run_id") {
          outcomes.insert(
            value_to_python_string(run_id),
            event.get("outcome").map_or_else(String::new, value_to_python_string),
          );
        }
      }
      _ => {}
    }
  }
  let delivered = run_records
    .iter()
    .rev()
    .find(|run| outcomes.get(&run.id).is_some_and(|outcome| outcome == "success"))
    .unwrap_or_else(|| run_records.last().expect("run records are non-empty"));
  Some(json!({
    "type": event_types::RUN_DELIVERY,
    "task_state": task_state,
    "summary": summary,
    "result_refs": close_refs,
    "raw_log_ref": delivered.raw_log_ref.as_deref().unwrap_or(""),
    "events_ref": delivered.events_ref.as_deref().unwrap_or(""),
    "result_ref": delivered.result_ref.as_deref().unwrap_or(""),
    "repo_path": delivered.repo_path.as_deref().unwrap_or(""),
    "base_branch": delivered.base_branch.as_deref().unwrap_or(""),
    "branch_name": delivered.branch_name.as_deref().unwrap_or(""),
    "run_id": delivered.id,
    "timestamp": charliebot_core::timefmt::utc_now_iso(),
  }))
}

fn revision(signature: &[Value], states: &BTreeMap<String, String>, task_state: &str) -> StoreResult<String> {
  let state_pairs: Vec<Value> = states.iter().map(|(run_id, state)| json!([run_id, state])).collect();
  let mut payload = signature.to_vec();
  payload.push(Value::Array(state_pairs));
  payload.push(Value::String(task_state.to_string()));
  let text = pyfmt::json_dumps(&Value::Array(payload), &pyfmt::PYTHON_JSON_DEFAULT);
  Ok(control_events::sha256_hex(&text)[..24].to_string())
}

fn value_to_python_string(value: &Value) -> String {
  match value {
    Value::String(value) => value.clone(),
    Value::Null => "None".to_string(),
    Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
    _ => value.to_string(),
  }
}

fn path_error(path: &Path, error: std::io::Error) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn failed_header_uses_newest_nonblank_error_and_removes_its_row() {
    let events = vec![
      json!({"type": "error", "message": "first failure"}),
      json!({"type": "error", "content": "  final failure  "}),
      json!({"type": "assistant", "content": "tail"}),
    ];
    assert_eq!(header_error("failed", &events), ("final failure".to_string(), Some(1)));
    assert_eq!(header_error("running", &events), (String::new(), None));
  }
}
