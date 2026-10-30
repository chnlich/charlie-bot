//! Python-value stringification for event fields: `str(value)` and `str(value or '')`.

use charliebot_core::text_repr::is_truthy;
use serde_json::Value;

pub(crate) fn truthy_string(value: Option<&Value>) -> String {
  if value.is_some_and(is_truthy) { value_str(value) } else { String::new() }
}

pub(crate) fn value_str(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "None".to_string(),
    Some(Value::String(value)) => value.clone(),
    Some(Value::Bool(true)) => "True".to_string(),
    Some(Value::Bool(false)) => "False".to_string(),
    Some(value) => value.to_string(),
  }
}
