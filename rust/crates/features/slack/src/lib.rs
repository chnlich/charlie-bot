//! `src/features/slack`: registers the `slack:` config section and the Slack fields of each session. The rest of the
//! package is not ported yet.

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};

/// `src/features/slack`: the `slack:` section, then the Slack fields of each session.
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
  })
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
