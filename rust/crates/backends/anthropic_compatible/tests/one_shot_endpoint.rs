//! The Anthropic-compatible entry's one-shot: its own endpoint, headers and extra body, and no `--model` flag.

use std::collections::HashMap;

use charliebot_claude_code::claude_launch;
use charliebot_claude_code::{
  CLAUDE_MODEL_ENV_NAMES, GATEWAY_AUTH_TOKEN_ENV, GATEWAY_BASE_URL_ENV, GATEWAY_CUSTOM_HEADERS_ENV,
  GATEWAY_EXTRA_BODY_ENV,
};
use charliebot_registry::backend::{AgentBackend, LaunchArgs};
use serde_json::Value;

// Python: tests/test_anthropic_compatible_backend.py::test_one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag
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

// Python: tests/test_anthropic_compatible_backend.py::test_one_shot_text_omits_effort_when_the_entry_has_none
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
  use charliebot_anthropic_compatible::factory::AnthropicCompatibleFactory;
  use charliebot_core::config::BackendOption;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::BackendFactory;

  let option = BackendOption {
    id: "anthropic".to_string(),
    label: "Anthropic compatible".to_string(),
    model: "synthetic/model-1".to_string(),
    prompt_overlay: None,
    backend_type: "cc-anthropic-compatible".to_string(),
    type_fields: serde_yaml::from_str(type_fields).unwrap(),
  };
  let home = tempfile::tempdir().unwrap();
  let cfg = Registry::new().load_config(home.path()).unwrap();
  AnthropicCompatibleFactory.build(Some(&option), &cfg, LaunchArgs::default()).unwrap()
}

/// The inherited process environment a one-shot receives: the fake `claude` directory on `PATH`.
fn inherited_env(bin: &std::path::Path) -> charliebot_registry::backend::Env {
  charliebot_registry::backend::Env::from([
    ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
    ("HOME".to_string(), bin.parent().unwrap().display().to_string()),
  ])
}

/// The fake-program helper that cargo builds beside this test binary. `CARGO_BIN_EXE_*` names only this package's
/// binaries, and the helper belongs to `charliebot-agents`.
fn fake_program_bin() -> std::path::PathBuf {
  let exe = std::env::current_exe().expect("the test binary path");
  exe
    .ancestors()
    .skip(1)
    .map(|directory| directory.join("fake_program"))
    .find(|candidate| candidate.is_file())
    .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
}

/// Stages a fake `claude` that records its argv and environment, reads stdin, prints `ok`, and exits 0. Returns the
/// bin directory and the recorded argv and environment files.
fn stage_fake_claude(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
  let bin = root.join("bin");
  std::fs::create_dir_all(&bin).unwrap();
  std::os::unix::fs::symlink(fake_program_bin(), bin.join("claude")).unwrap();
  let argv = root.join("argv.txt");
  let env_file = root.join("env.txt");
  let stdin = root.join("stdin.txt");
  std::fs::write(
    bin.join("probe.json"),
    serde_json::json!({"claude": [
      {"record_argv": argv},
      {"record_env": env_file},
      {"record_stdin": stdin},
      {"stdout": "ok"},
      {"exit": 0},
    ]})
    .to_string(),
  )
  .unwrap();
  (bin, argv, env_file)
}

fn recorded_lines(path: &std::path::Path) -> Vec<String> {
  std::fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
}

fn recorded_env(path: &std::path::Path) -> HashMap<String, String> {
  recorded_lines(path)
    .into_iter()
    .filter_map(|line| line.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())))
    .collect()
}
