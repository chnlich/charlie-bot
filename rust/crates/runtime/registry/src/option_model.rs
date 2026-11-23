//! The option-field helpers that the option model of each backend type shares.

use charliebot_core::config::OptionField;
use serde_yaml::{Mapping, Value};

/// The wording used when a backend option contains an unknown field.
#[derive(Clone, Copy)]
pub enum UnknownOptionFieldStyle {
  /// Name the field as unknown for this backend type.
  Unknown,
  /// Say that this backend type does not support the field.
  Unsupported,
}

/// Refuse keys outside `schema`, while keeping the backend type's option error wording.
pub fn validate_field_names(
  fields: &Mapping,
  schema: &[OptionField],
  backend_type: &str,
  unknown_style: UnknownOptionFieldStyle,
) -> Result<(), String> {
  for key in fields.keys() {
    match key.as_str() {
      Some(name) if schema.iter().any(|field| field.name == name) => {}
      Some(name) => {
        return Err(match unknown_style {
          UnknownOptionFieldStyle::Unknown => format!("unknown {backend_type} option field {name:?}"),
          UnknownOptionFieldStyle::Unsupported => format!("{backend_type} option field {name:?} is not supported"),
        });
      }
      None => return Err(format!("{backend_type} option key must be a string: {key:?}")),
    }
  }
  Ok(())
}

/// The option model of a backend type that declares no option field: an empty option passes, and any key is
/// refused with `backend_type` named.
pub fn no_fields(fields: &Mapping, backend_type: &str) -> Result<(), String> {
  validate_field_names(fields, &[], backend_type, UnknownOptionFieldStyle::Unknown)
}

/// The string value of the option field `name`: absent or null is `None`, a string is the value, and any other type is
/// refused with `backend_type` and `name` named.
pub fn optional_text(fields: &Mapping, backend_type: &str, name: &str) -> Result<Option<String>, String> {
  match fields.get(name) {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(text)) => Ok(Some(text.clone())),
    Some(_) => Err(format!("{backend_type} option {name} must be a string or null")),
  }
}
