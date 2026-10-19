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
