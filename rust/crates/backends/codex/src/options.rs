//! The codex option fields of a `backends.options[]` entry.

use charliebot_core::config::{OptionField, OptionFieldDefault};
use serde_yaml::{Mapping, Value};

const REASONING_EFFORT: &str = "model_reasoning_effort";
const AUTO_COMPACT_LIMIT: &str = "model_auto_compact_token_limit";

/// The `codex` option's type-specific fields in `CodexBackend`'s declaration order, each with the default an entry
/// that omits it takes. `charliebot config get backends` renders them.
pub const SCHEMA: &[OptionField] = &[
  OptionField::field("model_reasoning_effort", OptionFieldDefault::Null),
  OptionField::field("model_auto_compact_token_limit", OptionFieldDefault::Null),
];

/// The parsed codex fields. `None` is the option's absence: the effort then defaults to `xhigh` and no auto-compact
/// limit is passed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexOptions {
  pub model_reasoning_effort: Option<String>,
  pub model_auto_compact_token_limit: Option<i64>,
}

/// Validates the codex fields of one backend option at config load (`register_option_model`).
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  parse(fields).map(|_| ())
}

/// Parses the codex fields. A key that is not a codex field fails, as the pydantic model's `extra="forbid"` does.
pub fn parse(fields: &Mapping) -> Result<CodexOptions, String> {
  let mut options = CodexOptions::default();
  for (key, value) in fields {
    let Some(name) = key.as_str() else {
      return Err(format!("codex option key must be a string: {key:?}"));
    };
    match (name, value) {
      (REASONING_EFFORT, Value::Null) => {}
      (REASONING_EFFORT, Value::String(effort)) => options.model_reasoning_effort = Some(effort.clone()),
      (REASONING_EFFORT, _) => return Err(format!("codex option {name} must be a string or null")),
      (AUTO_COMPACT_LIMIT, Value::Null) => {}
      (AUTO_COMPACT_LIMIT, Value::Number(number)) => {
        let limit = number
          .as_i64()
          .filter(|limit| *limit > 0)
          .ok_or_else(|| format!("codex option {name} must be a positive integer"))?;
        options.model_auto_compact_token_limit = Some(limit);
      }
      (AUTO_COMPACT_LIMIT, _) => return Err(format!("codex option {name} must be a positive integer or null")),
      _ => return Err(format!("unknown codex option field {name:?}")),
    }
  }
  Ok(options)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn mapping(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  #[test]
  fn parses_both_fields_and_null_as_absent() {
    let options = parse(&mapping("model_reasoning_effort: high\nmodel_auto_compact_token_limit: 200000")).unwrap();
    assert_eq!(
      options,
      CodexOptions { model_reasoning_effort: Some("high".to_string()), model_auto_compact_token_limit: Some(200_000) }
    );
    assert_eq!(parse(&mapping("model_auto_compact_token_limit: null")).unwrap(), CodexOptions::default());
  }

  #[test]
  fn refuses_unknown_keys_and_non_positive_limits() {
    assert!(option_model(&mapping("effort: high")).unwrap_err().contains("unknown codex option field"));
    assert!(option_model(&mapping("model_auto_compact_token_limit: 0")).is_err());
    assert!(option_model(&mapping("model_reasoning_effort: 5")).is_err());
  }
}
