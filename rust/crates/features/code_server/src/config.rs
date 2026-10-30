//! The `code_server:` config section (`src/features/code_server/config.py`) and the values read from the code-server
//! config file that it names.

use std::io;
use std::path::{Path, PathBuf};
use std::str::Utf8Error;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::paths;
use charliebot_core::text_repr;
use nix::errno::Errno;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

const LEGACY_KEYS: &[(&str, &str)] =
  &[("code_server_bin", "code_server.bin"), ("code_server_config", "code_server.config")];

/// The `code_server:` section: `bin` names the executable (`PATH` is searched when unset), and `config` the code-server
/// config file, relative to the repository.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodeServerConfig {
  #[serde(default)]
  pub bin: Option<String>,
  #[serde(default = "default_config")]
  pub config: String,
}

fn default_config() -> String {
  "configs/code-server.yaml".to_string()
}

pub(crate) fn section() -> ConfigSection {
  ConfigSection { key: "code_server", legacy_keys: LEGACY_KEYS, parse: parse_section, dump: dump_section }
}

/// Render the section as its model dumps it (`charliebot config get code_server`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<CodeServerConfig>().ok_or("the code_server section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: CodeServerConfig =
    serde_yaml::from_value(raw.clone()).map_err(|error| format!("code_server: {error}"))?;
  Ok(Arc::new(settings))
}

/// `code_server_config_path`: `config` with `~` expanded, as given when absolute, else under the repository.
pub(crate) fn config_path(section: &CodeServerConfig, repo_root: &Path) -> PathBuf {
  let path = PathBuf::from(paths::expanduser(&pathlib_text(&section.config)));
  if path.is_absolute() { path } else { repo_root.join(path) }
}

/// `code_server_listen_port`: the integer after the last `:` of the file's `bind-addr`. The error text is the 500's
/// detail. A missing or empty file reads as an empty mapping, so it fails on `bind-addr`.
pub(crate) fn listen_port(config_path: &Path) -> Result<i64, String> {
  let data = read_document(config_path)?;
  let bind_addr = match &data {
    Value::Mapping(map) => map.get("bind-addr"),
    Value::Null => None,
    _ => return Err(format!("code-server config must be a YAML mapping: {}", paths::display(config_path))),
  };
  let missing = || format!("code-server config must define bind-addr: {}", paths::display(config_path));
  let Some(Value::String(bind_addr)) = bind_addr else {
    return Err(missing());
  };
  let Some((_, port_text)) = bind_addr.rsplit_once(':') else {
    return Err(missing());
  };
  python_int(port_text).ok_or_else(|| format!("code-server bind-addr port must be an integer: {bind_addr}"))
}

/// The document at `path`, as `yaml_utils.load_yaml` reads it: `Null` when the file is missing or holds no document.
fn read_document(path: &Path) -> Result<Value, String> {
  if !path.exists() {
    return Ok(Value::Null);
  }
  let bytes = std::fs::read(path).map_err(|error| os_error_text(&error, path))?;
  let text = std::str::from_utf8(&bytes).map_err(|error| decode_error_text(error, &bytes))?;
  serde_yaml::from_str(text).map_err(|error| error.to_string())
}

/// `str(OSError)`: `[Errno 13] Permission denied: '<path>'`.
fn os_error_text(error: &io::Error, path: &Path) -> String {
  let path = text_repr::str_repr(&paths::display(path));
  match error.raw_os_error() {
    Some(code) => format!("[Errno {code}] {}: {path}", Errno::from_raw(code).desc()),
    None => format!("{error}: {path}"),
  }
}

/// `str(UnicodeDecodeError)` for UTF-8: the first byte that cannot start or continue a sequence, and the reason.
fn decode_error_text(error: Utf8Error, bytes: &[u8]) -> String {
  let position = error.valid_up_to();
  let byte = bytes[position];
  let reason = if error.error_len().is_none() {
    "unexpected end of data"
  } else if (0x80..0xC0).contains(&byte) || byte == 0xC0 || byte == 0xC1 || byte >= 0xF5 {
    "invalid start byte"
  } else {
    "invalid continuation byte"
  };
  format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}")
}

/// `int(text)` for a port: surrounding whitespace, an optional sign, and digits with single underscores between them.
/// A value beyond the i64 range reads as not an integer here.
fn python_int(text: &str) -> Option<i64> {
  let text = text.trim();
  let (sign, digits) = match text.strip_prefix('-') {
    Some(rest) => (-1, rest),
    None => (1, text.strip_prefix('+').unwrap_or(text)),
  };
  let valid = digits.starts_with(|c: char| c.is_ascii_digit())
    && digits.ends_with(|c: char| c.is_ascii_digit())
    && !digits.contains("__")
    && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
  if !valid {
    return None;
  }
  let magnitude: i64 = digits.replace('_', "").parse().ok()?;
  Some(sign * magnitude)
}

/// `pathlib.PurePosixPath(text)` as text: repeated slashes and `.` parts drop, and a trailing slash goes. An empty path
/// is `.`.
pub(crate) fn pathlib_text(text: &str) -> String {
  let body: Vec<&str> = text.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
  match (text.starts_with('/'), body.is_empty()) {
    (true, _) => format!("/{}", body.join("/")),
    (false, true) => ".".to_string(),
    (false, false) => body.join("/"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn python_int_reads_what_int_reads_for_a_port() {
    assert_eq!(python_int("8080"), Some(8080));
    assert_eq!(python_int(" 8080 "), Some(8080));
    assert_eq!(python_int("+8080"), Some(8080));
    assert_eq!(python_int("-1"), Some(-1));
    assert_eq!(python_int("80_80"), Some(8080));
    assert_eq!(python_int(""), None);
    assert_eq!(python_int("80__80"), None);
    assert_eq!(python_int("_80"), None);
    assert_eq!(python_int("http"), None);
  }

  #[test]
  fn pathlib_text_drops_dot_parts_and_repeated_slashes() {
    assert_eq!(pathlib_text("./configs//code-server.yaml"), "configs/code-server.yaml");
    assert_eq!(pathlib_text("/abs/"), "/abs");
    assert_eq!(pathlib_text("/"), "/");
    assert_eq!(pathlib_text(""), ".");
    assert_eq!(pathlib_text("./code-server"), "code-server");
  }

  #[test]
  fn a_section_without_keys_takes_the_defaults_and_an_unknown_key_is_refused() {
    let defaults = parse_section(&serde_yaml::Value::Mapping(Default::default())).expect("defaults parse");
    let defaults = defaults.downcast_ref::<CodeServerConfig>().expect("the section holds its type");
    assert_eq!(defaults.bin, None);
    assert_eq!(defaults.config, "configs/code-server.yaml");
    let unknown: serde_yaml::Value = serde_yaml::from_str("bogus: 1").expect("the YAML parses");
    assert!(parse_section(&unknown).is_err());
  }
}
