//! `src/features/slack`: the Socket Mode summon listener, thread reply routes, service, session fields, and turn hooks.

mod api;
pub mod cli;
mod config;
mod event_types;
pub mod metadata;
mod service;
mod slack_client;
pub mod slack_listener;
pub mod thread_contract;
pub mod token;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Map, Value};
use service::SlackService;
use turn_contribution::SlackTurnContribution;

/// Register in the Python package order, omitting its Python-only CLI command.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(SlackService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: ["slack_origin", "slack_watermark_ts"]
        .into_iter()
        .map(|name| SlotField { name, default: serde_json::Value::Null })
        .collect(),
    },
    validate_session_fields,
  )?;
  registry.turn_contribution("slack", Arc::new(SlackTurnContribution))
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(origin) = values.get_mut("slack_origin")
    && !valid_origin(origin, &["team_id", "channel_id", "thread_ts"])
  {
    return Some("slack_origin must be a SlackOrigin or null".to_string());
  }
  if values.get("slack_watermark_ts").is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some("slack_watermark_ts must be a string or null".to_string());
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

#[cfg(test)]
mod test_support;
