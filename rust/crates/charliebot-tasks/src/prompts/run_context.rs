//! The historical run context: one run's stored snapshot and its pinned evidence references
//! (`get_run_context` and `_legacy_prompt_payload` in `src/runtime/api/sessions.py:1589-1688`).
//!
//! The read is read-only. A run recorded before the context stage has only a raw launch-text file, which is reported as
//! limited legacy evidence and never recomposed into a snapshot.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use charliebot_store::Store;
use charliebot_store::control_events::sha256_hex;
use serde_json::{Value, json};

use super::{LAUNCH_TEXT_FILENAME, PromptSnapshot, SNAPSHOT_FILENAME};
use crate::spawner_prompt::read_text;

const LEGACY_NOTE: &str = concat!(
  "raw launch text recorded before the context stage: the managed instructions are visible but per-source ",
  "provenance was not recorded, so this is limited evidence, not a full snapshot"
);

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
    .ok_or_else(|| ContextReadError::not_found(format!("run {run_id} not found in task {session_id}")))?;
  let mut snapshot = Value::Null;
  let mut legacy_prompt = Value::Null;
  match run.prompt_snapshot_ref.as_deref().filter(|reference| !reference.is_empty()) {
    Some(reference) => {
      // The recorded artifact names the classification: a snapshot file, or a raw launch-text path from before the
      // context stage.
      let path = PathBuf::from(reference);
      if path.file_name() == Some(OsStr::new(SNAPSHOT_FILENAME)) {
        snapshot = stored_snapshot(&path, reference)?;
      } else {
        if !path.is_file() {
          return Err(ContextReadError::internal(format!("recorded raw launch text missing at {reference}")));
        }
        legacy_prompt = legacy_prompt_payload(&path)?;
      }
    }
    None => {
      let legacy_path = store.run_dir(session_id, run_id).join(LAUNCH_TEXT_FILENAME);
      if legacy_path.is_file() {
        legacy_prompt = legacy_prompt_payload(&legacy_path)?;
      }
    }
  }
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
    "legacy_prompt": legacy_prompt,
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
  let stored: Value = serde_json::from_str(&text).map_err(|error| {
    ContextReadError::internal(format!("stored prompt snapshot unreadable at {reference}: {error}"))
  })?;
  let snapshot =
    PromptSnapshot::from_json_dict(&stored).map_err(|error| ContextReadError::internal(error.to_string()))?;
  Ok(snapshot.to_json_dict())
}

/// The legacy evidence of one raw launch-text file: its path, its sha256, and the note that it is limited evidence.
fn legacy_prompt_payload(path: &Path) -> Result<Value, ContextReadError> {
  let text = read_text(path).map_err(|error| ContextReadError::internal(error.python_message(path)))?;
  Ok(json!({
    "ref": path.display().to_string(),
    "sha256": sha256_hex(&text),
    "note": LEGACY_NOTE,
  }))
}
