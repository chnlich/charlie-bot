/// The file name of a session's, a run's and a thread's metadata. [`MetadataFile`] names the kinds that hold it.
pub const METADATA_FILE_NAME: &str = "metadata.json";

/// One package's top-level keys in a metadata file (src/infra/metadata_slot_registration.py).
pub type MetadataSlotValidator = fn(&mut serde_json::Map<String, serde_json::Value>) -> Option<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataFile {
  Session,
  Thread,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SlotField {
  pub name: &'static str,
  pub default: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetadataSlot {
  pub owner: &'static str,
  pub on: MetadataFile,
  pub after: Option<&'static str>,
  pub fields: Vec<SlotField>,
}

/// Whether `value` is null, or an object whose `fields` are all strings, keeping only those fields.
///
/// A session's origin names the chat thread the session belongs to. A validator drops unlisted keys so the stored
/// origin carries nothing beyond the platform's own fields.
pub fn valid_origin(value: &mut serde_json::Value, fields: &[&str]) -> bool {
  if value.is_null() {
    return true;
  }
  let Some(object) = value.as_object_mut() else { return false };
  let valid = fields.iter().all(|name| object.get(*name).is_some_and(serde_json::Value::is_string));
  if valid {
    object.retain(|name, _| fields.contains(&name.as_str()));
  }
  valid
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  #[test]
  fn valid_origin_accepts_null_and_keeps_only_the_named_string_fields() {
    let mut missing_field = json!({"guild_id": "g"});
    assert!(!valid_origin(&mut missing_field, &["guild_id", "thread_id"]));

    let mut non_string = json!({"guild_id": "g", "thread_id": 7});
    assert!(!valid_origin(&mut non_string, &["guild_id", "thread_id"]));

    let mut not_object = json!("guild");
    assert!(!valid_origin(&mut not_object, &["guild_id"]));

    let mut null = serde_json::Value::Null;
    assert!(valid_origin(&mut null, &["guild_id"]));

    let mut origin = json!({"guild_id": "g", "thread_id": "t", "extra": "x"});
    assert!(valid_origin(&mut origin, &["guild_id", "thread_id"]));
    assert_eq!(origin, json!({"guild_id": "g", "thread_id": "t"}));
  }
}
