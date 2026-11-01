//! The charlie-code option fields (`src/backends/charlie_code/options.py`). Each field carries the bound pydantic checks
//! for it, and the type refuses any key it does not name.

use charliebot_core::config::{
  API_BASE_KEY, CONTEXT_WINDOW_KEY, CREDENTIAL_KEY, OptionField, OptionFieldDefault, PROXY_URL_KEY,
};
use serde_yaml::{Mapping, Value};

const IMAGE_INPUT: &str = "image_input";
const STREAM: &str = "stream";
const TIMEOUT_SECONDS: &str = "timeout_seconds";
const TOP_P: &str = "top_p";
const TEMPERATURE: &str = "temperature";

/// The `charlie-code` option's type-specific fields in `CharlieCodeBackend`'s declaration order, each with the value
/// the Python model holds when an entry omits it. `charliebot config get backends` renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(API_BASE_KEY, OptionFieldDefault::Null),
  OptionField::field(CONTEXT_WINDOW_KEY, OptionFieldDefault::Null),
  OptionField::field(CREDENTIAL_KEY, OptionFieldDefault::Null),
  OptionField::field(PROXY_URL_KEY, OptionFieldDefault::Null),
  OptionField::field(IMAGE_INPUT, OptionFieldDefault::Bool(true)),
  OptionField::field(STREAM, OptionFieldDefault::Bool(true)),
  OptionField::field(TIMEOUT_SECONDS, OptionFieldDefault::Null),
  OptionField::float_field(TOP_P, OptionFieldDefault::Null),
  OptionField::float_field(TEMPERATURE, OptionFieldDefault::Null),
];

const FIELDS: [&str; 9] = [
  API_BASE_KEY,
  CONTEXT_WINDOW_KEY,
  CREDENTIAL_KEY,
  PROXY_URL_KEY,
  IMAGE_INPUT,
  STREAM,
  TIMEOUT_SECONDS,
  TOP_P,
  TEMPERATURE,
];

/// The type-specific fields of one `charlie-code` backend option.
#[derive(Debug, Clone, PartialEq)]
pub struct CharlieCodeOption {
  /// The OpenAI-compatible base URL. The backend refuses an option that has none.
  pub api_base: Option<String>,
  /// The compaction window in tokens; `None` takes the CLI's default.
  pub context_window: Option<i64>,
  /// The `credentials.yaml` section whose `api_key` the child env carries.
  pub credential: Option<String>,
  /// The per-entry proxy URL the child env carries.
  pub proxy_url: Option<String>,
  /// Whether the endpoint accepts image attachments, which are sent as `--image`.
  pub image_input: bool,
  /// Whether the endpoint streams; `false` emits `--no-stream`.
  pub stream: bool,
  /// The call budget in seconds; `None` takes the CLI's default.
  pub timeout_seconds: Option<i64>,
  /// The nucleus cutoff, in (0, 1]; `None` takes the CLI's default.
  pub top_p: Option<f64>,
  /// The sampling temperature, at least 0; `None` takes the CLI's default.
  pub temperature: Option<f64>,
}

impl CharlieCodeOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(name) if FIELDS.contains(&name) => {}
        Some(name) => return Err(format!("unknown charlie-code option field {name:?}")),
        None => return Err(format!("charlie-code option key must be a string: {key:?}")),
      }
    }
    Ok(Self {
      api_base: optional_text(fields, API_BASE_KEY)?,
      context_window: optional_positive_int(fields, CONTEXT_WINDOW_KEY)?,
      credential: optional_text(fields, CREDENTIAL_KEY)?,
      proxy_url: optional_text(fields, PROXY_URL_KEY)?,
      image_input: flag(fields, IMAGE_INPUT, true)?,
      stream: flag(fields, STREAM, true)?,
      timeout_seconds: optional_positive_int(fields, TIMEOUT_SECONDS)?,
      top_p: optional_float(fields, TOP_P, |value| value > 0.0 && value <= 1.0, "a number in (0, 1]")?,
      temperature: optional_float(fields, TEMPERATURE, |value| value >= 0.0, "a number of at least 0")?,
    })
  }
}

/// The option model the registry runs over each `charlie-code` entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CharlieCodeOption::from_fields(fields).map(|_| ())
}

/// The field's value when the option sets it to a value; pydantic reads an `Optional` field that is null as absent.
fn present<'a>(fields: &'a Mapping, name: &str) -> Option<&'a Value> {
  fields.get(name).filter(|value| !value.is_null())
}

fn optional_text(fields: &Mapping, name: &str) -> Result<Option<String>, String> {
  match present(fields, name) {
    None => Ok(None),
    Some(Value::String(text)) => Ok(Some(text.clone())),
    Some(_) => Err(format!("charlie-code option {name} must be a string or null")),
  }
}

fn optional_positive_int(fields: &Mapping, name: &str) -> Result<Option<i64>, String> {
  match present(fields, name) {
    None => Ok(None),
    Some(value) => match value.as_i64() {
      Some(count) if count > 0 => Ok(Some(count)),
      _ => Err(format!("charlie-code option {name} must be an integer greater than 0, or null")),
    },
  }
}

fn optional_float(
  fields: &Mapping,
  name: &str,
  valid: impl Fn(f64) -> bool,
  expected: &str,
) -> Result<Option<f64>, String> {
  match present(fields, name) {
    None => Ok(None),
    Some(value) => match value.as_f64().filter(|number| valid(*number)) {
      Some(number) => Ok(Some(number)),
      None => Err(format!("charlie-code option {name} must be {expected}, or null")),
    },
  }
}

/// A boolean field with its default. A null is refused, as pydantic refuses it for a non-optional `bool`.
fn flag(fields: &Mapping, name: &str, default: bool) -> Result<bool, String> {
  match fields.get(name) {
    None => Ok(default),
    Some(Value::Bool(flag)) => Ok(*flag),
    Some(_) => Err(format!("charlie-code option {name} must be a boolean")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  #[test]
  fn defaults_match_the_model_and_its_bounds_are_enforced() {
    let option = CharlieCodeOption::from_fields(&fields("api_base: http://test.invalid/v1\n")).unwrap();
    assert_eq!(option.api_base.as_deref(), Some("http://test.invalid/v1"));
    assert!(option.image_input && option.stream);
    assert_eq!(option.top_p, None);
    assert!(CharlieCodeOption::from_fields(&fields("top_p: 1.5\n")).is_err());
    assert!(CharlieCodeOption::from_fields(&fields("timeout_seconds: 0\n")).is_err());
    assert!(CharlieCodeOption::from_fields(&fields("image_input: null\n")).is_err());
    assert!(CharlieCodeOption::from_fields(&fields("api_base: x\nextra: 1\n")).is_err());
  }
}
