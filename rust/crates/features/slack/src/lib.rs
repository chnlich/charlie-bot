//! `web/features/slack`: the Socket Mode summon listener, thread reply routes, service, session fields, and turn hooks.

mod api;
pub mod cli;
mod config;
mod download;
mod draft;
mod draft_target;
mod event_types;
pub mod metadata;
mod service;
mod slack_client;
pub mod slack_listener;
pub mod thread_contract;
pub mod token;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField, valid_origin};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SUCCESSOR_SESSION_ID;
use serde_json::{Map, Value};
use service::SlackService;
use turn_contribution::SlackTurnContribution;

/// `register`: the service, the routes and the package's other registrations, in server order.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(SlackService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.draft_platform("slack", draft::platform_factory())?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: Some(SUCCESSOR_SESSION_ID),
      fields: ["slack_origin", "slack_watermark_ts"]
        .into_iter()
        .map(|name| SlotField { name, default: serde_json::Value::Null })
        .collect(),
    },
    validate_session_fields,
  )?;
  registry.thread_origin_field("slack_origin")?;
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

#[cfg(test)]
mod test_support;
