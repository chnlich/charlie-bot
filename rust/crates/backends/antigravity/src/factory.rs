//! The antigravity backend factory (`src/backends/antigravity/factory.py`), resolving the agy binary from the package's host.

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::antigravity_cli::AntigravityBackend;
use super::options;
use charliebot_agents::cli_base::CliHost;

/// Builds one antigravity backend per launch.
pub struct AntigravityFactory {
  host: CliHost,
}

impl AntigravityFactory {
  pub fn new(host: CliHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for AntigravityFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the antigravity backend type needs its backends.options entry")?;
    if option.model.is_empty() {
      bail!("backend '{}' has no default model", option.id);
    }
    let print_timeout = options::print_timeout(&option.type_fields).map_err(anyhow::Error::msg)?;
    let agy_bin = self
      .host
      .resolve_binary("agy")?
      .into_os_string()
      .into_string()
      .map_err(|path| anyhow::anyhow!("the agy binary path is not valid Unicode: {path:?}"))?;
    let user_local_bin = self.host.user_local_bin().to_string_lossy().into_owned();
    Ok(Box::new(AntigravityBackend::from_launch(agy_bin, user_local_bin, &option.model, print_timeout, launch)))
  }
}
