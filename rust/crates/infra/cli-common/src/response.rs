//! The server response shape a verb reads: a missing field is a broken server contract.

use serde_json::Value;

use crate::exit::CliExit;

/// The response field the verb reads. A missing one is a broken server contract.
pub fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, CliExit> {
  value.get(key).ok_or_else(|| CliExit::error(format!("the response lacks {key}")))
}
