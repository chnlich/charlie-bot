//! The registration entry: the one item the package registry calls to mount the cron feature.

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::CRON_SLOT_FIELDS;
use serde_json::Value;

use crate::slot_validation::validate_cron_fields;
use crate::turn::CronTurnContribution;
use crate::{api, host_gate, loader, rounds, scheduler, sequence_controller};

/// The feature registration: scheduled-task routes, metadata, the scheduler service and the sequence owner.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router_patterns(api::routes, api::ROUTE_PATTERNS)?;
  registry.turn_contribution("cron", Arc::new(CronTurnContribution))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "cron",
      on: MetadataFile::Session,
      after: Some("backend"),
      fields: CRON_SLOT_FIELDS.into_iter().map(|name| SlotField { name, default: Value::Null }).collect(),
    },
    validate_cron_fields,
  )?;
  registry.startup_check("cron_backend_refs", loader::backend_refs_check)?;
  registry.derived_waiting_items(host_gate::derived_waiting_rows)?;
  registry.bound_round(rounds::bound_round_readers())?;
  registry.bound_next_fire(scheduler::sessions_with_next_fire)?;
  let (registry_sender, registry_receiver) = tokio::sync::watch::channel(None);
  registry.service(Arc::new(sequence_controller::BindingContextService::new(registry_sender)))?;
  registry.service(Arc::new(scheduler::SchedulerService::default()))?;
  registry.sequence_controller("cron", Arc::new(sequence_controller::CronSequenceController::new(registry_receiver)))
}
