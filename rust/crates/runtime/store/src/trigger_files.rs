//! The trigger-file walk shared by sidebar probes and trigger readers. Its three stat-keyed memos follow
//! `src/runtime/session_sidebar.py:94-145,193-226`: 1024 parsed files, 1024 directory verdicts, and 4096 walks.
//! This module also owns the trigger record and its file: the record type, the one read and the one write.

#[cfg(test)]
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use charliebot_core::fsutil;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::layout;
use crate::metadata::UtcDateTime;
use crate::read_memos::{BoundedMemo, FileSignature, StatSignatureMemo};

const TRIGGER_META_MEMO_LIMIT: usize = 1024;
const TRIGGER_STATE_VERDICT_LIMIT: usize = 1024;
const TRIGGER_WALK_MEMO_LIMIT: usize = 4096;

type TriggerWalkMemo = StatSignatureMemo<(PathBuf, u32), Arc<Vec<(PathBuf, Metadata)>>>;
type TriggerMetaMemo = StatSignatureMemo<PathBuf, Arc<serde_json::Value>>;
type TriggerStateMemo = BoundedMemo<PathBuf, (FileSignature, usize, Option<UtcDateTime>)>;

static TRIGGER_WALKS: OnceLock<Mutex<TriggerWalkMemo>> = OnceLock::new();
static TRIGGER_META: OnceLock<Mutex<TriggerMetaMemo>> = OnceLock::new();
static TRIGGER_STATES: OnceLock<Mutex<TriggerStateMemo>> = OnceLock::new();

fn trigger_walks() -> &'static Mutex<TriggerWalkMemo> {
  TRIGGER_WALKS.get_or_init(|| Mutex::new(StatSignatureMemo::new(TRIGGER_WALK_MEMO_LIMIT)))
}

fn trigger_meta() -> &'static Mutex<TriggerMetaMemo> {
  TRIGGER_META.get_or_init(|| Mutex::new(StatSignatureMemo::new(TRIGGER_META_MEMO_LIMIT)))
}

fn trigger_states() -> &'static Mutex<TriggerStateMemo> {
  TRIGGER_STATES.get_or_init(|| Mutex::new(BoundedMemo::new(TRIGGER_STATE_VERDICT_LIMIT)))
}

#[cfg(test)]
#[derive(Default)]
struct TriggerWork {
  walks: usize,
  file_reads: usize,
}

#[cfg(test)]
static TRIGGER_WORK: OnceLock<Mutex<HashMap<PathBuf, TriggerWork>>> = OnceLock::new();

#[cfg(test)]
fn record_trigger_work(path: &Path, walk: bool) {
  let mut work = TRIGGER_WORK.get_or_init(Default::default).lock().unwrap();
  let work = work.entry(path.to_path_buf()).or_default();
  if walk {
    work.walks += 1;
  } else {
    work.file_reads += 1;
  }
}

#[cfg(test)]
fn trigger_work(path: &Path) -> (usize, usize) {
  let work = TRIGGER_WORK.get_or_init(Default::default).lock().unwrap();
  work.get(path).map_or((0, 0), |work| (work.walks, work.file_reads))
}

/// Return every regular `*.json` trigger file with the stat from the directory walk, sorted by file name.
///
/// The sort gives an unchanged directory the same order on every poll. A trigger file that vanishes between the
/// directory read and its stat is skipped. Errors opening or scanning the directory are returned to the caller.
pub fn iter_trigger_file_stats(triggers_dir: impl AsRef<Path>) -> std::io::Result<Vec<(PathBuf, Metadata)>> {
  let triggers_dir = triggers_dir.as_ref();
  let dir_stat = fs::metadata(triggers_dir)?;
  iter_trigger_file_stats_at(triggers_dir, &dir_stat).map(|files| files.as_ref().clone())
}

fn iter_trigger_file_stats_at(
  triggers_dir: &Path,
  dir_stat: &Metadata,
) -> std::io::Result<Arc<Vec<(PathBuf, Metadata)>>> {
  let key = (triggers_dir.to_path_buf(), dir_stat.mode());
  let signature = FileSignature::from_metadata(dir_stat);
  if let Some(files) = trigger_walks().lock().expect("trigger walk memo mutex is not poisoned").fresh(&key, signature) {
    return Ok(files);
  }
  let mut entries = fs::read_dir(triggers_dir)?.collect::<std::io::Result<Vec<_>>>()?;
  entries.sort_by_key(fs::DirEntry::file_name);
  let mut files = Vec::new();
  for entry in entries {
    if !entry.file_name().to_string_lossy().ends_with(".json") {
      continue;
    }
    let path = entry.path();
    let metadata = match fs::metadata(&path) {
      Ok(metadata) if metadata.is_file() => metadata,
      Ok(_) => continue,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error),
    };
    files.push((path, metadata));
  }
  #[cfg(test)]
  record_trigger_work(triggers_dir, true);
  let files = Arc::new(files);
  trigger_walks().lock().expect("trigger walk memo mutex is not poisoned").record(key, signature, Arc::clone(&files));
  Ok(files)
}

/// Return the pending trigger count and earliest parseable fire time for a trigger directory.
pub fn pending_trigger_state(triggers_dir: impl AsRef<Path>) -> (usize, Option<UtcDateTime>) {
  let triggers_dir = triggers_dir.as_ref();
  let dir_stat = match fs::metadata(triggers_dir) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (0, None),
    Err(error) => {
      tracing::debug!(path = %triggers_dir.display(), error = %error, "trigger_directory_read_failed");
      return (0, None);
    }
  };
  let dir_signature = FileSignature::from_metadata(&dir_stat);
  let dir_key = triggers_dir.to_path_buf();
  if let Some((signature, count, next)) =
    trigger_states().lock().expect("trigger verdict memo mutex is not poisoned").get(&dir_key)
    && signature == dir_signature
  {
    return (count, next);
  }
  let trigger_files = match iter_trigger_file_stats_at(triggers_dir, &dir_stat) {
    Ok(files) => files,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (0, None),
    Err(error) => {
      tracing::debug!(path = %triggers_dir.display(), error = %error, "trigger_directory_read_failed");
      return (0, None);
    }
  };
  let mut pending_count = 0;
  let mut next_trigger = None;
  for (path, metadata) in trigger_files.iter() {
    let signature = FileSignature::from_metadata(metadata);
    let cached = trigger_meta().lock().expect("trigger metadata memo mutex is not poisoned").fresh(path, signature);
    let trigger = if let Some(cached) = cached {
      cached
    } else {
      #[cfg(test)]
      record_trigger_work(triggers_dir, false);
      let trigger = match read_trigger_value(path) {
        Ok(trigger) => Arc::new(trigger),
        Err(error) => {
          tracing::debug!(path = %path.display(), error = %error, "trigger_meta_read_failed");
          continue;
        }
      };
      trigger_meta().lock().expect("trigger metadata memo mutex is not poisoned").record(
        path.clone(),
        signature,
        Arc::clone(&trigger),
      );
      trigger
    };
    if trigger.get("status").and_then(serde_json::Value::as_str) != Some("pending") {
      continue;
    }
    pending_count += 1;
    if let Some(fire_at) = trigger.get("fire_at").and_then(serde_json::Value::as_str)
      && let Ok(parsed) = UtcDateTime::parse(fire_at)
    {
      next_trigger = Some(next_trigger.map_or(parsed, |current: UtcDateTime| current.min(parsed)));
    }
  }
  trigger_states()
    .lock()
    .expect("trigger verdict memo mutex is not poisoned")
    .store(dir_key, (dir_signature, pending_count, next_trigger));
  (pending_count, next_trigger)
}

/// The trigger file of one trigger id, under the session directory's trigger directory.
pub fn trigger_path(session_dir: &Path, trigger_id: &str) -> PathBuf {
  layout::triggers_dir(session_dir).join(format!("{trigger_id}.json"))
}

/// Why a trigger file could not be read as its document.
#[derive(Debug)]
pub enum TriggerReadError {
  /// The file could not be opened or read. A `NotFound` error means the file is missing.
  Io(io::Error),
  /// The file is not one JSON document.
  Json(serde_json::Error),
}

impl fmt::Display for TriggerReadError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => write!(formatter, "{error}"),
      Self::Json(error) => write!(formatter, "{error}"),
    }
  }
}

impl std::error::Error for TriggerReadError {}

/// The trigger file at `path` as its JSON document: the store's one decode of the raw document, for the readers
/// that check the record's fields one by one. A missing file is `Err` with `NotFound`, so the caller decides
/// whether that means no record, and a document that is not one JSON value is the parse error of the file.
pub fn read_trigger_value(path: &Path) -> Result<Value, TriggerReadError> {
  let text = fs::read_to_string(path).map_err(TriggerReadError::Io)?;
  serde_json::from_str(&text).map_err(TriggerReadError::Json)
}

/// The trigger file at `path` as its trigger record. A missing file fails the read with the io error, and a file
/// that does not decode into a record fails with the parse error, each under the read's or the parse's context.
pub fn read_trigger(path: &Path) -> anyhow::Result<PendingTrigger> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Write one trigger record to its file under the session directory's trigger directory, replacing the file, and
/// mark the session's sidebar stale. Python writes `model_dump_json(indent=2)` as it is, with no trailing newline.
pub fn write_trigger(session_dir: &Path, trigger: &PendingTrigger) -> anyhow::Result<()> {
  let path = trigger_path(session_dir, &trigger.id);
  let parent = path.parent().expect("trigger file path has a parent");
  fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
  let text = serde_json::to_string_pretty(trigger).context("serialize trigger")?;
  fsutil::atomic_write_text(&path, &text).with_context(|| format!("publish {}", path.display()))?;
  crate::sidebar_state::mark_sidebar_dirty(&trigger.session_id);
  Ok(())
}

/// A trigger status written by Python as a string enum.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerStatus {
  #[default]
  Pending,
  Fired,
  Cancelled,
}

/// A tagged watched target. The discriminator and field order match Python's `WatchTarget` models.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum WatchTarget {
  #[serde(rename = "local_pid")]
  LocalPid {
    pid: i64,
    #[serde(default)]
    exit_file: Option<String>,
    #[serde(default)]
    log: Option<String>,
  },
  #[serde(rename = "remote_pid")]
  RemotePid {
    host: String,
    pid: i64,
    #[serde(default)]
    exit_file: Option<String>,
    #[serde(default)]
    log: Option<String>,
  },
  #[serde(rename = "slurm_job")]
  SlurmJob {
    #[serde(default)]
    host: Option<String>,
    job_id: i64,
  },
}

/// One durable pending, fired, or cancelled trigger.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PendingTrigger {
  /// The trigger's UUID and scheduled-input identity.
  #[serde(default = "fresh_uuid")]
  pub id: String,
  /// The session that receives the scheduled input.
  pub session_id: String,
  /// The UTC deadline.
  pub fire_at: UtcDateTime,
  /// The short wake label.
  pub message: String,
  /// The trigger creation time.
  #[serde(default = "UtcDateTime::now")]
  pub created_at: UtcDateTime,
  /// The durable trigger state.
  #[serde(default)]
  pub status: TriggerStatus,
  /// The UTC time the trigger was fired.
  #[serde(default)]
  pub fired_at: Option<UtcDateTime>,
  /// Every process or job the trigger watches.
  #[serde(default)]
  pub watch_targets: Vec<WatchTarget>,
  /// The fire result or delivery refusal.
  #[serde(default)]
  pub fire_reason: Option<String>,
}

/// The trigger UUID of a record that carries none, as the record's model default does.
pub fn fresh_uuid() -> String {
  Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lists_json_files_and_skips_non_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pending.json"), "{} ").unwrap();
    fs::write(root.path().join("ignored.txt"), "{} ").unwrap();
    fs::create_dir(root.path().join("directory.json")).unwrap();

    let files = iter_trigger_file_stats(root.path()).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0.file_name().unwrap(), "pending.json");
    assert!(files[0].1.is_file());
  }

  #[test]
  fn walk_returns_trigger_files_sorted_by_name() {
    // Python: src/runtime/trigger_files.py:24
    let root = tempfile::tempdir().unwrap();
    for name in ["e.json", "b.json", "h.json", "a.json", "g.json", "c.json", "f.json", "d.json"] {
      fs::write(root.path().join(name), "{}").unwrap();
    }

    let walked: Vec<String> = iter_trigger_file_stats(root.path())
      .unwrap()
      .iter()
      .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    assert_eq!(walked, ["a.json", "b.json", "c.json", "d.json", "e.json", "f.json", "g.json", "h.json"]);
  }

  #[test]
  fn directory_scan_errors_are_returned() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    assert_eq!(iter_trigger_file_stats(&missing).unwrap_err().kind(), std::io::ErrorKind::NotFound);
  }

  #[test]
  fn pending_trigger_scan_hits_directory_and_file_signature_memos() {
    let root = tempfile::tempdir().unwrap();
    let triggers = root.path().join("triggers");
    fs::create_dir(&triggers).unwrap();
    let path = triggers.join("pending.json");
    fs::write(&path, r#"{"status":"pending","fire_at":"2026-01-01T00:00:00Z"}"#).unwrap();

    assert_eq!(pending_trigger_state(&triggers).0, 1);
    assert_eq!(trigger_work(&triggers), (1, 1));
    assert_eq!(pending_trigger_state(&triggers).0, 1);
    assert_eq!(trigger_work(&triggers), (1, 1));

    let replacement = triggers.join("replacement.json");
    fs::write(&replacement, r#"{"status":"complete"}"#).unwrap();
    fs::rename(replacement, &path).unwrap();
    assert_eq!(pending_trigger_state(&triggers).0, 0);
    assert_eq!(trigger_work(&triggers), (2, 2));
  }

  /// The raw read: a missing file is `Err` with `NotFound`, and a file that is not one JSON value is the parse
  /// error, so a reader that skips bad files decides from the error.
  #[test]
  fn read_trigger_value_reports_a_missing_file_and_a_bad_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("trigger.json");
    assert!(matches!(read_trigger_value(&path).unwrap_err(), TriggerReadError::Io(error)
      if error.kind() == std::io::ErrorKind::NotFound));
    fs::write(&path, "{\"status\": \"pending\",\n").unwrap();
    assert!(matches!(read_trigger_value(&path).unwrap_err(), TriggerReadError::Json(_)));
  }

  /// The typed read and the write round-trip the record: the write's bytes are the record's pretty serialization,
  /// and the read returns the record.
  #[test]
  fn write_trigger_and_read_trigger_round_trip_the_record() {
    let root = tempfile::tempdir().unwrap();
    let trigger = PendingTrigger {
      id: "4f8a54e4-0996-4f9c-9cfc-61da5cda0413".to_string(),
      session_id: "session".to_string(),
      fire_at: UtcDateTime::parse("2026-10-29T01:01:08.835152Z").unwrap(),
      message: "wake".to_string(),
      created_at: UtcDateTime::parse("2026-10-28T21:01:08.838040Z").unwrap(),
      status: TriggerStatus::Pending,
      fired_at: None,
      watch_targets: vec![WatchTarget::LocalPid { pid: 42, exit_file: Some("exit".to_string()), log: None }],
      fire_reason: None,
    };
    write_trigger(root.path(), &trigger).unwrap();
    let path = trigger_path(root.path(), &trigger.id);
    assert_eq!(fs::read_to_string(&path).unwrap(), serde_json::to_string_pretty(&trigger).unwrap());
    let loaded = read_trigger(&path).unwrap();
    assert_eq!(loaded, trigger);

    let bad = root.path().join("bad.json");
    fs::write(&bad, "{\"status\": \"pending\",\n").unwrap();
    let error = read_trigger(&bad).unwrap_err().to_string();
    assert!(error.starts_with("parse "), "{error}");
    assert!(matches!(read_trigger(&path.with_extension("absent")).unwrap_err().to_string(),
      text if text.starts_with("read ")));
  }
}
