//! The persisted run record's types. The store's run-record state holds them, so they sit in a module of their own.

use serde::{Deserialize, Serialize};

use crate::metadata::UtcDateTime;

/// The execution category in a persisted run record.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
  ManagerTurn,
  #[default]
  Work,
  Review,
  Iteration,
  ScheduledStep,
}

/// The sequence that owns a scheduled run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceRef {
  pub kind: String,
  pub owner_ref: String,
  pub position: i64,
}

/// One run's persisted identity and evidence, in model field order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
  pub id: String,
  pub session_id: String,
  #[serde(default)]
  pub kind: RunKind,
  #[serde(default)]
  pub input_event_ids: Vec<String>,
  #[serde(default)]
  pub task_spec_hash: Option<String>,
  #[serde(default)]
  pub task_spec_ref: Option<String>,
  #[serde(default)]
  pub prompt_snapshot_ref: Option<String>,
  #[serde(default)]
  pub retry_of_run_id: Option<String>,
  #[serde(default)]
  pub review_of_run_id: Option<String>,
  #[serde(default)]
  pub backend: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub native_session_id: Option<String>,
  #[serde(default)]
  pub pid: Option<u32>,
  #[serde(default)]
  pub pid_start: Option<String>,
  #[serde(default)]
  pub started_at: Option<UtcDateTime>,
  #[serde(default)]
  pub ended_at: Option<UtcDateTime>,
  #[serde(default)]
  pub exit_code: Option<i32>,
  #[serde(default)]
  pub repo_path: Option<String>,
  #[serde(default)]
  pub base_branch: Option<String>,
  #[serde(default)]
  pub branch_name: Option<String>,
  #[serde(default)]
  pub worktree_path: Option<String>,
  #[serde(default)]
  pub sequence_ref: Option<SequenceRef>,
  #[serde(default)]
  pub raw_log_ref: Option<String>,
  #[serde(default)]
  pub events_ref: Option<String>,
  #[serde(default)]
  pub result_ref: Option<String>,
}
