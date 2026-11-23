//! The option-field helpers that the option model of each backend type shares.

use serde_yaml::{Mapping, Value};

/// The option model of a backend type that declares no option field: an empty option passes, and any key is
/// refused with `backend_type` named.
pub fn no_fields(fields: &Mapping, backend_type: &str) -> Result<(), String> {
  match fields.keys().next() {
    None => Ok(()),
    Some(key) => match key.as_str() {
      Some(name) => Err(format!("unknown {backend_type} option field {name:?}")),
      None => Err(format!("{backend_type} option key must be a string: {key:?}")),
    },
  }
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
