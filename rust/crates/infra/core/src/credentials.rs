//! `credentials.yaml`: the profile's secrets, `section -> key -> scalar` (`src/infra/credentials.py`).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::config::{self, CREDENTIALS_FILENAME, ConfigError};
use crate::paths;

/// One credential value. CPython admits `str` and `int`, and `bool` is an `int` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialValue {
  Text(String),
  Integer(i64),
  Flag(bool),
}

impl CredentialValue {
  /// Python truthiness: `""`, `0` and `False` are unset for every `or ""` reader.
  pub fn is_truthy(&self) -> bool {
    match self {
      CredentialValue::Text(text) => !text.is_empty(),
      CredentialValue::Integer(number) => *number != 0,
      CredentialValue::Flag(flag) => *flag,
    }
  }

  /// CPython `str(value)`.
  pub fn python_str(&self) -> String {
    match self {
      CredentialValue::Text(text) => text.clone(),
      CredentialValue::Integer(number) => number.to_string(),
      CredentialValue::Flag(true) => "True".to_string(),
      CredentialValue::Flag(false) => "False".to_string(),
    }
  }
}

/// One profile's loaded `credentials.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
  /// The file the sections were read from.
  pub path: PathBuf,
  sections: BTreeMap<String, BTreeMap<String, CredentialValue>>,
}

impl Credentials {
  /// The value under `section`/`key`, or `None` when unset.
  pub fn get(&self, section: &str, key: &str) -> Option<&CredentialValue> {
    self.sections.get(section).and_then(|keys| keys.get(key))
  }

  /// `configured_access_key()`: the `charliebot.access_key` credential, or `None` when it is unset or
  /// falsy. `None` means every access-key gate lets unauthenticated readers through, as in Python.
  pub fn access_key(&self) -> Option<String> {
    self.get("charliebot", "access_key").filter(|value| value.is_truthy()).map(CredentialValue::python_str)
  }

  /// `Credentials.require(section, key)`: the value as `str(...)`, or an error naming the key and the file
  /// when it is unset. A falsy value such as `""` or `False` is set, as in Python.
  pub fn require(&self, section: &str, key: &str) -> Result<String, ConfigError> {
    self
      .get(section, key)
      .map(CredentialValue::python_str)
      .ok_or_else(|| ConfigError(format!("credentials.{section}.{key} is not set in {}", paths::display(&self.path))))
  }
}

/// Load `<home>/credentials.yaml`. A missing or empty file loads as no sections; a `null` value counts
/// as unset. Any other shape is an error naming `credentials.<section>[.<key>]` and the file.
pub fn load_credentials(home: &Path) -> Result<Credentials, ConfigError> {
  let path = home.join(CREDENTIALS_FILENAME);
  let path_text = paths::display(&path);
  let document = match config::read_yaml_document(&path)? {
    Value::Null => serde_yaml::Mapping::new(),
    Value::Mapping(mapping) => mapping,
    _ => {
      return Err(ConfigError(format!("credentials must be a mapping of sections: {path_text}")));
    }
  };
  let mut sections = BTreeMap::new();
  for (section, keys) in &document {
    let section_name = key_text(section);
    let Value::Mapping(keys) = keys else {
      return Err(ConfigError(format!("credentials.{section_name} must be a mapping of keys: {path_text}")));
    };
    let mut entry = BTreeMap::new();
    for (key, value) in keys {
      let key_name = key_text(key);
      let parsed = match value {
        Value::Null => continue,
        Value::String(text) => CredentialValue::Text(text.clone()),
        Value::Bool(flag) => CredentialValue::Flag(*flag),
        Value::Number(number) if number.is_i64() => CredentialValue::Integer(number.as_i64().expect("is_i64 checked")),
        _ => {
          return Err(ConfigError(format!(
            "credentials.{section_name}.{key_name} must be a string or integer: {path_text}"
          )));
        }
      };
      entry.insert(key_name, parsed);
    }
    sections.insert(section_name, entry);
  }
  Ok(Credentials { path, sections })
}

/// A mapping key as CPython's `str()` prints the value PyYAML loaded for it.
fn key_text(key: &Value) -> String {
  match key {
    Value::String(text) => text.clone(),
    other => config::yaml_key_repr(other),
  }
}

/// One failed `credentials.yaml` update, named by the phase that failed so callers keep their
/// per-phase error messages.
#[derive(Debug)]
pub enum CredentialsWriteError {
  /// The original file's permissions could not be read.
  OriginalMode(std::io::Error),
  /// The temporary file could not be created next to the original.
  TempFile(std::io::Error),
  /// The temporary file could not be written or renamed over the original.
  Write(std::io::Error),
  /// A failed update's temporary file could not be removed.
  Cleanup(std::io::Error),
}

impl std::fmt::Display for CredentialsWriteError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let (phase, error) = match self {
      CredentialsWriteError::OriginalMode(error) => ("read the original permissions", error),
      CredentialsWriteError::TempFile(error) => ("create the temporary file", error),
      CredentialsWriteError::Write(error) => ("write the temporary file", error),
      CredentialsWriteError::Cleanup(error) => ("remove the temporary file", error),
    };
    write!(formatter, "could not {phase}: {error}")
  }
}

impl std::error::Error for CredentialsWriteError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      CredentialsWriteError::OriginalMode(error)
      | CredentialsWriteError::TempFile(error)
      | CredentialsWriteError::Write(error)
      | CredentialsWriteError::Cleanup(error) => Some(error),
    }
  }
}

/// Replace the file at `path` with `new_bytes` atomically: write a `.credentials.yaml.<random>.tmp`
/// file in the same directory with the original file's mode, then rename it over the original. A
/// failure leaves the original unchanged and removes the temporary file.
pub fn write_credentials_update(path: &Path, new_bytes: &[u8]) -> Result<(), CredentialsWriteError> {
  let original_mode =
    std::fs::metadata(path).map_err(CredentialsWriteError::OriginalMode)?.permissions().mode() & 0o7777;
  let parent = path.parent().expect("a credentials path always names a parent directory");
  let temporary = tempfile::Builder::new()
    .prefix(".credentials.yaml.")
    .suffix(".tmp")
    .rand_bytes(8)
    .tempfile_in(parent)
    .map_err(CredentialsWriteError::TempFile)?;
  let result = write_and_rename(&temporary, path, original_mode, new_bytes);
  if result.is_err() {
    temporary.close().map_err(CredentialsWriteError::Cleanup)?;
  }
  result
}

fn write_and_rename(
  temporary: &tempfile::NamedTempFile,
  path: &Path,
  original_mode: u32,
  new_bytes: &[u8],
) -> Result<(), CredentialsWriteError> {
  temporary
    .as_file()
    .set_permissions(std::fs::Permissions::from_mode(original_mode))
    .map_err(CredentialsWriteError::Write)?;
  temporary.as_file().write_all(new_bytes).map_err(CredentialsWriteError::Write)?;
  std::fs::rename(temporary.path(), path).map_err(CredentialsWriteError::Write)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn load(text: &str) -> Result<Credentials, ConfigError> {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), text).unwrap();
    load_credentials(home.path())
  }

  #[test]
  fn the_access_key_follows_python_truthiness() {
    assert_eq!(load("charliebot:\n  access_key: k-1\n").unwrap().access_key(), Some("k-1".to_string()));
    assert_eq!(load("charliebot:\n  access_key: 42\n").unwrap().access_key(), Some("42".to_string()));
    assert_eq!(load("charliebot:\n  access_key: \"\"\n").unwrap().access_key(), None);
    assert_eq!(load("charliebot:\n  access_key: null\n").unwrap().access_key(), None);
    assert_eq!(load("").unwrap().access_key(), None);
  }

  #[test]
  fn malformed_shapes_name_the_key() {
    let error = load("charliebot:\n  access_key: [1]\n").unwrap_err().to_string();
    assert!(error.starts_with("credentials.charliebot.access_key must be a string or integer: "), "{error}");
    let error = load("charliebot: 3\n").unwrap_err().to_string();
    assert!(error.starts_with("credentials.charliebot must be a mapping of keys: "), "{error}");
    let error = load("- a\n").unwrap_err().to_string();
    assert!(error.starts_with("credentials must be a mapping of sections: "), "{error}");
  }

  #[test]
  fn require_names_a_missing_section_or_key_and_the_file() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), "upstream:\n  api_key: k\n").unwrap();
    let credentials = load_credentials(home.path()).unwrap();
    let path = home.path().join(CREDENTIALS_FILENAME).display().to_string();

    let error = credentials.require("missing", "api_key").unwrap_err().to_string();
    assert_eq!(error, format!("credentials.missing.api_key is not set in {path}"));
    let error = credentials.require("upstream", "token").unwrap_err().to_string();
    assert_eq!(error, format!("credentials.upstream.token is not set in {path}"));
    assert_eq!(credentials.require("upstream", "api_key").unwrap(), "k");
  }

  #[test]
  fn require_returns_a_non_string_value_as_python_str() {
    let credentials = load("upstream:\n  count: 42\n  flag: false\n  empty: \"\"\n").unwrap();
    assert_eq!(credentials.require("upstream", "count").unwrap(), "42");
    assert_eq!(credentials.require("upstream", "flag").unwrap(), "False");
    assert_eq!(credentials.require("upstream", "empty").unwrap(), "");
  }

  #[test]
  fn a_missing_file_loads_as_no_sections() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(load_credentials(home.path()).unwrap().access_key(), None);
  }

  #[test]
  fn the_atomic_write_replaces_the_bytes_and_keeps_the_mode() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CREDENTIALS_FILENAME);
    std::fs::write(&path, b"before\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    write_credentials_update(&path, b"after\n").unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"after\n");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o640);
    assert!(temporary_files(home.path()).is_empty());
  }

  #[test]
  fn a_failed_atomic_write_leaves_the_file_and_cleans_the_temporary_file() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CREDENTIALS_FILENAME);
    // A directory cannot be replaced by a rename, so the write fails after the temporary file exists.
    std::fs::create_dir(&path).unwrap();

    let error = write_credentials_update(&path, b"after\n").unwrap_err();

    assert!(matches!(error, CredentialsWriteError::Write(_)), "{error:?}");
    assert!(path.is_dir());
    assert!(temporary_files(home.path()).is_empty());
  }

  fn temporary_files(home: &Path) -> Vec<String> {
    std::fs::read_dir(home)
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .filter(|name| name.starts_with(".credentials.yaml.") && name.ends_with(".tmp"))
      .collect()
  }
}
