//! A parsed JSON value whose object keys are interned across the process and whose objects and
//! arrays are stored without growth slack.
//!
//! `serde_json::Value` gives every parsed chat event a fresh `String` per object key inside an
//! `IndexMap` with hash-table and entry-vector slack, which is the measured representation gap
//! against Python's parsed events. This module keeps the same JSON semantics — insertion order,
//! the three `serde_json::Number` forms, and Python-compatible access — while sharing each
//! distinct key once and storing each object as one exact-fit `Box<[ObjectEntry]>`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// One interned object key: an index into the process-wide key table.
pub type Key = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum CompactValue {
  Null,
  Bool(bool),
  /// A non-negative JSON integer, matching `serde_json::Number::PosInt`.
  PosInt(u64),
  /// A negative JSON integer, matching `serde_json::Number::NegInt`.
  NegInt(i64),
  /// A JSON float, matching `serde_json::Number::Float`.
  Float(f64),
  Str(Box<str>),
  Array(Box<[CompactValue]>),
  Object(Box<[ObjectEntry]>),
}

/// One object field: an interned key and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectEntry {
  pub key: Key,
  pub value: CompactValue,
}

/// The static Null that `Index` returns for a missing key, as serde_json's `Index` does.
const NULL: CompactValue = CompactValue::Null;

/// The process-wide object-key table. Keys are interned once and never removed, so a key's name
/// outlives every event that references it.
struct KeyTable {
  ids: HashMap<&'static str, Key>,
  names: Vec<&'static str>,
}

fn key_table() -> &'static Mutex<KeyTable> {
  static TABLE: OnceLock<Mutex<KeyTable>> = OnceLock::new();
  TABLE.get_or_init(|| Mutex::new(KeyTable { ids: HashMap::new(), names: Vec::new() }))
}

/// Return the id of `name`, interning it when this is the first use.
pub fn intern_key(name: &str) -> Key {
  let mut table = key_table().lock().expect("object key table mutex is not poisoned");
  if let Some(key) = table.ids.get(name) {
    return *key;
  }
  let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
  let key = table.names.len() as Key;
  table.names.push(leaked);
  table.ids.insert(leaked, key);
  key
}

/// The name of an interned key.
#[must_use]
pub fn key_name(key: Key) -> &'static str {
  // Every id comes from `intern_key`, whose entries live for the process lifetime.
  key_table().lock().expect("object key table mutex is not poisoned").names[key as usize]
}

impl AsRef<CompactValue> for CompactValue {
  fn as_ref(&self) -> &Self {
    self
  }
}

impl From<Value> for CompactValue {
  fn from(value: Value) -> Self {
    Self::from_value(&value)
  }
}

/// String comparisons mirror `serde_json::Value`'s: any non-string form is unequal.
impl PartialEq<str> for CompactValue {
  fn eq(&self, other: &str) -> bool {
    self.as_str() == Some(other)
  }
}

impl PartialEq<&str> for CompactValue {
  fn eq(&self, other: &&str) -> bool {
    self == *other
  }
}

impl PartialEq<String> for CompactValue {
  fn eq(&self, other: &String) -> bool {
    self == other.as_str()
  }
}

impl CompactValue {
  /// Convert a parsed `serde_json::Value`, keeping insertion order and each number's form.
  #[must_use]
  pub fn from_value(value: &Value) -> Self {
    match value {
      Value::Null => Self::Null,
      Value::Bool(flag) => Self::Bool(*flag),
      Value::Number(number) => Self::from_number(number),
      Value::String(text) => Self::Str(text.as_str().into()),
      Value::Array(items) => Self::Array(items.iter().map(Self::from_value).collect()),
      Value::Object(fields) => Self::Object(
        fields
          .iter()
          .map(|(key, value)| ObjectEntry { key: intern_key(key), value: Self::from_value(value) })
          .collect(),
      ),
    }
  }

  fn from_number(number: &serde_json::Number) -> Self {
    if let Some(unsigned) = number.as_u64() {
      Self::PosInt(unsigned)
    } else if let Some(signed) = number.as_i64() {
      Self::NegInt(signed)
    } else {
      Self::Float(number.as_f64().expect("a parsed JSON number is a u64, an i64, or a finite f64"))
    }
  }

  /// Convert back to a `serde_json::Value` for the response boundary, where the Python
  /// `json.dumps` formatter runs.
  #[must_use]
  pub fn to_value(&self) -> Value {
    match self {
      Self::Null => Value::Null,
      Self::Bool(flag) => Value::Bool(*flag),
      Self::PosInt(unsigned) => Value::from(*unsigned),
      Self::NegInt(signed) => Value::from(*signed),
      Self::Float(float) => Value::Number(serde_json::Number::from_f64(*float).expect("a parsed JSON float is finite")),
      Self::Str(text) => Value::String(text.to_string()),
      Self::Array(items) => Value::Array(items.iter().map(Self::to_value).collect()),
      Self::Object(entries) => {
        let mut fields = serde_json::Map::with_capacity(entries.len());
        for entry in entries.iter() {
          fields.insert(key_name(entry.key).to_string(), entry.value.to_value());
        }
        Value::Object(fields)
      }
    }
  }

  /// The value of an object field, or `None` when this is not an object with that key.
  #[must_use]
  pub fn get(&self, name: &str) -> Option<&Self> {
    let key = key_table().lock().expect("object key table mutex is not poisoned").ids.get(name).copied()?;
    let Self::Object(entries) = self else { return None };
    entries.iter().find(|entry| entry.key == key).map(|entry| &entry.value)
  }

  #[must_use]
  pub fn as_str(&self) -> Option<&str> {
    match self {
      Self::Str(text) => Some(text),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_bool(&self) -> Option<bool> {
    match self {
      Self::Bool(flag) => Some(*flag),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_i64(&self) -> Option<i64> {
    match self {
      Self::PosInt(unsigned) => i64::try_from(*unsigned).ok(),
      Self::NegInt(signed) => Some(*signed),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_u64(&self) -> Option<u64> {
    match self {
      Self::PosInt(unsigned) => Some(*unsigned),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_f64(&self) -> Option<f64> {
    match self {
      Self::PosInt(unsigned) => Some(*unsigned as f64),
      Self::NegInt(signed) => Some(*signed as f64),
      Self::Float(float) => Some(*float),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_array(&self) -> Option<&[Self]> {
    match self {
      Self::Array(items) => Some(items),
      _ => None,
    }
  }

  #[must_use]
  pub fn as_object(&self) -> Option<&[ObjectEntry]> {
    match self {
      Self::Object(entries) => Some(entries),
      _ => None,
    }
  }

  #[must_use]
  pub fn is_object(&self) -> bool {
    matches!(self, Self::Object(_))
  }

  #[must_use]
  pub fn is_null(&self) -> bool {
    matches!(self, Self::Null)
  }

  /// Insert or replace one object field in Python's dict order: an existing key keeps its position.
  ///
  /// # Panics
  /// Panics when this is not an object; call [`CompactValue::is_object`] first.
  pub fn insert(&mut self, name: &str, value: Value) {
    let key = intern_key(name);
    let value = Self::from_value(&value);
    let Self::Object(entries) = self else { panic!("insert targets an object") };
    if let Some(slot) = entries.iter_mut().find(|entry| entry.key == key) {
      slot.value = value;
      return;
    }
    let mut grown = std::mem::take(entries).into_vec();
    grown.push(ObjectEntry { key, value });
    *entries = grown.into_boxed_slice();
  }

  #[must_use]
  pub fn is_string(&self) -> bool {
    matches!(self, Self::Str(_))
  }

  /// The element count of an array or object; other forms have no elements.
  #[must_use]
  pub fn len(&self) -> usize {
    match self {
      Self::Array(items) => items.len(),
      Self::Object(entries) => entries.len(),
      _ => 0,
    }
  }

  /// Whether the value holds no elements; scalars are empty.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl std::ops::IndexMut<&str> for CompactValue {
  /// Return the field's slot, inserting a `Null` field when missing, as serde_json's `IndexMut`
  /// does. Panics on a non-object, like serde_json.
  fn index_mut(&mut self, name: &str) -> &mut Self::Output {
    if !matches!(self, CompactValue::Object(_)) {
      panic!("cannot index a non-object event with a string key");
    }
    let key = intern_key(name);
    let CompactValue::Object(entries) = self else { unreachable!("checked above") };
    let position = entries.iter().position(|entry| entry.key == key);
    match position {
      Some(position) => {
        let CompactValue::Object(entries) = self else { unreachable!("still an object") };
        &mut entries[position].value
      }
      None => {
        let mut grown = std::mem::take(entries).into_vec();
        grown.push(ObjectEntry { key, value: CompactValue::Null });
        *entries = grown.into_boxed_slice();
        let CompactValue::Object(entries) = self else { unreachable!("still an object") };
        &mut entries.last_mut().expect("a pushed field exists").value
      }
    }
  }
}

impl charliebot_core::event_types::EventFields for CompactValue {
  fn field(&self, key: &str) -> Option<&Self> {
    self.get(key)
  }

  fn as_str_field(&self) -> Option<&str> {
    self.as_str()
  }

  fn is_object(&self) -> bool {
    matches!(self, Self::Object(_))
  }

  fn string_items(&self) -> Vec<&str> {
    self.as_array().map(|items| items.iter().filter_map(Self::as_str).collect()).unwrap_or_default()
  }
}

impl std::ops::Index<&str> for CompactValue {
  type Output = CompactValue;

  /// Return the field's value, or `Null` when missing or when this is not an object, matching
  /// serde_json's `Index`.
  fn index(&self, name: &str) -> &Self::Output {
    self.get(name).unwrap_or(&NULL)
  }
}

impl std::ops::Index<usize> for CompactValue {
  type Output = CompactValue;

  /// Return the element, or `Null` when out of bounds or when this is not an array, matching
  /// serde_json's `Index`.
  fn index(&self, index: usize) -> &Self::Output {
    match self.as_array() {
      Some(items) => items.get(index).unwrap_or(&NULL),
      None => &NULL,
    }
  }
}

/// The retained heap bytes of one value's own storage: its exact-fit element storage and its
/// string bytes. Interned keys cost nothing per event and are counted once by the key table.
#[must_use]
pub fn compact_capacity_bytes(value: &CompactValue) -> usize {
  match value {
    CompactValue::Str(text) => text.len(),
    CompactValue::Array(items) => {
      items.len() * std::mem::size_of::<CompactValue>() + items.iter().map(compact_capacity_bytes).sum::<usize>()
    }
    CompactValue::Object(entries) => {
      entries.len() * std::mem::size_of::<ObjectEntry>()
        + entries.iter().map(|entry| compact_capacity_bytes(&entry.value)).sum::<usize>()
    }
    _ => 0,
  }
}

/// The number of distinct interned keys and the table's own retained bytes.
#[must_use]
pub fn key_table_census() -> (usize, usize) {
  let table = key_table().lock().expect("object key table mutex is not poisoned");
  let bytes = table.ids.capacity() * (std::mem::size_of::<&str>() + std::mem::size_of::<Key>() + 8)
    + table.names.capacity() * std::mem::size_of::<&str>()
    + table.names.iter().map(|name| name.len()).sum::<usize>();
  (table.names.len(), bytes)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn from_value_keeps_order_numbers_and_strings() {
    let value = json!({"b": 1, "a": -2, "pi": 0.5, "s": "x", "n": null, "flag": true, "list": [1, "y"]});
    let compact = CompactValue::from_value(&value);
    let CompactValue::Object(entries) = &compact else { panic!("object expected") };
    let names: Vec<&str> = entries.iter().map(|entry| key_name(entry.key)).collect();
    assert_eq!(names, ["b", "a", "pi", "s", "n", "flag", "list"]);
    assert_eq!(compact["a"], CompactValue::NegInt(-2));
    assert_eq!(compact["pi"], CompactValue::Float(0.5));
    assert_eq!(compact["missing"], CompactValue::Null, "a missing key reads as Null, like serde_json");
    assert_eq!(compact["list"][1], CompactValue::Str("y".into()));
    assert_eq!(compact.to_value(), value, "the round trip keeps every field");
  }

  #[test]
  fn object_keys_are_shared_across_values() {
    let first = CompactValue::from_value(&json!({"type": "user", "content": "one"}));
    let second = CompactValue::from_value(&json!({"type": "user", "content": "two"}));
    let CompactValue::Object(first_entries) = &first else { panic!("object expected") };
    let CompactValue::Object(second_entries) = &second else { panic!("object expected") };
    assert_eq!(first_entries[0].key, second_entries[0].key, "the same key interns to one id");
    assert_eq!(
      compact_capacity_bytes(&first),
      2 * std::mem::size_of::<ObjectEntry>() + "user".len() + "one".len(),
      "interned keys add no per-event bytes; strings and entry slots are exact-fit"
    );
  }

  #[test]
  fn a_chat_event_is_exact_fit_with_no_growth_slack() {
    let value = json!({
      "type": "user", "id": "evt-1", "timestamp": "2026-10-23T00:00:00Z", "session_id": "sess",
      "event_index": 7, "content": "the question text", "parts": ["a", "b"],
    });
    let compact = CompactValue::from_value(&value);
    let string_bytes = ["user", "evt-1", "2026-10-23T00:00:00Z", "sess", "the question text", "a", "b"]
      .iter()
      .map(|text| text.len())
      .sum::<usize>();
    let expected = 7 * std::mem::size_of::<ObjectEntry>() + 2 * std::mem::size_of::<CompactValue>() + string_bytes;
    assert_eq!(
      compact_capacity_bytes(&compact),
      expected,
      "every allocation is exact-fit, like the Value form never is"
    );
  }

  #[test]
  fn index_on_non_object_reads_null() {
    let value = CompactValue::from_value(&json!([1, 2]));
    assert_eq!(value["key"], CompactValue::Null);
    assert_eq!(value[5], CompactValue::Null, "an out-of-bounds index reads as Null");
    assert_eq!(value[1], CompactValue::PosInt(2));
  }

  #[test]
  fn numbers_keep_their_serde_json_form() {
    let value = CompactValue::from_value(&json!(18446744073709551615u64));
    assert_eq!(value, CompactValue::PosInt(18446744073709551615));
    let value = CompactValue::from_value(&json!(-18446744073709551615f64));
    assert!(matches!(value, CompactValue::Float(_)));
  }
}
