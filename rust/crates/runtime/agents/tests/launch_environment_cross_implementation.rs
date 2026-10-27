use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_agents::agent_environment;
use charliebot_agents::backend_types;
use charliebot_agents::launch_loop::{self, ChildEnvPolicy};
use charliebot_claude_code as claude_code;
use charliebot_codex::CodexHost;
use charliebot_codex::factory::CodexFactory;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::Registry;
use charliebot_registry::backend::{BackendFactory, Env, LaunchArgs};
use charliebot_registry::context::StartupState;
use charliebot_store::run_records::RunKind;
use serde_json::{Value, json};
use serde_yaml::Mapping;

const PYTHON_FIXTURE: &str = include_str!("fixtures/launch_environment_reference.json");

struct Case {
  id: String,
  launch_kind: &'static str,
  backend_type: &'static str,
  process_env: Env,
  venv_bin: PathBuf,
  server_bin_dir: PathBuf,
}

#[test]
fn complete_agent_child_environments_match_python_for_each_launch_kind() {
  let root = tempfile::tempdir().unwrap();
  let venv_bin = root.path().join("venv/bin");
  let system_bin = root.path().join("system-bin");
  let server_bin_dir = root.path().join("release");
  fs::create_dir_all(&venv_bin).unwrap();
  fs::create_dir_all(&system_bin).unwrap();
  fs::create_dir_all(&server_bin_dir).unwrap();
  let cli = server_bin_dir.join("charliebot");
  fs::write(&cli, "test fixture; this file is not run\n").unwrap();
  let mut permissions = fs::metadata(&cli).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&cli, permissions).unwrap();
  symlink(env!("CARGO_BIN_EXE_fake_program"), system_bin.join("codex")).unwrap();
  fs::write(system_bin.join("probe.json"), json!({"codex": [{"exit": 0}]}).to_string()).unwrap();
  let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().to_path_buf();
  let cases = cases(&venv_bin, &system_bin, &server_bin_dir);
  let python = run_python_reference(&repository, &cases);
  let python_cases = python.as_array().expect("Python reference returns a case array");
  assert_eq!(cases.len(), 14);
  assert_eq!(python_cases.len(), cases.len());

  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", claude_code::register).unwrap();
  let registry = Arc::new(registry);
  let cfg = main_config();
  let mut rust_cases = Vec::new();

  for case in &cases {
    let rewritten =
      agent_environment::agent_environment(&case.process_env, &case.venv_bin, &case.server_bin_dir).unwrap();
    let startup = StartupState::default();
    startup.insert(rewritten).unwrap();
    let base = agent_environment::require_agent_base_environment(&startup).unwrap();
    let backend_option = backend_option(case.backend_type);
    let backend = if case.backend_type == "codex" {
      CodexFactory::new(CodexHost {
        path: system_bin.to_string_lossy().into_owned(),
        home: PathBuf::from("/synthetic/home"),
      })
      .build(Some(&backend_option), &cfg, LaunchArgs::default())
      .unwrap()
    } else {
      backend_types::build_backend(Arc::clone(&registry), &backend_option, &cfg, LaunchArgs::default()).unwrap().backend
    };
    let child_env = match case.launch_kind {
      "tool-capable-one-shot" => {
        let inherited =
          launch_loop::build_child_env(&base.values, &registry, ChildEnvPolicy::Inherited, None, &Env::new());
        backend.prepare_env(inherited).unwrap()
      }
      "cli-native-one-shot" if case.backend_type == "cc-claude" => base.values.clone(),
      "cli-native-one-shot" if case.backend_type == "codex" => backend.prepare_env(base.values.clone()).unwrap(),
      "manager-turn" => {
        let overlay = Env::from([
          ("GIT_CEILING_DIRECTORIES".to_string(), "/synthetic/home".to_string()),
          ("RUN_ADDITION".to_string(), "manager-turn".to_string()),
        ]);
        let inherited = launch_loop::build_child_env(
          &base.values,
          &registry,
          ChildEnvPolicy::RegisteredEdits,
          Some("current-session"),
          &overlay,
        );
        backend.prepare_env(inherited).unwrap()
      }
      "compaction" if case.backend_type == "cc-claude" => claude_code::claude_compaction::compaction_env_from(
        Path::new("/synthetic/home/accounts/claude-one"),
        &base.values,
        RunKind::ManagerTurn,
      )
      .unwrap(),
      _ => panic!("unsupported launch kind/backend pair: {}/{}", case.launch_kind, case.backend_type),
    };
    rust_cases.push(json!({
      "id": case.id,
      "base_env": base.values.clone(),
      "child_env": child_env,
    }));
  }

  assert_eq!(Value::Array(rust_cases), Value::Array(python_cases.clone()));
}

fn cases(venv_bin: &Path, system_bin: &Path, server_bin_dir: &Path) -> Vec<Case> {
  let combinations = [
    ("tool-capable-one-shot", "cc-claude"),
    ("tool-capable-one-shot", "codex"),
    ("cli-native-one-shot", "cc-claude"),
    ("cli-native-one-shot", "codex"),
    ("manager-turn", "cc-claude"),
    ("manager-turn", "codex"),
    ("compaction", "cc-claude"),
  ];
  let venv = venv_bin.parent().unwrap();
  let mut cases = Vec::new();
  for (launch_kind, backend_type) in combinations {
    for (pydantic_mode, pydantic_value) in [("unset", None), ("custom", Some("custom-policy"))] {
      let mut process_env = Env::from([
        ("CHARLIEBOT_SESSION_ID".to_string(), "parent-session".to_string()),
        ("CLAUDECODE".to_string(), "1".to_string()),
        ("HOME".to_string(), "/synthetic/home".to_string()),
        ("PATH".to_string(), format!("{}:{}", venv_bin.display(), system_bin.display())),
        ("SYNTHETIC_SERVER_VALUE".to_string(), "retained".to_string()),
        ("UV_RUN_RECURSION_DEPTH".to_string(), "1".to_string()),
        ("VIRTUAL_ENV".to_string(), venv.to_string_lossy().into_owned()),
      ]);
      if let Some(value) = pydantic_value {
        process_env.insert("PYDANTIC_DISABLE_PLUGINS".to_string(), value.to_string());
      }
      cases.push(Case {
        id: format!("{launch_kind}-{backend_type}-pydantic-{pydantic_mode}"),
        launch_kind,
        backend_type,
        process_env,
        venv_bin: venv_bin.to_path_buf(),
        server_bin_dir: server_bin_dir.to_path_buf(),
      });
    }
  }
  cases
}

fn run_python_reference(repository: &Path, cases: &[Case]) -> Value {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/agents/tests/launch_environment_reference.py");
  assert_eq!(fixture["base_commit"], "2b212ebd8549c0d3bc86885ae9c30ac648039a0d");
  let temporary = cases.first().unwrap().venv_bin.parent().unwrap().parent().unwrap();
  let mut expected = fixture["expected"].clone();
  replace_fixture_paths(
    &mut expected,
    &[
      ("$REPO", &repository.to_string_lossy()),
      ("$TMPROOT", &temporary.to_string_lossy()),
      ("$HOME", "/synthetic/home"),
    ],
  );
  assert_eq!(expected.as_array().unwrap().len(), cases.len());
  expected
}

fn replace_fixture_paths(value: &mut Value, replacements: &[(&str, &str)]) {
  match value {
    Value::String(text) => {
      for (placeholder, path) in replacements {
        *text = text.replace(placeholder, path);
      }
    }
    Value::Array(values) => values.iter_mut().for_each(|value| replace_fixture_paths(value, replacements)),
    Value::Object(fields) => fields.values_mut().for_each(|value| replace_fixture_paths(value, replacements)),
    _ => {}
  }
}

fn backend_option(backend_type: &str) -> BackendOption {
  let model = if backend_type == "cc-claude" { "claude-sonnet-4-5" } else { "gpt-5.5" };
  BackendOption {
    id: format!("synthetic-{backend_type}"),
    label: "Synthetic backend".to_string(),
    model: model.to_string(),
    prompt_overlay: None,
    backend_type: backend_type.to_string(),
    type_fields: Mapping::new(),
  }
}

fn main_config() -> CharlieBotConfig {
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
    sections: Sections::default(),
  }
}
