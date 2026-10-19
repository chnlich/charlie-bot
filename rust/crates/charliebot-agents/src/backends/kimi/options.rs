//! The cc-kimi option fields: `credential`, from `src/backends/kimi/options.py`.

use serde_yaml::{Mapping, Value};

const CREDENTIAL: &str = "credential";

/// The type-specific fields of one `cc-kimi` backend option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcKimiOption {
  /// The `credentials.yaml` section whose `api_key` is the Moonshot key.
  pub credential: String,
}

impl CcKimiOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(CREDENTIAL) => {}
        Some(name) => return Err(format!("unknown cc-kimi option field {name:?}")),
        None => return Err(format!("cc-kimi option key must be a string: {key:?}")),
      }
    }
    match fields.get(CREDENTIAL) {
      None => Err("cc-kimi option credential is required".to_string()),
      Some(Value::String(credential)) => Ok(Self { credential: credential.clone() }),
      Some(_) => Err("cc-kimi option credential must be a string".to_string()),
    }
  }
}

/// The option model the registry runs over each `cc-kimi` entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  CcKimiOption::from_fields(fields).map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  #[test]
  fn credential_is_required_and_no_other_field_is_accepted() {
    assert_eq!(
      CcKimiOption::from_fields(&fields("credential: moonshot\n")).unwrap(),
      CcKimiOption { credential: "moonshot".to_string() }
    );
    assert_eq!(CcKimiOption::from_fields(&fields("{}\n")).unwrap_err(), "cc-kimi option credential is required");
    assert_eq!(
      CcKimiOption::from_fields(&fields("credential: moonshot\nextra: 1\n")).unwrap_err(),
      "unknown cc-kimi option field \"extra\""
    );
  }
}
