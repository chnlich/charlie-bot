//! The registered cc-claude option fields.

use charliebot_core::config::{OptionField, OptionFieldDefault};
use serde_yaml::{Mapping, Value};

/// The `effort` key of a `backends.options[]` entry: the Claude Code `--effort` level. The Anthropic-compatible
/// backend launches `claude` too and reads the same field, so both backends import this name.
pub const EFFORT_KEY: &str = "effort";

/// The `cli_binary` key of a `backends.options[]` entry: the executable that replaces `claude`. The
/// Anthropic-compatible backend replaces `claude` the same way, so both backends import this name.
pub const CLI_BINARY_KEY: &str = "cli_binary";

/// The `cc-claude` option's type-specific fields in `CcClaudeBackend`'s declaration order, each with the value the
/// option model holds when an entry omits it. `charliebot config get backends` renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field(EFFORT_KEY, OptionFieldDefault::Null),
  OptionField::field("fast_mode", OptionFieldDefault::Bool(false)),
  OptionField::field(CLI_BINARY_KEY, OptionFieldDefault::Null),
  OptionField::field("account_pool", OptionFieldDefault::Null),
];

/// Validate the cc-claude-only fields of one backend option.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  for (key, value) in fields {
    let Some(name) = key.as_str() else {
      return Err(format!("cc-claude option key must be a string: {key:?}"));
    };
    match name {
      EFFORT_KEY | CLI_BINARY_KEY | "account_pool" => {
        if !matches!(value, Value::Null | Value::String(_)) {
          return Err(format!("cc-claude option {name} must be a string or null"));
        }
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

pub(crate) fn optional_text(fields: &Mapping, name: &str) -> Result<Option<String>, String> {
  match fields.get(Value::String(name.to_string())) {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(value)) => Ok(Some(value.clone())),
    Some(_) => Err(format!("cc-claude option {name} must be a string or null")),
  }
}

pub(crate) fn fast_mode(fields: &Mapping) -> Result<bool, String> {
  match fields.get(Value::String("fast_mode".to_string())) {
    None => Ok(false),
    Some(Value::Bool(value)) => Ok(*value),
    Some(_) => Err("cc-claude option fast_mode must be a boolean".to_string()),
  }
}

/// The launcher binary an entry names, read through the registry by the run-venue decision. A launcher runs the
/// entry's process where the launcher decides, so the server prepares no worktree for such an entry's runs.
pub fn cli_binary(fields: &Mapping) -> Result<Option<String>, String> {
  optional_text(fields, CLI_BINARY_KEY)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).expect("the fixture is YAML")
  }

  #[test]
  fn cli_binary_parses_through_the_registry_reader() {
    assert_eq!(cli_binary(&fields("effort: high\n")), Ok(None));
    assert_eq!(cli_binary(&fields("cli_binary: /path/to/launcher\n")), Ok(Some("/path/to/launcher".to_string())));
    assert_eq!(cli_binary(&fields("cli_binary: null\n")), Ok(None));
    assert!(cli_binary(&fields("cli_binary: 42\n")).is_err());
  }
}
