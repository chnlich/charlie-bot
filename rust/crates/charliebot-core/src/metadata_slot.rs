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
