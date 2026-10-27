//! Stale transport files are numbered before a fresh child starts.

use std::fs;
use std::io;
use std::path::Path;

/// Move nonempty raw output and its stderr peer to the first free numbered suffix.
pub fn rotate_stale_transport(
  log_dir: &Path,
  raw_path: &Path,
  stderr_path: &Path,
  cursor_path: &Path,
) -> io::Result<Option<u64>> {
  if !raw_path.exists() || fs::metadata(raw_path)?.len() == 0 {
    return Ok(None);
  }
  let raw_name = raw_path.file_name().expect("raw path has a filename").to_string_lossy();
  let mut suffix = 1u64;
  while log_dir.join(format!("{raw_name}.{suffix}")).exists() {
    suffix = suffix.checked_add(1).expect("transport rotation suffix overflow");
  }
  fs::rename(raw_path, log_dir.join(format!("{raw_name}.{suffix}")))?;
  if stderr_path.exists() {
    let stderr_name = stderr_path.file_name().expect("stderr path has a filename").to_string_lossy();
    fs::rename(stderr_path, log_dir.join(format!("{stderr_name}.{suffix}")))?;
  }
  match fs::remove_file(cursor_path) {
    Ok(()) => {}
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => return Err(error),
  }
  Ok(Some(suffix))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rotates_both_logs_and_unlinks_cursor() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let stderr = root.path().join("agent.stderr.log");
    let cursor = root.path().join("agent.raw.cursor");
    fs::write(&raw, b"raw").unwrap();
    fs::write(&stderr, b"stderr").unwrap();
    fs::write(&cursor, b"00000000000000000003").unwrap();
    fs::write(root.path().join("agent.raw.ndjson.1"), b"previous").unwrap();
    assert_eq!(rotate_stale_transport(root.path(), &raw, &stderr, &cursor).unwrap(), Some(2));
    assert_eq!(fs::read(root.path().join("agent.raw.ndjson.2")).unwrap(), b"raw");
    assert_eq!(fs::read(root.path().join("agent.stderr.log.2")).unwrap(), b"stderr");
    assert!(!cursor.exists());
  }

  #[test]
  fn empty_raw_file_does_not_rotate() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"").unwrap();
    assert_eq!(
      rotate_stale_transport(root.path(), &raw, &root.path().join("agent.stderr.log"), &root.path().join("cursor"))
        .unwrap(),
      None
    );
    assert!(raw.exists());
  }
}
