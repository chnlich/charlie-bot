//! The `discord:` config section: the account map of the summon entrypoint.

use std::collections::BTreeMap;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use serde::{Deserialize, Serialize};

/// The `discord:` section. `allowed_users` maps a Discord user id to the person that account belongs to; an empty map
/// admits nobody.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
  #[serde(default)]
  pub allowed_users: BTreeMap<String, String>,
}

pub fn section() -> ConfigSection {
  ConfigSection { key: "discord", legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// The section's value as its model dumps it, which `charliebot config get discord` prints.
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<DiscordConfig>().ok_or("the discord section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

/// The parsed section. A retired `allowed_user_ids` list fails with the message that names its successor.
fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  if raw.as_mapping().is_some_and(|mapping| mapping.contains_key("allowed_user_ids")) {
    return Err(
      "discord.allowed_user_ids is retired: move each allowed Discord user id into discord.allowed_users \
                as <user id>: <the person that account belongs to>"
        .to_string(),
    );
  }
  let settings: DiscordConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("discord: {error}"))?;
  Ok(Arc::new(settings))
}
