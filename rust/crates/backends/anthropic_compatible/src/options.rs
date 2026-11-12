//! The endpoint, declared context window, optional credential, optional effort, optional executable, request headers
//! and fixed request body fields of a `cc-anthropic-compatible` entry.

use charliebot_claude_code::options::{CLI_BINARY_KEY, EFFORT_KEY};
use charliebot_core::config::{
  API_BASE_KEY, CONTEXT_WINDOW_KEY, CREDENTIAL_KEY, EXTRA_BODY_KEY, OptionField, OptionFieldDefault,
};
use serde_json::{Map, Value as JsonValue};
use serde_yaml::{Mapping, Value};

use crate::TYPE_NAME;

const HEADERS: &str = "headers";
const HOST_AUTH_ALIAS: &str = "host_auth_alias";

/// The `cc-anthropic-compatible` option's type-specific fields in [`CcAnthropicCompatibleOption`]'s declaration
/// order, each with the value the option holds when an entry omits it. `charliebot config get backends` renders
/// them, and [`CcAnthropicCompatibleOption::from_fields`] accepts each of these names and no other.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(API_BASE_KEY, OptionFieldDefault::Required),
  OptionField::field(CONTEXT_WINDOW_KEY, OptionFieldDefault::Required),
  OptionField::field(CREDENTIAL_KEY, OptionFieldDefault::Null),
  OptionField::field(EFFORT_KEY, OptionFieldDefault::Null),
  OptionField::field(CLI_BINARY_KEY, OptionFieldDefault::Null),
  OptionField::field(HEADERS, OptionFieldDefault::EmptyMap),
  OptionField::field(EXTRA_BODY_KEY, OptionFieldDefault::EmptyMap),
  OptionField::field(HOST_AUTH_ALIAS, OptionFieldDefault::Null),
];

/// The type-specific fields of one Anthropic-compatible backend option.
#[derive(Debug, Clone, PartialEq)]
pub struct CcAnthropicCompatibleOption {
  /// The endpoint root Claude Code uses before appending its messages route.
  pub api_base: String,
  /// The declared model context window in tokens.
  pub context_window: u64,
  /// The credentials section whose `api_key` authenticates the endpoint, when present.
  pub credential: Option<String>,
  /// The Claude Code `--effort` level, passed on only when set.
  pub effort: Option<String>,
  /// The executable that replaces `claude`, as `cli_binary` does for a `cc-claude` entry, when set.
  pub cli_binary: Option<String>,
  /// The request headers the launch routes to the endpoint, in the entry's order.
  pub headers: Vec<(String, String)>,
  /// The JSON object Claude Code merges into the body of every request to the endpoint.
  pub extra_body: Map<String, JsonValue>,
  /// The ssh alias of the host this entry's login rides on. The scheduler probes it before each fire, and a fire
  /// waits while the host awaits the user's approval. `None` for an entry that names no host.
  pub host_auth_alias: Option<String>,
}

impl CcAnthropicCompatibleOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(name) if SCHEMA.iter().any(|field| field.name == name) => {}
        Some(name) => return Err(format!("unknown {TYPE_NAME} option field {name:?}")),
        None => return Err(format!("{TYPE_NAME} option key must be a string: {key:?}")),
      }
    }
    let api_base = match fields.get(API_BASE_KEY) {
      None => return Err(format!("{TYPE_NAME} option api_base is required")),
      Some(Value::String(api_base)) if !api_base.is_empty() => api_base.clone(),
      Some(Value::String(_)) => return Err(format!("{TYPE_NAME} option api_base must not be empty")),
      Some(_) => return Err(format!("{TYPE_NAME} option api_base must be a string")),
    };
    if api_base.trim_end_matches('/').ends_with("/v1") {
      return Err(format!("{TYPE_NAME} api_base must be the endpoint root without /v1"));
    }
    let context_window = match fields.get(CONTEXT_WINDOW_KEY) {
      None => return Err(format!("{TYPE_NAME} option context_window is required")),
      Some(Value::Number(value)) => match value.as_u64() {
        Some(window) if window > 0 => window,
        _ => return Err(format!("{TYPE_NAME} option context_window must be an integer greater than 0")),
      },
      Some(_) => {
        return Err(format!("{TYPE_NAME} option context_window must be an integer greater than 0"));
      }
    };
    let credential = match fields.get(CREDENTIAL_KEY) {
      None | Some(Value::Null) => None,
      Some(Value::String(credential)) => Some(credential.clone()),
      Some(_) => return Err(format!("{TYPE_NAME} option credential must be a string or null")),
    };
    let effort = match fields.get(EFFORT_KEY) {
      None | Some(Value::Null) => None,
      Some(Value::String(effort)) => Some(effort.clone()),
      Some(_) => return Err(format!("{TYPE_NAME} option effort must be a string or null")),
    };
    let cli_binary = match fields.get(CLI_BINARY_KEY) {
      None | Some(Value::Null) => None,
      Some(Value::String(cli_binary)) => Some(cli_binary.clone()),
      Some(_) => return Err(format!("{TYPE_NAME} option cli_binary must be a string or null")),
    };
    let headers = match fields.get(HEADERS) {
      None => Vec::new(),
      Some(Value::Mapping(headers)) => {
        let mut parsed = Vec::new();
        for (name, value) in headers {
          let Some(name) = name.as_str() else {
            return Err(format!("{TYPE_NAME} option headers keys must be strings"));
          };
          let Some(value) = value.as_str() else {
            return Err(format!("{TYPE_NAME} option headers values must be strings"));
          };
          parsed.push((name.to_string(), value.to_string()));
        }
        parsed
      }
      Some(_) => return Err(format!("{TYPE_NAME} option headers must be a mapping of strings")),
    };
    let extra_body = match fields.get(EXTRA_BODY_KEY) {
      None => Map::new(),
      Some(Value::Mapping(mapping)) => charliebot_core::config::yaml_mapping_to_json(mapping)
        .map_err(|error| format!("{TYPE_NAME} option extra_body: {error}"))?,
      Some(_) => return Err(format!("{TYPE_NAME} option extra_body must be a mapping")),
    };
    let host_auth_alias = match fields.get(HOST_AUTH_ALIAS) {
      None | Some(Value::Null) => None,
      Some(Value::String(alias)) if !alias.is_empty() => Some(alias.clone()),
      Some(_) => {
        return Err(format!("{TYPE_NAME} option host_auth_alias must be a non-empty string or null"));
      }
    };
    Ok(Self { api_base, context_window, credential, effort, cli_binary, headers, extra_body, host_auth_alias })
  }
}

/// The option model that validates the fields of a `cc-anthropic-compatible` config entry.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CcAnthropicCompatibleOption::from_fields(fields).map(|_| ())
}

/// The host alias an entry declares, read through the registry by the scheduler's host gate.
pub fn host_auth_alias(fields: &Mapping) -> Result<Option<String>, String> {
  Ok(CcAnthropicCompatibleOption::from_fields(fields)?.host_auth_alias)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).expect("the fixture is YAML")
  }

  #[test]
  fn host_auth_alias_parses_and_other_unknown_keys_stay_refused() {
    let base = "api_base: https://upstream.example\ncontext_window: 200000\n";
    assert_eq!(
      host_auth_alias(&fields(&format!("{base}host_auth_alias: gpu-box-1\n"))),
      Ok(Some("gpu-box-1".to_string()))
    );
    assert_eq!(host_auth_alias(&fields(&format!("{base}host_auth_alias: null\n"))), Ok(None));
    assert!(host_auth_alias(&fields(&format!("{base}host_auth_alias: ''\n"))).is_err());
    assert!(option_model(&fields(&format!("{base}host: gpu-box-1\n"))).is_err());
  }
}
