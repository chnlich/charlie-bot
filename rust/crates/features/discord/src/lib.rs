//! `web/features/discord`: registers the gateway service, internal routes, config section, session fields and turn
//! contribution for the Discord thread entrypoint.

mod api;
pub mod cli;
mod config;
pub mod discord_client;
mod discord_commands;
pub mod discord_listener;
mod event_types;
pub mod metadata;
mod service;
pub mod thread_contract;
mod turn_contribution;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField, ThreadSlot, validate_thread_slot};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SUCCESSOR_SESSION_ID;
use service::DiscordService;
use turn_contribution::DiscordTurnContribution;

/// The thread fields of a Discord session's metadata slot.
const THREAD_SLOT: ThreadSlot = ThreadSlot {
  origin_field: "discord_origin",
  origin_type: "DiscordOrigin",
  origin_keys: &["guild_id", "parent_channel_id", "thread_id"],
  watermark_field: "discord_watermark_id",
};

/// `register`: the service, the routes and the package's other registrations, in server order.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(DiscordService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "discord",
      on: MetadataFile::Session,
      after: Some(SUCCESSOR_SESSION_ID),
      fields: [THREAD_SLOT.origin_field, THREAD_SLOT.watermark_field]
        .into_iter()
        .map(|name| SlotField { name, default: serde_json::Value::Null })
        .collect(),
    },
    |values| validate_thread_slot(values, &THREAD_SLOT),
  )?;
  registry.thread_origin_field(THREAD_SLOT.origin_field)?;
  registry.turn_contribution("discord", Arc::new(DiscordTurnContribution))
}
