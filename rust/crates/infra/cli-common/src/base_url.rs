//! The internal-API base URL: the `server` section of `<home>/config.yaml` on every call.

use std::env::VarError;
use std::path::Path;

use charliebot_core::config::{self, ServerConfig};
use charliebot_core::home;
use charliebot_core::paths;
use serde_yaml::{Mapping, Value};

use crate::exit::CliExit;
use crate::improve_round;

/// The variable that names the round's direction, which is also the name of its prompt file. The runner exports it.
const DIRECTION_ENV: &str = "IMPROVE_DIRECTION";
/// The variable that names the kit: the `dev/` tree of the revision that the round runs. The runner exports it.
const KIT_DIR_ENV: &str = "IMPROVE_KIT_DIR";

/// Refuses at once when this process runs in an improve round. A command that needs the server calls it before any
/// side effect, and [`internal_base_url`] calls it before it reads the port. The refusal names the round's state
/// sources, which the round reads in place of the server.
pub fn refuse_in_round() -> Result<(), CliExit> {
  if improve_round::in_improve_round()? {
    return Err(CliExit::error(round_no_server_message()?));
  }
  Ok(())
}

/// The refusal's message: the round has no CharlieBot server, and the round's state sources, by their paths.
fn round_no_server_message() -> Result<String, CliExit> {
  let launch_dir = round_variable(improve_round::LAUNCH_DIR_ENV)?;
  let direction = round_variable(DIRECTION_ENV)?;
  let kit_dir = round_variable(KIT_DIR_ENV)?;
  let user_home = paths::expanduser("~");
  Ok(format!(
    "this improve round has no CharlieBot server, so the command cannot reach one. The round's state sources are \
     its launch directory {launch_dir}, its direction prompt {kit_dir}/dev/improve/{direction}.md, its evidence \
     directories under {user_home}/improve/{direction}/evidence, and git in its clone and at origin. Read these \
     sources; do not reuse an earlier round's result as the current state."
  ))
}

/// The value of a variable that the runner exports to every round. A missing, empty or non-UTF-8 value is a kit
/// defect, and the error names the variable.
fn round_variable(name: &str) -> Result<String, CliExit> {
  match std::env::var(name) {
    Ok(value) if !value.is_empty() => Ok(value),
    Ok(_) | Err(VarError::NotPresent) => Err(CliExit::error(format!(
      "{name} is unset or empty in this improve round: the runner exports it, so the round's kit is defective"
    ))),
    Err(VarError::NotUnicode(_)) => {
      Err(CliExit::error(format!("{name} is not valid UTF-8 in this improve round: the runner's kit is defective")))
    }
  }
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
    Value::Mapping(mapping) => mapping.get(Value::String(config::SERVER_KEY.to_string())).cloned(),
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
