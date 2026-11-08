//! The `feishu:` config section: the owner list that admits Feishu groups to the summon entrypoint.

use std::collections::BTreeMap;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use serde::{Deserialize, Serialize};

use crate::package::NAME;

/// The `feishu:` section. `allowed_users` maps the Feishu open_id of each owner to the person that account belongs to.
/// A group is admitted when one of these people is a member of it, and then every human member of the group can summon
/// the bot. An empty map leaves the Feishu feature unconfigured.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
  #[serde(default)]
  pub allowed_users: BTreeMap<String, String>,
}

pub fn section() -> ConfigSection {
  ConfigSection { key: NAME, legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// The section as its model dumps it (`charliebot config get feishu`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<FeishuConfig>().ok_or("the feishu section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: FeishuConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("feishu: {error}"))?;
  Ok(Arc::new(settings))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_host_config_with_allowed_users_still_parses() {
    let raw: serde_yaml::Value =
      serde_yaml::from_str("allowed_users:\n  ou_0000000000000000000000000000000000000001: tester\n").unwrap();
    let section = parse_section(&raw).unwrap();
    let settings = section.downcast_ref::<FeishuConfig>().unwrap();
    let owner = settings.allowed_users.get("ou_0000000000000000000000000000000000000001");
    assert_eq!(owner.map(String::as_str), Some("tester"));
  }

  #[test]
  fn an_unknown_key_is_refused() {
    let raw: serde_yaml::Value = serde_yaml::from_str("owners: {}\n").unwrap();
    assert!(parse_section(&raw).is_err());
  }
}
