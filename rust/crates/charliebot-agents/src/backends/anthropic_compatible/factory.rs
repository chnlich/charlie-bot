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

#[cfg(test)]
mod tests {
  use charliebot_core::config::BackendOption;
  use charliebot_registry::Registry;

  use super::*;

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
}
