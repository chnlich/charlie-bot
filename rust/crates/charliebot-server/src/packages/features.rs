//! The `src.features.cron` stub: the metadata slot of the session file. The other feature packages register from
//! their own crates under `rust/crates/features/`.

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};

/// `src/features/cron`: the scheduled-task fields of each session.
pub fn cron(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "cron",
    on: MetadataFile::Session,
    after: Some("backend"),
    fields: ["scheduled_task", "last_scheduled_run", "last_run_status", "last_scheduled_cron"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })
}
