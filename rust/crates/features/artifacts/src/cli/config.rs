//! The CLI's view of `<home>/config.yaml`.
//!
//! The Python CLI loads the whole pydantic config, whose packages the server process registers. A feature CLI
//! process registers no package, so it reads the fields the `artifact` verbs use — `headless_chrome_bin` and the
//! `backends` catalog — and takes the core sections' own serde defaults for the rest. Sections that a package parses
//! stay unparsed (`Sections::default`), which only a backend factory that reads one would notice.

use std::path::{Path, PathBuf};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::{
  self, BackendOption, BackendsConfig, CONFIG_FILENAME, CharlieBotConfig, RoutingConfig, Sections,
};
use charliebot_core::paths;
use serde_yaml::{Mapping, Value};

/// One loaded profile config.
pub(crate) struct CliConfig {
  pub config: CharlieBotConfig,
}

impl CliConfig {
  /// The config of one profile home.
  pub fn load_from(home: &Path) -> Result<CliConfig, CliExit> {
    let path = home.join(CONFIG_FILENAME);
    let path_text = paths::display(&path);
    let document = config::read_yaml_document(&path).map_err(|error| CliExit::error(error.0))?;
    let mapping = match document {
      Value::Null => Mapping::new(),
      Value::Mapping(mapping) => mapping,
      other => {
        return Err(CliExit::error(format!(
          "{path_text}: the document must be a mapping of sections, got {}",
          config::yaml_value_kind(&other)
        )));
      }
    };
    let headless_chrome_bin = match mapping.get("headless_chrome_bin") {
      None => String::new(),
      Some(Value::String(text)) => text.clone(),
      Some(other) => {
        return Err(CliExit::error(format!(
          "{path_text}: headless_chrome_bin: expected a string, got {}",
          config::yaml_value_kind(other)
        )));
      }
    };
    let config = CharlieBotConfig {
      charliebot_home: home.to_path_buf(),
      headless_chrome_bin,
      server: typed(&path_text, &mapping, "server")?,
      paths: typed(&path_text, &mapping, "paths")?,
      backends: parse_backends(&path_text, &mapping)?,
      ui: typed(&path_text, &mapping, "ui")?,
      telegram: typed(&path_text, &mapping, "telegram")?,
      sections: Sections::default(),
    };
    Ok(CliConfig { config })
  }

  /// `<home>/sessions`: the directory the session resolution and `plan diff` read.
  pub fn sessions_dir(&self) -> PathBuf {
    self.config.sessions_dir()
  }
}

/// One core section, with the model's defaults when the file omits it.
fn typed<T: serde::de::DeserializeOwned>(path_text: &str, mapping: &Mapping, key: &str) -> Result<T, CliExit> {
  let value = mapping.get(key).cloned().unwrap_or_else(|| Value::Mapping(Mapping::new()));
  serde_yaml::from_value(value)
    .map_err(|error| CliExit::error(format!("{path_text}: invalid config key {key}: {error}")))
}

/// `backends:`: the rotation pool, the optional router command and the option catalog.
fn parse_backends(path_text: &str, mapping: &Mapping) -> Result<BackendsConfig, CliExit> {
  let Some(value) = mapping.get("backends") else {
    return Ok(BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() });
  };
  let Value::Mapping(backends) = value else {
    return Err(CliExit::error(format!(
      "{path_text}: invalid config key backends: expected a mapping, got {}",
      config::yaml_value_kind(value)
    )));
  };
  let pool = match backends.get("pool") {
    None | Some(Value::Null) => Vec::new(),
    Some(Value::Sequence(entries)) => entries
      .iter()
      .enumerate()
      .map(|(index, entry)| {
        entry.as_str().map(str::to_string).ok_or_else(|| {
          CliExit::error(format!(
            "{path_text}: backends.pool[{index}]: expected a string, got {}",
            config::yaml_value_kind(entry)
          ))
        })
      })
      .collect::<Result<Vec<_>, _>>()?,
    Some(other) => {
      return Err(CliExit::error(format!(
        "{path_text}: invalid config key backends.pool: expected a list, got {}",
        config::yaml_value_kind(other)
      )));
    }
  };
  let routing = match backends.get("routing") {
    None | Some(Value::Null) => None,
    Some(value) => Some(
      serde_yaml::from_value::<RoutingConfig>(value.clone())
        .map_err(|error| CliExit::error(format!("{path_text}: invalid config key backends.routing: {error}")))?,
    ),
  };
  let options = match backends.get("options") {
    None | Some(Value::Null) => Vec::new(),
    Some(Value::Sequence(entries)) => entries
      .iter()
      .enumerate()
      .map(|(index, entry)| parse_option(path_text, index, entry))
      .collect::<Result<_, _>>()?,
    Some(other) => {
      return Err(CliExit::error(format!(
        "{path_text}: invalid config key backends.options: expected a list, got {}",
        config::yaml_value_kind(other)
      )));
    }
  };
  Ok(BackendsConfig { pool, routing, options })
}

/// One `backends.options[]` entry: the shared fields plus the type's own keys, kept raw.
fn parse_option(path_text: &str, index: usize, entry: &Value) -> Result<BackendOption, CliExit> {
  let Value::Mapping(entry) = entry else {
    return Err(CliExit::error(format!(
      "{path_text}: backends.options[{index}]: a backend entry must be a mapping, got {}",
      config::yaml_value_kind(entry)
    )));
  };
  let required = |key: &str| -> Result<String, CliExit> {
    match entry.get(key) {
      Some(Value::String(text)) => Ok(text.clone()),
      Some(other) => Err(CliExit::error(format!(
        "{path_text}: backends.options[{index}].{key}: expected a string, got {}",
        config::yaml_value_kind(other)
      ))),
      None => Err(CliExit::error(format!("{path_text}: backends.options[{index}].{key}: field required"))),
    }
  };
  let optional = |key: &str| -> Result<Option<String>, CliExit> {
    match entry.get(key) {
      None | Some(Value::Null) => Ok(None),
      Some(Value::String(text)) => Ok(Some(text.clone())),
      Some(other) => Err(CliExit::error(format!(
        "{path_text}: backends.options[{index}].{key}: expected a string, got {}",
        config::yaml_value_kind(other)
      ))),
    }
  };
  let id = required("id")?;
  let label = required("label")?;
  let model = optional("model")?.ok_or_else(|| {
    CliExit::error(format!("{path_text}: backends.options[{index}]: backend '{id}' requires 'model'"))
  })?;
  let prompt_overlay = optional("prompt_overlay")?;
  let backend_type = required("type")?;
  let type_fields: Mapping = entry
    .iter()
    .filter(|(key, _)| !matches!(key.as_str(), Some("id" | "label" | "model" | "prompt_overlay" | "type")))
    .map(|(key, value)| (key.clone(), value.clone()))
    .collect();
  Ok(BackendOption { id, label, model, prompt_overlay, backend_type, type_fields })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_missing_config_reads_the_defaults() {
    let home = tempfile::tempdir().unwrap();
    let config = CliConfig::load_from(home.path()).unwrap().config;
    assert_eq!(config.headless_chrome_bin, "");
    assert!(config.backends.pool.is_empty());
    assert!(config.backends.options.is_empty());
    assert_eq!(config.sessions_dir(), home.path().join("sessions"));
  }

  #[test]
  fn the_backends_catalog_and_renderer_path_load() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join(CONFIG_FILENAME),
      "headless_chrome_bin: /synthetic/chrome\nbackends:\n  pool: [alpha]\n  options:\n    - id: alpha\n      label: Alpha\n      type: codex\n      model: synthetic-model\n      effort: high\n",
    )
    .unwrap();
    let config = CliConfig::load_from(home.path()).unwrap().config;
    assert_eq!(config.headless_chrome_bin, "/synthetic/chrome");
    assert_eq!(config.backends.pool, ["alpha"]);
    let option = config.get_backend_option("alpha").unwrap();
    assert_eq!(option.backend_type, "codex");
    assert_eq!(option.type_fields.get("effort").and_then(Value::as_str), Some("high"));
  }
}
