//! Persisted run records and their chat-log facts.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use charliebot_core::event_types;
use charliebot_core::fsutil;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::proc_identity;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_core::run_token;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgid};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::compact::CompactValue;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::control_events::{self, ACTOR_SYSTEM};
use crate::event_types::{ChatEvent, SharedChatEvent};
pub use crate::metadata::MetadataReadError;
use crate::metadata::UtcDateTime;
pub use crate::run_facts::RunFacts;
#[cfg(test)]
use crate::run_facts::{CachedRunFacts, RunFactsMemo};
use crate::{ControlGuard, Store, StoreError, StoreResult};

pub use crate::layout::{DATA_DIR_NAME, MASTER_RUNS_DIR_NAME, RUNS_DIR_NAME, THREADS_DIR_NAME};
pub const RUN_METADATA_NAME: &str = METADATA_FILE_NAME;
pub const METADATA_NAME: &str = METADATA_FILE_NAME;
pub const RAW_LOG_NAME: &str = "agent.raw.ndjson";
pub const EVENTS_LOG_NAME: &str = "events.jsonl";
pub const STDERR_LOG_NAME: &str = "agent.stderr.log";
pub const CURSOR_NAME: &str = "agent.raw.cursor";
pub const RUN_EVENTS_NAME: &str = "events.jsonl";
pub const RUN_TASK_SPEC_NAME: &str = "task_spec.md";
pub const STOP_EXIT_POLL: Duration = Duration::from_millis(50);
pub const STOP_EXIT_WAIT: Duration = Duration::from_secs(10);
pub const STOP_KILL_EXIT_WAIT: Duration = Duration::from_secs(5);

const RUN_MODEL_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// Read one run's metadata file as its JSON value: the store's one decode of the file kind's text. A missing file is
/// `Err` with `NotFound`, so the caller decides whether that means no record.
pub fn read_run_metadata(path: &Path) -> Result<Value, MetadataReadError> {
  crate::metadata::read_metadata_value(path)
}

/// Read one run's metadata file as a run record. A missing file is `Err` with `NotFound`, so the caller decides whether
/// that means no record.
pub fn read_run_record_file(path: &Path) -> Result<RunRecord, MetadataReadError> {
  let text = crate::metadata::read_metadata_text(path)?;
  serde_json::from_str(&text).map_err(|error| MetadataReadError::Json(error.to_string()))
}

/// Write one run's metadata file as `record` rendered under `style`, replacing the file. The release helper's seed
/// writes its own check's record this way, as Python's `json.dumps` one-liner; the store's own saves write the model
/// dump through `Store::write_run_record`.
pub fn write_run_metadata(path: &Path, record: &Value, style: &JsonStyle) -> std::io::Result<()> {
  fs::write(path, pyfmt::json_dumps(record, style))
}

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

/// One run's persisted identity and evidence, in Python model field order.
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

/// The record fields a retry caller passes, in Python's keyword order. `input_event_ids` is empty for a worker
/// retry, which consumes whatever input is pending when it launches.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryRunFields {
  pub kind: RunKind,
  pub backend: Option<String>,
  pub model: Option<String>,
  pub repo_path: Option<String>,
  pub base_branch: Option<String>,
  pub branch_name: Option<String>,
  pub worktree_path: Option<String>,
  pub sequence_ref: Option<SequenceRef>,
  pub review_of_run_id: Option<String>,
  pub input_event_ids: Vec<String>,
}

/// The terminal outcome values written to `run_finished`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcomeKind {
  Success,
  Failed,
  Interrupted,
}

impl RunOutcomeKind {
  fn as_str(self) -> &'static str {
    match self {
      Self::Success => "success",
      Self::Failed => "failed",
      Self::Interrupted => "interrupted",
    }
  }
}

/// The terminal result and optional mirror values for one run finish.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
  pub outcome: RunOutcomeKind,
  pub input_event_ids: Vec<String>,
  pub exit_code: Option<i32>,
  pub ended_at: Option<UtcDateTime>,
}

impl RunOutcome {
  /// Build a terminal result with an empty input list and no supplied exit details.
  pub fn new(outcome: RunOutcomeKind) -> Self {
    Self { outcome, input_event_ids: Vec::new(), exit_code: None, ended_at: None }
  }
}

/// A fact-derived run display value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunDisplayState {
  Queued,
  Running,
  Attention,
  Stopped,
  Withheld,
  Success,
  Failed,
  Interrupted,
}

/// The host boot time, read from `/proc/stat` on first use: only a launched run without a terminal fact needs it.
#[derive(Debug, Default)]
pub struct HostBoot(Option<OffsetDateTime>);

impl HostBoot {
  fn get(&mut self) -> StoreResult<OffsetDateTime> {
    if let Some(boot) = self.0 {
      return Ok(boot);
    }
    let boot = proc_identity::read_host_boot_time().map_err(|error| StoreError::new(error.to_string()))?;
    self.0 = Some(boot);
    Ok(boot)
  }

  /// Whether a liveness judgment read `/proc`.
  pub fn was_read(&self) -> bool {
    self.0.is_some()
  }
}

/// Whether `terminal_outcome` maps a `run_finished` outcome to a run outcome.
fn is_known_outcome(value: &str) -> bool {
  matches!(value, "success" | "failed" | "interrupted")
}

impl RunFacts {
  pub(crate) fn observe(&mut self, event: &ChatEvent) {
    #[cfg(test)]
    {
      self.observed_events += 1;
    }
    match event.get("type").and_then(CompactValue::as_str) {
      Some(event_types::RUN_FINISHED) => {
        if let Some(run_id) = event.get("run_id").and_then(CompactValue::as_str) {
          let outcome = event.get("outcome").and_then(CompactValue::as_str).map(str::to_string);
          self.outcomes.insert(run_id.to_string(), outcome);
        }
      }
      Some(event_types::RUN_STOP_REQUESTED) => {
        if let Some(run_id) = event.get("run_id").and_then(CompactValue::as_str) {
          let request_id = event.get("request_id").and_then(CompactValue::as_str).map(str::to_string);
          self.stop_requests.entry(run_id.to_string()).or_default().insert(request_id);
        }
      }
      _ => {}
    }
  }

  /// One run's display state, as [`Store::run_display_state`] derives it from the history these facts fold.
  ///
  /// `withheld` answers whether the history records a `run_launch_withheld` reason for this run. Only a queued run
  /// that no stop request settled asks it, as `runs.run_display_state` asks `withheld_reason` only on that branch;
  /// every settled run answers from the outcome and stop-request maps, so a node's display-state pass never walks
  /// its history once per run.
  pub fn display_state(
    &self,
    run: &RunRecord,
    boot: &mut HostBoot,
    withheld: impl FnOnce() -> bool,
  ) -> StoreResult<RunDisplayState> {
    if let Some(outcome) = self.terminal_outcome(&run.id)? {
      return Ok(match outcome {
        RunOutcomeKind::Success => RunDisplayState::Success,
        RunOutcomeKind::Failed => RunDisplayState::Failed,
        RunOutcomeKind::Interrupted => RunDisplayState::Interrupted,
      });
    }
    if run.pid.is_none() {
      if self.stop_requested(&run.id, None) {
        return Ok(RunDisplayState::Stopped);
      }
      if withheld() {
        return Ok(RunDisplayState::Withheld);
      }
      return Ok(RunDisplayState::Queued);
    }
    let boot = boot.get()?;
    if proc_identity::is_run_alive(run.pid, run.pid_start.as_deref(), run.started_at.map(UtcDateTime::into_inner), boot)
    {
      Ok(RunDisplayState::Running)
    } else {
      Ok(RunDisplayState::Attention)
    }
  }

  /// Whether some run's last `run_finished` fact carries an outcome that [`RunFacts::terminal_outcome`] rejects.
  pub fn has_unknown_outcome(&self) -> bool {
    self.outcomes.values().any(|outcome| outcome.as_deref().is_some_and(|value| !is_known_outcome(value)))
  }

  /// Whether any `run_finished` fact exists for this run, even when it has no outcome value.
  pub fn has_terminal_fact(&self, run_id: &str) -> bool {
    self.outcomes.contains_key(run_id)
  }

  /// The outcome of the last `run_finished` fact for `run_id`.
  pub fn terminal_outcome(&self, run_id: &str) -> StoreResult<Option<RunOutcomeKind>> {
    match self.outcomes.get(run_id).and_then(Option::as_deref) {
      None => Ok(None),
      Some("success") => Ok(Some(RunOutcomeKind::Success)),
      Some("failed") => Ok(Some(RunOutcomeKind::Failed)),
      Some("interrupted") => Ok(Some(RunOutcomeKind::Interrupted)),
      Some(value) => Err(StoreError::new(format!("run {run_id} has unknown terminal outcome {value:?}"))),
    }
  }

  /// Whether a stop fact exists for this run and optional request id.
  pub fn stop_requested(&self, run_id: &str, request_id: Option<&str>) -> bool {
    self.stop_requests.get(run_id).is_some_and(|requests| match request_id {
      None => !requests.is_empty(),
      Some(request_id) => requests.iter().any(|candidate| candidate.as_deref() == Some(request_id)),
    })
  }
}

impl RunDisplayState {
  /// The persisted UI spelling for this state.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Queued => "queued",
      Self::Running => "running",
      Self::Attention => "attention",
      Self::Stopped => "stopped",
      Self::Withheld => "withheld",
      Self::Success => "success",
      Self::Failed => "failed",
      Self::Interrupted => "interrupted",
    }
  }
}

/// A run that blocks a structural task change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunBlocker {
  pub run_id: String,
  pub reason: String,
}

/// One keyset page of run records.
#[derive(Clone, Debug, PartialEq)]
pub struct RunPage {
  pub items: Vec<RunRecord>,
  pub next_cursor: Option<String>,
}

/// The order of a run page. A cursor records the order it was minted under and is refused under the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOrder {
  Ascending,
  Descending,
}

impl RunOrder {
  fn as_str(self) -> &'static str {
    match self {
      Self::Ascending => "ascending",
      Self::Descending => "descending",
    }
  }
}

/// The result of recording or reconciling a stop request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StopResult {
  pub run_id: String,
  pub stop_requested: bool,
  pub outcome: Option<RunOutcomeKind>,
}

/// The result of a new stop request.
pub type StopRequest = StopResult;

/// The boundary of a run page cursor. Python's cursor is `s`, `i`, and `d` (only when descending); unknown keys are
/// ignored, as Python ignores them.
#[derive(Deserialize)]
struct RunCursor {
  s: Value,
  i: String,
  #[serde(default)]
  d: Option<Value>,
}

fn run_sort_key(run: &RunRecord) -> (UtcDateTime, String) {
  let started_at = run
    .started_at
    .unwrap_or_else(|| UtcDateTime::parse("0001-01-01T00:00:00Z").expect("the minimum datetime is valid"));
  (started_at, run.id.clone())
}

fn run_cursor(run: &RunRecord, order: RunOrder) -> String {
  let started_at = run_sort_key(run).0.to_python_isoformat();
  let mut value = Map::new();
  value.insert("s".to_string(), Value::String(started_at));
  value.insert("i".to_string(), Value::String(run.id.clone()));
  if order == RunOrder::Descending {
    value.insert("d".to_string(), Value::Bool(true));
  }
  run_token::b64url_encode(serde_json::to_string(&value).expect("a run cursor serializes").as_bytes())
}

fn python_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(value) => *value,
    Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
    Value::String(value) => !value.is_empty(),
    Value::Array(value) => !value.is_empty(),
    Value::Object(value) => !value.is_empty(),
  }
}

/// Decode a cursor into its boundary and the order it was minted under. The error text is Python's `ValueError`,
/// which does not carry the decoding cause.
fn decode_run_cursor(cursor: &str) -> StoreResult<((UtcDateTime, String), RunOrder)> {
  let malformed = || StoreError::new(format!("malformed run page cursor: {}", pyfmt::str_repr(cursor)));
  let decoded = run_token::b64url_decode(cursor).map_err(|_| malformed())?;
  let value: RunCursor = serde_json::from_slice(&decoded).map_err(|_| malformed())?;
  let started_at = if python_truthy(&value.s) {
    UtcDateTime::parse(value.s.as_str().ok_or_else(malformed)?).map_err(|_| malformed())?
  } else {
    UtcDateTime::parse("0001-01-01T00:00:00Z").expect("the minimum datetime is valid")
  };
  let order = if value.d.as_ref().is_some_and(python_truthy) { RunOrder::Descending } else { RunOrder::Ascending };
  Ok(((started_at, value.i), order))
}

impl Store {
  /// Return the per-session run directory root.
  pub fn runs_root(&self, session_id: &str) -> PathBuf {
    self.session_dir(session_id).join(DATA_DIR_NAME).join(RUNS_DIR_NAME)
  }

  /// Return one run's directory.
  pub fn run_dir(&self, session_id: &str, run_id: &str) -> PathBuf {
    self.runs_root(session_id).join(run_id)
  }

  /// Return one run's metadata path.
  pub fn run_metadata_path(&self, session_id: &str, run_id: &str) -> PathBuf {
    self.run_dir(session_id, run_id).join(RUN_METADATA_NAME)
  }

  /// Return one run's pinned task-spec path.
  pub fn run_task_spec_path(&self, session_id: &str, run_id: &str) -> PathBuf {
    self.run_dir(session_id, run_id).join(RUN_TASK_SPEC_NAME)
  }

  /// Read one run record from disk, or return `None` when its metadata file is absent.
  pub fn read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>> {
    self.run_record_reads.fetch_add(1, Ordering::Relaxed);
    read_run_path(&self.run_metadata_path(session_id, run_id))
  }

  /// Count run-record metadata reads since the store opened.
  pub fn run_record_read_count(&self) -> u64 {
    self.run_record_reads.load(Ordering::Relaxed)
  }

  /// Read all run records for one session in chronological `(started_at, id)` order.
  pub fn list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>> {
    let root = self.runs_root(session_id);
    let mut records = Vec::new();
    for entry in read_directory(&root, true)? {
      if !entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_dir() {
        continue;
      }
      if let Some(record) = self.read_run_sync(session_id, &entry.file_name().to_string_lossy())? {
        records.push(record);
      }
    }
    records.sort_by_key(run_sort_key);
    Ok(records)
  }

  /// Read one node's run records while checking cancellation between directory entries and before sorting.
  pub fn list_run_records_until(
    &self,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Vec<RunRecord>>> {
    let root = self.runs_root(session_id);
    let entries = match fs::read_dir(&root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
      Err(error) => return Err(path_error(&root, error)),
    };
    let mut records = Vec::new();
    for entry in entries {
      if should_stop() {
        return Ok(None);
      }
      let entry = entry.map_err(|error| path_error(&root, error))?;
      if !entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_dir() {
        continue;
      }
      if let Some(record) = self.read_run_sync(session_id, &entry.file_name().to_string_lossy())? {
        records.push(record);
      }
    }
    if should_stop() {
      return Ok(None);
    }
    records.sort_by_key(run_sort_key);
    Ok(Some(records))
  }

  /// Read every run record under the sessions directory, paired with its owner session id.
  pub fn list_all_run_records_sync(&self) -> StoreResult<Vec<(String, RunRecord)>> {
    let mut records = Vec::new();
    for session in read_directory(&self.cfg.sessions_dir(), true)? {
      if !session.file_type().map_err(|error| path_error(&session.path(), error))?.is_dir() {
        continue;
      }
      let session_id = session.file_name().to_string_lossy().into_owned();
      for record in self.list_run_records_sync(&session_id)? {
        records.push((session_id.clone(), record));
      }
    }
    Ok(records)
  }

  /// Return every existing v2 run raw log path under the sessions directory.
  pub fn run_raw_log_paths_sync(&self) -> StoreResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for session in read_directory(&self.cfg.sessions_dir(), true)? {
      if !session.file_type().map_err(|error| path_error(&session.path(), error))?.is_dir() {
        continue;
      }
      let run_root = session.path().join(DATA_DIR_NAME).join(RUNS_DIR_NAME);
      for run in read_directory(&run_root, true)? {
        if !run.file_type().map_err(|error| path_error(&run.path(), error))?.is_dir() {
          continue;
        }
        let path = run.path().join(RAW_LOG_NAME);
        if path.is_file() {
          paths.push(path);
        }
      }
    }
    paths.sort();
    Ok(paths)
  }

  /// Return every existing legacy master-run raw log path.
  pub fn master_run_log_paths_sync(&self) -> StoreResult<Vec<PathBuf>> {
    self.legacy_log_paths(MASTER_RUNS_DIR_NAME, RAW_LOG_NAME)
  }

  /// Return every existing legacy thread event log path.
  pub fn thread_event_log_paths_sync(&self) -> StoreResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for session in read_directory(&self.cfg.sessions_dir(), true)? {
      if !session.file_type().map_err(|error| path_error(&session.path(), error))?.is_dir() {
        continue;
      }
      let threads = session.path().join(THREADS_DIR_NAME);
      for thread in read_directory(&threads, true)? {
        if !thread.file_type().map_err(|error| path_error(&thread.path(), error))?.is_dir() {
          continue;
        }
        let path = thread.path().join(DATA_DIR_NAME).join(EVENTS_LOG_NAME);
        if path.is_file() {
          paths.push(path);
        }
      }
    }
    paths.sort();
    Ok(paths)
  }

  fn legacy_log_paths(&self, container: &str, name: &str) -> StoreResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for session in read_directory(&self.cfg.sessions_dir(), true)? {
      if !session.file_type().map_err(|error| path_error(&session.path(), error))?.is_dir() {
        continue;
      }
      let parent = if container == MASTER_RUNS_DIR_NAME {
        session.path().join(DATA_DIR_NAME).join(container)
      } else {
        session.path().join(container)
      };
      for run in read_directory(&parent, true)? {
        if !run.file_type().map_err(|error| path_error(&run.path(), error))?.is_dir() {
          continue;
        }
        let path = run.path().join(name);
        if path.is_file() {
          paths.push(path);
        }
      }
    }
    paths.sort();
    Ok(paths)
  }

  /// Return the newest run by the canonical `(started_at, id)` order.
  pub fn newest_run_record_sync(&self, session_id: &str) -> StoreResult<Option<RunRecord>> {
    Ok(self.list_run_records_sync(session_id)?.pop())
  }

  /// Return the run whose terminal fact has the latest timestamp, or `None` before any finish.
  pub fn last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>> {
    let events = self.load_all_facts(session_id)?;
    let mut fact_times = HashMap::<String, UtcDateTime>::new();
    for event in &events {
      if event.get("type").and_then(CompactValue::as_str) != Some(event_types::RUN_FINISHED) {
        continue;
      }
      let Some(run_id) = event.get("run_id").and_then(CompactValue::as_str) else {
        continue;
      };
      let timestamp = event
        .get("timestamp")
        .and_then(CompactValue::as_str)
        .ok_or_else(|| StoreError::new("run_finished fact has no string timestamp"))?;
      let parsed = UtcDateTime::parse(timestamp)
        .map_err(|error| StoreError::new(format!("run_finished fact timestamp is invalid: {error}")))?;
      // The last fact per run wins, as Python's dict assignment in last_ended_run_id does.
      fact_times.insert(run_id.to_string(), parsed);
    }
    let runs = self.list_run_records_sync(session_id)?;
    let mut latest: Option<(UtcDateTime, usize, String)> = None;
    for (position, run) in runs.iter().enumerate() {
      let Some(fact_at) = fact_times.get(&run.id) else { continue };
      let key = (run.ended_at.unwrap_or(*fact_at), position, run.id.clone());
      if latest.as_ref().is_none_or(|current| key.0 > current.0 || (key.0 == current.0 && key.1 > current.1)) {
        latest = Some(key);
      }
    }
    Ok(latest.map(|(_, _, run_id)| run_id))
  }

  /// Return one page of records in the canonical `(started_at, id)` order, read forwards or backwards.
  /// A cursor minted under one order is refused under the other.
  pub fn list_runs_page_sync(
    &self,
    session_id: &str,
    cursor: Option<&str>,
    limit: usize,
    order: RunOrder,
  ) -> StoreResult<RunPage> {
    let descending = order == RunOrder::Descending;
    let mut records = self.list_run_records_sync(session_id)?;
    if descending {
      records.reverse();
    }
    // An empty cursor reads as no cursor, as Python's `if cursor` does.
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
      let (boundary, cursor_order) = decode_run_cursor(cursor)?;
      if cursor_order != order {
        return Err(StoreError::new(format!(
          "run page cursor was minted for {} order and cannot be replayed under {} order",
          cursor_order.as_str(),
          order.as_str()
        )));
      }
      if descending {
        records.retain(|run| run_sort_key(run) < boundary);
      } else {
        records.retain(|run| run_sort_key(run) > boundary);
      }
    }
    let has_more = records.len() > limit;
    records.truncate(limit);
    let next_cursor = has_more.then(|| records.last().map(|run| run_cursor(run, order))).flatten();
    Ok(RunPage { items: records, next_cursor })
  }

  /// Derive one run's display state from its record, facts and current process identity.
  ///
  /// A caller that derives the states of several runs reads [`Store::node_state`] once and asks
  /// [`RunFacts::display_state`] for each run, as Python passes one event list to `run_display_state`.
  pub fn run_display_state(&self, session_id: &str, run_id: &str) -> StoreResult<RunDisplayState> {
    let run = require_run(self, session_id, run_id)?;
    self.node_state(session_id)?.display_state(&run, &mut HostBoot::default())
  }

  /// Return every structural-change blocker in chronological run order; an empty list means none.
  /// Callers collect all of them, as Python's `_structural_blockers` does.
  pub fn run_blockers(&self, session_id: &str) -> StoreResult<Vec<RunBlocker>> {
    let mut blockers = Vec::new();
    let runs = self.list_run_records_sync(session_id)?;
    if runs.is_empty() {
      return Ok(blockers);
    }
    let node_state = self.node_state(session_id)?;
    let facts = &node_state.runs;
    let mut boot = HostBoot::default();
    for run in runs {
      if facts.terminal_outcome(&run.id)?.is_some() {
        continue;
      }
      if run.pid.is_none() {
        if facts.stop_requested(&run.id, None) {
          continue;
        }
        blockers.push(RunBlocker { reason: format!("run {} is queued (pending dispatch)", run.id), run_id: run.id });
        continue;
      }
      let boot_time = boot.get()?;
      let reason = if proc_identity::is_run_alive(
        run.pid,
        run.pid_start.as_deref(),
        run.started_at.map(UtcDateTime::into_inner),
        boot_time,
      ) {
        format!("run {} is active", run.id)
      } else {
        format!("run {} has an unresolved process identity (needs recovery)", run.id)
      };
      blockers.push(RunBlocker { reason, run_id: run.id });
    }
    Ok(blockers)
  }

  /// Return the generation after every committed run-record write.
  pub fn records_generation(&self) -> u64 {
    self.records_generation.load(Ordering::Relaxed)
  }

  /// Mark one session's sidebar snapshot dirty.
  pub fn mark_sidebar_dirty(&self, session_id: &str) {
    crate::sidebar_state::mark_sidebar_dirty(session_id);
  }

  /// Set event ids for deterministic cross-language replay tests.
  pub fn set_control_event_ids_for_replay(&self, event_ids: &[&str]) {
    *self.control_event_ids.lock().expect("control event id mutex is not poisoned") =
      Some(event_ids.iter().map(|id| (*id).to_string()).collect());
  }

  fn next_control_event_id(&self) -> StoreResult<String> {
    let mut ids = self.control_event_ids.lock().expect("control event id mutex is not poisoned");
    match ids.as_mut() {
      Some(ids) => ids.pop_front().ok_or_else(|| StoreError::new("control event id replay source is exhausted")),
      None => Ok(Uuid::new_v4().to_string()),
    }
  }

  /// Register a run under the control lock. A `Some` task spec is pinned to `task_spec.md` before the record names it.
  pub async fn register_run_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    mut run: RunRecord,
    task_spec: Option<&str>,
  ) -> StoreResult<RunRecord> {
    self.require_control_guard(guard)?;
    require_session_match(session_id, &run)?;
    if let Some(existing) = self.read_run_sync(session_id, &run.id)? {
      return Ok(existing);
    }
    let run_dir = self.run_dir(session_id, &run.id);
    fs::create_dir_all(&run_dir).map_err(|error| path_error(&run_dir, error))?;
    if let Some(task_spec) = task_spec {
      let spec_path = self.run_task_spec_path(session_id, &run.id);
      fsutil::atomic_write_text(&spec_path, task_spec).map_err(|error| path_error(&spec_path, error))?;
      run.task_spec_ref = Some(spec_path.to_string_lossy().into_owned());
      run.task_spec_hash = Some(format!("{:x}", Sha256::digest(task_spec.as_bytes())));
    }
    self.write_run_record(session_id, &run)?;
    self.mark_sidebar_dirty(session_id);
    Ok(run)
  }

  /// Create or return the stable retry record for one request, bound to the run it retries.
  pub async fn create_retry_run_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    request_id: &str,
    original_run_id: &str,
    task_spec_text: Option<&str>,
    fields: RetryRunFields,
  ) -> StoreResult<RunRecord> {
    let run = RunRecord {
      id: control_events::stable_run_id(session_id, request_id),
      session_id: session_id.to_string(),
      kind: fields.kind,
      input_event_ids: fields.input_event_ids,
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: Some(original_run_id.to_string()),
      review_of_run_id: fields.review_of_run_id,
      backend: fields.backend,
      model: fields.model,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: fields.repo_path,
      base_branch: fields.base_branch,
      branch_name: fields.branch_name,
      worktree_path: fields.worktree_path,
      sequence_ref: fields.sequence_ref,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    self.register_run_locked(guard, session_id, run, task_spec_text).await
  }

  /// Persist the newest process identity unless this run already has a stop fact.
  pub async fn record_launch(
    &self,
    session_id: &str,
    run_id: &str,
    pid: u32,
    pid_start: &str,
  ) -> StoreResult<RunRecord> {
    let guard = self.control_lock().await;
    let mut run = require_run(self, session_id, run_id)?;
    let facts = self.node_state(session_id)?;
    if facts.runs.stop_requested(run_id, None) {
      return Err(StoreError::new(format!("run {run_id} has a stop request; refusing new process")));
    }
    if run.started_at.is_none() {
      run.started_at = Some(UtcDateTime::now());
    }
    run.pid = Some(pid);
    run.pid_start = Some(pid_start.to_string());
    self.write_run_record(session_id, &run)?;
    self.mark_sidebar_dirty(session_id);
    drop(guard);
    Ok(run)
  }

  /// Persist only the named observation fields on a run record.
  pub async fn record_observation(
    &self,
    session_id: &str,
    run_id: &str,
    fields: Map<String, Value>,
  ) -> StoreResult<RunRecord> {
    let _guard = self.control_lock().await;
    let mut run = require_run(self, session_id, run_id)?;
    for (field, value) in fields {
      if !matches!(
        field.as_str(),
        "native_session_id"
          | "model"
          | "raw_log_ref"
          | "events_ref"
          | "result_ref"
          | "prompt_snapshot_ref"
          | "repo_path"
          | "base_branch"
          | "branch_name"
          | "worktree_path"
      ) {
        return Err(StoreError::new(format!("unknown run observation field {field}")));
      }
      if value.is_null() {
        continue;
      }
      let text = value
        .as_str()
        .ok_or_else(|| StoreError::new(format!("run observation field {field} must be a string or null")))?
        .to_string();
      match field.as_str() {
        "native_session_id" => run.native_session_id = Some(text),
        "model" => run.model = Some(text),
        "raw_log_ref" => run.raw_log_ref = Some(text),
        "events_ref" => run.events_ref = Some(text),
        "result_ref" => run.result_ref = Some(text),
        "prompt_snapshot_ref" => run.prompt_snapshot_ref = Some(text),
        "repo_path" => run.repo_path = Some(text),
        "base_branch" => run.base_branch = Some(text),
        "branch_name" => run.branch_name = Some(text),
        "worktree_path" => run.worktree_path = Some(text),
        _ => return Err(StoreError::new(format!("unknown run observation field {field}"))),
      }
    }
    self.write_run_record(session_id, &run)?;
    Ok(run)
  }

  /// Record the first terminal fact and mirror it on the run record.
  pub async fn record_finish_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    outcome: RunOutcome,
  ) -> StoreResult<RunRecord> {
    self.require_control_guard(guard)?;
    let node_state = self.node_state(session_id)?;
    let run_facts = &node_state.runs;
    let mut run = require_run(self, session_id, run_id)?;
    if run_facts.terminal_outcome(run_id)?.is_some() {
      // The outcome is the last fact's and the payload is the first fact's, as Python's `terminal_outcome` and
      // `_fact_input_payload` read them. An empty `ended_at` is the half-written end record: the caller's `ended_at`
      // and `exit_code` fill it, and values already written stay.
      let events = self.load_all_facts_shared_sync(session_id)?;
      let input_event_ids = events
        .iter()
        .find(|event| {
          event.get("type").and_then(CompactValue::as_str) == Some(event_types::RUN_FINISHED)
            && event.get("run_id").and_then(CompactValue::as_str) == Some(run_id)
        })
        .map(|event| event_input_ids(event))
        .unwrap_or_default();
      let mut changed = run.input_event_ids != input_event_ids;
      run.input_event_ids = input_event_ids;
      if run.ended_at.is_none() {
        run.ended_at = Some(outcome.ended_at.unwrap_or_else(UtcDateTime::now));
        if outcome.exit_code.is_some() {
          run.exit_code = outcome.exit_code;
        }
        changed = true;
      }
      if changed {
        self.write_run_record(session_id, &run)?;
      }
      return Ok(run);
    }

    let events = self.load_all_facts_shared_sync(session_id)?;
    let input_event_ids = self.finish_payload(&run, &events, &outcome.input_event_ids, run_facts)?;
    let mut outcome = outcome;
    if run_facts.stop_requested(run_id, None) {
      outcome.outcome = RunOutcomeKind::Interrupted;
    }
    let event_id = self.next_control_event_id()?;
    let event = control_events::build_control_event(
      event_types::RUN_FINISHED,
      ACTOR_SYSTEM,
      session_id,
      Some(&event_id),
      None,
      [
        ("run_id".to_string(), Value::String(run_id.to_string())),
        (event_types::INPUT_EVENT_IDS.to_string(), json!(input_event_ids)),
        ("outcome".to_string(), Value::String(outcome.outcome.as_str().to_string())),
      ],
    );
    self.append_control_fact(guard, session_id, event).await?;
    run.ended_at = Some(outcome.ended_at.unwrap_or_else(UtcDateTime::now));
    run.exit_code = outcome.exit_code;
    run.input_event_ids = input_event_ids;
    self.write_run_record(session_id, &run)?;
    self.mark_sidebar_dirty(session_id);
    Ok(run)
  }

  fn finish_payload(
    &self,
    run: &RunRecord,
    events: &[SharedChatEvent],
    supplied: &[String],
    run_facts: &RunFacts,
  ) -> StoreResult<Vec<String>> {
    if !run.input_event_ids.is_empty() {
      if !supplied.is_empty() && supplied != run.input_event_ids {
        return Err(StoreError::new(format!(
          "run {} finish payload {:?} does not match its registered input batch {:?}",
          run.id, supplied, run.input_event_ids
        )));
      }
      return Ok(run.input_event_ids.clone());
    }
    let known =
      events.iter().filter_map(|event| event.get("id").and_then(CompactValue::as_str)).collect::<HashSet<_>>();
    let unknown = supplied.iter().filter(|event_id| !known.contains(event_id.as_str())).cloned().collect::<Vec<_>>();
    if !unknown.is_empty() {
      return Err(StoreError::new(format!(
        "run {} acknowledges unknown input id(s) {:?}: not events of session {} and not its registered batch",
        run.id, unknown, run.session_id
      )));
    }
    let mut foreign = HashSet::new();
    for event in events {
      let event = event.as_ref();
      if event.get("type").and_then(CompactValue::as_str) != Some(event_types::RUN_FINISHED) {
        continue;
      }
      if event.get("run_id").and_then(CompactValue::as_str).is_none_or(|other_run_id| other_run_id != run.id) {
        foreign.extend(event_input_ids(event));
      }
    }
    for other in self.list_run_records_sync(&run.session_id)? {
      if other.id == run.id || run_facts.terminal_outcome(&other.id)?.is_some() {
        continue;
      }
      if other.pid.is_none() && run_facts.stop_requested(&other.id, None) {
        continue;
      }
      foreign.extend(other.input_event_ids);
    }
    let stolen = supplied.iter().filter(|event_id| foreign.contains(event_id.as_str())).cloned().collect::<Vec<_>>();
    if !stolen.is_empty() {
      return Err(StoreError::new(format!(
        "run {} cannot acknowledge input(s) {:?}: already the acknowledged payload or claimed batch of another run",
        run.id, stolen
      )));
    }
    Ok(supplied.to_vec())
  }

  /// Append input ids to a run record while holding the control lock.
  pub async fn claim_input_events(
    &self,
    session_id: &str,
    run_id: &str,
    event_ids: &[String],
  ) -> StoreResult<RunRecord> {
    let guard = self.control_lock().await;
    let run = self.claim_input_events_locked(&guard, session_id, run_id, event_ids)?;
    drop(guard);
    Ok(run)
  }

  /// Append input ids for a caller that already holds the control lock.
  pub fn claim_input_events_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    event_ids: &[String],
  ) -> StoreResult<RunRecord> {
    self.require_control_guard(guard)?;
    let mut run = require_run(self, session_id, run_id)?;
    run.input_event_ids.extend(event_ids.iter().cloned());
    self.write_run_record(session_id, &run)?;
    Ok(run)
  }

  /// Append one durable stop request before checking or signalling the process.
  pub async fn request_stop(&self, session_id: &str, run_id: &str, reason: &str) -> StoreResult<StopRequest> {
    let run;
    {
      let guard = self.control_lock().await;
      run = require_run(self, session_id, run_id)?;
      let facts = self.node_state(session_id)?;
      if let Some(outcome) = facts.runs.terminal_outcome(run_id)? {
        return Ok(StopResult { run_id: run_id.to_string(), stop_requested: false, outcome: Some(outcome) });
      }
      self.append_stop_request_with_facts_locked(&guard, session_id, run_id, reason, &facts.runs).await?;
    }
    if run.pid.is_none() {
      return Ok(StopResult { run_id: run_id.to_string(), stop_requested: true, outcome: None });
    }
    self.follow_through_stop(session_id, &run).await
  }

  /// Append one durable stop fact while the caller holds the shared control lock.
  pub async fn append_stop_request_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    request_id: &str,
  ) -> StoreResult<bool> {
    self.require_control_guard(guard)?;
    let facts = self.node_state(session_id)?;
    self.append_stop_request_with_facts_locked(guard, session_id, run_id, request_id, &facts.runs).await
  }

  async fn append_stop_request_with_facts_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    request_id: &str,
    facts: &RunFacts,
  ) -> StoreResult<bool> {
    self.require_control_guard(guard)?;
    if facts.terminal_outcome(run_id)?.is_some() {
      return Ok(false);
    }
    if facts.stop_requested(run_id, Some(request_id)) {
      return Ok(true);
    }
    let event_id = self.next_control_event_id()?;
    let event = control_events::build_control_event(
      event_types::RUN_STOP_REQUESTED,
      ACTOR_SYSTEM,
      session_id,
      Some(&event_id),
      None,
      [
        ("request_id".to_string(), Value::String(request_id.to_string())),
        ("run_id".to_string(), Value::String(run_id.to_string())),
      ],
    );
    self.append_control_fact(guard, session_id, event).await?;
    self.mark_sidebar_dirty(session_id);
    Ok(true)
  }

  /// Reconcile a durable stop request after a restart.
  pub async fn reconcile_stop_request(&self, session_id: &str, run_id: &str) -> StoreResult<StopResult> {
    let run = require_run(self, session_id, run_id)?;
    let facts = self.node_state(session_id)?;
    if let Some(outcome) = facts.runs.terminal_outcome(run_id)? {
      return Ok(StopResult { run_id: run_id.to_string(), stop_requested: false, outcome: Some(outcome) });
    }
    if !facts.runs.stop_requested(run_id, None) {
      return Ok(StopResult { run_id: run_id.to_string(), stop_requested: false, outcome: None });
    }
    if run.pid.is_none() {
      return Ok(StopResult { run_id: run_id.to_string(), stop_requested: true, outcome: None });
    }
    self.follow_through_stop(session_id, &run).await
  }

  async fn follow_through_stop(&self, session_id: &str, run: &RunRecord) -> StoreResult<StopResult> {
    let pid = run.pid.expect("a launched run has a pid");
    let raw_pid = i32::try_from(pid).map_err(|error| StoreError::new(error.to_string()))?;
    let process_group = Pid::from_raw(raw_pid);
    let mut exited = !validate_stop_identity(run)?;
    if !exited {
      match killpg(process_group, Signal::SIGTERM) {
        Ok(()) => exited = wait_for_exit(pid, STOP_EXIT_WAIT).await,
        Err(Errno::ESRCH) => exited = !validate_stop_identity(run)?,
        Err(error) => {
          return Err(StoreError::new(format!("signalling run {} with SIGTERM failed: {error}", run.id)));
        }
      }
    }
    if !exited {
      exited = !validate_stop_identity(run)?;
      if !exited {
        match killpg(process_group, Signal::SIGKILL) {
          Ok(()) => exited = wait_for_exit(pid, STOP_KILL_EXIT_WAIT).await,
          Err(Errno::ESRCH) => exited = !validate_stop_identity(run)?,
          Err(error) => {
            return Err(StoreError::new(format!("signalling run {} with SIGKILL failed: {error}", run.id)));
          }
        }
      }
    }
    if exited {
      let guard = self.control_lock().await;
      self.record_finish_locked(&guard, session_id, &run.id, RunOutcome::new(RunOutcomeKind::Interrupted)).await?;
      let facts = self.node_state(session_id)?;
      let outcome = facts.runs.terminal_outcome(&run.id)?;
      return Ok(StopResult { run_id: run.id.clone(), stop_requested: true, outcome });
    }
    Ok(StopResult { run_id: run.id.clone(), stop_requested: true, outcome: None })
  }

  fn write_run_record(&self, session_id: &str, run: &RunRecord) -> StoreResult<()> {
    require_session_match(session_id, run)?;
    let path = self.run_metadata_path(session_id, &run.id);
    #[cfg(any(test, feature = "write-faults"))]
    self.fail_run_record_write(run, &path)?;
    let parent = path.parent().expect("run metadata path has a parent");
    fs::create_dir_all(parent).map_err(|error| path_error(parent, error))?;
    let value = serde_json::to_value(run).map_err(|error| StoreError::new(error.to_string()))?;
    let text = pyfmt::model_json_dumps(&value, &RUN_MODEL_JSON);
    fsutil::atomic_write_text(&path, &text).map_err(|error| path_error(&path, error))?;
    self.records_generation.fetch_add(1, Ordering::Relaxed);
    Ok(())
  }
}

fn read_run_path(path: &Path) -> StoreResult<Option<RunRecord>> {
  match read_run_record_file(path) {
    Ok(record) => Ok(Some(record)),
    Err(MetadataReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(MetadataReadError::Io(error)) => Err(path_error(path, error)),
    Err(error) => Err(StoreError::new(format!("run metadata unreadable at {}: {error}", path.display()))),
  }
}

fn read_directory(path: &Path, missing_is_empty: bool) -> StoreResult<Vec<fs::DirEntry>> {
  let entries = match fs::read_dir(path) {
    Ok(entries) => entries,
    Err(error) if missing_is_empty && error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(path_error(path, error)),
  };
  entries.map(|entry| entry.map_err(|error| path_error(path, error))).collect()
}

fn path_error(path: &Path, error: impl std::fmt::Display) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

fn require_session_match(session_id: &str, run: &RunRecord) -> StoreResult<()> {
  if run.session_id != session_id {
    return Err(StoreError::new(format!(
      "run record session_id {:?} does not match requested session {:?}",
      run.session_id, session_id
    )));
  }
  Ok(())
}

/// The task-tree run-miss sentence (API: 404 detail).
pub fn run_not_found_in_task_text(run_id: &str, session_id: &str) -> String {
  format!("run {run_id} not found in task {session_id}")
}

fn require_run(store: &Store, session_id: &str, run_id: &str) -> StoreResult<RunRecord> {
  store
    .read_run_sync(session_id, run_id)?
    .ok_or_else(|| StoreError::not_found(run_not_found_in_task_text(run_id, session_id)))
}

/// The run's run_finished facts in log order.
fn finish_facts<'a, T: AsRef<ChatEvent>>(
  events: &'a [T],
  run_id: &'a str,
) -> impl DoubleEndedIterator<Item = &'a ChatEvent> {
  events.iter().filter_map(move |event| {
    let event = event.as_ref();
    (event.get("type").and_then(CompactValue::as_str) == Some(event_types::RUN_FINISHED)
      && event.get("run_id").and_then(CompactValue::as_str) == Some(run_id))
    .then_some(event)
  })
}

fn event_input_ids(event: &ChatEvent) -> Vec<String> {
  event
    .get(event_types::INPUT_EVENT_IDS)
    .and_then(CompactValue::as_array)
    .map(|values| values.iter().filter_map(CompactValue::as_str).map(str::to_string).collect())
    .unwrap_or_default()
}

/// The outcome of the run's last run_finished fact. None when the run has no finish fact or its last one carries no
/// outcome; an outcome string outside the known set is an error.
pub fn terminal_outcome<T: AsRef<ChatEvent>>(events: &[T], run_id: &str) -> StoreResult<Option<RunOutcomeKind>> {
  let event = finish_facts(events, run_id).next_back();
  match event.and_then(|event| event.get("outcome")).and_then(CompactValue::as_str) {
    None => Ok(None),
    Some("success") => Ok(Some(RunOutcomeKind::Success)),
    Some("failed") => Ok(Some(RunOutcomeKind::Failed)),
    Some("interrupted") => Ok(Some(RunOutcomeKind::Interrupted)),
    Some(value) => Err(StoreError::new(format!("run {run_id} has unknown terminal outcome {value:?}"))),
  }
}

/// Whether a durable `run_stop_requested` fact exists for the run, and for one request id when one is given. The pure
/// fact scan behind the stop checks of `RunStore.stop_requested` (`runs.py::stop_requested_in_events`); the task tree
/// reads the same facts through it.
pub fn stop_requested_in_events<T: AsRef<ChatEvent>>(events: &[T], run_id: &str, request_id: Option<&str>) -> bool {
  events.iter().any(|event| {
    let event = event.as_ref();
    event.get("type").and_then(CompactValue::as_str) == Some(event_types::RUN_STOP_REQUESTED)
      && event.get("run_id").and_then(CompactValue::as_str) == Some(run_id)
      && request_id.is_none_or(|request_id| event.get("request_id").and_then(CompactValue::as_str) == Some(request_id))
  })
}

fn validate_stop_identity(run: &RunRecord) -> StoreResult<bool> {
  let pid = run.pid.expect("a launched run has a pid");
  let Some(stat) = proc_identity::read_pid_stat(pid) else { return Ok(false) };
  if stat.state == "Z" {
    return Ok(false);
  }
  if run.pid_start.as_deref() != Some(stat.pid_start.as_str()) {
    return Err(StoreError::conflict(vec![format!(
      "run {} process identity mismatch: recorded pid_start {:?}, /proc/{pid} reports {:?}",
      run.id, run.pid_start, stat.pid_start
    )]));
  }
  let started_at = run.started_at.ok_or_else(|| {
    StoreError::conflict(vec![format!("run {} has no usable started_at for identity validation", run.id)])
  })?;
  let boot = proc_identity::read_host_boot_time().map_err(|error| StoreError::new(error.to_string()))?;
  if started_at.into_inner() <= boot {
    return Err(StoreError::conflict(vec![format!(
      "run {} started_at predates the current host boot; recorded identity is stale",
      run.id
    )]));
  }
  let raw_pid = i32::try_from(pid).map_err(|error| StoreError::new(error.to_string()))?;
  let process_group = match getpgid(Some(Pid::from_raw(raw_pid))) {
    Ok(process_group) => process_group,
    Err(Errno::ESRCH) => return Ok(false),
    Err(error) => return Err(StoreError::new(format!("reading run {} process group failed: {error}", run.id))),
  };
  if process_group.as_raw() != raw_pid {
    return Err(StoreError::conflict(vec![format!(
      "run {} process group mismatch: run pid {pid}, process group id {}",
      run.id,
      process_group.as_raw()
    )]));
  }
  Ok(true)
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    if proc_identity::read_pid_stat(pid).is_none_or(|stat| stat.state == "Z") {
      return true;
    }
    tokio::time::sleep(STOP_EXIT_POLL).await;
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A run metadata file holding `text` in a fresh temporary directory.
  fn run_metadata_with(text: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join(RUN_METADATA_NAME);
    std::fs::write(&path, text).expect("write the run metadata file");
    (dir, path)
  }

  #[test]
  fn read_run_metadata_reports_a_missing_file_by_kind_and_a_bad_file_as_a_failure() {
    let (dir, path) = run_metadata_with(br#"{"id": "run-a"}"#);
    let value = read_run_metadata(&path).unwrap();
    assert_eq!(value["id"], "run-a");

    let missing = read_run_metadata(&dir.path().join("absent")).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

    let (_dir, bad) = run_metadata_with(b"{not json");
    let error = read_run_metadata(&bad).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "key must be a string at line 1 column 2");
  }

  #[test]
  fn read_run_record_file_reports_a_missing_file_by_kind_and_decodes_a_record() {
    let (dir, path) = run_metadata_with(br#"{"id": "run-a", "session_id": "session-a"}"#);
    let record = read_run_record_file(&path).unwrap();
    assert_eq!(record.id, "run-a");
    assert_eq!(record.kind, RunKind::Work);

    let missing = read_run_record_file(&dir.path().join("absent")).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

    let (_dir, bad) = run_metadata_with(b"[1]");
    let error = read_run_record_file(&bad).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
  }

  #[test]
  fn write_run_metadata_writes_the_style_the_caller_names() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join(RUN_METADATA_NAME);
    let record = json!({"id": "run-a", "session_id": "s", "kind": "manager_turn"});

    write_run_metadata(&path, &record, &charliebot_core::pyfmt::PYTHON_JSON_DEFAULT).unwrap();
    assert_eq!(
      std::fs::read_to_string(&path).unwrap(),
      "{\"id\": \"run-a\", \"session_id\": \"s\", \"kind\": \"manager_turn\"}"
    );

    write_run_metadata(&path, &record, &RUN_MODEL_JSON).unwrap();
    assert_eq!(
      std::fs::read_to_string(&path).unwrap(),
      "{\n  \"id\": \"run-a\",\n  \"session_id\": \"s\",\n  \"kind\": \"manager_turn\"\n}"
    );
  }

  #[test]
  fn run_facts_retain_one_outcome_and_request_set_per_run() {
    let events = [
      json!({"type": "run_finished", "run_id": "run-a", "outcome": "success"}),
      json!({"type": "run_finished", "run_id": "run-b", "outcome": "future-outcome"}),
      json!({"type": "run_stop_requested", "run_id": "run-a", "request_id": "request-a"}),
      json!({"type": "run_stop_requested", "run_id": "run-a"}),
      json!({"type": "run_launch_withheld", "run_id": "run-b", "reason": "closed task"}),
    ];
    let mut facts = RunFacts::default();
    for event in &events {
      facts.observe(&ChatEvent::from_value(event));
    }

    assert_eq!(facts.outcomes.len(), 2, "one last outcome is retained per run");
    assert_eq!(facts.stop_requests.len(), 1, "one request set is retained per stopped run");
    assert_eq!(facts.stop_requests["run-a"].len(), 2, "the set preserves both request identity forms");
    assert!(facts.has_unknown_outcome());
    assert!(facts.stop_requested("run-a", None));
    assert!(facts.stop_requested("run-a", Some("request-a")));
    assert!(!facts.stop_requested("run-a", Some("other-request")));
  }

  #[test]
  fn run_facts_memo_keeps_the_python_sixty_four_list_bound() {
    let mut memo = RunFactsMemo::default();
    let mut first_identity = None;
    let mut first_events = None;
    let mut newest_identity = None;
    for generation in 1..=65 {
      let identity = Arc::new(());
      let events = Arc::new(vec![Arc::new(json!({"generation": generation}).into())]);
      if generation == 1 {
        first_identity = Some(Arc::clone(&identity));
        first_events = Some(Arc::clone(&events));
      }
      newest_identity = Some(Arc::clone(&identity));
      memo.insert(CachedRunFacts { identity, events, covered: 1, facts: Arc::new(RunFacts::default()) });
    }

    assert_eq!(memo.len(), 64, "the memo retains no more than Python's run-facts bound");
    assert!(memo.get(&first_identity.expect("first list identity was recorded")).is_none());
    assert_eq!(
      Arc::strong_count(&first_events.expect("first event list was recorded")),
      1,
      "evicting a list releases the memo's strong reference"
    );
    assert!(memo.get(&newest_identity.expect("newest list identity was recorded")).is_some());
  }

  #[test]
  fn run_facts_memo_keys_the_list_identity_and_retains_the_current_list() {
    let mut memo = RunFactsMemo::default();
    let identity = Arc::new(());
    let events = Arc::new(vec![Arc::new(json!({"type": "user", "content": "first"}).into())]);
    memo.insert(CachedRunFacts {
      identity: Arc::clone(&identity),
      events: Arc::clone(&events),
      covered: 4,
      facts: Arc::new(RunFacts::default()),
    });
    assert_eq!(Arc::strong_count(&events), 2, "the entry retains the event list like Python's memo");

    let appended =
      Arc::new(vec![events[0].clone(), Arc::new(json!({"type": "agent_message", "content": "second"}).into())]);
    memo.refresh_events(&identity, Arc::clone(&appended));
    let hit = memo.get(&identity).expect("an append preserves the list identity");
    assert_eq!(hit.covered, 4, "the memo keeps its suffix cursor across an append");
    assert!(Arc::ptr_eq(&hit.events, &appended), "the memo retains the appended list contents");

    assert!(memo.get(&Arc::new(())).is_none(), "a replacement list has a different identity");
  }

  #[test]
  fn run_record_rejects_extra_fields_and_keeps_python_field_order() {
    let record = RunRecord {
      id: "run-a".to_string(),
      session_id: "session-a".to_string(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
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
    };
    let value = serde_json::to_value(&record).unwrap();
    let rendered = pyfmt::model_json_dumps(&value, &RUN_MODEL_JSON);
    let keys = value.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
      keys,
      [
        "id",
        "session_id",
        "kind",
        "input_event_ids",
        "task_spec_hash",
        "task_spec_ref",
        "prompt_snapshot_ref",
        "retry_of_run_id",
        "review_of_run_id",
        "backend",
        "model",
        "native_session_id",
        "pid",
        "pid_start",
        "started_at",
        "ended_at",
        "exit_code",
        "repo_path",
        "base_branch",
        "branch_name",
        "worktree_path",
        "sequence_ref",
        "raw_log_ref",
        "events_ref",
        "result_ref",
      ]
    );
    assert!(rendered.starts_with("{\n  \"id\": \"run-a\",\n  \"session_id\": \"session-a\",\n  \"kind\": \"work\""));
    assert!(
      serde_json::from_str::<RunRecord>(
        &rendered.replace("\"result_ref\": null", "\"result_ref\": null,\n  \"extra\": true")
      )
      .is_err()
    );
  }

  #[test]
  fn stable_run_cursor_round_trips_the_python_boundary() {
    let record = RunRecord {
      id: "run-a".to_string(),
      session_id: "session-a".to_string(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: Some(UtcDateTime::parse("2025-02-03T04:05:06.123456Z").unwrap()),
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
    };
    let boundary = (record.started_at.unwrap(), "run-a".to_string());
    let ascending = run_cursor(&record, RunOrder::Ascending);
    assert_eq!(decode_run_cursor(&ascending).unwrap(), (boundary.clone(), RunOrder::Ascending));
    let descending = run_cursor(&record, RunOrder::Descending);
    assert_eq!(decode_run_cursor(&descending).unwrap(), (boundary, RunOrder::Descending));
  }
}
