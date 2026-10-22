//! The trigger-file walk shared by sidebar probes and trigger readers. Its three stat-keyed memos follow
//! `src/runtime/session_sidebar.py:94-145,193-226`: 1024 parsed files, 1024 directory verdicts, and 4096 walks.

#[cfg(test)]
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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
      let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
          tracing::debug!(path = %path.display(), error = %error, "trigger_meta_read_failed");
          continue;
        }
      };
      let trigger = match serde_json::from_str::<serde_json::Value>(&raw) {
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
}
