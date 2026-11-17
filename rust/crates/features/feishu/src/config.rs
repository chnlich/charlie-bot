//! The `feishu:` config section: the owner list that admits Feishu groups to the summon entrypoint.

use std::collections::BTreeMap;

use charliebot_core::config::{ConfigSection, SectionModel};
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

impl SectionModel for FeishuConfig {
  const KEY: &'static str = NAME;
}

pub fn section() -> ConfigSection {
  ConfigSection::of::<FeishuConfig>(&[])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_host_config_with_allowed_users_still_parses() {
    let raw: serde_yaml::Value =
      serde_yaml::from_str("allowed_users:\n  ou_0000000000000000000000000000000000000001: tester\n").unwrap();
    let section = (section().parse)(&raw).unwrap();
    let settings = section.downcast_ref::<FeishuConfig>().unwrap();
    let owner = settings.allowed_users.get("ou_0000000000000000000000000000000000000001");
    assert_eq!(owner.map(String::as_str), Some("tester"));
  }

  #[test]
  fn an_unknown_key_is_refused() {
    let raw: serde_yaml::Value = serde_yaml::from_str("owners: {}\n").unwrap();
    assert!((section().parse)(&raw).is_err());
  }
}
