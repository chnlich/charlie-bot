//! The internal-API base URL: the `server` section of `<home>/config.yaml` on every call.

use std::path::Path;

use charliebot_core::config::{self, ServerConfig};
use charliebot_core::home;
use charliebot_core::paths;
use serde_yaml::{Mapping, Value};

use crate::exit::CliExit;

/// `_internal_base_url`: `http://localhost:<server.port>`, the port read from `config.yaml` each
/// time. No port cache: Python's `cache/cli_base_url.json` is keyed on a Python source file and has
/// no Rust meaning.
pub fn internal_base_url() -> Result<String, CliExit> {
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let port = server_port(&home)?;
  Ok(format!("http://localhost:{port}"))
}

/// The `server.port` of `<home>/config.yaml`, with the model's defaults when the section is absent.
/// A config that does not parse is an error naming the file.
pub fn server_port(home: &Path) -> Result<u16, CliExit> {
  let path = home.join(config::CONFIG_FILENAME);
  let path_text = paths::display(&path);
  let document = config::read_yaml_document(&path).map_err(|error| CliExit::error(error.0))?;
  let section = match &document {
    Value::Mapping(mapping) => mapping.get(Value::String("server".to_string())).cloned(),
    Value::Null => None,
    other => {
      return Err(CliExit::error(format!(
        "{path_text}: the document must be a mapping of sections, got {}",
        config::yaml_value_kind(other)
      )));
    }
  };
  let server: ServerConfig =
    serde_path_to_error::deserialize(section.unwrap_or_else(|| Value::Mapping(Mapping::new())))
      .map_err(|error| CliExit::error(format!("{path_text}: invalid config key server: {}", error.into_inner())))?;
  Ok(server.port)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_config_with_a_server_section_answers_its_port() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(config::CONFIG_FILENAME), "server:\n  port: 49991\n").unwrap();
    assert_eq!(server_port(home.path()).unwrap(), 49991);
  }

  #[test]
  fn a_missing_file_takes_the_model_defaults() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(server_port(home.path()).unwrap(), 18498);
  }

  #[test]
  fn a_broken_server_section_is_an_error_naming_the_file() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(config::CONFIG_FILENAME), "server:\n  port: not-a-port\n").unwrap();
    let error = server_port(home.path()).unwrap_err();
    assert!(error.body["error"].as_str().unwrap().contains("invalid config key server"), "{error:?}");
    assert!(error.body["error"].as_str().unwrap().contains("config.yaml"), "{error:?}");
  }
}
