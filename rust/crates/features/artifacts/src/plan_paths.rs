//! Shared path handling for per-session plan artifacts (`src/features/artifacts/plan_paths.py`).

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use charliebot_core::paths;

/// Python's `Path.exists()`: a missing path, or one whose parent is not a directory, is absent. Any other failure to
/// stat the path is an error.
pub fn exists(path: &Path) -> io::Result<bool> {
  match fs::metadata(path) {
    Ok(_) => Ok(true),
    Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(false),
    Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Ok(false),
    Err(error) => Err(error),
  }
}

/// Resolve a plan artifact and return its in-session relative path, or `None` when it resolves outside the session.
pub fn resolve_plan_file(session_dir: &Path, file: &str) -> (PathBuf, Option<PathBuf>) {
  let resolved_session_dir = paths::realpath(session_dir);
  let candidate = paths::realpath(&resolved_session_dir.join(file));
  let relative = candidate.strip_prefix(&resolved_session_dir).ok().map(Path::to_path_buf);
  (candidate, relative)
}

/// `posixpath.normpath`: drops the `.` and empty parts of a POSIX path and folds each `..` into the part before it. Two
/// leading slashes stay two, and three or more fold to one. The empty path is `.`.
pub fn posix_normpath(path: &str) -> String {
  if path.is_empty() {
    return ".".to_string();
  }
  let initial_slashes = if path.starts_with("//") && !path.starts_with("///") {
    2
  } else if path.starts_with('/') {
    1
  } else {
    0
  };
  let mut parts: Vec<&str> = Vec::new();
  for part in path.split('/') {
    if part.is_empty() || part == "." {
      continue;
    }
    if part != ".." || (initial_slashes == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
      parts.push(part);
    } else {
      parts.pop();
    }
  }
  let normalized = format!("{}{}", "/".repeat(initial_slashes), parts.join("/"));
  if normalized.is_empty() { ".".to_string() } else { normalized }
}

/// A child-relative path for an artifact outside the parent session: the path below the sessions directory minus its
/// session id, or `artifacts/<name>` when the artifact is not under a session.
pub fn fallback_relative_path(session_dir: &Path, candidate: &Path) -> PathBuf {
  let sessions_dir = paths::realpath(session_dir);
  let sessions_dir = sessions_dir.parent().expect("a session directory has a parent");
  if let Ok(under_sessions) = candidate.strip_prefix(sessions_dir) {
    let parts: Vec<Component<'_>> = under_sessions.components().collect();
    if parts.len() > 1 {
      return parts[1..].iter().collect();
    }
  }
  Path::new("artifacts").join(candidate.file_name().unwrap_or_default())
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::symlink;

  use super::*;

  #[test]
  fn posix_normpath_matches_posixpath_normpath() {
    let cases = [
      ("", "."),
      (".", "."),
      ("./a/./b/../c", "a/c"),
      ("/../x", "/x"),
      ("//x", "//x"),
      ("///x", "/x"),
      ("a/../..", ".."),
      ("../a", "../a"),
      ("a//b/", "a/b"),
      ("//", "//"),
      ("/a/b/..", "/a"),
    ];
    for (path, normalized) in cases {
      assert_eq!(posix_normpath(path), normalized, "normpath of {path:?}");
    }
  }

  #[test]
  fn a_symlink_loop_is_absent_like_pathlib_exists() {
    let dir = tempfile::tempdir().expect("a temporary directory is available");
    let loop_path = dir.path().join("loop");
    symlink(&loop_path, &loop_path).expect("the symlink loop is created");
    assert!(!exists(&loop_path).expect("Path.exists treats a symlink loop as absent"));
  }
}
