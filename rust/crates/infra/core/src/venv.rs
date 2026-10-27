//! The virtualenv bin directory used by server shims in agent child processes.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub const VENV_BIN_ENV: &str = "CHARLIEBOT_VENV_BIN";

/// The server virtualenv's bin directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenvBin(PathBuf);

/// A `CHARLIEBOT_VENV_BIN` value that is missing, empty, or not Unicode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VenvBinError {
  /// The environment variable is unset.
  Missing,
  /// The environment variable is empty.
  Empty,
  /// The environment variable contains non-Unicode bytes.
  NonUnicode,
}

impl fmt::Display for VenvBinError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let detail = match self {
      VenvBinError::Missing => "is required",
      VenvBinError::Empty => "must not be empty",
      VenvBinError::NonUnicode => "must be valid Unicode",
    };
    write!(f, "{VENV_BIN_ENV} {detail}")
  }
}

impl std::error::Error for VenvBinError {}

impl VenvBin {
  /// Validate the optional operating-system value of `CHARLIEBOT_VENV_BIN`.
  pub fn new(value: Option<OsString>) -> Result<VenvBin, VenvBinError> {
    let value = value.ok_or(VenvBinError::Missing)?;
    let value = value.into_string().map_err(|_| VenvBinError::NonUnicode)?;
    if value.is_empty() {
      return Err(VenvBinError::Empty);
    }
    Ok(VenvBin(PathBuf::from(value)))
  }

  /// Return the validated virtualenv bin directory.
  pub fn path(&self) -> &Path {
    &self.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn constructor_requires_a_set_nonempty_unicode_path() {
    assert!(VenvBin::new(None).unwrap_err().to_string().contains(VENV_BIN_ENV));
    assert!(VenvBin::new(Some(OsString::new())).unwrap_err().to_string().contains(VENV_BIN_ENV));
    assert_eq!(
      VenvBin::new(Some(OsString::from("/synthetic/venv/bin"))).unwrap().path(),
      Path::new("/synthetic/venv/bin")
    );
  }

  #[cfg(unix)]
  #[test]
  fn constructor_rejects_a_non_unicode_path() {
    use std::os::unix::ffi::OsStringExt;

    assert!(VenvBin::new(Some(OsString::from_vec(vec![0xff]))).unwrap_err().to_string().contains(VENV_BIN_ENV));
  }
}
