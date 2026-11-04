//! The `feishu:` config section: the allow-list of the summon entrypoint.

use std::collections::BTreeMap;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use serde::{Deserialize, Serialize};

/// The `feishu:` section. `allowed_users` maps a Feishu open_id to the person that account belongs to; an empty map
/// admits nobody.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
  #[serde(default)]
  pub allowed_users: BTreeMap<String, String>,
}

pub fn section() -> ConfigSection {
  ConfigSection { key: "feishu", legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// Render the section as its model dumps it (`charliebot config get feishu`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<FeishuConfig>().ok_or("the feishu section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: FeishuConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("feishu: {error}"))?;
  Ok(Arc::new(settings))
}
