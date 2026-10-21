//! Build Claude Code for an Anthropic-compatible endpoint.

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::options::CcAnthropicCompatibleOption;
use crate::backends::claude_code::AnthropicEndpointBackend;
use crate::backends::claude_code::factory::{launch_config_dir, launch_with_kwargs};
use crate::transport::cgroup::SessionCgroupLimits;

const NO_AUTH_TOKEN: &str = "no-auth";

/// The factory for a `cc-anthropic-compatible` option.
pub struct AnthropicCompatibleFactory;

impl BackendFactory for AnthropicCompatibleFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let Some(option) = option else {
      bail!("cc-anthropic-compatible has no translate fallback: it builds only from its backend option");
    };
    let fields = CcAnthropicCompatibleOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?;
    let auth_token = match fields.credential.as_deref() {
      None => NO_AUTH_TOKEN.to_string(),
      Some(credential) => credentials::load_credentials(&cfg.charliebot_home)?.require(credential, "api_key")?,
    };
    if option.model.is_empty() {
      bail!("backend '{}' has no default model", option.id);
    }
    let launch = launch_with_kwargs(launch)?;
    let config_dir = launch_config_dir(&launch)?;
    let backend = AnthropicEndpointBackend::new(
      fields.api_base.trim_end_matches('/').to_string(),
      auth_token,
      option.model.clone(),
      Some(fields.context_window),
      config_dir,
      launch,
      SessionCgroupLimits::from_server(&cfg.server),
    )?;
    Ok(Box::new(backend))
  }
}
