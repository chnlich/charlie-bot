//! Python-value stringification for event fields: `str(value)` and `str(value or '')`.

use charliebot_core::pyfmt::python_truthy;
use serde_json::Value;

pub(crate) fn truthy_string(value: Option<&Value>) -> String {
  if value.is_some_and(python_truthy) { python_string(value) } else { String::new() }
}

pub(crate) fn python_string(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "None".to_string(),
    Some(Value::String(value)) => value.clone(),
    Some(Value::Bool(true)) => "True".to_string(),
    Some(Value::Bool(false)) => "False".to_string(),
    Some(value) => value.to_string(),
  }
}
