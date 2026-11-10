//! The `agy` JSON envelope: one object whose status field names the turn's outcome.

use serde_json::{Map, Value};

/// The envelope field that names the turn's status.
const STATUS_FIELD: &str = "status";

/// The status a successful `agy` turn carries.
pub(crate) const SUCCESS: &str = "SUCCESS";

/// An `agy` JSON envelope: an object whose status field is a string, with the other fields kept.
pub(crate) struct Envelope {
  pub(crate) status: String,
  pub(crate) fields: Map<String, Value>,
}

/// The stdout as an envelope, or `None` when it is not one.
pub(crate) fn parse(stdout: &str) -> Option<Envelope> {
  match serde_json::from_str::<Value>(stdout) {
    Ok(Value::Object(mut fields)) => match fields.remove(STATUS_FIELD) {
      Some(Value::String(status)) => Some(Envelope { status, fields }),
      _ => None,
    },
    _ => None,
  }
}

/// The status an `agy` JSON envelope value carries, or `None` when the value is not one.
pub(crate) fn status(value: &Value) -> Option<&str> {
  value.get(STATUS_FIELD).and_then(Value::as_str)
}
