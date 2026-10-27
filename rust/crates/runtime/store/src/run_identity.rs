//! Refuse run tokens that do not identify a registered, launched, active run.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::run_records::RunRecord;
use crate::{Store, StoreError, StoreResult};

pub const RUN_IDENTITY_UNKNOWN_DETAIL: &str = "run token does not reference an active run";
pub const RUN_IDENTITY_NOT_LAUNCHED_DETAIL: &str = "run token references a run that has not launched";
pub const SESSION_METADATA_NAME: &str = "metadata.json";

/// The identity fields read from a run record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunIdentity {
  pub id: String,
  pub pid: Option<u32>,
  pub pid_start: Option<String>,
}

/// Return the refusal detail for a run that cannot authorize an agent caller.
pub fn run_identity_refusal(run: Option<&RunRecord>, events: &[Value]) -> Option<&'static str> {
  let Some(run) = run else { return Some(RUN_IDENTITY_UNKNOWN_DETAIL) };
  if events.iter().any(|event| {
    event.get("type").and_then(Value::as_str) == Some("run_finished")
      && event.get("run_id").and_then(Value::as_str) == Some(run.id.as_str())
  }) {
    return Some(RUN_IDENTITY_UNKNOWN_DETAIL);
  }
  if run.pid.is_none() || run.pid_start.is_none() {
    return Some(RUN_IDENTITY_NOT_LAUNCHED_DETAIL);
  }
  None
}

/// Read only `id`, `pid` and `pid_start` from a run record file.
pub fn read_run_identity_sync(path: &Path) -> StoreResult<Option<RunIdentity>> {
  if !path.is_file() {
    return Ok(None);
  }
  let raw = fs::read_to_string(path).map_err(|error| run_metadata_error(path, error))?;
  let record: Value = serde_json::from_str(&raw).map_err(|error| run_metadata_error(path, error))?;
  let id = record.get("id").and_then(Value::as_str).ok_or_else(|| {
    StoreError::new(format!("run metadata unreadable at {}: run id must be a string", path.display()))
  })?;
  let pid = match record.get("pid") {
    None | Some(Value::Null) => None,
    Some(value) => Some(value.as_u64().and_then(|pid| u32::try_from(pid).ok()).ok_or_else(|| {
      StoreError::new(format!("run metadata unreadable at {}: pid must be an unsigned 32-bit integer", path.display()))
    })?),
  };
  let pid_start = match record.get("pid_start") {
    None | Some(Value::Null) => None,
    Some(value) => Some(
      value
        .as_str()
        .ok_or_else(|| {
          StoreError::new(format!("run metadata unreadable at {}: pid_start must be a string", path.display()))
        })?
        .to_string(),
    ),
  };
  Ok(Some(RunIdentity { id: id.to_string(), pid, pid_start }))
}

impl Store {
  /// Return why a run token fails the Python `require_caller` identity check.
  pub fn run_identity_refusal(&self, session_id: &str, run_id: &str) -> StoreResult<Option<&'static str>> {
    let run = self.read_run_sync(session_id, run_id)?;
    let Some(run) = run.as_ref() else {
      return Ok(Some(RUN_IDENTITY_UNKNOWN_DETAIL));
    };
    let facts = self.node_state(session_id)?;
    if facts.runs.has_terminal_fact(&run.id) {
      return Ok(Some(RUN_IDENTITY_UNKNOWN_DETAIL));
    }
    if run.pid.is_none() || run.pid_start.is_none() {
      return Ok(Some(RUN_IDENTITY_NOT_LAUNCHED_DETAIL));
    }
    Ok(None)
  }
}

fn run_metadata_error(path: &Path, error: impl std::fmt::Display) -> StoreError {
  StoreError::new(format!("run metadata unreadable at {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn run(pid: Option<u32>, pid_start: Option<&str>) -> RunRecord {
    RunRecord {
      id: "run-a".to_string(),
      session_id: "session-a".to_string(),
      kind: crate::run_records::RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid,
      pid_start: pid_start.map(str::to_string),
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

  #[test]
  fn identity_refusal_requires_an_unfinished_run_with_a_pinned_pair() {
    assert_eq!(run_identity_refusal(None, &[]), Some(RUN_IDENTITY_UNKNOWN_DETAIL));
    assert_eq!(run_identity_refusal(Some(&run(None, None)), &[]), Some(RUN_IDENTITY_NOT_LAUNCHED_DETAIL));
    assert_eq!(run_identity_refusal(Some(&run(Some(42), None)), &[]), Some(RUN_IDENTITY_NOT_LAUNCHED_DETAIL));
    assert_eq!(run_identity_refusal(Some(&run(Some(42), Some("100"))), &[]), None);
    assert_eq!(
      run_identity_refusal(
        Some(&run(Some(42), Some("100"))),
        &[serde_json::json!({
          "type": "run_finished",
          "run_id": "run-a"
        })]
      ),
      Some(RUN_IDENTITY_UNKNOWN_DETAIL)
    );
  }
}
