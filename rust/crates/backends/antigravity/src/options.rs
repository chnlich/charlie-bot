//! The antigravity option fields. A turn runs `agy acp` with no backend-specific setting, so the option takes no field.

use charliebot_core::config::OptionField;
use charliebot_registry::option_model::{self, UnknownOptionFieldStyle};
use serde_yaml::Mapping;

/// The backend type this package registers: the `type` a `backends.options` entry names, and the key `SCHEMA` below
/// is registered under.
pub const TYPE_NAME: &str = "antigravity";

/// The `antigravity` option's type-specific fields: none.
pub const SCHEMA: &[OptionField] = &[];

/// The option model the registry runs over each `antigravity` entry at config load. Every key is refused.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  option_model::validate_field_names(fields, SCHEMA, TYPE_NAME, UnknownOptionFieldStyle::Unknown)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_antigravity_option_takes_no_field() {
    let fields = |text: &str| -> Mapping { serde_yaml::from_str(text).unwrap() };
    assert!(option_model(&fields("")).is_ok());
    assert!(option_model(&fields("print_timeout: 2h\n")).is_err());
  }
}
