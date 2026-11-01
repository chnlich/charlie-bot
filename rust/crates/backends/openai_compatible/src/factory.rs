//! The cc-openai-compatible factory: the proxy base URL, the access key and the model, from `factory.py`.

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::openai_compatible_claude;
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_claude_code::factory::{launch_config_dir, launch_with_kwargs};

pub struct OpenAiCompatibleFactory;

impl BackendFactory for OpenAiCompatibleFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let Some(option) = option else {
      bail!("cc-openai-compatible has no translate fallback: it builds only from its backend option");
    };
    let proxy_base_url =
      format!("http://localhost:{}/api/anthropic-proxy/openai-compatible/{}", cfg.server.port, option.id);
    let auth_token = credentials::load_credentials(&cfg.charliebot_home)?.require("charliebot", "access_key")?;
    let model = option.required_model("backend").map_err(anyhow::Error::msg)?;
    let launch = launch_with_kwargs(launch)?;
    let config_dir = launch_config_dir(&launch)?;
    let backend = openai_compatible_claude::backend(
      &proxy_base_url,
      &auth_token,
      &model,
      config_dir,
      cfg.server.port,
      launch,
      SessionCgroupConfig::from_config(cfg),
    )?;
    Ok(Box::new(backend))
  }
}
