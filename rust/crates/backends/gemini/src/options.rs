//! The gemini option model. The type names no field of its own, so every key of its option is refused.

use serde_yaml::Mapping;

/// The `gemini` option's type-specific fields: none, as `GeminiBackend` names none.
pub const SCHEMA: &[charliebot_core::config::OptionField] = &[];

/// Accepts an option with no type-specific fields and refuses any other key.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  match fields.keys().next() {
    None => Ok(()),
    Some(key) => match key.as_str() {
      Some(name) => Err(format!("unknown gemini option field {name:?}")),
      None => Err(format!("gemini option key must be a string: {key:?}")),
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_option_with_no_fields_is_accepted_and_any_field_is_refused() {
    assert!(option_model(&Mapping::new()).is_ok());
    let extra: Mapping = serde_yaml::from_str("effort: high\n").unwrap();
    assert_eq!(option_model(&extra).unwrap_err(), "unknown gemini option field \"effort\"");
  }
}
