//! The cursor stores the last processed raw-log byte offset as a fixed-width decimal.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const CURSOR_FIELD_BYTES: usize = 20;

/// Read a consumed byte offset; a missing or malformed file requests replay from zero.
pub fn read_raw_cursor(path: &Path) -> u64 {
  fs::read_to_string(path).ok().and_then(|contents| contents.trim().parse().ok()).unwrap_or(0)
}

/// Hold one descriptor and rewrite cursor offsets with one positional write.
pub struct RawCursorWriter {
  path: PathBuf,
  file: Option<File>,
}

impl RawCursorWriter {
  /// Create a held-descriptor writer for `path`.
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self { path: path.into(), file: None }
  }

  /// Write a zero-padded decimal at byte offset zero without truncating the file.
  pub fn write(&mut self, offset: u64) -> io::Result<()> {
    if self.file.is_none() {
      if let Some(parent) = self.path.parent() {
        fs::create_dir_all(parent)?;
      }
      self.file = Some(OpenOptions::new().write(true).create(true).truncate(false).open(&self.path)?);
    }
    let data = format!("{offset:0width$}", width = CURSOR_FIELD_BYTES);
    let written = self.file.as_ref().expect("cursor file opened").write_at(data.as_bytes(), 0)?;
    if written != data.len() {
      return Err(io::Error::new(io::ErrorKind::WriteZero, "short cursor pwrite"));
    }
    Ok(())
  }

  /// Close the held cursor descriptor.
  pub fn close(&mut self) -> io::Result<()> {
    let Some(file) = self.file.take() else {
      return Ok(());
    };
    let fd = file.into_raw_fd();
    if unsafe { libc::close(fd) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_store::run_records::CURSOR_NAME;

  #[test]
  fn cursor_wire_format_is_shared_with_python() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(CURSOR_NAME);
    let mut writer = RawCursorWriter::new(&path);
    writer.write(12345).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "00000000000000012345");
    assert_eq!(read_raw_cursor(&path), 12345);
  }

  #[test]
  fn unreadable_cursor_requests_replay() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(read_raw_cursor(&root.path().join(CURSOR_NAME)), 0);
    let path = root.path().join(CURSOR_NAME);
    fs::write(&path, "invalid").unwrap();
    assert_eq!(read_raw_cursor(&path), 0);
  }
}
