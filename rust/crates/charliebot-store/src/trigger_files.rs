//! The trigger-file walk shared by sidebar probes and trigger readers.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

/// Return every regular `*.json` trigger file with the stat from the directory walk.
///
/// A trigger file that vanishes between the directory read and its stat is skipped. Errors opening or scanning the
/// directory are returned to the caller.
pub fn iter_trigger_file_stats(triggers_dir: impl AsRef<Path>) -> std::io::Result<Vec<(PathBuf, Metadata)>> {
  let triggers_dir = triggers_dir.as_ref();
  let mut files = Vec::new();
  for entry in fs::read_dir(triggers_dir)? {
    let entry = entry?;
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
  fn directory_scan_errors_are_returned() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    assert_eq!(iter_trigger_file_stats(&missing).unwrap_err().kind(), std::io::ErrorKind::NotFound);
  }
}
