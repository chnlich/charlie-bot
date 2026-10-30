//! `credentials.yaml`: the profile's secrets, `section -> key -> scalar` (`src/infra/credentials.py`).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
  pub fn value_str(&self) -> String {
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
    self.get("charliebot", "access_key").filter(|value| value.is_truthy()).map(CredentialValue::value_str)
  }

  /// `Credentials.require(section, key)`: the value as `str(...)`, or an error naming the key and the file
  /// when it is unset. A falsy value such as `""` or `False` is set, as in Python.
  pub fn require(&self, section: &str, key: &str) -> Result<String, ConfigError> {
    self
      .get(section, key)
      .map(CredentialValue::value_str)
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

/// Write `bytes` over `<home>/credentials.yaml`: a temporary file in the same directory takes the
/// original mode, then one rename puts it in place.
pub fn write_credentials(home: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
  static COUNTER: AtomicU64 = AtomicU64::new(0);
  let path = home.join(CREDENTIALS_FILENAME);
  let path_text = paths::display(&path);
  let directory = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
  let mode = std::fs::metadata(&path).map(|metadata| metadata.permissions().mode() & 0o7777).unwrap_or(0o600);
  let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
  let temp = directory.join(format!(
    ".{CREDENTIALS_FILENAME}.{}.{stamp}.{}.tmp",
    std::process::id(),
    COUNTER.fetch_add(1, Ordering::Relaxed)
  ));
  let write = || -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&temp, &path)
  };
  write().map_err(|error| {
    let _ = std::fs::remove_file(&temp);
    ConfigError(format!("{path_text}: {error}"))
  })
}

/// The YAML text with `section`'s `key` value lines replaced by `values`, keeping every other byte:
/// a token refresh rewrites only the two value lines it changes.
pub fn with_credential_values(text: &str, section: &str, values: &[(&str, &str)]) -> Result<String, ConfigError> {
  let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
  let Some(start) = section_start(&lines, section) else {
    return Err(ConfigError(format!("credentials.{section} is not set")));
  };
  let end = section_end(&lines, start);
  let mut missing: Vec<(&str, &str)> = Vec::new();
  for (key, value) in values {
    match (start + 1..end).find(|index| is_value_line(&lines[*index], key)) {
      Some(index) => {
        let body = trim_line_end(&lines[index]);
        let indent = &body[..body.len() - body.trim_start_matches([' ', '\t']).len()];
        let newline = &lines[index][body.len()..];
        lines[index] = format!("{indent}{key}: {}{newline}", yaml_scalar(value));
      }
      None => missing.push((key, value)),
    }
  }
  if !missing.is_empty() {
    let mut added: Vec<String> = Vec::new();
    if end == lines.len() && lines.last().is_some_and(|line| !line.ends_with('\n')) {
      added.push("\n".to_string());
    }
    for (key, value) in missing {
      added.push(format!("  {key}: {}\n", yaml_scalar(value)));
    }
    lines.splice(end..end, added);
  }
  Ok(lines.concat())
}

/// The index of the top-level `section:` line.
fn section_start(lines: &[String], section: &str) -> Option<usize> {
  lines.iter().position(|line| {
    let body = trim_line_end(line);
    if body.is_empty() || body.starts_with([' ', '\t', '#']) {
      return false;
    }
    match body.split_once(':') {
      Some((key, rest)) => key == section && (rest.is_empty() || rest.starts_with([' ', '\t', '#'])),
      None => false,
    }
  })
}

/// The first top-level line after the section: the next section header or a top-level key.
fn section_end(lines: &[String], start: usize) -> usize {
  (start + 1..lines.len())
    .find(|index| {
      let body = trim_line_end(&lines[*index]);
      !body.is_empty() && !body.starts_with([' ', '\t', '#'])
    })
    .unwrap_or(lines.len())
}

/// Whether the line sets the indented `key` of its section.
fn is_value_line(line: &str, key: &str) -> bool {
  let body = trim_line_end(line);
  let trimmed = body.trim_start_matches([' ', '\t']);
  if !body.starts_with([' ', '\t']) {
    return false;
  }
  match trimmed.strip_prefix(key) {
    Some(rest) => rest.starts_with(':') && (rest[1..].is_empty() || rest[1..].starts_with([' ', '\t', '#'])),
    None => false,
  }
}

fn trim_line_end(line: &str) -> &str {
  line.trim_end_matches(['\r', '\n'])
}

/// A YAML scalar for one value: the token itself when it is a plain scalar, otherwise a quoted string.
fn yaml_scalar(value: &str) -> String {
  let plain = !value.is_empty()
    && !matches!(value.to_ascii_lowercase().as_str(), "null" | "true" | "false" | "yes" | "no" | "on" | "off")
    && value.chars().all(|character| character.is_ascii_alphanumeric() || "-_.~+/=".contains(character));
  if plain { value.to_string() } else { format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")) }
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
  fn the_access_key_follows_truthiness() {
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
  fn require_returns_a_non_string_value_as_str() {
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

  const TOKENS: &str = "# profile secrets\n\
       charliebot:\n  access_key: k\n\
       feishu:\n  app_id: a\n  app_secret: s\n  user_access_token: old-access\n  refresh_token: old-refresh\n\
       linear:\n  api_key: l\n";

  #[test]
  fn replacing_values_changes_only_those_two_lines() {
    let updated = with_credential_values(
      TOKENS,
      "feishu",
      &[("user_access_token", "new-access"), ("refresh_token", "new-refresh")],
    )
    .unwrap();

    assert_eq!(updated, TOKENS.replace("old-access", "new-access").replace("old-refresh", "new-refresh"));
    assert_eq!(updated.lines().count(), TOKENS.lines().count());
  }

  #[test]
  fn a_missing_value_line_is_added_inside_its_section() {
    let text = "feishu:\n  app_id: a\nlinear:\n  api_key: l\n";
    let updated = with_credential_values(text, "feishu", &[("user_access_token", "new-access")]).unwrap();
    assert_eq!(updated, "feishu:\n  app_id: a\n  user_access_token: new-access\nlinear:\n  api_key: l\n");
  }

  #[test]
  fn a_replaced_value_on_the_last_line_keeps_its_missing_newline() {
    let text = "feishu:\n  user_access_token: old-access";
    let updated = with_credential_values(text, "feishu", &[("user_access_token", "new-access")]).unwrap();
    assert_eq!(updated, "feishu:\n  user_access_token: new-access");
  }

  #[test]
  fn an_unsafe_value_is_written_as_a_quoted_scalar() {
    let home = tempfile::tempdir().unwrap();
    let updated = with_credential_values(TOKENS, "feishu", &[("refresh_token", "two words")]).unwrap();
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), &updated).unwrap();
    assert_eq!(load_credentials(home.path()).unwrap().require("feishu", "refresh_token").unwrap(), "two words");
  }

  #[test]
  fn writing_keeps_the_original_mode_and_leaves_no_temp_file() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(CREDENTIALS_FILENAME);
    std::fs::write(&path, TOKENS).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    write_credentials(home.path(), b"feishu:\n  user_access_token: new-access\n").unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "feishu:\n  user_access_token: new-access\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o640);
    let names: Vec<String> = std::fs::read_dir(home.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(names, vec![CREDENTIALS_FILENAME.to_string()]);
  }
}
