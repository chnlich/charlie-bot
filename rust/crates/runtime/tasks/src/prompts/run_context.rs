//! The historical run context: one run's stored snapshot and its pinned evidence references.

use std::fmt;
use std::path::Path;

use charliebot_core::json_loads;
use charliebot_store::Store;
use charliebot_store::run_logs;
use charliebot_store::run_records::{
  OBSERVATION_EVENTS_REF, OBSERVATION_RAW_LOG_REF, OBSERVATION_RESULT_REF, run_not_found_in_task_text,
};
use serde_json::{Value, json};

use super::PromptSnapshot;
use crate::spawner_prompt::read_text;

/// A failed context read: the HTTP status the route answers with, and its detail text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReadError {
  pub status: u16,
  pub detail: String,
}

impl ContextReadError {
  fn not_found(detail: impl Into<String>) -> Self {
    Self { status: 404, detail: detail.into() }
  }

  fn internal(detail: impl Into<String>) -> Self {
    Self { status: 500, detail: detail.into() }
  }
}

impl fmt::Display for ContextReadError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.detail)
  }
}

impl std::error::Error for ContextReadError {}

/// The historical context payload of one run (`mode: historical`), or the status and detail of why it cannot be read.
pub fn read_run_context(store: &Store, session_id: &str, run_id: &str) -> Result<Value, ContextReadError> {
  let run = store
    .read_run_sync(session_id, run_id)
    .map_err(|error| ContextReadError::internal(error.to_string()))?
    .ok_or_else(|| ContextReadError::not_found(run_not_found_in_task_text(run_id, session_id)))?;
  let snapshot = match run.prompt_snapshot_ref.as_deref().filter(|reference| !reference.is_empty()) {
    Some(reference) => stored_snapshot(Path::new(reference), reference)?,
    None => Value::Null,
  };
  let task_spec = match run.task_spec_ref.as_deref().filter(|reference| !reference.is_empty()) {
    Some(_) => json!({"ref": run.task_spec_ref, "sha256": run.task_spec_hash}),
    None => Value::Null,
  };
  Ok(json!({
    "session_id": session_id,
    "run_id": run_id,
    "kind": run.kind,
    "mode": "historical",
    "snapshot": snapshot,
    "task_spec": task_spec,
    "logs": {
      (OBSERVATION_RAW_LOG_REF): resolved_ref(run.raw_log_ref.as_deref()),
      (OBSERVATION_EVENTS_REF): resolved_ref(run.events_ref.as_deref()),
      (OBSERVATION_RESULT_REF): resolved_ref(run.result_ref.as_deref()),
    },
  }))
}

/// A stored log reference as the file an agent can open: the plain or compressed log the compaction left, or the
/// stored reference itself when neither file exists. A missing log still reads as the path the record names.
fn resolved_ref(reference: Option<&str>) -> Value {
  match reference.filter(|reference| !reference.is_empty()) {
    Some(reference) => match run_logs::existing_path(Path::new(reference)) {
      Some(path) => json!(path.to_string_lossy()),
      None => json!(reference),
    },
    None => Value::Null,
  }
}

/// Reads and validates a stored snapshot file, and returns its canonical payload.
fn stored_snapshot(path: &Path, reference: &str) -> Result<Value, ContextReadError> {
  if !path.is_file() {
    return Err(ContextReadError::internal(format!("stored prompt snapshot missing at {reference}")));
  }
  let text = read_text(path).map_err(|error| {
    ContextReadError::internal(format!("stored prompt snapshot unreadable at {reference}: {}", error.error_text(path)))
  })?;
  let stored = json_loads::loads_text(&text).map_err(|error| {
    ContextReadError::internal(format!("stored prompt snapshot unreadable at {reference}: {error}"))
  })?;
  let snapshot =
    PromptSnapshot::from_json_dict(&stored).map_err(|error| ContextReadError::internal(error.to_string()))?;
  Ok(snapshot.to_json_dict())
}
