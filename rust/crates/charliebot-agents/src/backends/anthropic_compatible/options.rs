//! The endpoint, declared context window and optional credential of a `cc-anthropic-compatible` entry.

use serde_yaml::{Mapping, Value};

const API_BASE: &str = "api_base";
const CONTEXT_WINDOW: &str = "context_window";
const CREDENTIAL: &str = "credential";
const FIELDS: [&str; 3] = [API_BASE, CONTEXT_WINDOW, CREDENTIAL];

/// The type-specific fields of one Anthropic-compatible backend option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcAnthropicCompatibleOption {
  /// The endpoint root Claude Code uses before appending its messages route.
  pub api_base: String,
  /// The declared model context window in tokens.
  pub context_window: u64,
  /// The credentials section whose `api_key` authenticates the endpoint, when present.
  pub credential: Option<String>,
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
    Ok(Self { api_base, context_window, credential })
  }
}

/// Validate the fields of a `cc-anthropic-compatible` config entry.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CcAnthropicCompatibleOption::from_fields(fields).map(|_| ())
}
