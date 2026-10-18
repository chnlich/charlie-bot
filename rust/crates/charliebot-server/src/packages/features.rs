//! The stubs of `src/features/backend_pool` and `src/features/cron`: the metadata slots of the session file. The other
//! feature packages register from their own crates under `rust/crates/features/`.

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::json;

/// `src/features/backend_pool`: the pool flag on each session.
pub fn backend_pool(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "backend_pool",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "backend_from_pool", default: json!(false) }],
  })
}

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
