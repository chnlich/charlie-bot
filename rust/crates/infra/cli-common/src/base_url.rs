//! The internal-API base URL: the `server` section of `<home>/config.yaml` on every call.

use std::path::Path;

use charliebot_core::config::{self, ServerConfig};
use charliebot_core::home;
use charliebot_core::paths;
use serde_yaml::{Mapping, Value};

use crate::exit::CliExit;
use crate::improve_round;

/// The error of a server command that runs in an improve round: the round's host has no CharlieBot server to reach.
pub const ROUND_NO_SERVER_MESSAGE: &str =
  "this improve round has no CharlieBot server, so the command cannot reach one: the round waits inside its run";

/// Refuses at once when this process runs in an improve round. A command that needs the server calls it before any
/// side effect, and [`internal_base_url`] calls it before it reads the port.
pub fn refuse_in_round() -> Result<(), CliExit> {
  if improve_round::in_improve_round()? {
    return Err(CliExit::error(ROUND_NO_SERVER_MESSAGE));
  }
  Ok(())
}

/// The internal-API base URL, `http://localhost:<server.port>`, with the port read from
/// `config.yaml` each time.
pub fn internal_base_url() -> Result<String, CliExit> {
  refuse_in_round()?;
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let port = server_port(&home)?;
  Ok(format!("http://localhost:{port}"))
}

/// The `server.port` of `<home>/config.yaml`, with the model's defaults when the file has no `server` section.
/// A missing file is an error naming the file and the profile home, so no request goes to a guessed port. A config
/// that does not parse is an error naming the file.
pub fn server_port(home: &Path) -> Result<u16, CliExit> {
  let path = home.join(config::CONFIG_FILENAME);
  let path_text = paths::display(&path);
  let present = path.try_exists().map_err(|error| CliExit::error(format!("{path_text}: {error}")))?;
  if !present {
    return Err(CliExit::error(format!(
      "{path_text} is missing: the profile home {} has no config.yaml, so the server port is unknown; \
       set CHARLIEBOT_HOME to a profile home that has one",
      paths::display(home)
    )));
  }
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
  fn a_config_without_a_server_section_takes_the_model_defaults() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(config::CONFIG_FILENAME), "backends:\n  options: []\n").unwrap();
    assert_eq!(server_port(home.path()).unwrap(), 18498);
  }

  #[test]
  fn a_missing_file_is_an_error_naming_the_file() {
    let home = tempfile::tempdir().unwrap();
    let expected_path = home.path().join(config::CONFIG_FILENAME);
    let error = server_port(home.path()).unwrap_err();
    let message = error.body["error"].as_str().unwrap();
    assert!(message.contains(&expected_path.display().to_string()), "{error:?}");
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
