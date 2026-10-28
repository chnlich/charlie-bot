//! The endpoint, declared context window, optional credential, optional effort, request headers and fixed request
//! body fields of a `cc-anthropic-compatible` entry.

use charliebot_core::config::{OptionField, OptionFieldDefault};
use serde_json::{Map, Value as JsonValue};
use serde_yaml::{Mapping, Value};

const API_BASE: &str = "api_base";
const CONTEXT_WINDOW: &str = "context_window";
const CREDENTIAL: &str = "credential";
const EFFORT: &str = "effort";
const HEADERS: &str = "headers";
const EXTRA_BODY: &str = "extra_body";
const FIELDS: [&str; 6] = [API_BASE, CONTEXT_WINDOW, CREDENTIAL, EFFORT, HEADERS, EXTRA_BODY];

/// The `cc-anthropic-compatible` option's type-specific fields in `CcAnthropicCompatibleBackend`'s declaration
/// order, each with the value the Python model holds when an entry omits it. `charliebot config get backends`
/// renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(API_BASE, OptionFieldDefault::Required),
  OptionField::field(CONTEXT_WINDOW, OptionFieldDefault::Required),
  OptionField::field(CREDENTIAL, OptionFieldDefault::Null),
  OptionField::field(EFFORT, OptionFieldDefault::Null),
  OptionField::field(HEADERS, OptionFieldDefault::EmptyMap),
  OptionField::field(EXTRA_BODY, OptionFieldDefault::EmptyMap),
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
  /// The request headers the launch routes to the endpoint, in the entry's order.
  pub headers: Vec<(String, String)>,
  /// The JSON object Claude Code merges into the body of every request to the endpoint.
  pub extra_body: Map<String, JsonValue>,
}

impl CcAnthropicCompatibleOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(name) if FIELDS.contains(&name) => {}
        Some(name) => return Err(format!("unknown cc-anthropic-compatible option field {name:?}")),
        None => return Err(format!("cc-anthropic-compatible option key must be a string: {key:?}")),
      }
    }
    let api_base = match fields.get(API_BASE) {
      None => return Err("cc-anthropic-compatible option api_base is required".to_string()),
      Some(Value::String(api_base)) if !api_base.is_empty() => api_base.clone(),
      Some(Value::String(_)) => return Err("cc-anthropic-compatible option api_base must not be empty".to_string()),
      Some(_) => return Err("cc-anthropic-compatible option api_base must be a string".to_string()),
    };
    if api_base.trim_end_matches('/').ends_with("/v1") {
      return Err("cc-anthropic-compatible api_base must be the endpoint root without /v1".to_string());
    }
    let context_window = match fields.get(CONTEXT_WINDOW) {
      None => return Err("cc-anthropic-compatible option context_window is required".to_string()),
      Some(Value::Number(value)) => match value.as_u64() {
        Some(window) if window > 0 => window,
        _ => return Err("cc-anthropic-compatible option context_window must be an integer greater than 0".to_string()),
      },
      Some(_) => {
        return Err("cc-anthropic-compatible option context_window must be an integer greater than 0".to_string());
      }
    };
    let credential = match fields.get(CREDENTIAL) {
      None | Some(Value::Null) => None,
      Some(Value::String(credential)) => Some(credential.clone()),
      Some(_) => return Err("cc-anthropic-compatible option credential must be a string or null".to_string()),
    };
    let effort = match fields.get(EFFORT) {
      None | Some(Value::Null) => None,
      Some(Value::String(effort)) => Some(effort.clone()),
      Some(_) => return Err("cc-anthropic-compatible option effort must be a string or null".to_string()),
    };
    let headers = match fields.get(HEADERS) {
      None => Vec::new(),
      Some(Value::Mapping(headers)) => {
        let mut parsed = Vec::new();
        for (name, value) in headers {
          let Some(name) = name.as_str() else {
            return Err("cc-anthropic-compatible option headers keys must be strings".to_string());
          };
          let Some(value) = value.as_str() else {
            return Err("cc-anthropic-compatible option headers values must be strings".to_string());
          };
          parsed.push((name.to_string(), value.to_string()));
        }
        parsed
      }
      Some(_) => return Err("cc-anthropic-compatible option headers must be a mapping of strings".to_string()),
    };
    let extra_body = match fields.get(EXTRA_BODY) {
      None => Map::new(),
      Some(Value::Mapping(mapping)) => charliebot_core::config::yaml_mapping_to_json(mapping)
        .map_err(|error| format!("cc-anthropic-compatible option extra_body: {error}"))?,
      Some(_) => return Err("cc-anthropic-compatible option extra_body must be a mapping".to_string()),
    };
    Ok(Self { api_base, context_window, credential, effort, headers, extra_body })
  }
}

/// Validate the fields of a `cc-anthropic-compatible` config entry.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CcAnthropicCompatibleOption::from_fields(fields).map(|_| ())
}
