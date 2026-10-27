//! `src/features/discord`: registers the gateway service, internal routes, config section, session fields and turn
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
mod thread_contract;
mod turn_contribution;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Map, Value};
use service::DiscordService;
use turn_contribution::DiscordTurnContribution;

/// Register in Python package order, omitting its Python-only `discord` CLI command.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(DiscordService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "discord",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: ["discord_origin", "discord_watermark_id"]
        .into_iter()
        .map(|name| SlotField { name, default: serde_json::Value::Null })
        .collect(),
    },
    validate_session_fields,
  )?;
  registry.turn_contribution("discord", Arc::new(DiscordTurnContribution))
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(origin) = values.get_mut("discord_origin")
    && !valid_origin(origin, &["guild_id", "parent_channel_id", "thread_id"])
  {
    return Some("discord_origin must be a DiscordOrigin or null".to_string());
  }
  if values.get("discord_watermark_id").is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some("discord_watermark_id must be a string or null".to_string());
  }
  None
}

fn valid_origin(value: &mut Value, fields: &[&str]) -> bool {
  if value.is_null() {
    return true;
  }
  let Some(object) = value.as_object_mut() else { return false };
  let valid = fields.iter().all(|name| object.get(*name).is_some_and(Value::is_string));
  if valid {
    object.retain(|name, _| fields.contains(&name.as_str()));
  }
  valid
}
