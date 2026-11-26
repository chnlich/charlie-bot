//! Stale transport files are numbered before a fresh child starts.

use std::fs;
use std::io;
use std::path::Path;

use charliebot_store::run_records::{MIDTURN_INPUT_FIFO_NAME, MIDTURN_INPUT_LOG_NAME};

/// Removes `path` if present, propagating any non-NotFound I/O error.
fn remove_if_present(path: &Path) -> io::Result<()> {
  match fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error),
  }
}

/// The nonempty raw output, its stderr peer, and any mid-turn input ledger, moved to the first free numbered suffix.
/// Stale cursors and input pipes from the prior process are always cleared before a fresh child starts.
pub fn rotate_stale_transport(
  log_dir: &Path,
  raw_path: &Path,
  stderr_path: &Path,
  cursor_path: &Path,
) -> io::Result<Option<u64>> {
  let input_log_path = log_dir.join(MIDTURN_INPUT_LOG_NAME);
  let fifo_path = log_dir.join(MIDTURN_INPUT_FIFO_NAME);
  if !raw_path.exists() || fs::metadata(raw_path)?.len() == 0 {
    remove_if_present(&input_log_path)?;
    remove_if_present(&fifo_path)?;
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
  if input_log_path.exists() {
    fs::rename(&input_log_path, log_dir.join(format!("{MIDTURN_INPUT_LOG_NAME}.{suffix}")))?;
  }
  remove_if_present(cursor_path)?;
  remove_if_present(&fifo_path)?;
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
    let input_log = root.path().join(MIDTURN_INPUT_LOG_NAME);
    let fifo = root.path().join(MIDTURN_INPUT_FIFO_NAME);
    fs::write(&raw, b"raw").unwrap();
    fs::write(&stderr, b"stderr").unwrap();
    fs::write(&cursor, b"00000000000000000003").unwrap();
    fs::write(&input_log, b"{\"key\":\"4\",\"inputs\":[]}\n").unwrap();
    fs::write(&fifo, b"").unwrap();
    fs::write(root.path().join("agent.raw.ndjson.1"), b"previous").unwrap();
    assert_eq!(rotate_stale_transport(root.path(), &raw, &stderr, &cursor).unwrap(), Some(2));
    assert_eq!(fs::read(root.path().join("agent.raw.ndjson.2")).unwrap(), b"raw");
    assert_eq!(fs::read(root.path().join("agent.stderr.log.2")).unwrap(), b"stderr");
    assert_eq!(
      fs::read(root.path().join(format!("{MIDTURN_INPUT_LOG_NAME}.2"))).unwrap(),
      b"{\"key\":\"4\",\"inputs\":[]}\n"
    );
    assert!(!cursor.exists());
    assert!(!input_log.exists());
    assert!(!fifo.exists());
  }

  #[test]
  fn empty_raw_file_does_not_rotate() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let input_log = root.path().join(MIDTURN_INPUT_LOG_NAME);
    fs::write(&raw, b"").unwrap();
    fs::write(&input_log, b"{\"key\":\"4\",\"inputs\":[]}\n").unwrap();
    assert_eq!(
      rotate_stale_transport(root.path(), &raw, &root.path().join("agent.stderr.log"), &root.path().join("cursor"))
        .unwrap(),
      None
    );
    assert!(raw.exists());
    assert!(!input_log.exists());
  }
}
