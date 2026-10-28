//! The `preflight-charlie-code` command (the Python inside `scripts/preflight-charlie-code.sh`): assert every
//! mechanism a `type: charlie-code` backend option's session depends on — the config entry exists, the
//! charlie-code binary resolves and runs, the CLI contract matches what the backend emits, and the endpoint
//! serves the configured model — so one run covers the whole fault class before a session or delegation is
//! started on it.
//!
//! Each failure prints its line to stderr and exits 1; the `OK:` line goes to stdout and exits 0. The failure
//! lines are the script's stderr contract, so they stay plain text, not the CLI's shared JSON failure object.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use charliebot_agents::cli_base::CliHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::BackendOption;
use charliebot_core::home;
use charliebot_registry::backend::{BackendFactory, Env, LaunchArgs};
use clap::{CommandFactory, FromArgMatches, Parser};
use serde_json::Value;
use wait_timeout::ChildExt;

/// The `charlie-code --help` flags the backend's command carries, so the installed CLI answers to it.
const CONTRACT_FLAGS: [&str; 6] = ["--task-file", "--json", "--model", "--api-base", "--resume", "--context-window"];

/// The reinstall guidance the `binary-missing` and `binary-broken` lines carry.
const REINSTALL: &str = "reinstall charlie-code (uv tool install -e ~/workspace/charlie-code)";

#[derive(Parser)]
#[command(name = "preflight-charlie-code", about = "Preflight a type: charlie-code backend option")]
struct PreflightCli {
  /// The backends.options id to preflight
  backend_id: String,
}

/// Parse the arguments and run the checks. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2); the launcher script gates its own usage exit 2.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(PreflightCli::command());
  let argv = std::iter::once(OsString::from("charliebot preflight-charlie-code")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = PreflightCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  if let Err(line) = preflight(&cli.backend_id) {
    eprintln!("{line}");
    std::process::exit(1);
  }
  Ok(())
}

/// Run the five mechanism checks in order; the error is the failure line.
fn preflight(backend_id: &str) -> Result<(), String> {
  let registry = charliebot_packages::registry().map_err(|error| error.to_string())?;
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let cfg = registry.load_config(&home_dir).map_err(|error| error.to_string())?;
  let home_env = home::CHARLIEBOT_HOME_ENV;
  let config_file = charliebot_core::config::CONFIG_FILENAME;

  // Mechanism 1: the config entry exists and is a charlie-code backend.
  let Some(option) = cfg.get_backend_option(backend_id) else {
    return Err(format!(
      "option-missing: add a backends.options entry with id {backend_id} and type charlie-code to ${home_env}/{config_file}"
    ));
  };
  if option.backend_type != "charlie-code" {
    return Err(format!(
      "option-missing: backend option {backend_id} has type {}, not charlie-code; \
       add a backends.options entry with id {backend_id} and type charlie-code to ${home_env}/{config_file}",
      option.backend_type
    ));
  }

  // Mechanism 2: the backend builds, so its binary resolves.
  let host = CliHost::from_process().map_err(|error| error.to_string())?;
  let backend = match charliebot_charlie_code::factory::CharlieCodeFactory::new(host.clone()).build(
    Some(option),
    &cfg,
    LaunchArgs::default(),
  ) {
    Ok(backend) => backend,
    Err(error) => {
      let text = format!("{error:#}");
      if text.starts_with("charlie-code binary not found on PATH") {
        return Err(format!(
          "binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin ({REINSTALL})"
        ));
      }
      return Err(format!("option-missing: {text}"));
    }
  };
  let bin = host.resolve_binary("charlie-code").map_err(|_| {
    format!("binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin ({REINSTALL})")
  })?;

  // Mechanism 3: the binary actually runs.
  let child_env = backend
    .prepare_env(std::env::vars().collect::<Env>())
    .map_err(|error| format!("binary-broken: {error:#}; {REINSTALL}"))?;
  let help_stdout = run_help(&bin, &child_env)?;

  // Mechanism 4: the CLI contract matches what CharlieCodeBackend emits.
  for flag in CONTRACT_FLAGS {
    if !help_stdout.contains(flag) {
      return Err(format!(
        "contract-missing: charlie-code --help lacks {flag}; update the charlie-code checkout and reinstall"
      ));
    }
  }

  // Mechanism 5: the endpoint serves the configured model.
  let api_base = parsed_api_base(option).trim_end_matches('/').to_string();
  let url = format!("{api_base}/models");
  let served_ids = match fetch_served_models(&url) {
    Ok(served_ids) => served_ids,
    Err(error) => return Err(format!("model-not-served: {url} unreachable ({error})")),
  };
  // The litellm provider prefix is stripped at request time; the endpoint serves the bare id.
  let served_model = option.model.split_once('/').map_or(option.model.as_str(), |(_, bare)| bare);
  if !served_ids.iter().any(|id| id == served_model) {
    return Err(format!(
      "model-not-served: {url} lists [{}]; fix the model field or the endpoint",
      served_ids.join(", ")
    ));
  }

  println!("OK: {backend_id} -> {} serves {served_model} at {api_base}", bin.display());
  Ok(())
}

/// The option's endpoint, which mechanism 2's build has already required.
fn parsed_api_base(option: &BackendOption) -> String {
  let fields = charliebot_charlie_code::options::CharlieCodeOption::from_fields(&option.type_fields)
    .expect("mechanism 2 built this option, so its fields parse");
  fields.api_base.unwrap_or_default()
}

/// Run `<bin> --help` under the backend's child env with Python's 60 s budget; the error is a
/// `binary-broken` line naming the first stderr line on a nonzero exit.
fn run_help(bin: &std::path::Path, child_env: &Env) -> Result<String, String> {
  let mut child = Command::new(bin)
    .arg("--help")
    .env_clear()
    .envs(child_env)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|error| format!("binary-broken: {error}; {REINSTALL}"))?;
  let mut stdout = child.stdout.take().expect("piped stdout");
  let mut stderr = child.stderr.take().expect("piped stderr");
  let status = match child.wait_timeout(Duration::from_secs(60)) {
    Err(error) => return Err(format!("binary-broken: {error}; {REINSTALL}")),
    Ok(None) => {
      let _ = child.kill();
      let _ = child.wait();
      return Err(format!("binary-broken: timed out after 60s; {REINSTALL}"));
    }
    Ok(Some(status)) => status,
  };
  let mut help_text = String::new();
  stdout.read_to_string(&mut help_text).map_err(|error| format!("binary-broken: {error}; {REINSTALL}"))?;
  let mut stderr_text = String::new();
  stderr.read_to_string(&mut stderr_text).map_err(|error| format!("binary-broken: {error}; {REINSTALL}"))?;
  if !status.success() {
    let first_stderr = stderr_text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    return Err(format!("binary-broken: {first_stderr}; {REINSTALL}"));
  }
  Ok(help_text)
}

/// The model ids the endpoint's `/models` lists, sorted, under Python's 15 s budget; any failure of the
/// request, the body read or the payload shape is an unreachable endpoint.
fn fetch_served_models(url: &str) -> Result<Vec<String>, String> {
  charliebot_cli_common::http::ensure_crypto_provider();
  let runtime =
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())?;
  runtime.block_on(async {
    let client =
      reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|error| error.to_string())?;
    let response = client.get(url).send().await.map_err(|error| error.to_string())?;
    let response = response.error_for_status().map_err(|error| error.to_string())?;
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    let payload: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    let entries =
      payload.get("data").and_then(Value::as_array).ok_or_else(|| "the payload has no data list".to_string())?;
    let mut ids: Vec<String> = entries
      .iter()
      .map(|entry| {
        entry
          .get("id")
          .map(|id| match id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
          })
          .ok_or_else(|| "a data entry has no id".to_string())
      })
      .collect::<Result<Vec<_>, _>>()?;
    ids.sort();
    Ok(ids)
  })
}
