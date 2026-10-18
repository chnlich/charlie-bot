//! The data that the feature packages register before their ports land: config sections with their legacy keys, and
//! the metadata slots of the session file.

use charliebot_core::config::ConfigSection;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{Registry, RegistryError};
use serde_json::json;

use super::raw_section;

/// `src/features/artifacts`: the `publish:` section.
pub fn artifacts(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "publish",
    legacy_keys: &[("publish_dir", "publish.dir"), ("public_base_url", "publish.public_base_url")],
    parse: raw_section,
  })
}

/// `src/features/backend_pool`: the pool flag on each session.
pub fn backend_pool(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "backend_pool",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "backend_from_pool", default: json!(false) }],
  })
}

/// `src/features/code_server`: the `code_server:` section.
pub fn code_server(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "code_server",
    legacy_keys: &[("code_server_bin", "code_server.bin"), ("code_server_config", "code_server.config")],
    parse: raw_section,
  })
}

/// `src/features/cron`: the scheduled-task fields of each session.
pub fn cron(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "cron",
    on: MetadataFile::Session,
    after: Some("backend"),
    fields: ["scheduled_task", "last_scheduled_run", "last_run_status", "last_scheduled_cron"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })
}

/// `src/features/slack`: the `slack:` section, then the Slack fields of each session.
pub fn slack(registry: &mut Registry) -> Result<(), RegistryError> {
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

/// `src/features/discord`: the `discord:` section, then the Discord fields of each session.
pub fn discord(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection { key: "discord", legacy_keys: &[], parse: raw_section })?;
  registry.metadata_slot(MetadataSlot {
    owner: "discord",
    on: MetadataFile::Session,
    after: Some("successor_session_id"),
    fields: ["discord_origin", "discord_watermark_id"]
      .into_iter()
      .map(|name| SlotField { name, default: serde_json::Value::Null })
      .collect(),
  })
}

/// `src/features/web_reply`: the web-reply status of each session.
pub fn web_reply(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "web_reply",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "web_status", default: serde_json::Value::Null }],
  })
}
