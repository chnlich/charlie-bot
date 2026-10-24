//! Cross-implementation checks of the gemini, charlie-code, antigravity and Anthropic-compatible backend types.
//! `cli_backends_reference.py` runs every Python section in one `uv run python` process over the synthetic inputs built
//! here. The Rust port runs the same inputs, and the test fails with every section that differs.
//!
//! Sections: the argv/env/stdin/instructions matrix, built through each type's factory (19 cases); the translator
//! streams of gemini and charlie-code (2); and the antigravity finish cases, each run through the `AgentRunner`
//! final-stdout path over a fake `agy` (11 cases).

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backends::anthropic_compatible::{self, factory::AnthropicCompatibleFactory};
use charliebot_agents::backends::antigravity::factory::AntigravityFactory;
use charliebot_agents::backends::charlie_code::factory::CharlieCodeFactory;
use charliebot_agents::backends::gemini::factory::GeminiFactory;
use charliebot_agents::cli_base::CliHost;
use charliebot_agents::launch_loop::ChildEnvPolicy;
use charliebot_agents::runner::{AgentRunner, RunRequest};
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, BackendFactory, Env, LaunchArgs};
use charliebot_registry::usage::UsageSource;
use serde_json::{Value, json};
use serde_yaml::Mapping;

const PROMPT: &str = "Review \"the\" synthetic\nprompt \\ with \u{fc}n\u{ef}code\tand a tab";
const INSTRUCTIONS: &str = "Rules: \"quoted\" ]\n[section] \\ \u{7f} \u{fc}n\u{ef}";
const WORKER_FILE: &str = "/synthetic/instructions/worker.md";
const GEMINI_MODEL: &str = "gemini-synthetic-pro";
const CLC_MODEL: &str = "clc-synthetic-model";
const AGY_MODEL: &str = "agy-synthetic-model";
const ANTHROPIC_MODEL: &str = "anthropic-compatible-synthetic-model";
const ANTHROPIC_API_BASE: &str = "https://anthropic-compatible.synthetic.example";
const ANTHROPIC_CONTEXT_WINDOW: u64 = 409_600;
const CLC_API_BASE: &str = "https://synthetic.example/v1";

/// The synthetic homes, binaries and files of one run, written under one temporary root.
struct Fixture {
  root: PathBuf,
  home: PathBuf,
  charliebot_home: PathBuf,
  no_credentials_home: PathBuf,
  bin: PathBuf,
  transport: PathBuf,
}

impl Fixture {
  /// The PATH the factories resolve their binaries on: the fake binaries first, then the system directories.
  fn path(&self) -> String {
    format!("{}:/usr/bin:/bin", path_text(&self.bin))
  }

  fn host(&self) -> CliHost {
    CliHost { path: self.path(), home: self.home.clone() }
  }

  /// The host environment of both sides. The Python reference's os.environ is exactly this map, and the Claude headless
  /// names in it are the ones `headless_claude_env` forwards, one of them overriding CharlieBot's default window.
  fn env_base(&self) -> Value {
    json!({
      "HOME": path_text(&self.home),
      "PATH": "/usr/bin:/bin",
      "GEMINI_API_KEY": "synthetic-gemini-key",
      "GOOGLE_API_KEY": "synthetic-google-key",
      "NO_PROXY": "internal.synthetic.example",
      "OTHER_VAR": "keep",
      "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "250000",
      "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT": "1",
      "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "345000",
      "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "0.8",
      "CLAUDE_CODE_THINKING_DISPLAY_UPDATES": "1",
      "ANTHROPIC_API_KEY": "inherited-value",
      "CLAUDE_CODE_OAUTH_TOKEN": "inherited-value",
    })
  }

  fn config(&self) -> CharlieBotConfig {
    CharlieBotConfig {
      charliebot_home: self.charliebot_home.clone(),
      headless_chrome_bin: String::new(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18499,
        subprocess_buffer_limit_mb: 1024,
        session_memory_max_mb: 12288,
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
}

// Python: tests/test_anthropic_compatible_backend.py:37.
// Python: tests/test_anthropic_compatible_backend.py:72.
#[test]
fn cli_backends_match_the_python_package_on_synthetic_inputs() {
  let root = tempfile::tempdir().unwrap();
  let fx = write_fixtures(root.path());
  let request = request(&fx);
  let python = run_reference(&request);

  let argv = request["argv_cases"].as_array().unwrap();
  let streams = request["translate_streams"].as_array().unwrap();
  let finish = request["finish_cases"].as_array().unwrap();
  assert_eq!(argv.len(), 19);
  assert_eq!(streams.len(), 2);
  assert_eq!(finish.len(), 11);

  // The finish equality below says nothing about a run that emitted nothing, so the reference must reach both paths.
  assert_eq!(python["finish"][0]["events"].as_array().unwrap().len(), 3);
  assert!(python["finish"].as_array().unwrap().iter().any(|case| case["guard"].is_string()));

  let mut failures = Vec::new();
  check_argv(&fx, argv, &python, &mut failures);
  check_translate(&fx, streams, &python, &mut failures);
  check_finish(&fx, finish, &python, &mut failures);
  assert!(failures.is_empty(), "CLI backend sections differ from the Python package:\n{}", failures.join("\n"));
}

fn write_fixtures(root: &Path) -> Fixture {
  let fx = Fixture {
    root: root.to_path_buf(),
    home: root.join("home"),
    charliebot_home: root.join("charliebot"),
    no_credentials_home: root.join("no-credentials"),
    bin: root.join("bin"),
    transport: root.join("transport"),
  };
  for dir in [&fx.home, &fx.charliebot_home, &fx.no_credentials_home, &fx.bin, &fx.transport] {
    fs::create_dir_all(dir).unwrap();
  }
  write_fakes(&fx.bin, json!({"gemini": [{"exit": 0}], "charlie-code": [{"exit": 0}], "agy": [{"exit": 0}]}));
  write_text(
    &fx.charliebot_home.join("credentials.yaml"),
    "anthropic-main:\n  api_key: synthetic-anthropic-key\nclc-main:\n  api_key: synthetic-clc-key\n",
  );
  write_text(&fx.no_credentials_home.join("credentials.yaml"), "not: a mapping of credential sections\n");
  for spec in finish_specs() {
    let dir = root.join("finish").join(spec.id);
    fs::create_dir_all(dir.join("cwd")).unwrap();
    let steps = json!([{"stdout": spec.stdout}, {"stderr": spec.stderr}, {"exit": spec.exit}]);
    write_fakes(&dir, json!({"agy": steps}));
  }
  fx
}

fn request(fx: &Fixture) -> Value {
  json!({
    "home": path_text(&fx.home),
    "charliebot_home": path_text(&fx.charliebot_home),
    "path": fx.path(),
    "transport": path_text(&fx.transport),
    "env_base": fx.env_base(),
    "argv_cases": argv_cases(),
    "translate_streams": translate_streams(),
    "finish_cases": finish_cases(fx),
  })
}

fn register_anthropic_compatible(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  anthropic_compatible::register(registry)
}

fn register_claude_usage_source(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.usage_source(UsageSource {
    name: "Claude Code",
    id_prefixes: &[],
    run_logs_only: false,
    implementation: None,
  })
}

fn argv_cases() -> Vec<Value> {
  let clc_full = json!({
    "api_base": CLC_API_BASE,
    "context_window": 128000,
    "credential": "clc-main",
    "image_input": true,
    "stream": true,
    "timeout_seconds": 600,
    "top_p": 0.9,
    "temperature": 0.2,
    "proxy_url": "http://proxy.synthetic:3128",
  });
  vec![
    argv_case("gemini_plain", "gemini", json!({}), &[], None, None, None),
    argv_case(
      "gemini_flags_resume",
      "gemini",
      json!({}),
      &["--include-dirs", "/synthetic/extra"],
      Some("gem-sess-1"),
      None,
      None,
    ),
    argv_case("gemini_content", "gemini", json!({}), &[], None, None, Some(INSTRUCTIONS)),
    argv_case("gemini_file_and_content", "gemini", json!({}), &[], None, Some(WORKER_FILE), Some(INSTRUCTIONS)),
    argv_case("clc_minimal", "charlie-code", json!({"api_base": CLC_API_BASE}), &[], None, None, None),
    argv_case("clc_full", "charlie-code", clc_full, &["--verbose"], Some("clc-sess-1"), Some(WORKER_FILE), None),
    argv_case(
      "clc_no_stream_no_images",
      "charlie-code",
      json!({"api_base": CLC_API_BASE, "stream": false, "image_input": false}),
      &[],
      None,
      None,
      None,
    ),
    argv_case(
      "clc_content_only",
      "charlie-code",
      json!({"api_base": CLC_API_BASE, "context_window": 4096}),
      &[],
      None,
      None,
      Some(INSTRUCTIONS),
    ),
    argv_case(
      "clc_unit_sampling",
      "charlie-code",
      json!({"api_base": CLC_API_BASE, "top_p": 1.0, "temperature": 1.0, "timeout_seconds": 30, "credential": "clc-main"}),
      &[],
      None,
      None,
      None,
    ),
    argv_case(
      "clc_proxy",
      "charlie-code",
      json!({"api_base": CLC_API_BASE, "proxy_url": "http://proxy.synthetic:3128"}),
      &[],
      None,
      None,
      None,
    ),
    argv_case("agy_plain", "antigravity", json!({}), &[], None, None, None),
    argv_case(
      "agy_print_timeout_flags",
      "antigravity",
      json!({"print_timeout": "30m"}),
      &["--verbose"],
      None,
      None,
      None,
    ),
    argv_case("agy_resume", "antigravity", json!({}), &[], Some("conv-synth-7"), None, None),
    argv_case("agy_content", "antigravity", json!({}), &[], None, None, Some(INSTRUCTIONS)),
    argv_case(
      "agy_file_and_content",
      "antigravity",
      json!({"print_timeout": "5m"}),
      &[],
      None,
      Some(WORKER_FILE),
      Some(INSTRUCTIONS),
    ),
    argv_case(
      "anthropic_compatible_plain",
      "cc-anthropic-compatible",
      json!({"api_base": ANTHROPIC_API_BASE, "context_window": ANTHROPIC_CONTEXT_WINDOW}),
      &[],
      None,
      None,
      None,
    ),
    argv_case(
      "anthropic_compatible_flags",
      "cc-anthropic-compatible",
      json!({"api_base": format!("{ANTHROPIC_API_BASE}/"), "context_window": ANTHROPIC_CONTEXT_WINDOW, "credential": "anthropic-main"}),
      &["--max-turns", "3"],
      None,
      None,
      None,
    ),
    argv_case(
      "anthropic_compatible_resume",
      "cc-anthropic-compatible",
      json!({"api_base": ANTHROPIC_API_BASE, "context_window": ANTHROPIC_CONTEXT_WINDOW, "credential": "anthropic-main"}),
      &[],
      Some("anthropic-sess-1"),
      None,
      None,
    ),
    argv_case(
      "anthropic_compatible_file_and_content",
      "cc-anthropic-compatible",
      json!({"api_base": ANTHROPIC_API_BASE, "context_window": ANTHROPIC_CONTEXT_WINDOW, "credential": "anthropic-main"}),
      &[],
      None,
      Some(WORKER_FILE),
      Some(INSTRUCTIONS),
    ),
  ]
}

fn argv_case(
  id: &str,
  backend_type: &str,
  type_fields: Value,
  extra_flags: &[&str],
  resume: Option<&str>,
  instructions_file: Option<&str>,
  instructions_content: Option<&str>,
) -> Value {
  let model = match backend_type {
    "gemini" => GEMINI_MODEL,
    "charlie-code" => CLC_MODEL,
    "antigravity" => AGY_MODEL,
    "cc-anthropic-compatible" => ANTHROPIC_MODEL,
    other => panic!("no synthetic model for {other}"),
  };
  json!({
    "id": id,
    "backend_type": backend_type,
    "model": model,
    "type_fields": type_fields,
    "extra_flags": extra_flags,
    "resume_session_id": resume,
    "instructions_file": instructions_file,
    "instructions_content": instructions_content,
    "prompt": PROMPT,
  })
}

fn translate_streams() -> Vec<Value> {
  vec![
    json!({"id": "gemini", "backend_type": "gemini", "model": GEMINI_MODEL, "type_fields": {}, "events": gemini_events()}),
    json!({
      "id": "charlie-code",
      "backend_type": "charlie-code",
      "model": CLC_MODEL,
      "type_fields": {"api_base": CLC_API_BASE},
      "events": charlie_code_events(),
    }),
  ]
}

fn gemini_events() -> Vec<Value> {
  vec![
    json!({"type": "init", "session_id": "gem-sess-9"}),
    json!({"type": "message", "role": "user", "content": "ignored by the translator"}),
    json!({"type": "message", "role": "assistant", "content": "Hel", "delta": true}),
    json!({"type": "message", "role": "assistant", "content": "lo", "delta": true}),
    json!({"type": "tool_use", "tool_name": "read_file", "tool_id": "t1", "parameters": {"path": "/synthetic/a.txt"}}),
    json!({"type": "tool_result", "tool_id": "t1", "status": "success", "output": "file body"}),
    json!({"type": "message", "role": "assistant", "content": "Done.", "delta": false}),
    json!({"type": "tool_result", "tool_id": "t2", "status": "error", "error": {"message": "no such file"}}),
    json!({"type": "tool_result", "tool_id": "t3", "status": "error", "error": "plain text error"}),
    json!({"type": "tool_result", "tool_id": "t4", "status": "pending"}),
    json!({"type": "message", "role": "assistant", "content": "tail", "delta": true}),
    json!({"type": "error", "message": "quota reached"}),
    json!({"type": "message", "role": "assistant", "content": "buffered end", "delta": true}),
    json!({"type": "message", "role": "system", "content": "system line"}),
    json!({"type": "mystery"}),
    json!({"type": "result", "stats": {"input_tokens": 11, "output_tokens": 7, "cached": 3}}),
    json!({"type": "result", "stats": {}}),
  ]
}

fn charlie_code_events() -> Vec<Value> {
  let long_command =
    "python3 -m pytest   tests/unit/test_synthetic_module_with_a_long_name.py   -k 'alpha or beta or gamma or delta'";
  vec![
    json!({"type": "session", "session_id": "clc-sess-1"}),
    json!({"type": "thought", "text": "Planning the synthetic change"}),
    json!({"type": "command", "id": "c1", "command": "ls   -la\n  /synthetic"}),
    json!({"type": "command_progress", "id": "c1", "killed": false, "elapsed_seconds": 65.5, "pid": 4242}),
    json!({"type": "observation", "id": "c1", "output": "listing"}),
    json!({"type": "command", "id": "c2", "command": long_command}),
    json!({"type": "command_progress", "id": "c2", "killed": true, "elapsed_seconds": 3600.0, "pid": 7}),
    json!({"type": "command_progress", "id": "zz", "killed": false, "elapsed_seconds": 1.0, "pid": 9}),
    json!({"type": "observation", "id": "c2", "output": "killed"}),
    json!({"type": "result", "usage": {"input_tokens": 120, "output_tokens": 30}}),
    json!({"type": "error", "message": "synthetic failure"}),
    json!({"type": "compact", "trigger": "auto", "pre_tokens": 90000}),
    json!({
      "type": "context",
      "model": "clc-synthetic-model",
      "prompt_tokens": 1200,
      "context_window": 128000,
      "compact_threshold": 100000,
    }),
    json!({"type": "context", "model": null, "prompt_tokens": "x", "context_window": 1, "compact_threshold": 2}),
    json!({"type": "mystery"}),
  ]
}

/// One fake `agy` run: the stdout and stderr it prints, the exit code it leaves with, and the conversation it resumes.
struct FinishSpec {
  id: &'static str,
  stdout: String,
  stderr: &'static str,
  exit: i32,
  resume: Option<&'static str>,
}

fn spec(id: &'static str, stdout: String, stderr: &'static str, exit: i32, resume: Option<&'static str>) -> FinishSpec {
  FinishSpec { id, stdout, stderr, exit, resume }
}

/// The fake `agy` runs. A non-string `response` and a non-object `usage` are left out: Python raises TypeError (from
/// `re.sub`) or AttributeError (from `.get`) on them, and the port reports a named envelope guard instead.
fn finish_specs() -> Vec<FinishSpec> {
  vec![
    spec(
      "success",
      envelope(json!({
        "status": "SUCCESS",
        "conversation_id": "conv-1",
        "response": "Answer <SYSTEM_MESSAGE>\nplatform note\n</SYSTEM_MESSAGE>\n  body",
        "usage": {"input_tokens": 10, "output_tokens": 4, "thinking_tokens": 2, "cache_read_tokens": 6},
      })),
      "",
      0,
      None,
    ),
    spec("exit_with_envelope_error", envelope(json!({"status": "FAILED", "error": "quota"})), "", 3, None),
    spec("exit_with_plain_stdout", "plain crash text".to_string(), "", 2, None),
    spec("exit_with_empty_stdout", String::new(), "", 5, None),
    spec("exit_zero_non_envelope", "not json at all".to_string(), "", 0, None),
    spec(
      "print_timeout",
      envelope(json!({"status": "SUCCESS", "conversation_id": "conv-2", "response": "partial"})),
      "[agy] print timeout after 30m with turn in progress\n",
      0,
      None,
    ),
    spec("failed_with_response", envelope(json!({"status": "FAILED", "response": "partial answer"})), "", 0, None),
    spec("failed_bare", envelope(json!({"status": "FAILED"})), "", 0, None),
    spec("success_without_conversation", envelope(json!({"status": "SUCCESS", "response": "orphan"})), "", 0, None),
    spec(
      "resume_mismatch",
      envelope(json!({"status": "SUCCESS", "conversation_id": "conv-other", "response": "x"})),
      "",
      0,
      Some("conv-anchor"),
    ),
    spec(
      "resume_match",
      envelope(json!({
        "status": "SUCCESS",
        "conversation_id": "conv-1",
        "response": "again",
        "usage": {"input_tokens": 3},
      })),
      "",
      0,
      Some("conv-1"),
    ),
  ]
}

fn envelope(value: Value) -> String {
  value.to_string()
}

fn finish_cases(fx: &Fixture) -> Vec<Value> {
  finish_specs()
    .iter()
    .map(|spec| {
      json!({
        "id": spec.id,
        "backend_type": "antigravity",
        "model": AGY_MODEL,
        "type_fields": {},
        "resume": spec.resume,
        "prompt": PROMPT,
        "dir": path_text(&fx.root.join("finish").join(spec.id)),
      })
    })
    .collect()
}

fn run_reference(request: &Value) -> Value {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let helper = "rust/crates/charliebot-agents/tests/cli_backends_reference.py";
  let mut child = Command::new("uv")
    .args(["run", "python", helper])
    .current_dir(repo_root)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
  let output = child.wait_with_output().unwrap();
  assert!(
    output.status.success(),
    "Python CLI backend reference failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  let stdout = String::from_utf8(output.stdout).unwrap();
  let last = stdout.lines().last().expect("the Python CLI backend reference printed nothing");
  serde_json::from_str(last).unwrap()
}

fn check_argv(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let expected = python["argv"].as_array().expect("the argv section");
  expect_eq(failures, "argv case count", &json!(cases.len()), &json!(expected.len()));
  for (case, python_case) in cases.iter().zip(expected) {
    let mut backend = build_backend(fx, &option_of(case), launch_of(case));
    backend.prepare_transport(&fx.transport).unwrap();
    let prompt = text(&case["prompt"]);
    let rust = json!({
      "id": case["id"],
      "argv": backend.build_command(prompt).unwrap(),
      "env": backend.prepare_env(env_map(&fx.env_base())).unwrap(),
      "stdin": backend.stdin_prompt(prompt),
      "instructions_target": backend.instructions_target().map(|(file, event)| json!([file, event])),
    });
    expect_eq(failures, &format!("argv {}", text(&case["id"])), &rust, python_case);
  }
}

fn check_translate(fx: &Fixture, streams: &[Value], python: &Value, failures: &mut Vec<String>) {
  for stream in streams {
    let backend = build_backend(fx, &option_of(stream), LaunchArgs::default());
    let mut translator = backend.translator();
    let rust: Vec<Vec<Value>> =
      stream["events"].as_array().unwrap().iter().map(|event| translator.translate(event.clone()).unwrap()).collect();
    let id = text(&stream["id"]);
    expect_eq(failures, &format!("translate {id}"), &json!(rust), &python["translate"][id]);
  }
}

fn check_finish(fx: &Fixture, cases: &[Value], python: &Value, failures: &mut Vec<String>) {
  let expected = python["finish"].as_array().expect("the finish section");
  expect_eq(failures, "finish case count", &json!(cases.len()), &json!(expected.len()));
  let runtime = tokio::runtime::Runtime::new().unwrap();
  for (case, python_case) in cases.iter().zip(expected) {
    let rust = runtime.block_on(run_finish(fx, case));
    expect_eq(failures, &format!("finish {}", text(&case["id"])), &rust, python_case);
  }
}

/// Runs one fake `agy` through the runner's final-stdout path. Returns the events it emitted, the exit code the runner
/// recorded, and the guard error that ended the run.
async fn run_finish(fx: &Fixture, case: &Value) -> Value {
  let dir = PathBuf::from(text(&case["dir"]));
  let host = CliHost { path: path_text(&dir), home: fx.home.clone() };
  let launch = LaunchArgs { resume_session_id: opt_text(&case["resume"]), ..LaunchArgs::default() };
  let backend = AntigravityFactory::new(host).build(Some(&option_of(case)), &fx.config(), launch).unwrap();
  let runner = AgentRunner::new(backend, LaunchArgs::default());
  let environment =
    AgentBaseEnvironment { values: env_map(&json!({"PATH": format!("{}:/usr/bin:/bin", path_text(&dir))})) };
  let mut request = RunRequest::new(
    text(&case["prompt"]).to_string(),
    dir.join("cwd"),
    environment,
    fx.root.join("cgroups"),
    Arc::new(Registry::new()),
  );
  request.child_env_policy = ChildEnvPolicy::Inherited;
  let seen = Arc::new(Mutex::new(Vec::new()));
  let sink = Arc::clone(&seen);
  request.on_event = Arc::new(move |event| {
    let sink = Arc::clone(&sink);
    Box::pin(async move {
      sink.lock().unwrap().push(event);
      Ok(())
    })
  });
  let guard = runner.run(request).await.err().map(|error| error.to_string());
  let events = seen.lock().unwrap().clone();
  json!({"id": case["id"], "events": events, "exit_code": runner.exit_code(), "guard": guard})
}

fn build_backend(fx: &Fixture, option: &BackendOption, launch: LaunchArgs) -> Box<dyn AgentBackend> {
  let mut cfg = fx.config();
  if option.backend_type == "cc-anthropic-compatible"
    && !option.type_fields.contains_key(serde_yaml::Value::String("credential".to_string()))
  {
    cfg.charliebot_home = fx.no_credentials_home.clone();
  }
  let built = match option.backend_type.as_str() {
    "gemini" => GeminiFactory::new(fx.host()).build(Some(option), &cfg, launch),
    "charlie-code" => CharlieCodeFactory::new(fx.host()).build(Some(option), &cfg, launch),
    "antigravity" => AntigravityFactory::new(fx.host()).build(Some(option), &cfg, launch),
    "cc-anthropic-compatible" => AnthropicCompatibleFactory.build(Some(option), &cfg, launch),
    other => panic!("no factory for backend type {other}"),
  };
  built.unwrap()
}

fn option_of(case: &Value) -> BackendOption {
  BackendOption {
    id: text(&case["id"]).to_string(),
    label: "Synthetic".to_string(),
    model: text(&case["model"]).to_string(),
    prompt_overlay: None,
    backend_type: text(&case["backend_type"]).to_string(),
    type_fields: serde_yaml::from_value::<Mapping>(serde_yaml::to_value(&case["type_fields"]).unwrap()).unwrap(),
  }
}

// Python: tests/test_anthropic_compatible_backend.py:83.
#[test]
fn anthropic_compatible_options_require_a_positive_window_and_endpoint_root() {
  let root = tempfile::tempdir().unwrap();
  let config = |api_base: &str, window: i64| {
    format!(
      "backends:\n  options:\n    - id: anthropic\n      label: Anthropic compatible\n      type: cc-anthropic-compatible\n      model: synthetic-model\n      api_base: {api_base}\n      context_window: {window}\n"
    )
  };
  let mut registry = Registry::new();
  registry.register_package("src.backends.anthropic_compatible", register_anthropic_compatible).unwrap();
  let home = root.path().join("home");
  fs::create_dir_all(&home).unwrap();

  fs::write(home.join("config.yaml"), config(ANTHROPIC_API_BASE, ANTHROPIC_CONTEXT_WINDOW as i64)).unwrap();
  let option = registry.load_config(&home).unwrap().backends.options.remove(0);
  assert!(!option.type_fields.contains_key(serde_yaml::Value::String("credential".to_string())));
  assert_eq!(option.type_fields["context_window"].as_u64(), Some(ANTHROPIC_CONTEXT_WINDOW));

  fs::write(home.join("config.yaml"), config(ANTHROPIC_API_BASE, 0)).unwrap();
  assert!(registry.load_config(&home).unwrap_err().to_string().contains("context_window"));
  fs::write(home.join("config.yaml"), config(&format!("{ANTHROPIC_API_BASE}/v1///"), 409_600)).unwrap();
  assert!(registry.load_config(&home).unwrap_err().to_string().contains("endpoint root without /v1"));
  fs::write(home.join("config.yaml"), config("''", 409_600)).unwrap();
  assert!(registry.load_config(&home).unwrap_err().to_string().contains("api_base"));
}

// Python: tests/test_anthropic_compatible_backend.py:90.
#[test]
fn anthropic_compatible_registration_matches_python_traits_and_usage_attribution() {
  use charliebot_registry::backend::{BackendTraits, ResumeStyle};
  use charliebot_store::run_records::RunKind;

  let mut registry = Registry::new();
  registry.register_package("test.claude_usage_source", register_claude_usage_source).unwrap();
  registry.register_package("src.backends.anthropic_compatible", register_anthropic_compatible).unwrap();

  let registration = registry.backend_type_of("cc-anthropic-compatible").unwrap();
  assert_eq!(
    registration.traits,
    BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    }
  );
  assert!(registration.lifecycle.is_some());
  let option = option_of(&argv_case(
    "anthropic",
    "cc-anthropic-compatible",
    json!({"api_base": ANTHROPIC_API_BASE, "context_window": ANTHROPIC_CONTEXT_WINDOW}),
    &[],
    None,
    None,
    None,
  ));
  let limits = registry.reading_limits_for("claude", RunKind::ManagerTurn, Some(&option)).unwrap();
  assert_eq!(limits.declared_window, Some(ANTHROPIC_CONTEXT_WINDOW));
  assert_eq!(limits.compact_reserve, Some(33_000));
  assert_eq!(registry.usage_source_for("cc-anthropic-compatible").unwrap().unwrap().name, "Claude Code");
}

// Python: tests/test_backend_hooks.py:129.
#[test]
fn anthropic_compatible_round_reports_a_reply_from_outside_the_pinned_family() {
  let mut registry = Registry::new();
  registry.register_package("src.backends.anthropic_compatible", register_anthropic_compatible).unwrap();
  let lifecycle = registry.backend_type_of("cc-anthropic-compatible").unwrap().lifecycle.clone().unwrap();
  let option = option_of(&argv_case(
    "variant",
    "cc-anthropic-compatible",
    json!({"api_base": ANTHROPIC_API_BASE, "context_window": ANTHROPIC_CONTEXT_WINDOW}),
    &[],
    None,
    None,
    None,
  ));
  let reply = |model: &str| json!({"type": "assistant", "message": {"model": model, "content": [{"type": "text", "text": "hi"}]}});

  let notices = lifecycle.round_notices(&option, &[reply("claude-haiku-4-5")]);

  assert_eq!(notices.len(), 1);
  assert_eq!(notices[0]["type"], "model_fallback_notice");
  assert_eq!(notices[0]["backend"], option.id);
  assert_eq!(notices[0]["served_models"], json!(["claude-haiku-4-5"]));
  assert!(lifecycle.round_notices(&option, &[reply(&option.model)]).is_empty());
}

fn launch_of(case: &Value) -> LaunchArgs {
  LaunchArgs {
    extra_flags: strings(&case["extra_flags"]),
    resume_session_id: opt_text(&case["resume_session_id"]),
    instructions_content: opt_text(&case["instructions_content"]),
    instructions_file: opt_text(&case["instructions_file"]).map(PathBuf::from),
    ..LaunchArgs::default()
  }
}

fn expect_eq(failures: &mut Vec<String>, label: &str, rust: &Value, python: &Value) {
  if rust != python {
    failures.push(format!("{label}\n  rust:   {rust}\n  python: {python}"));
  }
}

fn write_text(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

/// Links each role of `roles` to the fake-program helper in `dir`, and states their steps in `dir/probe.json`.
fn write_fakes(dir: &Path, roles: Value) {
  for role in roles.as_object().unwrap().keys() {
    symlink(env!("CARGO_BIN_EXE_fake_program"), dir.join(role)).unwrap();
  }
  write_text(&dir.join("probe.json"), &roles.to_string());
}

fn text(value: &Value) -> &str {
  value.as_str().expect("a string")
}

fn opt_text(value: &Value) -> Option<String> {
  value.as_str().map(str::to_string)
}

fn strings(value: &Value) -> Vec<String> {
  value.as_array().unwrap().iter().map(|item| text(item).to_string()).collect()
}

fn env_map(value: &Value) -> Env {
  value.as_object().unwrap().iter().map(|(name, val)| (name.clone(), text(val).to_string())).collect()
}

fn path_text(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}
