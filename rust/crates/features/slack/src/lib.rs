//! `src/features/slack`: registers the `slack:` config section, the Slack fields of each session and the turn
//! contribution that carries the thread reply contract in manager runs and renders `slack_reply` events. The rest of
//! the package is not ported yet.

mod event_types;
pub mod metadata;
mod thread_contract;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::{Map, Value};
use turn_contribution::SlackTurnContribution;

/// `src/features/slack`: the `slack:` section, then the Slack fields of each session, then the turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "slack",
    legacy_keys: &[
      ("slack_allowed_user_ids", "slack.allowed_user_ids"),
      ("credentials: slack_bot_token", "slack.bot_token"),
      ("credentials: slack_app_token", "slack.app_token"),
      ("credentials: slack_user_token", "slack.user_token"),
    ],
    parse: raw_section,
  })?;
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

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
