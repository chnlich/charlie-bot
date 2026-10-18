//! The registered cc-claude option fields.

use serde_yaml::{Mapping, Value};

/// Validate the cc-claude-only fields of one backend option.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  for (key, value) in fields {
    let Some(name) = key.as_str() else {
      return Err(format!("cc-claude option key must be a string: {key:?}"));
    };
    match name {
      "effort" | "cli_binary" | "account_pool" => {
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
