//! The Python semantics the proxy's translations rest on: truthiness, `dict.get`, iteration, subscripts, and the
//! exception each misuse raises. A translation runs over whatever JSON the client sent, so each operation here
//! answers what the same operation answers on the value `json.loads` produced.

use charliebot_compat::json_loads::{self, LoadError};
use charliebot_core::text_repr::JsonStyle;
use serde_json::{Map, Value};

/// `json.dumps(value, separators=(",", ":"))` with the default `ensure_ascii`: characters above `~` are escaped.
pub const COMPACT_ASCII_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// The exception a translation raises. A `ValueError` is answered with a status and its text; any other exception is
/// an unhandled one, which answers 500 and names its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
  Value(String),
  Raised { kind: &'static str, message: String },
}

pub type PyResult<T> = Result<T, Fault>;

pub fn value_error<T>(message: impl Into<String>) -> PyResult<T> {
  Err(Fault::Value(message.into()))
}

pub fn raised<T>(kind: &'static str, message: impl Into<String>) -> PyResult<T> {
  Err(Fault::Raised { kind, message: message.into() })
}

/// The class name of a value as `json.loads` makes it.
pub fn type_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(number) if number.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// Python truthiness: `None`, `False`, zero, and the empty str, list and dict are falsy.
pub fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// `value.get(key)` on a dict: `None` for a missing key, and a present `null` is `Some(Null)`.
pub fn get<'a>(value: &'a Value, key: &str) -> PyResult<Option<&'a Value>> {
  match value {
    Value::Object(members) => Ok(members.get(key)),
    other => raised("AttributeError", format!("'{}' object has no attribute 'get'", type_name(other))),
  }
}

/// `value.get(key) or {}`: the member when it is truthy, else an empty dict. Used where Python writes `or {}`.
pub fn get_or_empty(value: &Value, key: &str) -> PyResult<Value> {
  Ok(get(value, key)?.filter(|member| truthy(member)).cloned().unwrap_or_else(|| Value::Object(Map::new())))
}

/// `for item in value`: a list yields its items, a dict its keys, a str its characters, and other values raise.
pub fn iterate(value: &Value) -> PyResult<Vec<Value>> {
  match value {
    Value::Array(items) => Ok(items.clone()),
    Value::Object(members) => Ok(members.keys().map(|key| Value::String(key.clone())).collect()),
    Value::String(text) => Ok(text.chars().map(|character| Value::String(character.to_string())).collect()),
    other => raised("TypeError", format!("'{}' object is not iterable", type_name(other))),
  }
}

/// `iterate(value)` for a member that may be absent: `value or []`, so a missing or falsy member iterates nothing.
pub fn iterate_member(members: Option<&Value>) -> PyResult<Vec<Value>> {
  match members.filter(|member| truthy(member)) {
    Some(member) => iterate(member),
    None => Ok(Vec::new()),
  }
}

/// `value[0]`, for a value already known to be truthy.
pub fn first_item(value: &Value) -> PyResult<Value> {
  match value {
    Value::Array(items) => match items.first() {
      Some(item) => Ok(item.clone()),
      None => raised("IndexError", "list index out of range"),
    },
    Value::Object(_) => raised("KeyError", "0"),
    Value::String(text) => match text.chars().next() {
      Some(character) => Ok(Value::String(character.to_string())),
      None => raised("IndexError", "string index out of range"),
    },
    other => raised("TypeError", format!("'{}' object is not subscriptable", type_name(other))),
  }
}

/// `_require_dict(value, label)`: the value as a dict, or a `ValueError` naming `label`.
pub fn require_dict<'a>(value: &'a Value, label: &str) -> PyResult<&'a Map<String, Value>> {
  match value {
    Value::Object(members) => Ok(members),
    _ => value_error(format!("{label} must be an object")),
  }
}

/// `json.loads(text)` on a str, with the exception a load failure raises.
pub fn loads_text(text: &str) -> PyResult<Value> {
  json_loads::loads_text(text).map_err(fault_of_load)
}

/// `json.loads(body)` on bytes, with the exception a load failure raises.
pub fn loads_bytes(body: &[u8]) -> PyResult<Value> {
  json_loads::loads(body).map_err(fault_of_load)
}

fn fault_of_load(error: LoadError) -> Fault {
  match error {
    LoadError::Value(text) => Fault::Value(text),
    LoadError::Unrepresentable(text) => Fault::Raised { kind: "Unrepresentable", message: text },
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn truthiness_follows_python() {
    for falsy in [json!(null), json!(false), json!(0), json!(0.0), json!(""), json!([]), json!({})] {
      assert!(!truthy(&falsy), "{falsy} is falsy");
    }
    for truthy_value in [json!(true), json!(1), json!(-1), json!("0"), json!([0]), json!({"k": null})] {
      assert!(truthy(&truthy_value), "{truthy_value} is truthy");
    }
  }

  #[test]
  fn misuses_raise_the_python_exception_class() {
    assert_eq!(
      get(&json!([1]), "k").unwrap_err(),
      Fault::Raised { kind: "AttributeError", message: "'list' object has no attribute 'get'".to_string() }
    );
    assert_eq!(
      iterate(&json!(5)).unwrap_err(),
      Fault::Raised { kind: "TypeError", message: "'int' object is not iterable".to_string() }
    );
    assert_eq!(iterate(&json!("ab")).unwrap(), vec![json!("a"), json!("b")]);
    assert_eq!(first_item(&json!({"0": 1})).unwrap_err(), Fault::Raised { kind: "KeyError", message: "0".into() });
  }
}
