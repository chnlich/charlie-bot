//! Python value semantics the codex port reproduces: `str.splitlines`, `str` slicing and truthiness.

use std::sync::LazyLock;

use serde_json::{Map, Value};

pub use charliebot_agents::cli_base::truthy;

/// The empty object a missing `x.get(key, {})` falls back to.
static EMPTY_OBJECT: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);

/// The empty object a missing `x.get(key, {})` falls back to.
pub fn empty_object() -> &'static Map<String, Value> {
  &EMPTY_OBJECT
}

/// Python's `str.splitlines()`: the lines of `text` without their terminators. The terminators are wider than `\n`.
pub fn splitlines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((index, c)) = chars.next() {
    let terminator = match c {
      '\r' if matches!(chars.peek(), Some((_, '\n'))) => {
        chars.next();
        2
      }
      '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
        c.len_utf8()
      }
      _ => continue,
    };
    lines.push(&text[start..index]);
    start = index + terminator;
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

/// Python's `s[n:]` for a count of code points: the text after its first `n` characters.
pub fn slice_from(text: &str, skip: usize) -> String {
  text.chars().skip(skip).collect()
}

/// Python's `len(s)` for a string: its code point count.
pub fn code_point_len(text: &str) -> usize {
  text.chars().count()
}

/// `value.get(key)` of a Python dict where a missing or falsy value reads as absent: `value.get(key) or {}`.
pub fn object_or_empty<'a>(map: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a Map<String, Value>> {
  match map.get(key) {
    None => Ok(&EMPTY_OBJECT),
    Some(value) if !truthy(value) => Ok(&EMPTY_OBJECT),
    Some(Value::Object(object)) => Ok(object),
    Some(other) => anyhow::bail!("codex {key} must be a JSON object, got {other}"),
  }
}

/// A count field read the way Python's `value.get(key) or 0` reads it: absent or falsy is 0, an integer is itself.
pub fn count_or_zero(map: &Map<String, Value>, key: &str) -> anyhow::Result<i64> {
  match map.get(key) {
    None => Ok(0),
    Some(value) if !truthy(value) => Ok(0),
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
  fn splitlines_breaks_where_str_does() {
    assert_eq!(splitlines("a\nb\r\nc\rd"), ["a", "b", "c", "d"]);
    assert_eq!(splitlines("x\u{2028}y\u{85}z\n"), ["x", "y", "z"]);
    assert_eq!(splitlines("\n"), [""]);
    assert!(splitlines("").is_empty());
  }

  #[test]
  fn strip_takes_the_information_separators_too() {
    assert_eq!(str_methods::strip("\u{1c} padded \u{a0}"), "padded");
  }

  #[test]
  fn truthiness_follows_value_rules() {
    assert!(!truthy(&json!(0)));
    assert!(!truthy(&json!("")));
    assert!(!truthy(&json!([])));
    assert!(truthy(&json!({"a": 1})));
    assert!(truthy(&json!(0.5)));
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
