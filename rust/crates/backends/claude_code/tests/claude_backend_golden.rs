use std::path::PathBuf;
use std::sync::Arc;

use charliebot_agents::backend_types;
use charliebot_claude_code as claude_code;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::Registry;
use charliebot_registry::backend::{Env, LaunchArgs};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as YamlValue};

const PYTHON_FIXTURE: &str = include_str!("fixtures/claude_backend_golden.json");

fn main_module() -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: PathBuf::from("/synthetic/home"),
    headless_chrome_bin: String::new(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18498,
      subprocess_buffer_limit_mb: 1024,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 4096,
      min_free_disk_gib: 10,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: charliebot_core::config::Sections::default(),
  }
}

fn cases() -> Vec<Value> {
  vec![
    json!({
      "id":"fable-fresh", "label":"Fable", "model":"claude-fable-5-1", "type_fields":{},
      "extra_flags":[], "claude_session_id":null, "claude_account":null,
      "instructions_file":null, "prompt":"fresh prompt", "env":{"HOME":"/synthetic/home", "PATH":"/synthetic/bin", "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS":"0"}
    }),
    json!({
      "id":"opus-resume", "label":"Opus", "model":"claude-opus-4-8", "type_fields":{"fast_mode":true},
      "extra_flags":["--resume", "session-001"], "claude_session_id":null, "claude_account":null,
      "instructions_file":null, "prompt":"resume prompt", "env":{"HOME":"/synthetic/home", "PATH":"/synthetic/bin"}
    }),
    json!({
      "id":"sonnet-preassigned", "label":"Sonnet", "model":"claude-sonnet-4-5", "type_fields":{},
      "extra_flags":[], "claude_session_id":"preassigned-001", "claude_account":null,
      "instructions_file":"/synthetic/run/harness_instructions.md", "prompt":"task prompt", "env":{"HOME":"/synthetic/home", "PATH":"/synthetic/bin"}
    }),
    json!({
      "id":"haiku-flags", "label":"Haiku", "model":"claude-haiku-4-5", "type_fields":{"effort":"high", "cli_binary":"/synthetic/bin/claude"},
      "extra_flags":["--extra-option", "value with spaces"], "claude_session_id":null, "claude_account":{"label":"synthetic", "config_dir":"~/accounts/one"},
      "instructions_file":null, "prompt":"flags prompt", "env":{"HOME":"/synthetic/home", "PATH":"/synthetic/bin"}
    }),
    json!({
      "id":"fable-headless", "label":"Fable", "model":"claude-fable-5-1", "type_fields":{},
      "extra_flags":[], "claude_session_id":null, "claude_account":null,
      "instructions_file":null, "prompt":"host env prompt", "env":{"HOME":"/synthetic/home", "PATH":"/synthetic/bin", "CLAUDE_CODE_AUTO_COMPACT_WINDOW":"345000", "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE":"0.8", "CLAUDE_CODE_MAX_CONTEXT_TOKENS":"250000", "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT":"1"}
    }),
  ]
}

fn option(case: &Value) -> BackendOption {
  let mut type_fields = Mapping::new();
  for (name, value) in case["type_fields"].as_object().unwrap() {
    type_fields.insert(YamlValue::String(name.clone()), serde_yaml::to_value(value).unwrap());
  }
  BackendOption {
    id: case["id"].as_str().unwrap().to_string(),
    label: case["label"].as_str().unwrap().to_string(),
    model: case["model"].as_str().unwrap().to_string(),
    prompt_overlay: None,
    backend_type: "cc-claude".to_string(),
    type_fields,
  }
}

fn rust_result(case: &Value, cfg: &CharlieBotConfig, registry: &Arc<Registry>) -> Value {
  let mut kwargs = Map::new();
  if !case["claude_session_id"].is_null() {
    kwargs.insert("claude_session_id".to_string(), case["claude_session_id"].clone());
  }
  if !case["claude_account"].is_null() {
    kwargs.insert("claude_account".to_string(), case["claude_account"].clone());
  }
  let instructions_file = case["instructions_file"].as_str().map(PathBuf::from);
  let launch = LaunchArgs {
    extra_flags: case["extra_flags"]
      .as_array()
      .unwrap()
      .iter()
      .map(|value| value.as_str().unwrap().to_string())
      .collect(),
    instructions_file,
    backend_kwargs: kwargs,
    ..LaunchArgs::default()
  };
  let backend = backend_types::build_backend(Arc::clone(registry), &option(case), cfg, launch).unwrap().backend;
  let argv = backend.build_command(case["prompt"].as_str().unwrap()).unwrap();
  let inherited: Env = case["env"]
    .as_object()
    .unwrap()
    .iter()
    .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
    .collect();
  let prepared = backend.prepare_env(inherited.clone()).unwrap();
  let env_delta = prepared
    .iter()
    .filter(|(name, value)| inherited.get(*name) != Some(*value))
    .map(|(name, value)| (name.clone(), Value::String(value.clone())))
    .collect::<Map<_, _>>();
  let stdin = backend.stdin_prompt(case["prompt"].as_str().unwrap()).map_or(Value::Null, Value::String);
  let instructions_target =
    backend.instructions_target().map(|(name, event)| json!([name, event])).unwrap_or(Value::Null);
  json!({"argv":argv,"env_delta":env_delta,"stdin":stdin,"instructions_target":instructions_target})
}

fn materialize_paths(value: &mut Value) {
  match value {
    Value::String(text) => {
      for (placeholder, path) in [("$HOME", "/synthetic/home"), ("$BIN", "/synthetic/bin"), ("$RUN", "/synthetic/run")]
      {
        *text = text.replace(placeholder, path);
      }
    }
    Value::Array(values) => values.iter_mut().for_each(materialize_paths),
    Value::Object(values) => values.values_mut().for_each(materialize_paths),
    _ => {}
  }
}

#[test]
fn registered_claude_factory_matches_the_python_argv_and_env_matrix() {
  let cases = cases();
  let cfg = main_module();
  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", claude_code::register).unwrap();
  let registry = Arc::new(registry);
  let expected_rust = cases.iter().map(|case| rust_result(case, &cfg, &registry)).collect::<Vec<_>>();
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/agents/tests/claude_backend_golden.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let mut expected_python = fixture["expected"].clone();
  materialize_paths(&mut expected_python);
  assert_eq!(serde_json::to_value(expected_rust).unwrap(), expected_python);
  assert_eq!(cases.len(), 5);
}
