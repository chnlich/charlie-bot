//! The option model of a backend type that declares no option field.

use serde_yaml::Mapping;

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
