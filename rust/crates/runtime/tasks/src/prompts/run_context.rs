//! The historical run context: one run's stored snapshot and its pinned evidence references
//! (`get_run_context` in `src/runtime/api/sessions.py`).

use std::fmt;
use std::path::Path;

use charliebot_compat::json_loads;
use charliebot_store::Store;
use charliebot_store::run_records::run_not_found_in_task_text;
use serde_json::{Value, json};

use super::PromptSnapshot;
use crate::spawner_prompt::read_text;

/// A failed context read: the HTTP status the route answers with, and the detail text Python's `HTTPException` carries.
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
      "raw_log_ref": run.raw_log_ref,
      "events_ref": run.events_ref,
      "result_ref": run.result_ref,
    },
  }))
}

/// Reads and validates a stored snapshot file, and returns its canonical payload.
fn stored_snapshot(path: &Path, reference: &str) -> Result<Value, ContextReadError> {
  if !path.is_file() {
    return Err(ContextReadError::internal(format!("stored prompt snapshot missing at {reference}")));
  }
  let text = read_text(path).map_err(|error| {
    ContextReadError::internal(format!(
      "stored prompt snapshot unreadable at {reference}: {}",
      error.python_message(path)
    ))
  })?;
  let stored = json_loads::loads_text(&text).map_err(|error| {
    ContextReadError::internal(format!("stored prompt snapshot unreadable at {reference}: {error}"))
  })?;
  let snapshot =
    PromptSnapshot::from_json_dict(&stored).map_err(|error| ContextReadError::internal(error.to_string()))?;
  Ok(snapshot.to_json_dict())
}
