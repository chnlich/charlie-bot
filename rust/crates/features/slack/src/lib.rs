//! `src/features/slack`: registers the `slack:` config section, the Slack fields of each session and the turn
//! contribution that renders `slack_reply` events. The rest of the package is not ported yet.

mod event_types;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
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
  registry.metadata_slot(MetadataSlot {
    owner: "slack",
    on: MetadataFile::Session,
    after: Some("successor_session_id"),
    fields: ["slack_origin", "slack_watermark_ts"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })?;
  registry.turn_contribution("slack", Arc::new(SlackTurnContribution))
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
