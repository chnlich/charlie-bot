//! The cc-openai-compatible option fields: `api_base` and `credential`.

use charliebot_core::config::{API_BASE_KEY, CREDENTIAL_KEY, OptionField, OptionFieldDefault};
use serde_yaml::{Mapping, Value};

/// The `cc-openai-compatible` option's type-specific fields, in the order this crate declares them, each with the
/// value it holds when an entry omits it. `charliebot config get backends` renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(API_BASE_KEY, OptionFieldDefault::Required),
  OptionField::field(CREDENTIAL_KEY, OptionFieldDefault::Null),
];

/// The type-specific fields of one `cc-openai-compatible` backend option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcOpenAiOption {
  /// The upstream OpenAI-compatible base URL.
  pub api_base: String,
  /// The `credentials.yaml` section whose `api_key` is the upstream bearer, when the option names one.
  pub credential: Option<String>,
}

impl CcOpenAiOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(API_BASE_KEY | CREDENTIAL_KEY) => {}
        Some(name) => return Err(format!("unknown cc-openai-compatible option field {name:?}")),
        None => return Err(format!("cc-openai-compatible option key must be a string: {key:?}")),
      }
    }
    let api_base = match fields.get(API_BASE_KEY) {
      None => return Err("cc-openai-compatible option api_base is required".to_string()),
      Some(Value::String(api_base)) => api_base.clone(),
      Some(_) => return Err("cc-openai-compatible option api_base must be a string".to_string()),
    };
    let credential = match fields.get(CREDENTIAL_KEY) {
      None | Some(Value::Null) => None,
      Some(Value::String(credential)) => Some(credential.clone()),
      Some(_) => return Err("cc-openai-compatible option credential must be a string or null".to_string()),
    };
    Ok(Self { api_base, credential })
  }
}

/// The option model the registry runs over each `cc-openai-compatible` entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CcOpenAiOption::from_fields(fields).map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  #[test]
  fn api_base_is_required_and_credential_is_optional() {
    let option = CcOpenAiOption::from_fields(&fields("api_base: https://upstream.example/v1\n")).unwrap();
    assert_eq!(option, CcOpenAiOption { api_base: "https://upstream.example/v1".to_string(), credential: None });
    let option = CcOpenAiOption::from_fields(&fields("api_base: x\ncredential: upstream\n")).unwrap();
    assert_eq!(option.credential.as_deref(), Some("upstream"));
    assert_eq!(
      CcOpenAiOption::from_fields(&fields("credential: upstream\n")).unwrap_err(),
      "cc-openai-compatible option api_base is required"
    );
    assert_eq!(
      CcOpenAiOption::from_fields(&fields("api_base: x\nextra: 1\n")).unwrap_err(),
      "unknown cc-openai-compatible option field \"extra\""
    );
  }
}
