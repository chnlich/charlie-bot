//! `credentials.yaml`: the profile's secrets, `section -> key -> scalar` (`src/infra/credentials.py`).

use std::collections::BTreeMap;
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
}
