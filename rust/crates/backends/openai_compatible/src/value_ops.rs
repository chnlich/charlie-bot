//! The value operations the proxy's translations use: truthiness, key lookup, iteration, subscripts, and the error
//! each misuse raises. A translation runs over whatever JSON the client sent, so each operation answers on a value
//! that `charliebot_core::json_loads` produced, and each error carries the text the route reports.

use charliebot_core::json_loads::{self, LoadError};
use charliebot_core::text_repr::{JsonStyle, is_truthy, type_name};
use serde_json::{Map, Value};

/// Compact JSON with ASCII output: `,` and `:` separators, and every character outside the range from space to `~`
/// escaped.
pub const COMPACT_ASCII_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// The fault a translation raises. A value error is answered with a status and its text; any other fault is
/// unhandled, which answers 500 and names its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
  Value(String),
  Raised { kind: &'static str, message: String },
}

pub type ValueResult<T> = Result<T, Fault>;

pub fn value_error<T>(message: impl Into<String>) -> ValueResult<T> {
  Err(Fault::Value(message.into()))
}

pub fn raised<T>(kind: &'static str, message: impl Into<String>) -> ValueResult<T> {
  Err(Fault::Raised { kind, message: message.into() })
}

/// The member `key` of an object: `None` for a missing key, and a present `null` is `Some(Null)`. Any other value
/// is an error.
pub fn get<'a>(value: &'a Value, key: &str) -> ValueResult<Option<&'a Value>> {
  match value {
    Value::Object(members) => Ok(members.get(key)),
    other => raised("AttributeError", format!("'{}' object has no attribute 'get'", type_name(other))),
  }
}

/// The member `key` when it is truthy, else an empty object. Used where a translation reads an optional object.
pub fn get_or_empty(value: &Value, key: &str) -> ValueResult<Value> {
  Ok(get(value, key)?.filter(|member| is_truthy(member)).cloned().unwrap_or_else(|| Value::Object(Map::new())))
}

/// The items of a value: a list yields its items, an object its keys, a string its characters, and other values raise.
pub fn iterate(value: &Value) -> ValueResult<Vec<Value>> {
  match value {
    Value::Array(items) => Ok(items.clone()),
    Value::Object(members) => Ok(members.keys().map(|key| Value::String(key.clone())).collect()),
    Value::String(text) => Ok(text.chars().map(|character| Value::String(character.to_string())).collect()),
    other => raised("TypeError", format!("'{}' object is not iterable", type_name(other))),
  }
}

/// The items of a member that may be absent: a missing or falsy member has none.
pub fn iterate_member(members: Option<&Value>) -> ValueResult<Vec<Value>> {
  match members.filter(|member| is_truthy(member)) {
    Some(member) => iterate(member),
    None => Ok(Vec::new()),
  }
}

/// The Python meaning of `value[0]` for a truthy value: a list's first element or a string's first character, and a
/// `KeyError` for an object.
pub fn first_item(value: &Value) -> ValueResult<Value> {
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

/// The value as an object, or a value error that names `label`.
pub fn require_dict<'a>(value: &'a Value, label: &str) -> ValueResult<&'a Map<String, Value>> {
  match value {
    Value::Object(members) => Ok(members),
    _ => value_error(format!("{label} must be an object")),
  }
}

/// The value parsed from JSON text, with the fault a load failure raises.
pub fn loads_text(text: &str) -> ValueResult<Value> {
  json_loads::loads_text(text).map_err(fault_of_load)
}

/// The value parsed from JSON bytes, with the fault a load failure raises.
pub fn loads_bytes(body: &[u8]) -> ValueResult<Value> {
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
  fn truthiness_follows_value_rules() {
    for falsy in [json!(null), json!(false), json!(0), json!(0.0), json!(""), json!([]), json!({})] {
      assert!(!is_truthy(&falsy), "{falsy} is falsy");
    }
    for truthy_value in [json!(true), json!(1), json!(-1), json!("0"), json!([0]), json!({"k": null})] {
      assert!(is_truthy(&truthy_value), "{truthy_value} is truthy");
    }
  }

  #[test]
  fn misuses_raise_the_exception_class() {
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
