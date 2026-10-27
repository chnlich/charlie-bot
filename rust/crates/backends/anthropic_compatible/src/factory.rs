//! Build Claude Code for an Anthropic-compatible endpoint.

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::options::CcAnthropicCompatibleOption;
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_claude_code::AnthropicEndpointBackend;
use charliebot_claude_code::factory::{launch_config_dir, launch_with_kwargs};

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
      fields.effort,
      config_dir,
      launch,
      SessionCgroupConfig::from_config(cfg),
    )?
    .with_headers(fields.headers)
    .with_extra_body(fields.extra_body);
    Ok(Box::new(backend))
  }
}

#[cfg(test)]
mod tests {
  use charliebot_core::config::BackendOption;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::Env;

  use super::*;
  use charliebot_claude_code::{GATEWAY_CUSTOM_HEADERS_ENV, GATEWAY_EXTRA_BODY_ENV};

  fn option(type_fields: &str) -> BackendOption {
    BackendOption {
      id: "anthropic".to_string(),
      label: "Anthropic compatible".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "cc-anthropic-compatible".to_string(),
      type_fields: serde_yaml::from_str(type_fields).unwrap(),
    }
  }

  /// Builds the backend of an option with these type fields, or the factory's error text.
  fn build(type_fields: &str) -> Result<Box<dyn AgentBackend>, String> {
    let home = tempfile::tempdir().unwrap();
    let cfg = Registry::new().load_config(home.path()).unwrap();
    AnthropicCompatibleFactory
      .build(Some(&option(type_fields)), &cfg, LaunchArgs::default())
      .map_err(|error| format!("{error:#}"))
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_routes_effort_to_cli_command
  // Python: tests/test_anthropic_compatible_backend.py::test_factory_omits_effort_from_cli_command_by_default
  #[test]
  fn effort_reaches_the_claude_command_only_when_the_option_sets_it() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";

    let with_effort = build(&format!("{endpoint}effort: high\n"));
    assert!(with_effort.is_ok(), "the factory refused effort: {:?}", with_effort.as_ref().err());
    let command = with_effort.unwrap().build_command("prompt").unwrap();
    assert!(command.windows(2).any(|pair| pair == ["--effort", "high"]), "{command:?}");

    let plain = build(endpoint);
    assert!(plain.is_ok(), "the factory refused an option without effort: {:?}", plain.as_ref().err());
    let command = plain.unwrap().build_command("prompt").unwrap();
    assert!(!command.iter().any(|value| value == "--effort"), "{command:?}");
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_routes_custom_headers
  #[test]
  fn factory_routes_custom_headers() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";
    let backend = build(&format!("{endpoint}headers:\n  x-litellm-tags: deepseek\n")).unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek");
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_routes_multiple_custom_headers
  #[test]
  fn factory_routes_multiple_custom_headers() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";
    let backend =
      build(&format!("{endpoint}headers:\n  x-litellm-tags: deepseek\n  x-client-name: charliebot\n")).unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek\nx-client-name: charliebot");
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_omits_custom_headers_when_option_has_none_and_parent_sets_them
  #[test]
  fn factory_omits_custom_headers_when_the_option_has_none_and_the_parent_sets_them() {
    let backend = build("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n").unwrap();
    let env = backend
      .prepare_env(Env::from([(GATEWAY_CUSTOM_HEADERS_ENV.to_string(), "inherited-value".to_string())]))
      .unwrap();
    assert!(!env.contains_key(GATEWAY_CUSTOM_HEADERS_ENV));
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_routes_extra_body
  #[test]
  fn factory_routes_extra_body() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";
    let backend = build(&format!("{endpoint}extra_body:\n  cache:\n    no-store: true\n    no-cache: true\n")).unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&env[GATEWAY_EXTRA_BODY_ENV]).unwrap(),
      serde_json::json!({"cache": {"no-store": true, "no-cache": true}})
    );
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_factory_omits_extra_body_when_option_has_none_and_parent_sets_it
  #[test]
  fn factory_omits_extra_body_when_the_option_has_none_and_the_parent_sets_it() {
    let backend = build("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n").unwrap();
    let env =
      backend.prepare_env(Env::from([(GATEWAY_EXTRA_BODY_ENV.to_string(), "inherited-value".to_string())])).unwrap();
    assert!(!env.contains_key(GATEWAY_EXTRA_BODY_ENV));
  }
}
