//! The factory that builds Claude Code for an Anthropic-compatible endpoint.

use anyhow::bail;
use charliebot_core::cgroup::SessionCgroupConfig;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::credentials;
use charliebot_registry::backend::{AgentBackend, BackendFactory, LaunchArgs};

use super::options::{CcAnthropicCompatibleOption, TYPE_NAME};
use charliebot_claude_code::AnthropicEndpointBackend;
use charliebot_claude_code::factory::{launch_config_dir, launch_with_kwargs};

const NO_AUTH_TOKEN: &str = "no-auth";

/// The factory for a `cc-anthropic-compatible` option.
pub struct AnthropicCompatibleFactory;

impl BackendFactory for AnthropicCompatibleFactory {
  fn one_shot_is_tool_less(&self) -> bool {
    true
  }

  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let Some(option) = option else {
      bail!("{TYPE_NAME} has no translate fallback: it builds only from its backend option");
    };
    let fields = CcAnthropicCompatibleOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?;
    let auth_token = match fields.credential.as_deref() {
      None => NO_AUTH_TOKEN.to_string(),
      Some(credential) => credentials::load_credentials(&cfg.charliebot_home)?.require_api_key(credential)?,
    };
    let model = option.required_model("backend").map_err(anyhow::Error::msg)?;
    let launch = launch_with_kwargs(launch)?;
    let config_dir = launch_config_dir(&launch)?;
    let backend = AnthropicEndpointBackend::new(
      fields.api_base.trim_end_matches('/').to_string(),
      auth_token,
      model,
      Some(fields.context_window),
      fields.effort,
      charliebot_claude_code::claude_config::enable_subagents(cfg),
      config_dir,
      cfg.server.port,
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

  #[test]
  fn factory_declares_its_native_one_shot_tool_less() {
    assert!(AnthropicCompatibleFactory.one_shot_is_tool_less());
  }

  fn option(type_fields: &str) -> BackendOption {
    BackendOption {
      id: "anthropic".to_string(),
      label: "Anthropic compatible".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      billing: None,
      backend_type: TYPE_NAME.to_string(),
      type_fields: serde_yaml::from_str(type_fields).unwrap(),
      visible: true,
    }
  }

  /// The backend of an option with these type fields, or the factory's error text. The launch is a worker run, which
  /// names its session and run in its compaction-context hook.
  fn build(type_fields: &str) -> Result<Box<dyn AgentBackend>, String> {
    let home = tempfile::tempdir().unwrap();
    let cfg = Registry::new().load_config(home.path()).unwrap();
    let launch = LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: None,
      log_dir: None,
      cgroup_session_id: Some("worker-session".to_string()),
      remote_launch: None,
      backend_kwargs: serde_json::json!({"run_id": "worker-run"}).as_object().unwrap().clone(),
      manager_turn: false,
    };
    AnthropicCompatibleFactory.build(Some(&option(type_fields)), &cfg, launch).map_err(|error| format!("{error:#}"))
  }

  // The option's effort reaches the claude command as `--effort <value>`; an option without effort adds no flag.
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

  // The option's headers reach the child as the gateway's custom-headers value.
  #[test]
  fn factory_routes_custom_headers() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";
    let backend = build(&format!("{endpoint}headers:\n  x-litellm-tags: deepseek\n")).unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek");
  }

  // Multiple headers reach the child in option order, one header per line.
  #[test]
  fn factory_routes_multiple_custom_headers() {
    let endpoint = "api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n";
    let backend =
      build(&format!("{endpoint}headers:\n  x-litellm-tags: deepseek\n  x-client-name: charliebot\n")).unwrap();
    let env = backend.prepare_env(Env::new()).unwrap();
    assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek\nx-client-name: charliebot");
  }

  // An option without headers sets no custom-headers value, so the parent's value does not carry over.
  #[test]
  fn factory_omits_custom_headers_when_the_option_has_none_and_the_parent_sets_them() {
    let backend = build("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n").unwrap();
    let env = backend
      .prepare_env(Env::from([(GATEWAY_CUSTOM_HEADERS_ENV.to_string(), "inherited-value".to_string())]))
      .unwrap();
    assert!(!env.contains_key(GATEWAY_CUSTOM_HEADERS_ENV));
  }

  // The option's extra_body reaches the child as the gateway's extra-body JSON.
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

  // An option without extra_body sets no extra-body value, so the parent's value does not carry over.
  #[test]
  fn factory_omits_extra_body_when_the_option_has_none_and_the_parent_sets_it() {
    let backend = build("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n").unwrap();
    let env =
      backend.prepare_env(Env::from([(GATEWAY_EXTRA_BODY_ENV.to_string(), "inherited-value".to_string())])).unwrap();
    assert!(!env.contains_key(GATEWAY_EXTRA_BODY_ENV));
  }

  #[test]
  fn an_empty_credential_key_is_refused() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(charliebot_core::config::CREDENTIALS_FILENAME), "endpoint:\n  api_key: \"\"\n")
      .unwrap();
    let cfg = Registry::new().load_config(home.path()).unwrap();
    let option =
      option("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\ncredential: endpoint\n");
    let launch = LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: None,
      log_dir: None,
      cgroup_session_id: None,
      remote_launch: None,
      backend_kwargs: serde_json::Map::new(),
      manager_turn: false,
    };
    let error = AnthropicCompatibleFactory
      .build(Some(&option), &cfg, launch)
      .err()
      .map(|error| format!("{error:#}"))
      .expect("an empty key is refused");
    assert!(error.contains("credentials.endpoint.api_key is empty"), "{error}");
  }
}
