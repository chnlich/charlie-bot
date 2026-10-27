//! Typed configuration for Claude account pools and Sonnet compaction thresholds.

use std::collections::BTreeMap;

use charliebot_core::config::CharlieBotConfig;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeAccount {
  pub label: String,
  pub config_dir: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCompactionConfig {
  #[serde(default = "default_relay_tokens", deserialize_with = "positive_tokens")]
  pub relay_tokens: u64,
  #[serde(default = "default_expired_cache_tokens", deserialize_with = "positive_tokens")]
  pub expired_cache_tokens: u64,
  #[serde(default = "default_idle_tokens", deserialize_with = "positive_tokens")]
  pub idle_tokens: u64,
}

impl Default for ClaudeCompactionConfig {
  fn default() -> Self {
    Self {
      relay_tokens: default_relay_tokens(),
      expired_cache_tokens: default_expired_cache_tokens(),
      idle_tokens: default_idle_tokens(),
    }
  }
}

fn default_relay_tokens() -> u64 {
  100_000
}

fn default_expired_cache_tokens() -> u64 {
  50_000
}

fn default_idle_tokens() -> u64 {
  100_000
}

fn positive_tokens<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
  D: Deserializer<'de>,
{
  let value = u64::deserialize(deserializer)?;
  if value == 0 {
    return Err(serde::de::Error::custom("must be greater than 0"));
  }
  Ok(value)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountsConfig {
  #[serde(default)]
  pub claude: Vec<ClaudeAccount>,
  #[serde(default)]
  pub claude_pools: BTreeMap<String, Vec<String>>,
  #[serde(default)]
  pub claude_compaction: ClaudeCompactionConfig,
}

pub fn parse_accounts(value: &Value) -> Result<charliebot_core::config::SectionValue, String> {
  let parsed: AccountsConfig = serde_yaml::from_value(value.clone()).map_err(|error| error.to_string())?;
  Ok(std::sync::Arc::new(parsed))
}

pub fn check_claude_pools(cfg: &CharlieBotConfig) -> Result<(), String> {
  let accounts = cfg.section::<AccountsConfig>("accounts");
  let labels = accounts.claude.iter().map(|account| account.label.as_str()).collect::<std::collections::HashSet<_>>();
  for (pool_name, pool_labels) in &accounts.claude_pools {
    let unknown =
      pool_labels.iter().filter(|label| !labels.contains(label.as_str())).map(String::as_str).collect::<Vec<_>>();
    if !unknown.is_empty() {
      return Err(format!(
        "accounts.claude_pools['{pool_name}'] names accounts missing from accounts.claude: {}",
        unknown.join(", ")
      ));
    }
    if pool_labels.is_empty() {
      return Err(format!("accounts.claude_pools['{pool_name}'] lists no account"));
    }
  }
  for option in &cfg.backends.options {
    if option.backend_type != "cc-claude" {
      continue;
    }
    let pool = option.type_fields.get(Value::String("account_pool".to_string())).and_then(Value::as_str);
    if accounts.claude_pools.is_empty() {
      if let Some(pool) = pool {
        return Err(format!(
          "backend '{}' sets account_pool '{}' but accounts.claude_pools defines no pools",
          option.id, pool
        ));
      }
      continue;
    }
    let Some(pool) = pool else {
      return Err(format!(
        "backend '{}' (cc-claude) names no account_pool; defined pools: {}",
        option.id,
        accounts.claude_pools.keys().cloned().collect::<Vec<_>>().join(", ")
      ));
    };
    if !accounts.claude_pools.contains_key(pool) {
      return Err(format!(
        "backend '{}' names undefined account_pool '{}'; defined pools: {}",
        option.id,
        pool,
        accounts.claude_pools.keys().cloned().collect::<Vec<_>>().join(", ")
      ));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;

  use super::*;

  fn config(accounts_yaml: &str, account_pool: Option<&str>) -> CharlieBotConfig {
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    let pool_field = account_pool.map_or_else(String::new, |pool| format!("      account_pool: {pool}\n"));
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n{pool_field}accounts:\n{accounts_yaml}"
      ),
    )
    .unwrap();
    registry.load_config(home.path()).unwrap()
  }

  #[test]
  fn config_defaults_and_legacy_shape_are_typed() {
    let cfg = config("  claude: []\n", None);
    let accounts = cfg.section::<AccountsConfig>("accounts");
    assert_eq!(accounts.claude_compaction.relay_tokens, 100_000);
    assert_eq!(accounts.claude_compaction.expired_cache_tokens, 50_000);
    assert_eq!(accounts.claude_compaction.idle_tokens, 100_000);
  }

  #[test]
  fn config_rejects_invalid_pool_members_and_missing_option_binding() {
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join("config.yaml"),
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\naccounts:\n  claude:\n    - label: a\n      config_dir: /synthetic/a\n  claude_pools:\n    blue: [missing]\n",
    )
    .unwrap();
    assert!(registry.load_config(home.path()).unwrap_err().to_string().contains("missing from accounts.claude"));
  }

  #[test]
  fn registration_keeps_the_legacy_keys_and_shared_pool_membership() {
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    let section = registry.config_schema().sections.into_iter().find(|section| section.key == "accounts").unwrap();
    assert_eq!(
      section.legacy_keys,
      &[("claude_accounts", "accounts.claude"), ("claude_compaction", "accounts.claude_compaction")]
    );

    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join("config.yaml"),
      concat!(
        "backends:\n  options:\n",
        "    - {id: claude-a, label: A, type: cc-claude, model: m, account_pool: alpha}\n",
        "    - {id: claude-b, label: B, type: cc-claude, model: m, account_pool: beta}\n",
        "accounts:\n  claude:\n",
        "    - {label: a, config_dir: /synthetic/a}\n",
        "    - {label: b, config_dir: /synthetic/b}\n",
        "    - {label: c, config_dir: /synthetic/c}\n",
        "  claude_pools:\n    alpha: [a, b]\n    beta: [b, c]\n",
      ),
    )
    .unwrap();
    let cfg = registry.load_config(home.path()).unwrap();
    let accounts = cfg.section::<AccountsConfig>("accounts");
    assert_eq!(accounts.claude_pools["alpha"], ["a", "b"]);
    assert_eq!(accounts.claude_pools["beta"], ["b", "c"]);
    assert_eq!(cfg.backends.options.len(), 2);
  }

  #[test]
  fn invalid_pool_bindings_name_the_configured_option_or_pool() {
    let cases = [
      (
        "accounts:\n  claude:\n    - {label: a, config_dir: /synthetic/a}\n  claude_pools:\n    alpha: []\n",
        None,
        "accounts.claude_pools['alpha']",
      ),
      (
        "accounts:\n  claude:\n    - {label: a, config_dir: /synthetic/a}\n  claude_pools:\n    alpha: [a]\n",
        Some("beta"),
        "backend 'fable'",
      ),
      (
        "accounts:\n  claude:\n    - {label: a, config_dir: /synthetic/a}\n  claude_pools:\n    alpha: [a]\n",
        None,
        "backend 'fable'",
      ),
    ];
    for (accounts, account_pool, expected) in cases {
      let mut registry = Registry::new();
      registry.register_package("src.backends.claude_code", crate::register).unwrap();
      let home = tempfile::tempdir().unwrap();
      let binding = account_pool.map_or_else(String::new, |pool| format!("      account_pool: {pool}\n"));
      std::fs::write(
        home.path().join("config.yaml"),
        format!(
          "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n{binding}{accounts}"
        ),
      )
      .unwrap();
      assert!(registry.load_config(home.path()).unwrap_err().to_string().contains(expected));
    }
  }
}
