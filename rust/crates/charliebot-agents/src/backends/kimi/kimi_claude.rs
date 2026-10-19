//! Claude Code run against Kimi's Anthropic-compatible endpoint: `KimiBackend` from `kimi.py`, which is
//! `AnthropicEndpointBackend` at the Moonshot base URL with the option's key.

use anyhow::bail;
use charliebot_registry::backend::LaunchArgs;

use crate::backends::claude_code::AnthropicEndpointBackend;

/// The Moonshot endpoint. It routes every API call to api.moonshot.cn instead of api.anthropic.com.
const MOONSHOT_BASE_URL: &str = "https://api.moonshot.cn/anthropic";

/// The backend for `model` at the Moonshot endpoint, authenticated with `api_key`.
pub fn backend(
  api_key: &str,
  model: &str,
  claude_config_dir: Option<String>,
  launch: LaunchArgs,
) -> anyhow::Result<AnthropicEndpointBackend> {
  if model.is_empty() {
    bail!("kimi backend requires a model");
  }
  AnthropicEndpointBackend::new(
    MOONSHOT_BASE_URL.to_string(),
    api_key.to_string(),
    model.to_string(),
    claude_config_dir,
    launch,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_registry::backend::{AgentBackend, Env};

  #[test]
  fn the_prepared_environment_routes_the_key_to_the_moonshot_endpoint() {
    let backend = backend("moonshot-test-key", "kimi-k2.5", None, LaunchArgs::default()).unwrap();
    let env = backend.prepare_env(Env::from([("PATH".to_string(), "/usr/bin".to_string())])).unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.moonshot.cn/anthropic");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "moonshot-test-key");
    assert_eq!(env["ANTHROPIC_MODEL"], "kimi-k2.5");
  }

  #[test]
  fn an_empty_model_is_refused() {
    let error = backend("moonshot-test-key", "", None, LaunchArgs::default()).err().unwrap();
    assert!(format!("{error:#}").contains("requires a model"));
  }
}
