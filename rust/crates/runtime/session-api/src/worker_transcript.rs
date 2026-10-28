//! Read-only worker transcript projection over task-run event logs.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use charliebot_core::event_types;
use charliebot_core::pyfmt;
use charliebot_store::ChatEvent;
use charliebot_store::CompactValue;
use charliebot_store::aggregator;
use charliebot_store::control_events;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::projection::MessageProjection;
use charliebot_store::run_records::{HostBoot, RUN_EVENTS_NAME, RUN_METADATA_NAME, RunKind, RunRecord};
use charliebot_store::{SharedChatEvent, Store, StoreError, StoreResult};
use charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME;
use lru::LruCache;
use serde_json::{Value, json};

const WORKER_PROJECTION_MEMO_LIMIT: usize = 32;
const FAILED_STATES: [&str; 3] = ["failed", "interrupted", "attention"];
const SIGNALLABLE_STATES: [&str; 2] = ["running", "queued"];

type Signature = Vec<Value>;
type TranscriptMemo = Mutex<LruCache<String, (Signature, Arc<TranscriptEntry>)>>;

static WORKER_MEMO: LazyLock<TranscriptMemo> = LazyLock::new(|| {
  Mutex::new(LruCache::new(
    NonZeroUsize::new(WORKER_PROJECTION_MEMO_LIMIT).expect("the worker transcript memo limit is nonzero"),
  ))
});

/// The synthesized worker history and the values shared by transcript readers.
pub struct TranscriptEntry {
  pub revision: String,
  rebuild_key: String,
  pub events: Arc<Vec<SharedChatEvent>>,
  pub projection: Arc<MessageProjection>,
  pub summary: Value,
  pub active_run_id: Option<String>,
  states: BTreeMap<String, String>,
}

/// Load a memoized worker transcript, rebuilding after a run-file signature or task-state change.
pub fn load_worker_transcript(store: &Store, session_id: &str) -> StoreResult<Arc<TranscriptEntry>> {
  let signature = worker_signature(store, session_id)?;
  let task_state = store.task_state(session_id)?;
  {
    let mut memo = WORKER_MEMO.lock().expect("worker transcript memo mutex is not poisoned");
    if let Some((cached_signature, cached)) = memo.pop(session_id) {
      // An entry whose run files changed stays memoized: only the rebuild below replaces it.
      memo.put(session_id.to_string(), (cached_signature, Arc::clone(&cached)));
      if cached.rebuild_key == revision(&signature, &cached.states, &task_state)? {
        return Ok(cached);
      }
    }
  }
  let entry = Arc::new(build_worker_transcript(store, session_id, signature)?);
  let mut memo = WORKER_MEMO.lock().expect("worker transcript memo mutex is not poisoned");
  memo.put(session_id.to_string(), (worker_signature(store, session_id)?, Arc::clone(&entry)));
  Ok(entry)
}

fn build_worker_transcript(store: &Store, session_id: &str, signature: Signature) -> StoreResult<TranscriptEntry> {
  let run_records = store.list_run_records_sync(session_id)?;
  let facts_events = store.load_all_facts_shared_sync(session_id)?;
  let node = store.node_state(session_id)?;
  let mut boot = HostBoot::default();
  let mut states = BTreeMap::new();
  for run in &run_records {
    states.insert(run.id.clone(), node.display_state(run, &mut boot)?.as_str().to_string());
  }

  let cfg = store.config();
  // The transcript events stay in the compact form from parse to projection fold: Python's builder
  // (src/runtime/worker_transcript.py) shares its parsed dicts between the summary, the map, and the
  // projection input, and the compact form is the one tree the fold and the summary read in place.
  let mut transcript: Vec<SharedChatEvent> = Vec::new();
  let mut run_events_map = HashMap::new();
  let mut run_tools = HashMap::new();
  let mut run_causes = HashMap::new();
  for run in &run_records {
    let state = states.get(&run.id).expect("every listed run has a display state");
    let run_dir = store.run_dir(session_id, &run.id);
    let mut run_events: Vec<ChatEvent> = read_run_events(store, &run_dir.join(RUN_EVENTS_NAME));
    let mut error = String::new();
    if FAILED_STATES.contains(&state.as_str())
      && let Some((text, first_line, index, timestamp)) =
        aggregator::run_cause(&run_events.iter().collect::<Vec<&ChatEvent>>())
    {
      error = text;
      run_causes.insert(run.id.clone(), (first_line, timestamp));
      run_events.remove(index);
    }
    let tool_calls = aggregator::run_tool_call_count(&run_events.iter().collect::<Vec<&ChatEvent>>());
    run_tools.insert(run.id.clone(), tool_calls);
    // One shared tree per event: the map, the summary, and the transcript read the same parse.
    let shared: Vec<SharedChatEvent> = run_events.into_iter().map(Arc::new).collect();
    run_events_map.insert(run.id.clone(), shared.clone());
    let launch_prompt_path = run_dir.join(LAUNCH_TEXT_FILENAME);
    let launch_prompt_ref =
      if launch_prompt_path.is_file() { launch_prompt_path.to_string_lossy().into_owned() } else { String::new() };
    let backend = run.backend.as_deref().unwrap_or("");
    let backend_label = cfg.get_backend_option(backend).map_or(backend, |option| option.label.as_str());
    transcript.push(Arc::new(ChatEvent::from_value(&run_header_event(
      run,
      state,
      &error,
      backend_label,
      &launch_prompt_ref,
      if state == "withheld" { withheld_reason(facts_events.as_slice(), &run.id) } else { None },
      (!SIGNALLABLE_STATES.contains(&state.as_str())).then_some(tool_calls),
    ))));
    for event in shared {
      // The rewrite replaces the transcript's copy only: the summary map keeps the raw event, as
      // Python's `{**event, "type": ET.RUN_ERROR}` does.
      let event = if event.get("type").and_then(CompactValue::as_str) == Some(event_types::ERROR) {
        let mut value = event.as_ref().to_value();
        value["type"] = json!(event_types::RUN_ERROR);
        Arc::new(ChatEvent::from_value(&value))
      } else {
        event
      };
      transcript.push(event);
    }
    if !SIGNALLABLE_STATES.contains(&state.as_str()) {
      transcript.push(Arc::new(ChatEvent::from_value(&run_end_event(run))));
    }
  }

  let task_state = store.task_state(session_id)?;
  if let Some(delivery) = delivery_event(&task_state, facts_events.as_slice(), &run_records)? {
    transcript.push(Arc::new(ChatEvent::from_value(&delivery)));
  }
  let active_run_id = run_records.iter().rev().find_map(|run| {
    states.get(&run.id).filter(|state| SIGNALLABLE_STATES.contains(&state.as_str())).map(|_| run.id.clone())
  });
  let revision = revision(&signature, &states, &task_state)?;
  let client_revision = client_revision(&signature, &run_records, &states, &task_state)?;
  let summary = build_summary(
    store,
    session_id,
    SummaryInputs {
      task_state: &task_state,
      facts_events: facts_events.as_slice(),
      run_records: &run_records,
      states: &states,
      run_events: &run_events_map,
      run_tools: &run_tools,
      run_causes: &run_causes,
    },
  )?;
  let projection = store.build_message_projection(&transcript, 0)?;
  Ok(TranscriptEntry {
    revision: client_revision,
    rebuild_key: revision,
    events: Arc::new(transcript),
    projection: Arc::new(projection),
    summary,
    active_run_id,
    states,
  })
}

/// One run's shared events borrowed as the fold input the aggregator helpers read.
fn run_event_refs<'a>(run_events: &'a HashMap<String, Vec<SharedChatEvent>>, run_id: &str) -> Vec<&'a ChatEvent> {
  run_events.get(run_id).map(|events| events.iter().map(Arc::as_ref).collect()).unwrap_or_default()
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
          stat_signature(&path.join(RUN_METADATA_NAME))?,
          stat_signature(&path.join(RUN_EVENTS_NAME))?
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

/// One run's events.jsonl parsed into the compact form; a missing or unreadable log is an empty segment.
fn read_run_events(store: &Store, path: &Path) -> Vec<ChatEvent> {
  match store_read(store, path) {
    Ok(events) => events,
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "worker transcript run log could not be read");
      Vec::new()
    }
  }
}

fn store_read(store: &Store, path: &Path) -> StoreResult<Vec<ChatEvent>> {
  let mut events = store.read_ndjson_compact(path)?;
  events.retain(|event| event.get("type").and_then(CompactValue::as_str) != Some(event_types::SESSION_ATTACHED));
  Ok(events)
}

fn run_header_event(
  run: &RunRecord,
  state: &str,
  error: &str,
  backend_label: &str,
  launch_prompt_ref: &str,
  withheld: Option<String>,
  tool_calls: Option<usize>,
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
    "tool_calls": tool_calls,
    "withheld": withheld,
    "started_at": started,
    "timestamp": started.or(terminal),
    "launched": run.started_at.is_some(),
    "launch_failed": FAILED_STATES.contains(&state) && run.started_at.is_none(),
    "task_spec_ref": run.task_spec_ref.as_deref().unwrap_or(""),
    "launch_prompt_ref": launch_prompt_ref,
  })
}

fn run_end_event(run: &RunRecord) -> Value {
  let duration_seconds = match (run.started_at, run.ended_at) {
    (Some(started), Some(ended)) => Some((ended.into_inner() - started.into_inner()).whole_seconds()),
    _ => None,
  };
  json!({
    "type": event_types::RUN_END,
    "run_id": run.id,
    "timestamp": run.ended_at.map(UtcDateTime::to_python_isoformat),
    "duration_seconds": duration_seconds,
  })
}

fn withheld_reason(events: &[SharedChatEvent], run_id: &str) -> Option<String> {
  let mut reason = None;
  for held in events {
    let event = held.to_value();
    if event.get("type").and_then(Value::as_str) != Some(event_types::RUN_LAUNCH_WITHHELD)
      || event.get("run_id").and_then(Value::as_str) != Some(run_id)
    {
      continue;
    }
    reason = event.get("reason").and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);
  }
  reason
}

fn delivery_event(
  task_state: &str,
  facts_events: &[SharedChatEvent],
  run_records: &[RunRecord],
) -> StoreResult<Option<Value>> {
  if task_state == "open" || run_records.is_empty() {
    return Ok(None);
  }
  let mut summary = String::new();
  let mut close_refs = Vec::new();
  // A non-open task state only comes from a task_closed fact, so the loop below sets this.
  let mut close_at = String::new();
  let mut outcomes = BTreeMap::new();
  for held in facts_events {
    let event = held.to_value();
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
        close_at = event
          .get("timestamp")
          .map(value_to_python_string)
          .ok_or_else(|| StoreError::new("task_closed fact has no timestamp"))?;
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
  Ok(Some(json!({
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
    "timestamp": close_at,
  })))
}

struct SummaryInputs<'a> {
  task_state: &'a str,
  facts_events: &'a [SharedChatEvent],
  run_records: &'a [RunRecord],
  states: &'a BTreeMap<String, String>,
  run_events: &'a HashMap<String, Vec<SharedChatEvent>>,
  run_tools: &'a HashMap<String, usize>,
  run_causes: &'a HashMap<String, (String, String)>,
}

fn build_summary(store: &Store, session_id: &str, inputs: SummaryInputs<'_>) -> StoreResult<Value> {
  let SummaryInputs { task_state, facts_events, run_records, states, run_events, run_tools, run_causes } = inputs;
  let mut closed_at = String::new();
  let mut landed = String::new();
  let mut outcomes = BTreeMap::new();
  for held in facts_events {
    let event = held.to_value();
    match event.get("type").and_then(Value::as_str) {
      Some(event_types::TASK_CLOSED) => {
        closed_at = truthy_python_string(event.get("timestamp"));
        if let Some(refs) = event.get("result_refs").and_then(Value::as_array) {
          for reference in refs {
            let reference = value_to_python_string(reference);
            if let Some(value) = reference.strip_prefix("landed:") {
              landed = value.to_string();
            }
          }
        }
      }
      Some(event_types::RUN_FINISHED) => {
        let run_id = event.get("run_id").map(value_to_python_string).unwrap_or_else(|| "None".to_string());
        outcomes.insert(run_id, truthy_python_string(event.get("outcome")));
      }
      _ => {}
    }
  }

  let runs = run_records
    .iter()
    .map(|run| {
      let backend = run.backend.as_deref().unwrap_or("");
      let backend_label = store.config().get_backend_option(backend).map_or(backend, |option| option.label.as_str());
      json!({
        "run_id": run.id,
        "kind": serde_json::to_value(run.kind).expect("run kind serializes"),
        "backend_label": backend_label,
        "state": states.get(&run.id).map(String::as_str).unwrap_or("queued"),
        "started_at": run.started_at.map(UtcDateTime::to_python_isoformat),
        "ended_at": run.ended_at.map(UtcDateTime::to_python_isoformat),
        "tool_calls": run_tools.get(&run.id).copied().unwrap_or(0),
      })
    })
    .collect::<Vec<_>>();

  let newest = run_records.last();
  let active = match newest.filter(|run| states.get(&run.id).is_some_and(|state| state == "running")) {
    Some(run) => {
      let events = run_event_refs(run_events, &run.id);
      json!({
        "run_id": run.id,
        "started_at": run.started_at.map(UtcDateTime::to_python_isoformat),
        "tool_calls": run_tools.get(&run.id).copied().unwrap_or(0),
        "last_text": aggregator::run_last_assistant_text(&events)?,
        "last_tool": aggregator::run_last_tool(&events),
      })
    }
    None => Value::Null,
  };

  let closing_for = |kind: RunKind| -> StoreResult<Value> {
    let Some(run) = run_records.iter().rev().find(|run| run.kind == kind) else { return Ok(Value::Null) };
    let text = aggregator::run_last_text(&run_event_refs(run_events, &run.id))?
      .and_then(|value| value.get("text").cloned())
      .unwrap_or(Value::Null);
    Ok(json!({"run_id": run.id, "text": text}))
  };
  let closing = json!({"work": closing_for(RunKind::Work)?, "review": closing_for(RunKind::Review)?});

  let cause = run_records
    .iter()
    .rev()
    .find(|run| states.get(&run.id).is_some_and(|state| FAILED_STATES.contains(&state.as_str())))
    .and_then(|run| {
      run_causes.get(&run.id).map(|(first_line, timestamp)| {
        json!({
          "run_id": run.id,
          "text": first_line,
          "timestamp": timestamp,
        })
      })
    })
    .unwrap_or(Value::Null);

  let stop = facts_events
    .iter()
    .rev()
    .find(|held| {
      held.get("type").and_then(charliebot_store::compact::CompactValue::as_str)
        == Some(event_types::RUN_STOP_REQUESTED)
    })
    .map(|held| {
      let event = held.to_value();
      json!({
        "run_id": truthy_python_string(event.get("run_id")),
        "request_id": truthy_python_string(event.get("request_id")),
        "timestamp": truthy_python_string(event.get("timestamp")),
      })
    })
    .unwrap_or(Value::Null);

  let delivery = if task_state != "open" {
    run_records.last().map(|last| {
      let delivered = run_records
        .iter()
        .rev()
        .find(|run| outcomes.get(&run.id).is_some_and(|outcome| outcome == "success"))
        .unwrap_or(last);
      json!({
        "run_id": delivered.id,
        "raw_log_ref": delivered.raw_log_ref.as_deref().unwrap_or(""),
        "events_ref": delivered.events_ref.as_deref().unwrap_or(""),
        "result_ref": delivered.result_ref.as_deref().unwrap_or(""),
        "repo_path": delivered.repo_path.as_deref().unwrap_or(""),
        "base_branch": delivered.base_branch.as_deref().unwrap_or(""),
        "branch_name": delivered.branch_name.as_deref().unwrap_or(""),
      })
    })
  } else {
    None
  }
  .unwrap_or(Value::Null);

  let links = newest
    .map(|run| {
      let launch_prompt_path = store.run_dir(session_id, &run.id).join(LAUNCH_TEXT_FILENAME);
      json!({
        "task_spec_ref": run.task_spec_ref.as_deref().unwrap_or(""),
        "launch_prompt_ref": if launch_prompt_path.is_file() { launch_prompt_path.to_string_lossy().into_owned() } else { String::new() },
        "raw_log_ref": run.raw_log_ref.as_deref().unwrap_or(""),
        "events_ref": run.events_ref.as_deref().unwrap_or(""),
      })
    })
    .unwrap_or_else(|| json!({}));

  Ok(json!({
    "task_state": task_state,
    "closed_at": if closed_at.is_empty() { Value::Null } else { json!(closed_at) },
    "landed": if landed.is_empty() { Value::Null } else { json!(landed) },
    "delivery": delivery,
    "runs": runs,
    "active": active,
    "closing": closing,
    "cause": cause,
    "stop": stop,
    "links": links,
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

fn client_revision(
  signature: &[Value],
  run_records: &[RunRecord],
  states: &BTreeMap<String, String>,
  task_state: &str,
) -> StoreResult<String> {
  let mut stats = HashMap::new();
  for part in signature.iter().take(signature.len() - 1) {
    let Some(values) = part.as_array() else { continue };
    let Some(run_id) = values.first().and_then(Value::as_str) else { continue };
    let metadata = values.get(1).cloned().unwrap_or(Value::Null);
    let events = values.get(2).cloned().unwrap_or(Value::Null);
    stats.insert(run_id.to_string(), (metadata, events));
  }
  let run_ids = run_records.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
  let metadata_stats = run_records
    .iter()
    .map(|run| stats.get(&run.id).map(|(metadata, _)| metadata.clone()).unwrap_or(Value::Null))
    .collect::<Vec<_>>();
  let state_values = run_records
    .iter()
    .map(|run| states.get(&run.id).map(|state| json!(state)).unwrap_or(Value::Null))
    .collect::<Vec<_>>();
  let newest_index = run_records.len().saturating_sub(1);
  let event_stats = run_records
    .iter()
    .enumerate()
    .filter(|(index, run)| *index != newest_index || states.get(&run.id).map(String::as_str) != Some("running"))
    .map(|(_, run)| stats.get(&run.id).map(|(_, events)| events.clone()).unwrap_or(Value::Null))
    .collect::<Vec<_>>();
  let payload = json!([
    run_ids,
    metadata_stats,
    signature.last().expect("worker signature includes the chat stat"),
    state_values,
    task_state,
    event_stats,
  ]);
  let text = pyfmt::json_dumps(&payload, &pyfmt::PYTHON_JSON_DEFAULT);
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

fn truthy_python_string(value: Option<&Value>) -> String {
  let Some(value) = value else { return String::new() };
  let truthy = match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  };
  if truthy { value_to_python_string(value) } else { String::new() }
}

fn path_error(path: &Path, error: std::io::Error) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::run_records::{RunOutcome, RunOutcomeKind};

  use super::*;
  use crate::test_support::{create_node, feature_context};

  async fn worker_context(label: &str) -> (tempfile::TempDir, Arc<charliebot_registry::FeatureContext>, String) {
    let (home, features) = feature_context();
    let root = create_node(features.store.as_ref(), &format!("{label}-manager"), None, TaskProfile::Manager).await;
    let worker =
      create_node(features.store.as_ref(), &format!("{label}-worker"), Some(&root.id), TaskProfile::Worker).await;
    (home, features, worker.id)
  }

  fn run_record(id: &str, session_id: &str, kind: RunKind, started_at: Option<UtcDateTime>) -> RunRecord {
    RunRecord {
      id: id.to_string(),
      session_id: session_id.to_string(),
      kind,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic-a".to_string()),
      model: Some("model-a".to_string()),
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at,
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

  async fn register_run(store: &Store, run: RunRecord) -> RunRecord {
    let session_id = run.session_id.clone();
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &session_id, run, None).await.unwrap()
  }

  async fn finish_run(store: &Store, session_id: &str, run_id: &str, outcome: RunOutcomeKind, ended_at: UtcDateTime) {
    let guard = store.control_lock().await;
    store
      .record_finish_locked(
        &guard,
        session_id,
        run_id,
        RunOutcome {
          outcome,
          input_event_ids: Vec::new(),
          exit_code: Some(if outcome == RunOutcomeKind::Success { 0 } else { 1 }),
          ended_at: Some(ended_at),
        },
      )
      .await
      .unwrap();
  }

  fn write_run_events(store: &Store, session_id: &str, run_id: &str, events: &[Value]) {
    let path = store.run_dir(session_id, run_id).join("events.jsonl");
    let lines = events.iter().map(|event| serde_json::to_string(event).unwrap()).collect::<Vec<_>>().join("\n");
    fs::write(path, format!("{lines}\n")).unwrap();
  }

  #[tokio::test]
  async fn delivered_summary_has_run_rows_closing_text_and_bootstrap_links() {
    let (_home, features, session_id) = worker_context(&format!("delivered-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    let work_started = UtcDateTime::parse("2026-10-17T09:00:00Z").unwrap();
    let work_ended = UtcDateTime::parse("2026-10-17T09:00:10.900Z").unwrap();
    let review_started = UtcDateTime::parse("2026-10-17T09:00:11Z").unwrap();
    let review_ended = UtcDateTime::parse("2026-10-17T09:00:13Z").unwrap();
    let work = run_record("run-work", &session_id, RunKind::Work, Some(work_started));
    register_run(store, work).await;
    write_run_events(
      store,
      &session_id,
      "run-work",
      &[
        json!({"type": "tool_use", "name": "Bash", "input": {"command": "uv run pytest"}, "timestamp": "tool-time"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "All checks pass."}]}, "timestamp": "answer-time"}),
      ],
    );
    finish_run(store, &session_id, "run-work", RunOutcomeKind::Success, work_ended).await;

    let review = run_record("run-review", &session_id, RunKind::Review, Some(review_started));
    register_run(store, review).await;
    write_run_events(
      store,
      &session_id,
      "run-review",
      &[
        json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/diff"}, "timestamp": "review-tool-time"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Review approved."}]}, "timestamp": "review-answer-time"}),
      ],
    );
    finish_run(store, &session_id, "run-review", RunOutcomeKind::Success, review_ended).await;
    store
      .append_chat_event(
        &session_id,
        json!({
          "type": "task_closed",
          "outcome": "completed",
          "summary": "Delivered result",
          "result_refs": ["run:run-work", "review:run-review", "landed:origin/main@abc123"],
          "timestamp": "2026-10-17T09:00:14Z",
        }),
      )
      .await
      .unwrap();

    let entry = load_worker_transcript(store, &session_id).unwrap();
    let summary = &entry.summary;
    assert_eq!(summary["task_state"], "completed");
    assert_eq!(summary["closed_at"], "2026-10-17T09:00:14Z");
    assert_eq!(summary["landed"], "origin/main@abc123");
    assert_eq!(summary["delivery"]["run_id"], "run-review");
    assert_eq!(summary["active"], Value::Null);
    assert_eq!(summary["cause"], Value::Null);
    assert_eq!(summary["stop"], Value::Null);
    assert_eq!(summary["runs"].as_array().unwrap().len(), 2);
    assert_eq!(summary["runs"][0]["state"], "success");
    assert_eq!(summary["runs"][0]["tool_calls"], 1);
    assert_eq!(summary["runs"][1]["tool_calls"], 1);
    assert_eq!(summary["closing"]["work"], json!({"run_id": "run-work", "text": "All checks pass."}));
    assert_eq!(summary["closing"]["review"], json!({"run_id": "run-review", "text": "Review approved."}));
    assert_eq!(summary["links"]["task_spec_ref"], "");
    assert_eq!(summary["links"]["launch_prompt_ref"], "");
    assert_eq!(summary["delivery"]["raw_log_ref"], "");
    assert!(summary["links"].get("raw_log_ref").is_some());
    assert!(summary["links"].get("events_ref").is_some());

    let messages = entry.projection.committed();
    let headers = messages.iter().filter(|message| message["kind"] == event_types::RUN_HEADER).collect::<Vec<_>>();
    let ends = messages.iter().filter(|message| message["kind"] == event_types::RUN_END).collect::<Vec<_>>();
    let deliveries = messages.iter().filter(|message| message["role"] == event_types::RUN_DELIVERY).collect::<Vec<_>>();
    assert_eq!([headers.len(), ends.len(), deliveries.len()], [2, 2, 1]);
    assert!(headers[0]["content"].as_str().unwrap().ends_with("1 tool call"));
    assert_eq!(ends[0]["thinking_seconds"], 10);
    assert_eq!(ends[1]["thinking_seconds"], 2);
    let run_end_events = entry.events.iter().filter(|event| event["type"] == event_types::RUN_END).collect::<Vec<_>>();
    assert_eq!(run_end_events[0]["timestamp"].to_value(), summary["runs"][0]["ended_at"]);
    assert_eq!(run_end_events[0]["duration_seconds"].to_value(), 10);

    let bootstrap = crate::detail::build_bootstrap_payload(
      store,
      &features.registry,
      &features.config.get().unwrap(),
      &features.store.get_session(&session_id).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(bootstrap["worker_summary"], summary.clone());
  }

  /// The rebuild parses each run log once for its whole byte count: the counter moves by the log
  /// sizes on the first build, by nothing on an unchanged memo hit, and by the grown log only after
  /// an append moves that log's signature.
  #[tokio::test]
  async fn a_transcript_rebuild_parses_each_run_log_once_for_its_bytes() {
    let (_home, features, session_id) = worker_context(&format!("run-bytes-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    let started_at = UtcDateTime::parse("2026-10-17T09:00:00Z").unwrap();
    register_run(store, run_record("run-bytes-a", &session_id, RunKind::Work, Some(started_at))).await;
    register_run(store, run_record("run-bytes-b", &session_id, RunKind::Work, Some(started_at))).await;
    write_run_events(store, &session_id, "run-bytes-a", &[json!({"type": "result", "result": "first"})]);
    write_run_events(store, &session_id, "run-bytes-b", &[json!({"type": "result", "result": "second"})]);
    let log_a = store.run_dir(&session_id, "run-bytes-a").join("events.jsonl");
    let log_b = store.run_dir(&session_id, "run-bytes-b").join("events.jsonl");
    let both = fs::metadata(&log_a).unwrap().len() + fs::metadata(&log_b).unwrap().len();

    let before = store.history_work().run_event_bytes_read;
    load_worker_transcript(store, &session_id).unwrap();
    assert_eq!(store.history_work().run_event_bytes_read - before, both, "the first build parses both logs");

    let before = store.history_work().run_event_bytes_read;
    load_worker_transcript(store, &session_id).unwrap();
    assert_eq!(store.history_work().run_event_bytes_read - before, 0, "an unchanged memo hit parses nothing");

    use std::io::Write as _;
    let mut log = fs::OpenOptions::new().append(true).open(&log_b).unwrap();
    log.write_all(b"{\"type\":\"result\",\"result\":\"appended\"}\n").unwrap();
    log.flush().unwrap();
    let grown = fs::metadata(&log_b).unwrap().len();

    let before = store.history_work().run_event_bytes_read;
    load_worker_transcript(store, &session_id).unwrap();
    let size_a = fs::metadata(&log_a).unwrap().len();
    assert_eq!(
      store.history_work().run_event_bytes_read - before,
      size_a + grown,
      "the rebuild parses every run log once, never twice"
    );
  }

  #[tokio::test]
  async fn worker_transcript_cache_hit_uses_the_sealed_state_without_reading_history() {
    let (_home, features, session_id) = worker_context(&format!("cache-hit-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    let started_at = UtcDateTime::parse("2026-10-17T09:00:00Z").unwrap();
    register_run(store, run_record("run-cache", &session_id, RunKind::Work, Some(started_at))).await;
    write_run_events(
      store,
      &session_id,
      "run-cache",
      &[json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "cached"}]}})],
    );

    let first = load_worker_transcript(store, &session_id).unwrap();
    store.evict_recovery_history(&session_id);
    let file_reads = store.history_work().file_reads;

    let cached = load_worker_transcript(store, &session_id).unwrap();

    assert!(Arc::ptr_eq(&first, &cached), "unchanged run files keep the transcript memo entry");
    assert_eq!(store.history_work().file_reads, file_reads, "a cache hit reads no chat history file");
  }

  #[tokio::test]
  async fn running_summary_tracks_appends_without_moving_the_client_revision() {
    let (_home, features, session_id) = worker_context(&format!("running-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    register_run(store, run_record("run-live", &session_id, RunKind::Work, None)).await;
    let process = charliebot_core::proc_identity::read_pid_stat(std::process::id()).unwrap();
    store.record_launch(&session_id, "run-live", std::process::id(), &process.pid_start).await.unwrap();

    let entry = load_worker_transcript(store, &session_id).unwrap();
    let summary = &entry.summary;
    assert_eq!(summary["task_state"], "open");
    assert_eq!(summary["delivery"], Value::Null);
    assert_eq!(summary["active"]["run_id"], "run-live");
    assert_eq!(summary["active"]["last_text"], Value::Null);
    assert_eq!(summary["active"]["last_tool"], Value::Null);
    assert_eq!(summary["closing"]["work"], json!({"run_id": "run-live", "text": null}));
    assert_eq!(summary["runs"][0]["tool_calls"], 0);
    assert!(!entry.projection.committed().iter().any(|message| message["kind"] == event_types::RUN_END));
    let revision = entry.revision.clone();
    assert_eq!(load_worker_transcript(store, &session_id).unwrap().revision, revision);

    write_run_events(
      store,
      &session_id,
      "run-live",
      &[
        json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/notes.md"}, "timestamp": "tool-time"}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Working through it."}]}, "timestamp": "assistant-time"}),
        json!({"type": "result", "result": "A later result event", "timestamp": "result-time"}),
      ],
    );
    let entry = load_worker_transcript(store, &session_id).unwrap();
    assert_eq!(entry.summary["active"]["tool_calls"], 1);
    assert_eq!(
      entry.summary["active"]["last_tool"],
      json!({
        "name": "Read",
        "input": {"file_path": "/tmp/notes.md"},
        "timestamp": "tool-time",
      })
    );
    assert_eq!(entry.summary["active"]["last_text"]["text"], "Working through it.");
    assert_eq!(entry.summary["active"]["last_text"]["timestamp"], "assistant-time");
    assert_eq!(entry.summary["closing"]["work"]["text"], "A later result event");
    assert_eq!(entry.summary["runs"][0]["tool_calls"], 1);
    assert_eq!(entry.revision, revision);

    finish_run(store, &session_id, "run-live", RunOutcomeKind::Success, UtcDateTime::now()).await;
    let finished = load_worker_transcript(store, &session_id).unwrap();
    assert_ne!(finished.revision, revision);
    let finished_revision = finished.revision.clone();
    write_run_events(
      store,
      &session_id,
      "run-live",
      &[json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Post-finish update."}]}})],
    );
    assert_ne!(load_worker_transcript(store, &session_id).unwrap().revision, finished_revision);
  }

  #[tokio::test]
  async fn failed_summary_selects_the_run_cause_and_latest_stop_request() {
    let (_home, features, session_id) = worker_context(&format!("failed-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    let started = UtcDateTime::now();
    register_run(store, run_record("run-dead", &session_id, RunKind::Work, Some(started))).await;
    write_run_events(
      store,
      &session_id,
      "run-dead",
      &[
        json!({
          "type": "error",
          "content": "peer closed connection\nwhile streaming",
          "message": "peer closed connection\nwhile streaming",
          "timestamp": "cause-time",
        }),
        json!({"type": "error", "content": "newer stderr noise", "timestamp": "noise-time"}),
      ],
    );
    store.request_stop(&session_id, "run-dead", "chat-cancel:synthetic-session").await.unwrap();
    finish_run(store, &session_id, "run-dead", RunOutcomeKind::Failed, UtcDateTime::now()).await;

    let entry = load_worker_transcript(store, &session_id).unwrap();
    let summary = &entry.summary;
    assert_eq!(summary["task_state"], "open");
    assert_eq!(
      summary["cause"],
      json!({
        "run_id": "run-dead",
        "text": "peer closed connection",
        "timestamp": "cause-time",
      })
    );
    assert_eq!(summary["stop"]["run_id"], "run-dead");
    assert_eq!(summary["stop"]["request_id"], "chat-cancel:synthetic-session");
    assert!(!summary["stop"]["timestamp"].as_str().unwrap().is_empty());
    let messages = entry.projection.committed();
    let headers = messages.iter().filter(|message| message["kind"] == event_types::RUN_HEADER).collect::<Vec<_>>();
    let ends = messages.iter().filter(|message| message["kind"] == event_types::RUN_END).collect::<Vec<_>>();
    let run_errors = messages.iter().filter(|message| message["kind"] == event_types::RUN_ERROR).collect::<Vec<_>>();
    assert_eq!([headers.len(), ends.len(), run_errors.len()], [1, 1, 1]);
    assert_eq!(headers[0]["error"], "peer closed connection\nwhile streaming");
    assert_eq!(run_errors[0]["content"], "Error: newer stderr noise");
    assert!(ends[0]["timestamp"].is_string());
    assert!(ends[0]["thinking_seconds"].as_i64().unwrap() >= 0);
  }

  async fn append_task_closed(store: &Store, session_id: &str, timestamp: &str) {
    store
      .append_chat_event(
        session_id,
        json!({
          "type": "task_closed",
          "outcome": "completed",
          "summary": "Delivered result",
          "result_refs": [],
          "timestamp": timestamp,
        }),
      )
      .await
      .unwrap();
  }

  fn delivery_timestamp(entry: &TranscriptEntry) -> String {
    let messages = entry.projection.committed();
    let deliveries = messages.iter().filter(|message| message["role"] == event_types::RUN_DELIVERY).collect::<Vec<_>>();
    assert_eq!(deliveries.len(), 1);
    deliveries[0]["timestamp"].as_str().unwrap().to_string()
  }

  #[tokio::test]
  async fn delivery_card_timestamp_is_the_task_close_time() {
    let (_home, features, session_id) = worker_context(&format!("close-time-{}", uuid::Uuid::new_v4())).await;
    let store = features.store.as_ref();
    register_run(store, run_record("run-work", &session_id, RunKind::Work, None)).await;
    let work_ended = UtcDateTime::parse("2026-10-17T09:00:10Z").unwrap();
    finish_run(store, &session_id, "run-work", RunOutcomeKind::Success, work_ended).await;
    append_task_closed(store, &session_id, "2026-10-17T09:00:14Z").await;

    let entry = load_worker_transcript(store, &session_id).unwrap();
    assert_eq!(delivery_timestamp(&entry), "2026-10-17T09:00:14Z");

    // A fresh build of the same run files, bypassing the memo: the memo is process-wide, and clearing it would
    // drop the entries of the tests that run beside this one.
    let signature = worker_signature(store, &session_id).unwrap();
    let rebuilt = build_worker_transcript(store, &session_id, signature).unwrap();
    assert_eq!(delivery_timestamp(&rebuilt), "2026-10-17T09:00:14Z");

    store
      .append_chat_event(
        &session_id,
        json!({"type": "task_reopened", "reason": "test", "timestamp": "2026-10-17T09:01:00Z"}),
      )
      .await
      .unwrap();
    append_task_closed(store, &session_id, "2026-10-17T09:05:00Z").await;

    // The appended events grow the chat log, so this load rebuilds.
    let entry = load_worker_transcript(store, &session_id).unwrap();
    assert_eq!(delivery_timestamp(&entry), "2026-10-17T09:05:00Z");
  }
}
