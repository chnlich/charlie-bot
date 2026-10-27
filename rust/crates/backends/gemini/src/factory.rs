//! The gemini backend factory (`src/backends/gemini/factory.py`), resolving the gemini binary from the package's host.

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::gemini_cli::GeminiBackend;
use charliebot_agents::cli_base::CliHost;

/// Builds one gemini backend per launch.
pub struct GeminiFactory {
  host: CliHost,
}

impl GeminiFactory {
  pub fn new(host: CliHost) -> Self {
    Self { host }
  }
}

impl BackendFactory for GeminiFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the gemini backend type needs its backends.options entry")?;
    if option.model.is_empty() {
      bail!("backend '{}' has no default model", option.id);
    }
    let gemini_bin = self
      .host
      .resolve_binary("gemini")?
      .into_os_string()
      .into_string()
      .map_err(|path| anyhow::anyhow!("the gemini binary path is not valid Unicode: {path:?}"))?;
    Ok(Box::new(GeminiBackend::from_launch(gemini_bin, &option.model, launch)))
  }
}
