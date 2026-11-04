//! `web/features/feishu`: registers the long-connection service, internal routes, config section, session fields and
//! turn contribution for the Feishu thread entrypoint, and the `charliebot feishu` verbs.

mod adapter;
mod api;
pub mod cli;
mod client;
mod commands;
mod config;
mod event;
mod event_types;
mod frame;
mod listener;
pub mod metadata;
mod service;
#[cfg(test)]
mod test_support;
pub mod thread_contract;
mod turn_contribution;
mod user_token;

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Map, Value};
use service::FeishuService;
use turn_contribution::FeishuTurnContribution;

/// `register`: the service, the routes and the package's other registrations, in server order.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(FeishuService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "feishu",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: ["feishu_origin", "feishu_watermark_id"]
        .into_iter()
        .map(|name| SlotField { name, default: Value::Null })
        .collect(),
    },
    validate_session_fields,
  )?;
  registry.thread_origin_field("feishu_origin")?;
  registry.turn_contribution("feishu", Arc::new(FeishuTurnContribution))
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(origin) = values.get_mut("feishu_origin")
    && !metadata::valid_origin(origin)
  {
    return Some("feishu_origin must be a FeishuOrigin or null".to_string());
  }
  if values.get("feishu_watermark_id").is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some("feishu_watermark_id must be a string or null".to_string());
  }
  None
}
