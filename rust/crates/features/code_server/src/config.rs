//! The `code_server:` config section and the values read from the code-server
//! config file that it names.

use std::io;
use std::path::{Path, PathBuf};

use charliebot_core::config::{ConfigSection, SectionModel};
use charliebot_core::int_text;
use charliebot_core::json_loads;
use charliebot_core::paths;
use charliebot_core::repo_root::CONFIGS_DIR_NAME;
use charliebot_core::text_repr;
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
  format!("{CONFIGS_DIR_NAME}/code-server.yaml")
}

impl SectionModel for CodeServerConfig {
  const KEY: &'static str = "code_server";
}

pub(crate) fn section() -> ConfigSection {
  ConfigSection::of::<CodeServerConfig>(LEGACY_KEYS)
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
  int_text::int_value(port_text).ok_or_else(|| format!("code-server bind-addr port must be an integer: {bind_addr}"))
}

/// The document at `path`: `Null` when the file is missing or holds no document.
fn read_document(path: &Path) -> Result<Value, String> {
  if !path.exists() {
    return Ok(Value::Null);
  }
  let bytes = std::fs::read(path).map_err(|error| os_error_text(&error, path))?;
  let text = std::str::from_utf8(&bytes).map_err(|error| json_loads::utf8_error_text(&bytes, &error))?;
  serde_yaml::from_str(text).map_err(|error| error.to_string())
}

/// The failure text: `[Errno 13] Permission denied: '<path>'`.
fn os_error_text(error: &io::Error, path: &Path) -> String {
  charliebot_core::os_error::text(error, &text_repr::str_repr(&paths::display(path)))
}

/// `text` normalized as a POSIX path: repeated slashes and `.` parts drop, and a trailing slash goes. An empty path is
/// `.`.
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
  fn pathlib_text_drops_dot_parts_and_repeated_slashes() {
    assert_eq!(pathlib_text("./configs//code-server.yaml"), "configs/code-server.yaml");
    assert_eq!(pathlib_text("/abs/"), "/abs");
    assert_eq!(pathlib_text("/"), "/");
    assert_eq!(pathlib_text(""), ".");
    assert_eq!(pathlib_text("./code-server"), "code-server");
  }

  #[test]
  fn a_section_without_keys_takes_the_defaults_and_an_unknown_key_is_refused() {
    let defaults = (section().parse)(&serde_yaml::Value::Mapping(Default::default())).expect("defaults parse");
    let defaults = defaults.downcast_ref::<CodeServerConfig>().expect("the section holds its type");
    assert_eq!(defaults.bin, None);
    assert_eq!(defaults.config, "configs/code-server.yaml");
    let unknown: serde_yaml::Value = serde_yaml::from_str("bogus: 1").expect("the YAML parses");
    assert!((section().parse)(&unknown).is_err());
  }
}
