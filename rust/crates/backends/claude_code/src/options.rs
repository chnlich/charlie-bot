//! The registered cc-claude option fields.

use charliebot_core::backend_types;
use charliebot_core::config::{OptionField, OptionFieldDefault};
use charliebot_registry::option_model;
use serde_yaml::{Mapping, Value};

/// The `effort` key of a `backends.options[]` entry: the Claude Code `--effort` level. The Anthropic-compatible
/// backend launches `claude` too and reads the same field, so both backends import this name.
pub const EFFORT_KEY: &str = "effort";

/// The `cc-claude` option's type-specific fields in `CcClaudeBackend`'s declaration order, each with the value the
/// option model holds when an entry omits it. `charliebot config get backends` renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(EFFORT_KEY, OptionFieldDefault::Null),
  OptionField::field("fast_mode", OptionFieldDefault::Bool(false)),
  OptionField::field("account_pool", OptionFieldDefault::Null),
];

/// Validate the cc-claude-only fields of one backend option.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  for (key, value) in fields {
    let Some(name) = key.as_str() else {
      return Err(format!("cc-claude option key must be a string: {key:?}"));
    };
    match name {
      EFFORT_KEY | "account_pool" => {
        optional_text(fields, name)?;
      }
      "fast_mode" => {
        if !matches!(value, Value::Bool(_)) {
          return Err("cc-claude option fast_mode must be a boolean".to_string());
        }
      }
      _ => return Err(format!("unknown cc-claude option field {name:?}")),
    }
  }
  Ok(())
}

/// The string value of the cc-claude option field `name`, read by the registry's reader under the cc-claude type name.
pub(crate) fn optional_text(fields: &Mapping, name: &str) -> Result<Option<String>, String> {
  option_model::optional_text(fields, backend_types::CC_CLAUDE, name)
}

pub(crate) fn fast_mode(fields: &Mapping) -> Result<bool, String> {
  match fields.get(Value::String("fast_mode".to_string())) {
    None => Ok(false),
    Some(Value::Bool(value)) => Ok(*value),
    Some(_) => Err("cc-claude option fast_mode must be a boolean".to_string()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).expect("the fixture is YAML")
  }

  #[test]
  fn a_launcher_or_host_key_fails_the_option_model_by_name() {
    for key in ["cli_binary", "host_auth_alias"] {
      let error = option_model(&fields(&format!("effort: high\n{key}: x\n"))).unwrap_err();
      assert!(error.contains(key), "{error}");
    }
  }

  #[test]
  fn a_text_field_that_is_not_a_string_fails_the_option_model_by_name() {
    for key in [EFFORT_KEY, "account_pool"] {
      let error = option_model(&fields(&format!("{key}: 3\n"))).unwrap_err();
      assert_eq!(error, format!("cc-claude option {key} must be a string or null"));
    }
    option_model(&fields("effort: high\naccount_pool:\n")).expect("a string or an unset field passes");
  }
}
