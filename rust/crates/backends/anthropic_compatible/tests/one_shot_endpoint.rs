//! The Anthropic-compatible entry's one-shot: its own endpoint, headers and extra body, and no `--model` flag.

use charliebot_claude_code::claude_launch;
use charliebot_claude_code::test_support::{inherited_env, recorded_env, recorded_lines, stage_fake_claude};
use charliebot_claude_code::{
  CLAUDE_MODEL_ENV_NAMES, GATEWAY_AUTH_TOKEN_ENV, GATEWAY_BASE_URL_ENV, GATEWAY_CUSTOM_HEADERS_ENV,
  GATEWAY_EXTRA_BODY_ENV,
};
use charliebot_registry::backend::{AgentBackend, LaunchArgs};
use serde_json::Value;

#[ignore = "integration"]
// A one-shot runs on the entry's endpoint with its headers and extra body, and passes no --model flag.
#[tokio::test]
async fn one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag() {
  let root = tempfile::tempdir().unwrap();
  let (bin, argv, env_file) = stage_fake_claude(root.path());
  let mut backend = endpoint_backend(concat!(
    "api_base: http://localhost:8123/api/anthropic-proxy\n",
    "context_window: 409600\n",
    "effort: high\n",
    "headers:\n  x-litellm-tags: deepseek\n",
    "extra_body:\n  cache:\n    no-store: true\n    no-cache: true\n",
  ));
  let base_env = inherited_env(&bin);

  assert_eq!(backend.one_shot_text(&base_env, "reply with ok", "system", None).unwrap().await.unwrap(), "ok");

  let args = recorded_lines(&argv);
  assert!(!args.iter().any(|value| value == "--model"), "{args:?}");
  assert_eq!(args[args.iter().position(|value| value == "--effort").unwrap() + 1], "high");
  let env = recorded_env(&env_file);
  assert_eq!(env[GATEWAY_BASE_URL_ENV], "http://localhost:8123/api/anthropic-proxy");
  assert_eq!(env[GATEWAY_AUTH_TOKEN_ENV], "no-auth");
  for name in CLAUDE_MODEL_ENV_NAMES {
    assert_eq!(env[name], "synthetic/model-1");
  }
  assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek");
  assert_eq!(
    serde_json::from_str::<Value>(&env[GATEWAY_EXTRA_BODY_ENV]).unwrap(),
    serde_json::json!({"cache": {"no-store": true, "no-cache": true}})
  );
  assert_eq!(env[claude_launch::AUTO_COMPACT_WINDOW_ENV], "409600");
  assert_eq!(env[claude_launch::MAX_CONTEXT_TOKENS_ENV], "409600");
}

#[ignore = "integration"]
// A one-shot of an entry without effort passes no --effort flag.
#[tokio::test]
async fn one_shot_text_omits_effort_when_the_entry_has_none() {
  let root = tempfile::tempdir().unwrap();
  let (bin, argv, _env_file) = stage_fake_claude(root.path());
  let mut backend = endpoint_backend("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n");

  backend.one_shot_text(&inherited_env(&bin), "reply with ok", "system", None).unwrap().await.unwrap();

  assert!(!recorded_lines(&argv).iter().any(|value| value == "--effort"));
}

/// The backend the Anthropic-compatible factory builds from these type fields.
fn endpoint_backend(type_fields: &str) -> Box<dyn AgentBackend> {
  use charliebot_anthropic_compatible::TYPE_NAME;
  use charliebot_anthropic_compatible::factory::AnthropicCompatibleFactory;
  use charliebot_core::config::BackendOption;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::BackendFactory;

  let option = BackendOption {
    id: "anthropic".to_string(),
    label: "Anthropic compatible".to_string(),
    model: "synthetic/model-1".to_string(),
    prompt_overlay: None,
    billing: None,
    backend_type: TYPE_NAME.to_string(),
    type_fields: serde_yaml::from_str(type_fields).unwrap(),
    visible: true,
  };
  let home = tempfile::tempdir().unwrap();
  let cfg = Registry::new().load_config(home.path()).unwrap();
  AnthropicCompatibleFactory.build(Some(&option), &cfg, LaunchArgs::default()).unwrap()
}
