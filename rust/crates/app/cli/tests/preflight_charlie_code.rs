//! The `preflight-charlie-code` command (the Python inside `scripts/preflight-charlie-code.sh`), against a
//! scratch home: every failure line keeps its Python wording and exit 1, and a healthy option prints the
//! `OK:` line and exits 0.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use charliebot_shell_harness::{self as harness, Run, Sandbox};
use serde_json::json;

/// The flags a healthy `charlie-code --help` prints, in the order the contract check requires.
const HELP_TEXT: &str = "usage: charlie-code [--json] [--model MODEL] [--api-base URL] [--resume ID] [--context-window N] \
   [--task-file PATH] [--timeout-seconds S]";

const REINSTALL: &str = "reinstall charlie-code (uv tool install -e ~/workspace/charlie-code)";

/// A stub `charlie-code` whose `--help` prints the full contract, in the sandbox stub directory.
fn stub_charlie_code(sandbox: &Sandbox, body: &str) -> PathBuf {
  sandbox.stub("charlie-code", body);
  sandbox.bin().join("charlie-code")
}

fn healthy_help_stub() -> String {
  format!("if [ \"${{1:-}}\" = \"--help\" ]; then\n  printf '%s\\n' '{HELP_TEXT}'\n  exit 0\nfi\nexit 3\n")
}

/// A scratch home whose config names one `charlie-code` option pointing at `api_base`.
fn config_home(sandbox: &Sandbox, api_base: &str) -> PathBuf {
  let home = sandbox.dir("home");
  std::fs::write(
    home.join("config.yaml"),
    format!(
      "backends:\n  options:\n    - id: synthetic-cc\n      label: Synthetic Charlie Code\n      \
       type: charlie-code\n      model: synthetic-gateway/synthetic-model\n      api_base: {api_base}\n"
    ),
  )
  .expect("write the scratch config");
  home
}

/// Runs the command against the sandbox: the stub directory leads a system-only PATH and
/// `HOME` is the sandbox root, so neither PATH nor `~/.local/bin` holds a stray charlie-code.
fn run_preflight(sandbox: &Sandbox, home: &Path, backend_id: &str) -> Run {
  let mut env = sandbox.env();
  env.insert(charliebot_core::home::CHARLIEBOT_HOME_ENV.to_string(), home.display().to_string());
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert("PATH".to_string(), format!("{}:/usr/local/bin:/usr/bin:/bin", sandbox.bin().display()));
  let mut command = harness::command(env!("CARGO_BIN_EXE_charliebot"), &env);
  command.args(["preflight-charlie-code", backend_id]);
  harness::run(&mut command)
}

/// Serves `body` as one JSON `GET` answer on a loopback port, on a background thread.
fn serve_models(body: &serde_json::Value) -> String {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind the loopback endpoint");
  let port = listener.local_addr().expect("the bound port").port();
  let body = body.to_string();
  thread::spawn(move || {
    let Ok((mut stream, _)) = listener.accept() else { return };
    let mut request = [0u8; 1024];
    let _ = stream.read(&mut request);
    let response =
      format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let _ = stream.write_all(response.as_bytes());
  });
  format!("http://127.0.0.1:{port}/v1")
}

#[test]
fn an_unknown_option_id_names_the_config_file() {
  let sandbox = Sandbox::new();
  let home = config_home(&sandbox, "http://127.0.0.1:9/v1");
  let run = run_preflight(&sandbox, &home, "no-such-id");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(
    run.stderr,
    format!(
      "option-missing: add a backends.options entry with id no-such-id and type charlie-code to \
       ${}/config.yaml\n",
      charliebot_core::home::CHARLIEBOT_HOME_ENV
    )
  );
  assert!(run.stdout.is_empty(), "{}", run.stdout);
}

#[test]
fn a_non_charlie_code_option_is_refused_with_its_type() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  std::fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: synthetic-cc\n      label: Synthetic\n      type: codex\n      \
     model: synthetic-codex-1\n",
  )
  .expect("write the scratch config");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(
    run.stderr,
    format!(
      "option-missing: backend option synthetic-cc has type codex, not charlie-code; add a \
       backends.options entry with id synthetic-cc and type charlie-code to ${}/config.yaml\n",
      charliebot_core::home::CHARLIEBOT_HOME_ENV
    )
  );
}

#[test]
fn an_option_without_an_endpoint_is_refused_before_the_binary_resolves() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  std::fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: synthetic-cc\n      label: Synthetic\n      type: charlie-code\n      \
     model: synthetic-gateway/synthetic-model\n",
  )
  .expect("write the scratch config");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(
    run.stderr,
    "option-missing: charlie-code backend requires api_base (set api_base on its backends.options entry \
     in config.yaml)\n"
  );
}

#[test]
fn a_missing_binary_names_the_install_guidance() {
  let sandbox = Sandbox::new();
  let home = config_home(&sandbox, "http://127.0.0.1:9/v1");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  // Python's line carries the bare install command, without the `reinstall charlie-code` prefix its
  // `binary-broken` line has.
  assert_eq!(
    run.stderr,
    "binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin \
     (uv tool install -e ~/workspace/charlie-code)\n"
  );
}

#[test]
fn a_failing_help_run_reports_its_first_stderr_line() {
  let sandbox = Sandbox::new();
  stub_charlie_code(&sandbox, "echo 'boom' >&2\nexit 3\n");
  let home = config_home(&sandbox, "http://127.0.0.1:9/v1");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(run.stderr, format!("binary-broken: boom; {REINSTALL}\n"));
}

#[test]
fn a_help_output_missing_a_contract_flag_is_refused() {
  let sandbox = Sandbox::new();
  stub_charlie_code(
    &sandbox,
    "if [ \"${1:-}\" = \"--help\" ]; then\n  printf '%s\\n' 'usage: charlie-code [--json]'\n  exit 0\nfi\nexit 3\n",
  );
  let home = config_home(&sandbox, "http://127.0.0.1:9/v1");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(
    run.stderr,
    "contract-missing: charlie-code --help lacks --task-file; update the charlie-code checkout and reinstall\n"
  );
}

#[test]
fn an_unreachable_endpoint_is_reported() {
  let sandbox = Sandbox::new();
  stub_charlie_code(&sandbox, &healthy_help_stub());
  // Port 9 on loopback answers nothing: the connect fails inside the 15 s budget.
  let home = config_home(&sandbox, "http://127.0.0.1:9/v1");
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.starts_with("model-not-served: http://127.0.0.1:9/v1/models unreachable ("), "{}", run.stderr);
}

#[test]
fn an_endpoint_without_the_model_is_reported() {
  let sandbox = Sandbox::new();
  stub_charlie_code(&sandbox, &healthy_help_stub());
  let api_base = serve_models(&json!({"data": [{"id": "synthetic-other"}, {"id": "synthetic-second"}]}));
  let home = config_home(&sandbox, &api_base);
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 1, "{}", run.describe());
  let url = format!("{api_base}/models");
  assert_eq!(
    run.stderr,
    format!(
      "model-not-served: {url} lists ['synthetic-other', 'synthetic-second']; fix the model field or the endpoint\n"
    )
  );
}

#[test]
fn a_healthy_option_prints_the_ok_line() {
  let sandbox = Sandbox::new();
  let stub = stub_charlie_code(&sandbox, &healthy_help_stub());
  let api_base = serve_models(&json!({"data": [{"id": "synthetic-model"}]}));
  let home = config_home(&sandbox, &api_base);
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.is_empty(), "{}", run.stderr);
  // The endpoint serves the bare id: the litellm-style provider prefix is stripped at request time.
  assert_eq!(run.stdout, format!("OK: synthetic-cc -> {} serves synthetic-model at {api_base}\n", stub.display()));
}

#[test]
fn a_trailing_slash_on_the_endpoint_prints_the_rstripped_base() {
  let sandbox = Sandbox::new();
  stub_charlie_code(&sandbox, &healthy_help_stub());
  let api_base = format!("{}/", serve_models(&json!({"data": [{"id": "synthetic-model"}]})));
  let home = config_home(&sandbox, &api_base);
  let run = run_preflight(&sandbox, &home, "synthetic-cc");

  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.trim_end().ends_with(&format!("at {}", api_base.trim_end_matches('/'))), "{}", run.stdout);
}
