//! The normal form of a POSIX path string, computed without touching the file system.
//!
//! Two path spellings that name the same path compare equal after `normpath`, so a caller that matches a recorded path
//! against a fresh one normalizes both.

/// The normal form of `path`: empty and `.` parts drop, a part before `..` cancels it, and a `..` that would leave a
/// relative path stays. Exactly two leading slashes stay two and any other run folds to one, so an absolute path stays
/// absolute. An empty path normalizes to `.`.
///
/// A caller relies on the leading slashes surviving: an absolute recorded path never equals a relative one.
pub fn normpath(path: &str) -> String {
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

#[cfg(test)]
mod tests {
  use super::normpath;

  #[test]
  fn the_empty_part_and_dot_parts_drop() {
    assert_eq!(normpath(""), ".");
    assert_eq!(normpath("."), ".");
    assert_eq!(normpath("a//b/"), "a/b");
    assert_eq!(normpath("./a/./b/../c"), "a/c");
  }

  #[test]
  fn a_dot_dot_cancels_the_part_before_it_and_stops_at_the_root() {
    assert_eq!(normpath("/a/b/.."), "/a");
    assert_eq!(normpath("/../x"), "/x");
    assert_eq!(normpath("a/../.."), "..");
    assert_eq!(normpath("../a"), "../a");
  }

  #[test]
  fn two_leading_slashes_stay_two_and_a_longer_run_folds_to_one() {
    assert_eq!(normpath("//x"), "//x");
    assert_eq!(normpath("//"), "//");
    assert_eq!(normpath("///x"), "/x");
  }
}
