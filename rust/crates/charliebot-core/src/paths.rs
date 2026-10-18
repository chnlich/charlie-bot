//! CPython's path resolution: `os.path.expanduser` and the non-strict `os.path.realpath`.
//!
//! The home resolution, the fence's containment check and the static-file containment check all
//! compare paths these two functions produced on the Python side, so they follow posixpath.py
//! (CPython 3.14) step for step rather than `std::fs::canonicalize`, which fails on a missing tail.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// `os.path.expanduser`: `~` and `~user` prefixes become the home directory; other paths pass through.
///
/// `~` reads `HOME`, falling back to the password database entry of the current uid; `~user` reads
/// that user's entry. An unknown user leaves the path unchanged, as CPython does.
pub fn expanduser(path: &str) -> String {
  if !path.starts_with('~') {
    return path.to_string();
  }
  let end = path[1..].find('/').map_or(path.len(), |offset| offset + 1);
  let home = if end == 1 {
    match std::env::var_os("HOME") {
      Some(home) => Some(home.to_string_lossy().into_owned()),
      None => nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir.to_string_lossy().into_owned()),
    }
  } else {
    nix::unistd::User::from_name(&path[1..end]).ok().flatten().map(|user| user.dir.to_string_lossy().into_owned())
  };
  match home {
    None => path.to_string(),
    Some(home) => {
      let joined = format!("{}{}", home.trim_end_matches('/'), &path[end..]);
      if joined.is_empty() { "/".to_string() } else { joined }
    }
  }
}

/// `os.path.realpath(path, strict=False)`: an absolute path with every symlink resolved.
///
/// A component that does not exist, or that fails to stat, is kept as written and resolution goes on
/// with the components after it. A symlink loop leaves the looping link unresolved.
pub fn realpath(path: &Path) -> PathBuf {
  let filename = path.as_os_str().as_bytes();
  let mut rest: Vec<Option<Vec<u8>>> = filename.split(|b| *b == b'/').rev().map(|part| Some(part.to_vec())).collect();
  let mut part_count = rest.len();
  let mut resolved: Vec<u8> = if filename.starts_with(b"/") {
    b"/".to_vec()
  } else {
    std::env::current_dir().expect("the working directory is readable").into_os_string().into_vec()
  };
  // Symlink path -> fully resolved target; None marks a link whose resolution is in progress.
  let mut seen: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();

  while part_count > 0 {
    let name = match rest.pop().expect("part_count counts the stack's unprocessed parts") {
      None => {
        let link = rest.pop().flatten().expect("a resolution marker follows its link path");
        seen.insert(link, Some(resolved.clone()));
        continue;
      }
      Some(name) => name,
    };
    part_count -= 1;
    if name.is_empty() || name == b"." {
      continue;
    }
    if name == b".." {
      let cut = resolved.iter().rposition(|b| *b == b'/').expect("the resolved path is absolute");
      resolved.truncate(cut);
      if resolved.is_empty() {
        resolved.push(b'/');
      }
      continue;
    }
    let mut candidate = resolved.clone();
    if candidate != b"/" {
      candidate.push(b'/');
    }
    candidate.extend_from_slice(&name);
    let candidate_path = PathBuf::from(OsString::from_vec(candidate.clone()));
    let is_link = match std::fs::symlink_metadata(&candidate_path) {
      Ok(metadata) => metadata.file_type().is_symlink(),
      Err(_) => false,
    };
    if !is_link {
      resolved = candidate;
      continue;
    }
    if let Some(cached) = seen.get(&candidate) {
      match cached {
        Some(target) => resolved = target.clone(),
        None => resolved = candidate,
      }
      continue;
    }
    let target = match std::fs::read_link(&candidate_path) {
      Ok(target) => target.into_os_string().into_vec(),
      Err(_) => {
        resolved = candidate;
        continue;
      }
    };
    if target.starts_with(b"/") {
      resolved = b"/".to_vec();
    }
    seen.insert(candidate.clone(), None);
    rest.push(Some(candidate));
    rest.push(None);
    let target_parts: Vec<Vec<u8>> = target.split(|b| *b == b'/').map(|part| part.to_vec()).collect();
    part_count += target_parts.len();
    rest.extend(target_parts.into_iter().rev().map(Some));
  }
  PathBuf::from(OsString::from_vec(resolved))
}

/// `os.path.commonpath([path, directory]) == directory` (equally `PurePath.is_relative_to`) for two
/// absolute normalized paths: `path` is `directory` or lies under it.
pub fn is_within(path: &Path, directory: &Path) -> bool {
  path.starts_with(directory)
}

/// The text of `path` as CPython's `str(path)` prints it (lossy for non-UTF-8 bytes).
pub fn display(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn realpath_resolves_links_and_keeps_a_missing_tail() {
    let dir = tempfile::tempdir().unwrap();
    let base = realpath(dir.path());
    std::fs::create_dir(base.join("real")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
    assert_eq!(realpath(&base.join("link/missing/x")), base.join("real/missing/x"));
    assert_eq!(realpath(&base.join("real/../link")), base.join("real"));
    std::os::unix::fs::symlink(base.join("loop"), base.join("loop")).unwrap();
    assert_eq!(realpath(&base.join("loop")), base.join("loop"));
  }

  #[test]
  fn expanduser_reads_home_for_a_bare_tilde() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(expanduser("~/x"), format!("{}/x", home.trim_end_matches('/')));
    assert_eq!(expanduser("/abs"), "/abs");
  }
}
