//! The Feishu package's registrations: the service, the routes, the config section, the thread fields of a session and
//! the turn contribution.

use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField, valid_origin};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SUCCESSOR_SESSION_ID;
use serde_json::{Map, Value};

use crate::package::NAME;
use crate::service::FeishuService;
use crate::turn_contribution::FeishuTurnContribution;
use crate::{api, config, metadata};

/// The service, the routes and the package's other registrations, all registered in server order.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.service(Arc::new(FeishuService::default()))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(config::section())?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: NAME,
      on: MetadataFile::Session,
      after: Some(SUCCESSOR_SESSION_ID),
      fields: [metadata::FEISHU_ORIGIN, metadata::FEISHU_WATERMARK_ID]
        .into_iter()
        .map(|name| SlotField { name, default: Value::Null })
        .collect(),
    },
    validate_session_fields,
  )?;
  registry.thread_origin_field(NAME, metadata::FEISHU_ORIGIN)?;
  registry.turn_contribution(NAME, Arc::new(FeishuTurnContribution))
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(origin) = values.get_mut(metadata::FEISHU_ORIGIN)
    && !valid_origin(origin, &metadata::ORIGIN_REQUIRED_FIELDS, &metadata::ORIGIN_OPTIONAL_FIELDS)
  {
    return Some("feishu_origin must be a FeishuOrigin or null".to_string());
  }
  if values.get(metadata::FEISHU_WATERMARK_ID).is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some("feishu_watermark_id must be a string or null".to_string());
  }
  None
}
