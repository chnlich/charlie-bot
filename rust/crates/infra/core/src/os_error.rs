//! `str(OSError)`: the text Python prints for a filesystem failure, `[Errno 2] No such file or directory: 'x'`.
//!
//! Rust's `io::Error` carries no filename, though Python's `OSError` prints one. The caller names the file, in the
//! rendering its message shows, and this module builds the rest.

use std::io;

/// The `[Errno N] <strerror>` opening of `error`'s text, without a filename; `None` when the failure carries no
/// errno. A caller appends its own filename rendering to it.
pub fn errno_line_of(error: &io::Error) -> Option<String> {
  let errno = error.raw_os_error()?;
  // Rust's `Display` appends " (os error N)" to the strerror text, so the suffix comes off here.
  let rendered = error.to_string();
  let suffix = format!(" (os error {errno})");
  Some(errno_line(errno, rendered.strip_suffix(&suffix).unwrap_or(&rendered)))
}

/// The `[Errno N] <strerror>` opening of an errno and its strerror text, read without an `io::Error`.
pub fn errno_line(errno: i32, strerror: &str) -> String {
  format!("[Errno {errno}] {strerror}")
}

/// The `[Errno N] <strerror>: <filename>` text of `error`, or `None` when the failure carries no errno.
pub fn errno_text(error: &io::Error, filename: &str) -> Option<String> {
  Some(format!("{}: {filename}", errno_line_of(error)?))
}

/// The `OSError` text of `error` for the file the caller names as `filename`; without an errno, the error's own text
/// with the same filename.
pub fn text(error: &io::Error, filename: &str) -> String {
  errno_text(error, filename).unwrap_or_else(|| format!("{error}: {filename}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_errno_prints_python_oserror_text() {
    let error = io::Error::from_raw_os_error(2);
    assert_eq!(errno_line_of(&error).unwrap(), "[Errno 2] No such file or directory");
    assert_eq!(errno_line(2, "No such file or directory"), "[Errno 2] No such file or directory");
    assert_eq!(errno_text(&error, "'/missing'").unwrap(), "[Errno 2] No such file or directory: '/missing'");
  }

  #[test]
  fn a_failure_without_an_errno_prints_its_own_text() {
    let error = io::Error::new(io::ErrorKind::InvalidData, "bad bytes");
    assert_eq!(errno_text(&error, "'/x'"), None);
    assert_eq!(text(&error, "'/x'"), "bad bytes: '/x'");
  }
}
