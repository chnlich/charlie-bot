//! The devin backend factory, resolving the devin binary from the package's host.

use anyhow::Context;
use charliebot_agents::cli_base::CliHost;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::devin_cli::DevinBackend;

/// Builds one devin backend per launch.
pub struct DevinFactory {
  host: CliHost,
}

impl DevinFactory {
  pub fn new(host: CliHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for DevinFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the devin backend type needs its backends.options entry")?;
    let model = option.required_model("backend").map_err(anyhow::Error::msg)?;
    let devin_bin = self
      .host
      .resolve_binary("devin")?
      .into_os_string()
      .into_string()
      .map_err(|path| anyhow::anyhow!("the devin binary path is not valid Unicode: {path:?}"))?;
    Ok(Box::new(DevinBackend::from_launch(devin_bin, &model, launch)))
  }
}
