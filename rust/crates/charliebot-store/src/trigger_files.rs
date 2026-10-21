//! The trigger-file walk shared by sidebar probes and trigger readers.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use crate::metadata::UtcDateTime;

/// Return every regular `*.json` trigger file with the stat from the directory walk, sorted by file name.
///
/// The sort gives an unchanged directory the same order on every poll. A trigger file that vanishes between the
/// directory read and its stat is skipped. Errors opening or scanning the directory are returned to the caller.
pub fn iter_trigger_file_stats(triggers_dir: impl AsRef<Path>) -> std::io::Result<Vec<(PathBuf, Metadata)>> {
  let triggers_dir = triggers_dir.as_ref();
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
  Ok(files)
}

/// Return the pending trigger count and earliest parseable fire time for a trigger directory.
pub fn pending_trigger_state(triggers_dir: impl AsRef<Path>) -> (usize, Option<UtcDateTime>) {
  let triggers_dir = triggers_dir.as_ref();
  let trigger_files = match iter_trigger_file_stats(triggers_dir) {
    Ok(files) => files,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => {
      tracing::debug!(path = %triggers_dir.display(), error = %error, "trigger_directory_read_failed");
      Vec::new()
    }
  };
  let mut pending_count = 0;
  let mut next_trigger = None;
  for (path, _) in trigger_files {
    let raw = match fs::read_to_string(&path) {
      Ok(raw) => raw,
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "trigger_meta_read_failed");
        continue;
      }
    };
    let trigger: serde_json::Value = match serde_json::from_str(&raw) {
      Ok(trigger) => trigger,
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "trigger_meta_read_failed");
        continue;
      }
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
}
