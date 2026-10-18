//! `src/features/web_reply`: registers the web-reply status field of each session. The rest of the package is not
//! ported yet.

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};

/// `src/features/web_reply`: the web-reply status of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "web_reply",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "web_status", default: serde_json::Value::Null }],
  })
}
