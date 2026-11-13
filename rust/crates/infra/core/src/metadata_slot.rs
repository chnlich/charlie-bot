/// The file name of a session's, a run's and a thread's metadata. [`MetadataFile`] names the kinds that hold it.
pub const METADATA_FILE_NAME: &str = "metadata.json";

/// One package's top-level keys in a metadata file.
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

/// The two thread fields a chat platform's session slot carries, under the platform's own key names: the origin
/// object, and the follow watermark beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSlot {
  /// The origin key: [`valid_origin`]'s object of `origin_keys` strings, or null.
  pub origin_field: &'static str,
  /// The origin type as a refusal names it, such as `DiscordOrigin`.
  pub origin_type: &'static str,
  /// The origin's own keys. [`valid_origin`] drops every other key the stored object carries.
  pub origin_keys: &'static [&'static str],
  /// The follow watermark key: a string or null.
  pub watermark_field: &'static str,
}

/// The refusal when `values` carries an origin that is not an object of its keys' strings, or a watermark that is
/// neither null nor a string; `None` when both hold.
pub fn validate_thread_slot(
  values: &mut serde_json::Map<String, serde_json::Value>,
  slot: &ThreadSlot,
) -> Option<String> {
  if let Some(origin) = values.get_mut(slot.origin_field)
    && !valid_origin(origin, slot.origin_keys)
  {
    return Some(format!("{} must be a {} or null", slot.origin_field, slot.origin_type));
  }
  if values.get(slot.watermark_field).is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some(format!("{} must be a string or null", slot.watermark_field));
  }
  None
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

  #[test]
  fn validate_thread_slot_refuses_a_bad_origin_or_watermark_and_keeps_the_origin_keys() {
    const SLOT: ThreadSlot = ThreadSlot {
      origin_field: "discord_origin",
      origin_type: "DiscordOrigin",
      origin_keys: &["guild_id", "thread_id"],
      watermark_field: "discord_watermark_id",
    };

    let sample = json!({"discord_origin": {"guild_id": "g", "thread_id": "t", "extra": "x"}});
    let mut values = sample.as_object().expect("the sample slot is an object").clone();
    assert_eq!(validate_thread_slot(&mut values, &SLOT), None);
    assert_eq!(values, *json!({"discord_origin": {"guild_id": "g", "thread_id": "t"}}).as_object().unwrap());

    let mut missing_key = json!({"discord_origin": {"guild_id": "g"}});
    assert_eq!(
      validate_thread_slot(missing_key.as_object_mut().expect("the sample slot is an object"), &SLOT),
      Some("discord_origin must be a DiscordOrigin or null".to_string())
    );

    let mut bad_watermark = json!({"discord_watermark_id": 7});
    assert_eq!(
      validate_thread_slot(bad_watermark.as_object_mut().expect("the sample slot is an object"), &SLOT),
      Some("discord_watermark_id must be a string or null".to_string())
    );
  }
}
