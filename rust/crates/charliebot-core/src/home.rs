//! The profile home: the one reader of `CHARLIEBOT_HOME` (`src/infra/home.py`).

use std::fmt;
use std::path::PathBuf;

use crate::{paths, pyfmt};

pub const CHARLIEBOT_HOME_ENV: &str = "CHARLIEBOT_HOME";

/// A `CHARLIEBOT_HOME` value that names no usable home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeError {
  raw: String,
}

impl fmt::Display for HomeError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {}", pyfmt::str_repr(&self.raw))
  }
}

impl std::error::Error for HomeError {}

/// The state directory this process belongs to.
///
/// `CHARLIEBOT_HOME` unset or blank selects `~/.charliebot`. A set value must be absolute or start
/// with `~`, and resolves through `expanduser` then `realpath`; a relative value is an error, since
/// each process would resolve it against its own working directory.
pub fn charliebot_home_dir() -> Result<PathBuf, HomeError> {
  // Unset reads as "", the same as Python's `os.environ.get(CHARLIEBOT_HOME_ENV, "")`.
  let raw = std::env::var_os(CHARLIEBOT_HOME_ENV).map(|value| value.to_string_lossy().into_owned());
  resolve_home(raw.as_deref().unwrap_or("").trim())
}

fn resolve_home(raw: &str) -> Result<PathBuf, HomeError> {
  if raw.is_empty() {
    return Ok(PathBuf::from(paths::expanduser("~")).join(".charliebot"));
  }
  if !raw.starts_with('~') && !raw.starts_with('/') {
    return Err(HomeError { raw: raw.to_string() });
  }
  Ok(paths::realpath(&PathBuf::from(paths::expanduser(raw))))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_relative_home_is_refused_with_the_python_message() {
    let error = resolve_home("rel/home").unwrap_err();
    assert_eq!(error.to_string(), "CHARLIEBOT_HOME must be an absolute path or start with '~'; got 'rel/home'");
  }

  #[test]
  fn an_absolute_home_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let home = resolve_home(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(home, paths::realpath(dir.path()));
  }
}
