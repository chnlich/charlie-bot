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
