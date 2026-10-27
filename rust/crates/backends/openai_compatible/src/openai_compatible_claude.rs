//! Claude Code run against the server's Anthropic proxy: `OpenAICompatibleClaudeBackend` from
//! `openai_compatible_claude.py`, which is `AnthropicEndpointBackend` with the proxy URL and the access key.

use anyhow::bail;
use charliebot_registry::backend::LaunchArgs;

use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_claude_code::AnthropicEndpointBackend;

/// The backend for `model` behind the proxy at `proxy_base_url`, authenticated with the access key `auth_token`.
/// The trailing slashes of the URL are dropped, as `rstrip("/")` drops them.
pub fn backend(
  proxy_base_url: &str,
  auth_token: &str,
  model: &str,
  claude_config_dir: Option<String>,
  launch: LaunchArgs,
  cgroup_config: SessionCgroupConfig,
) -> anyhow::Result<AnthropicEndpointBackend> {
  if proxy_base_url.is_empty() {
    bail!("cc-openai-compatible backend requires proxy_base_url");
  }
  if auth_token.is_empty() {
    bail!("cc-openai-compatible backend requires an auth_token for proxy auth");
  }
  if model.is_empty() {
    bail!("cc-openai-compatible backend requires a model");
  }
  AnthropicEndpointBackend::new(
    proxy_base_url.trim_end_matches('/').to_string(),
    auth_token.to_string(),
    model.to_string(),
    None,
    None,
    claude_config_dir,
    launch,
    cgroup_config,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_constructor_check_names_its_missing_input() {
    let url = "http://localhost:8000/proxy";
    let message = |result: anyhow::Result<AnthropicEndpointBackend>| format!("{:#}", result.err().unwrap());
    let limits = SessionCgroupConfig::disabled();
    assert!(
      message(backend("", "key", "model", None, LaunchArgs::default(), limits.clone()))
        .contains("requires proxy_base_url")
    );
    assert!(message(backend(url, "", "model", None, LaunchArgs::default(), limits.clone())).contains("auth_token"));
    assert!(message(backend(url, "key", "", None, LaunchArgs::default(), limits)).contains("requires a model"));
  }

  #[test]
  fn the_prepared_environment_points_at_the_proxy_with_the_model_pinned() {
    let backend = backend(
      "http://localhost:8000/proxy/",
      "charliebot-key",
      "nvidia/GLM-5.2-NVFP4",
      None,
      LaunchArgs::default(),
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let env = charliebot_registry::backend::AgentBackend::prepare_env(&backend, Default::default()).unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://localhost:8000/proxy");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "charliebot-key");
    assert_eq!(env["ANTHROPIC_MODEL"], "nvidia/GLM-5.2-NVFP4");
  }
}
