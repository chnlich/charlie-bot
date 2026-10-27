//! A parsed JSON value whose object keys are interned across the process and whose objects and
//! arrays are stored without growth slack.
//!
//! `serde_json::Value` gives every parsed chat event a fresh `String` per object key inside an
//! `IndexMap` with hash-table and entry-vector slack, which is the measured representation gap
//! against Python's parsed events. This module keeps the same JSON semantics — insertion order,
//! the three `serde_json::Number` forms, and Python-compatible access — while sharing each
//! distinct key once and storing each object as one exact-fit `Box<[ObjectEntry]>`.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use serde_json::Value;

/// One interned object key's name. Every object entry that uses the key holds an `Arc` to it, so the key
/// lives exactly as long as some cached event uses it.
#[derive(Debug, PartialEq)]
pub struct KeyName(Box<str>);

impl KeyName {
  /// The key's name.
  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.0
  }
}

/// An interned object key, shared by every object entry that uses it.
pub type Key = Arc<KeyName>;

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

/// The process-wide object-key table: each live key's name, with a weak reference to the key. A key leaves
/// the table when its last object entry drops, so the table holds only keys that some cached event uses.
type KeyTable = HashMap<Box<str>, Weak<KeyName>>;

#[cfg(test)]
thread_local! {
  static TABLE_LOCKS: Cell<usize> = const { Cell::new(0) };
  static OBJECT_COPIES: Cell<usize> = const { Cell::new(0) };
}

fn key_table() -> &'static Mutex<KeyTable> {
  static TABLE: OnceLock<Mutex<KeyTable>> = OnceLock::new();
  TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_key_table() -> MutexGuard<'static, KeyTable> {
  #[cfg(test)]
  TABLE_LOCKS.with(|count| count.set(count.get() + 1));
  key_table().lock().expect("object key table mutex is not poisoned")
}

/// Return the interned key for `name`, interning it when no live object entry uses it yet.
pub fn intern_key(name: &str) -> Key {
  let mut table = lock_key_table();
  if let Some(key) = table.get(name).and_then(Weak::upgrade) {
    return key;
  }
  let key = Arc::new(KeyName(name.into()));
  table.insert(name.into(), Arc::downgrade(&key));
  key
}

impl Drop for KeyName {
  fn drop(&mut self) {
    // The last entry that used this key is gone. A concurrent intern may already have replaced the table entry with a
    // live key of the same name, and that key stays.
    let mut table = lock_key_table();
    if table.get(self.0.as_ref()).is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self)) {
      table.remove(self.0.as_ref());
    }
  }
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

/// Deserializes straight from the parser's bytes: one pass, no `serde_json::Value` intermediate.
///
/// The cold event-log read used to parse each line into a `Value` and re-walk it through
/// [`CompactValue::from_value`], allocating the whole tree twice; the visitor below builds the compact
/// form as the parser scans, interning each object key once and sizing every container from the
/// parser's hint. The accepted grammar and the resulting values are serde_json's own, so a document
/// this impl accepts is exactly one `from_value(serde_json::from_str(..))` would have produced.
impl<'de> serde::Deserialize<'de> for CompactValue {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct CompactVisitor;

    impl<'de> serde::de::Visitor<'de> for CompactVisitor {
      type Value = CompactValue;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value")
      }

      fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Self::Value::Bool(value))
      }

      fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Self::Value::NegInt(value))
      }

      fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Self::Value::PosInt(value))
      }

      fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(Self::Value::Float(value))
      }

      fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Self::Value::Str(value.into()))
      }

      fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(Self::Value::Null)
      }

      fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element::<Self::Value>()? {
          items.push(item);
        }
        Ok(Self::Value::Array(items.into_boxed_slice()))
      }

      fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut entries: Vec<ObjectEntry> = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(key) = access.next_key_seed(KeySeed)? {
          let value = access.next_value::<Self::Value>()?;
          // A repeated key keeps its first position and takes the last value, as serde_json's map
          // insert (and Python's dict) does.
          match entries.iter_mut().find(|entry| entry.key == key) {
            Some(entry) => entry.value = value,
            None => entries.push(ObjectEntry { key, value }),
          }
        }
        Ok(Self::Value::Object(entries.into_boxed_slice()))
      }
    }

    deserializer.deserialize_any(CompactVisitor)
  }
}

/// The map key's seed: interns the name as the parser scans, so a repeated key never allocates its
/// name twice (most keys repeat across a log's events).
struct KeySeed;

impl<'de> serde::de::DeserializeSeed<'de> for KeySeed {
  type Value = Key;

  fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct KeyVisitor;

    impl<'de> serde::de::Visitor<'de> for KeyVisitor {
      type Value = Key;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an object key")
      }

      fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(intern_key(value))
      }
    }

    deserializer.deserialize_str(KeyVisitor)
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
        #[cfg(test)]
        OBJECT_COPIES.with(|count| count.set(count.get() + 1));
        let mut fields = serde_json::Map::with_capacity(entries.len());
        for entry in entries.iter() {
          fields.insert(entry.key.as_str().to_string(), entry.value.to_value());
        }
        Value::Object(fields)
      }
    }
  }

  /// The value of an object field, or `None` when this is not an object with that key. Reads take no lock:
  /// the name is compared with each entry's key, which owns its name.
  #[must_use]
  pub fn get(&self, name: &str) -> Option<&Self> {
    let Self::Object(entries) = self else { return None };
    entries.iter().find(|entry| entry.key.as_str() == name).map(|entry| &entry.value)
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
    let value = Self::from_value(&value);
    let Self::Object(entries) = self else { panic!("insert targets an object") };
    if let Some(slot) = entries.iter_mut().find(|entry| entry.key.as_str() == name) {
      slot.value = value;
      return;
    }
    let key = intern_key(name);
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
    let CompactValue::Object(entries) = self else { unreachable!("checked above") };
    let position = entries.iter().position(|entry| entry.key.as_str() == name);
    match position {
      Some(position) => {
        let CompactValue::Object(entries) = self else { unreachable!("still an object") };
        &mut entries[position].value
      }
      None => {
        let key = intern_key(name);
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

  fn is_null(&self) -> bool {
    matches!(self, Self::Null)
  }

  fn as_bool_field(&self) -> Option<bool> {
    self.as_bool()
  }

  fn as_i64_field(&self) -> Option<i64> {
    self.as_i64()
  }

  fn as_u64_field(&self) -> Option<u64> {
    self.as_u64()
  }

  fn as_f64_field(&self) -> Option<f64> {
    self.as_f64()
  }

  fn array_items(&self) -> Option<&[Self]> {
    self.as_array()
  }

  fn object_len(&self) -> Option<usize> {
    match self {
      Self::Object(entries) => Some(entries.len()),
      _ => None,
    }
  }

  fn to_owned_value(&self) -> Value {
    self.to_value()
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

/// Whether the key table holds an entry for `name`.
#[cfg(test)]
fn key_table_contains(name: &str) -> bool {
  lock_key_table().contains_key(name)
}

/// The key-table lock acquisitions made on this thread.
#[cfg(test)]
fn table_lock_count() -> usize {
  TABLE_LOCKS.with(Cell::get)
}

/// The owned object copies made on this thread by [`CompactValue::to_value`].
#[cfg(test)]
pub(crate) fn object_copy_count() -> usize {
  OBJECT_COPIES.with(Cell::get)
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
    let names: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
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
    assert!(
      Arc::ptr_eq(&first_entries[0].key, &second_entries[0].key),
      "the same key interns to one shared allocation"
    );
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

  #[test]
  fn a_field_read_by_name_takes_no_key_table_lock() {
    let event = CompactValue::from_value(&json!({"type": "user", "content": "hi", "id": "e1"}));
    let before = table_lock_count();
    for _ in 0..100 {
      assert!(event.get("content").is_some());
      assert!(event.get("missing").is_none());
      assert_eq!(event["type"], "user");
    }
    assert_eq!(table_lock_count() - before, 0, "a field read by name takes no key-table lock");

    let before = table_lock_count();
    let key = intern_key("a key that no event has read yet");
    assert_eq!(table_lock_count() - before, 1, "interning takes the key-table lock once");
    drop(key);
  }

  #[test]
  fn a_key_leaves_the_table_with_the_last_event_that_uses_it() {
    let name = "a key freed with its last event";
    let mut fields = serde_json::Map::new();
    fields.insert(name.to_string(), json!(1));
    let first = CompactValue::from_value(&Value::Object(fields));
    let second = first.clone();
    assert!(key_table_contains(name));
    drop(first);
    assert!(key_table_contains(name), "a clone of the event still uses the key");
    drop(second);
    assert!(!key_table_contains(name), "the key's storage is freed with its last event");
  }
}
