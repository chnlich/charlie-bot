//! The charlie-bot checkout the server serves, named by the `REPO_ROOT_ENV` environment variable.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub const REPO_ROOT_ENV: &str = "CHARLIEBOT_REPO_ROOT";

/// The charlie-bot checkout whose `web/`, page packages and `configs/` the server serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRoot(PathBuf);

/// A `REPO_ROOT_ENV` value that is missing, empty, or not Unicode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoRootError {
  /// The environment variable is unset.
  Missing,
  /// The environment variable is empty.
  Empty,
  /// The environment variable contains non-Unicode bytes.
  NonUnicode,
}

impl fmt::Display for RepoRootError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let detail = match self {
      RepoRootError::Missing => "is not set; set it to the charlie-bot checkout the server serves",
      RepoRootError::Empty => "must not be empty; set it to the charlie-bot checkout the server serves",
      RepoRootError::NonUnicode => "must be valid Unicode",
    };
    write!(f, "{REPO_ROOT_ENV} {detail}")
  }
}

impl std::error::Error for RepoRootError {}

impl RepoRoot {
  /// Validate the optional operating-system value of `REPO_ROOT_ENV`.
  pub fn new(value: Option<OsString>) -> Result<RepoRoot, RepoRootError> {
    let value = value.ok_or(RepoRootError::Missing)?;
    let value = value.into_string().map_err(|_| RepoRootError::NonUnicode)?;
    if value.is_empty() {
      return Err(RepoRootError::Empty);
    }
    Ok(RepoRoot(PathBuf::from(value)))
  }

  /// Return the validated checkout directory.
  pub fn path(&self) -> &Path {
    &self.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn constructor_requires_a_set_nonempty_unicode_path() {
    assert!(RepoRoot::new(None).unwrap_err().to_string().contains(REPO_ROOT_ENV));
    assert!(RepoRoot::new(Some(OsString::new())).unwrap_err().to_string().contains(REPO_ROOT_ENV));
    assert_eq!(
      RepoRoot::new(Some(OsString::from("/synthetic/checkout"))).unwrap().path(),
      Path::new("/synthetic/checkout")
    );
  }

  #[cfg(unix)]
  #[test]
  fn constructor_rejects_a_non_unicode_path() {
    use std::os::unix::ffi::OsStringExt;

    assert!(RepoRoot::new(Some(OsString::from_vec(vec![0xff]))).unwrap_err().to_string().contains(REPO_ROOT_ENV));
  }
}
