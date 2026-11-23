//! The vocabulary that scans match against. It holds backend ids, models, and endpoint hosts from the loaded
//! configuration. It also includes registered backend type id prefixes, the Gemini host, and price-table models.
//!
//! The configuration is read through the loader, so a key that the loader refuses never reaches the vocabulary. Two
//! files feed it: the profile's `config.yaml`, when it exists, and the repository's configuration example, when a
//! repository is given. A repository check therefore names the example's ids, and a host check names the host's too.

use std::collections::BTreeSet;
use std::path::Path;

use charliebot_core::config::{self, API_BASE_KEY, CharlieBotConfig};
use charliebot_core::list_price;
use charliebot_core::providers::GEMINI_HOST;
use charliebot_core::repo_root::{CONFIG_EXAMPLE_FILE_NAME, CONFIGS_DIR_NAME};
use charliebot_registry::Registry;

/// The words a scan matches: option, pool and model ids, endpoint hosts, and registered id prefixes.
#[derive(Debug, Default)]
pub struct Vocabulary {
  pub options: BTreeSet<String>,
  pub pools: BTreeSet<String>,
  pub models: BTreeSet<String>,
  pub hosts: BTreeSet<String>,
  pub id_prefixes: BTreeSet<String>,
}

impl Vocabulary {
  /// The vocabulary the check reads from files: the registered packages, the `config.yaml` under `home` when it exists,
  /// and the configuration example under `repo` when a repository is given. `charliebot backend check` and CI read it.
  pub fn load(registry: &Registry, home: &Path, repo: Option<&Path>) -> anyhow::Result<Self> {
    let schema = registry.config_schema();
    let mut vocab = Vocabulary::registered(registry)?;
    if home.join(config::CONFIG_FILENAME).is_file() {
      vocab.add_config(&config::load_config(home, &schema)?);
    }
    if let Some(repo) = repo {
      let example = repo.join(CONFIGS_DIR_NAME).join(CONFIG_EXAMPLE_FILE_NAME);
      if example.is_file() {
        vocab.add_config(&config::load_config_file(repo, &example, &schema)?);
      }
    }
    Ok(vocab)
  }

  /// The vocabulary of a gate that runs on the configuration already loaded: the registered packages and `cfg`. The
  /// server's gates read the host's configuration this way, and so never load the repository's example.
  pub fn of_config(registry: &Registry, cfg: &CharlieBotConfig) -> anyhow::Result<Self> {
    let mut vocab = Vocabulary::registered(registry)?;
    vocab.add_config(cfg);
    Ok(vocab)
  }

  /// The words that every vocabulary holds: the id prefixes of the registered backend types, the Gemini host, and the
  /// models of the price table.
  fn registered(registry: &Registry) -> anyhow::Result<Self> {
    let mut vocab = Vocabulary::default();
    for backend_type in &registry.config_schema().backend_types {
      if let Some(source) = registry.usage_source_for(backend_type)? {
        vocab.id_prefixes.extend(source.id_prefixes.iter().map(|prefix| prefix.to_string()));
      }
    }
    vocab.hosts.insert(GEMINI_HOST.to_string());
    vocab.models.extend(list_price::price_models().map(str::to_string));
    Ok(vocab)
  }

  fn add_config(&mut self, cfg: &CharlieBotConfig) {
    for option in &cfg.backends.options {
      self.options.insert(option.id.clone());
      if !option.model.is_empty() {
        self.models.insert(option.model.clone());
      }
      if let Some(host) = option.type_fields.get(API_BASE_KEY).and_then(|value| value.as_str()).and_then(endpoint_host)
      {
        self.hosts.insert(host);
      }
    }
    self.pools.extend(cfg.backends.pools.iter().map(|pool| pool.id.clone()));
  }
}

/// The host of an endpoint URL, lower-case: the text between `://` and the first `/` or `:`. A text without a scheme
/// names no endpoint.
fn endpoint_host(url: &str) -> Option<String> {
  let (_, rest) = url.split_once("://")?;
  let host = rest.split(['/', ':']).next()?;
  (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn endpoint_hosts_are_read_from_urls() {
    assert_eq!(endpoint_host("https://AIGW.example.team/v1").as_deref(), Some("aigw.example.team"));
    assert_eq!(endpoint_host("http://127.0.0.1:9/v1").as_deref(), Some("127.0.0.1"));
    assert_eq!(endpoint_host("aigw.example.team"), None);
  }
}
