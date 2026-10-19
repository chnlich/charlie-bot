//! The Python string and path rules the viewer's messages, cache keys and page paths follow.

use std::io;

use charliebot_core::pyfmt;
use nix::errno::Errno;

/// `str.strip()`: Python's whitespace, which is Unicode whitespace plus the separator controls U+001C to U+001F.
pub fn strip(text: &str) -> &str {
  text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// `text[:count]` in code points, as Python slices a `str`.
pub fn take_chars(text: &str, count: usize) -> &str {
  match text.char_indices().nth(count) {
    Some((end, _)) => &text[..end],
    None => text,
  }
}

/// `str(pathlib.Path(text))` for an absolute POSIX path: empty and `.` components drop out and a trailing slash goes.
/// `..` stays, as pathlib keeps it.
pub fn pathlib_str(text: &str) -> String {
  let parts: Vec<&str> = text.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
  format!("{}{}", leading_slashes(text), parts.join("/"))
}

/// `os.path.normpath` for an absolute POSIX path: `.` and empty components drop out, and each `..` removes the
/// component before it, or nothing at the root. `os.path.abspath` of an absolute path is this.
pub fn normpath(text: &str) -> String {
  let mut parts: Vec<&str> = Vec::new();
  for part in text.split('/') {
    match part {
      "" | "." => {}
      ".." => {
        parts.pop();
      }
      part => parts.push(part),
    }
  }
  format!("{}{}", leading_slashes(text), parts.join("/"))
}

/// The last component of an absolute path, as `os.path.basename` and `pathlib.Path.name` give it.
pub fn basename(text: &str) -> &str {
  text.rsplit('/').next().unwrap_or("")
}

/// `str(OSError)` for a failed call on `path`: `[Errno N] strerror: 'path'`.
pub fn os_error_text(error: &io::Error, path: &str) -> String {
  match error.raw_os_error() {
    Some(number) => format!("[Errno {number}] {}: {}", Errno::from_raw(number).desc(), pyfmt::str_repr(path)),
    None => error.to_string(),
  }
}

/// POSIX keeps exactly two leading slashes and folds any longer run into one.
fn leading_slashes(text: &str) -> &'static str {
  if text.starts_with("//") && !text.starts_with("///") { "//" } else { "/" }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pathlib_and_normpath_fold_the_spellings_python_folds() {
    assert_eq!(pathlib_str("/a//b/./c/"), "/a/b/c");
    assert_eq!(pathlib_str("/a/../b"), "/a/../b");
    assert_eq!(pathlib_str("//srv/x.ncu-rep"), "//srv/x.ncu-rep");
    assert_eq!(normpath("/a/../b/./c//d"), "/b/c/d");
    assert_eq!(normpath("/../x"), "/x");
    assert_eq!(normpath("///x"), "/x");
    assert_eq!(basename("/a/b.ncu-rep"), "b.ncu-rep");
  }

  #[test]
  fn strip_and_take_follow_python_text_rules() {
    assert_eq!(strip("\u{1c} padded \u{1f}\t"), "padded");
    assert_eq!(take_chars("é€x", 2), "é€");
    assert_eq!(take_chars("ab", 5), "ab");
  }
}
