//! The charlie-code backend factory (`src/backends/charlie_code/factory.py`): the option's model and endpoint fields, and
//! the credential's `api_key`, read in Python's argument order.

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::charlie_code_cli::CharlieCodeBackend;
use super::options::CharlieCodeOption;
use charliebot_agents::cli_base::CliHost;

/// Builds one charlie-code backend per launch, resolving the binary from the package's host.
pub struct CharlieCodeFactory {
  host: CliHost,
}

impl CharlieCodeFactory {
  pub fn new(host: CliHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for CharlieCodeFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the charlie-code backend type needs its backends.options entry")?;
    if option.model.is_empty() {
      bail!("backend '{}' has no default model", option.id);
    }
    let fields = CharlieCodeOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?;
    let api_key = match fields.credential.as_deref().filter(|section| !section.is_empty()) {
      Some(section) => Some(credentials::load_credentials(&cfg.charliebot_home)?.require(section, "api_key")?),
      None => None,
    };
    let backend = CharlieCodeBackend::from_launch(&self.host, &option.model, fields, api_key, launch)?;
    Ok(Box::new(backend))
  }
}
