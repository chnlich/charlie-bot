//! The value helpers the codex code reads: `splitlines` and `is_truthy`, each re-exported from its one owner.

use std::sync::LazyLock;

use serde_json::{Map, Value};

pub use charliebot_core::str_methods::splitlines;
pub use charliebot_core::text_repr::is_truthy;

/// The empty object a missing key falls back to.
static EMPTY_OBJECT: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

/// The empty object a missing key falls back to.
pub fn empty_object() -> &'static Map<String, Value> {
  &EMPTY_OBJECT
}

/// The object under `key`: a missing or falsy value reads as an empty object, and any other non-object is an error.
pub fn object_or_empty<'a>(map: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a Map<String, Value>> {
  match map.get(key) {
    None => Ok(&EMPTY_OBJECT),
    Some(value) if !is_truthy(value) => Ok(&EMPTY_OBJECT),
    Some(Value::Object(object)) => Ok(object),
    Some(other) => anyhow::bail!("codex {key} must be a JSON object, got {other}"),
  }
}

/// A count field: absent or falsy is 0, an integer is itself, and anything else is an error.
pub fn count_or_zero(map: &Map<String, Value>, key: &str) -> anyhow::Result<i64> {
  match map.get(key) {
    None => Ok(0),
    Some(value) if !is_truthy(value) => Ok(0),
    Some(Value::Number(number)) => {
      number.as_i64().ok_or_else(|| anyhow::anyhow!("codex count {key} must be an integer, got {number}"))
    }
    Some(other) => anyhow::bail!("codex count {key} must be an integer, got {other}"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::str_methods;
  use serde_json::json;

  #[test]
  fn strip_takes_the_information_separators_too() {
    assert_eq!(str_methods::strip("\u{1c} padded \u{a0}"), "padded");
  }

  #[test]
  fn count_fields_default_to_zero_and_refuse_text() {
    let map = json!({"a": 3, "b": null, "c": "x"});
    let map = map.as_object().unwrap();
    assert_eq!(count_or_zero(map, "a").unwrap(), 3);
    assert_eq!(count_or_zero(map, "b").unwrap(), 0);
    assert_eq!(count_or_zero(map, "missing").unwrap(), 0);
    assert!(count_or_zero(map, "c").is_err());
  }
}
