//! The Discord package's registrations: the service, the routes, the config section, the thread fields of a session and
//! the turn contribution.

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField, ThreadSlot, validate_thread_slot};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SUCCESSOR_SESSION_ID;

use crate::service::DiscordService;
use crate::turn_contribution::DiscordTurnContribution;
use crate::{api, config, metadata, thread_contract};

/// The thread fields of a Discord session's metadata slot.
const THREAD_SLOT: ThreadSlot = ThreadSlot {
  origin_field: metadata::DISCORD_ORIGIN,
  origin_type: "DiscordOrigin",
  origin_keys: &["guild_id", "parent_channel_id", "thread_id"],
  watermark_field: thread_contract::DISCORD.watermark_field,
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
  registry.thread_origin_field("discord", THREAD_SLOT.origin_field)?;
  registry.turn_contribution("discord", Arc::new(DiscordTurnContribution))
}
