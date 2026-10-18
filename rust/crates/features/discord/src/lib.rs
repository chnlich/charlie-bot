//! `src/features/discord`: registers the `discord:` config section, the Discord fields of each session and the turn
//! contribution that renders `discord_reply` events. The rest of the package is not ported yet.

mod event_types;
mod turn_contribution;

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use turn_contribution::DiscordTurnContribution;

/// `src/features/discord`: the `discord:` section, then the Discord fields of each session, then the turn contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection { key: "discord", legacy_keys: &[], parse: raw_section })?;
  registry.metadata_slot(MetadataSlot {
    owner: "discord",
    on: MetadataFile::Session,
    after: Some("successor_session_id"),
    fields: ["discord_origin", "discord_watermark_id"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })?;
  registry.turn_contribution("discord", Arc::new(DiscordTurnContribution))
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
