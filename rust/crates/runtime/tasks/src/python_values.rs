//! Python-value stringification for event fields: `str(value)` and `str(value or '')`.

use serde_json::Value;

pub(crate) fn truthy_string(value: Option<&Value>) -> String {
  if python_truthy(value) { python_string(value) } else { String::new() }
}

pub(crate) fn python_truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(value)) => *value,
    Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(value)) => !value.is_empty(),
    Some(Value::Object(value)) => !value.is_empty(),
  }
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
