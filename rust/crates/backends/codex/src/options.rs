//! The codex option fields of a `backends.options[]` entry.

use charliebot_core::backend_types;
use charliebot_core::config::{OptionField, OptionFieldDefault};
use charliebot_registry::option_model::{self, UnknownOptionFieldStyle};
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

/// The validation of one backend option's codex fields at config load (`register_option_model`).
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  parse(fields).map(|_| ())
}

/// The codex fields parsed from one option's mapping. A key that is not a codex field fails.
pub fn parse(fields: &Mapping) -> Result<CodexOptions, String> {
  option_model::validate_field_names(fields, SCHEMA, backend_types::CODEX, UnknownOptionFieldStyle::Unknown)?;
  Ok(CodexOptions {
    model_reasoning_effort: option_model::optional_text(fields, backend_types::CODEX, REASONING_EFFORT)?,
    model_auto_compact_token_limit: auto_compact_limit(fields)?,
  })
}

/// The auto-compact token limit: absent or null is `None`, and a positive integer is the limit.
fn auto_compact_limit(fields: &Mapping) -> Result<Option<i64>, String> {
  match fields.get(AUTO_COMPACT_LIMIT) {
    None | Some(Value::Null) => Ok(None),
    Some(Value::Number(number)) => number
      .as_i64()
      .filter(|limit| *limit > 0)
      .map(Some)
      .ok_or_else(|| format!("codex option {AUTO_COMPACT_LIMIT} must be a positive integer")),
    Some(_) => Err(format!("codex option {AUTO_COMPACT_LIMIT} must be a positive integer or null")),
  }
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
