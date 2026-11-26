//! The `agy` JSON envelope of the usage reading: one object whose status field names the reading's outcome.

use serde_json::Value;

/// The envelope field that names the turn's status.
const STATUS_FIELD: &str = "status";

/// The status a successful `agy` turn carries.
pub(crate) const SUCCESS: &str = "SUCCESS";

/// The status an `agy` JSON envelope value carries, or `None` when the value is not one.
pub(crate) fn status(value: &Value) -> Option<&str> {
  value.get(STATUS_FIELD).and_then(Value::as_str)
}
