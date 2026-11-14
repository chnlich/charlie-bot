//! The `preflight-charlie-code` command: assert every
//! mechanism a `type: charlie-code` backend option's session depends on — the config entry exists, the
//! charlie-code binary resolves and runs, the CLI contract matches what the backend emits, and the endpoint
//! serves the configured model — so one run covers the whole fault class before a session or delegation is
//! started on it.
//!
//! Each failure prints its line to stderr and exits 1; the `OK:` line goes to stdout and exits 0. The failure
//! lines are the script's stderr contract, so they stay plain text, not the CLI's shared JSON failure object.
//! The `/models` request authenticates with the key the backend's child env carries, so a gateway that requires one
//! passes the model check while a refusal by that credential gets its own `auth-refused` line.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use charliebot_agents::cli_base::CliHost;
use charliebot_charlie_code::flags;
use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::home;
use charliebot_registry::backend::{BackendFactory, Env, LaunchArgs};
use clap::Parser;
use serde_json::Value;
use wait_timeout::ChildExt;

/// The `charlie-code --help` flags the backend's command carries, so the installed CLI answers to it.
const CONTRACT_FLAGS: [&str; 7] = [
  flags::TASK_FILE,
  flags::JSON,
  flags::MODEL,
  flags::API_BASE,
  flags::RESUME,
  flags::CONTEXT_WINDOW,
  flags::RESET_NOTE,
];

/// The install command both failure lines carry; the `binary-missing` line is the bare command, and the
/// `binary-broken` line prefixes the reinstall instruction.
const INSTALL_HINT: &str = "uv tool install -e ~/workspace/charlie-code";

/// The reinstall guidance the `binary-broken` line carries.
const REINSTALL: &str = "reinstall charlie-code (uv tool install -e ~/workspace/charlie-code)";

#[derive(Parser)]
#[command(name = "preflight-charlie-code", about = "Preflight a type: charlie-code backend option")]
struct PreflightCli {
  /// The backends.options id to preflight
  backend_id: String,
}

/// Parse the arguments and run the checks. Parser errors print and exit on their own
/// (help exits 0, a usage error exits 2); the launcher script gates its own usage exit 2.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: PreflightCli = cli_args::parse("charliebot preflight-charlie-code", args);
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
  let config_file = home::display_path(charliebot_core::config::CONFIG_FILENAME);

  // Mechanism 1: the config entry exists and is a charlie-code backend.
  let Some(option) = cfg.get_backend_option(backend_id) else {
    return Err(format!(
      "option-missing: add a backends.options entry with id {backend_id} and type charlie-code to {config_file}"
    ));
  };
  if option.backend_type != charliebot_charlie_code::TYPE_NAME {
    return Err(format!(
      "option-missing: backend option {backend_id} has type {}, not charlie-code; \
       add a backends.options entry with id {backend_id} and type charlie-code to {config_file}",
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
          "binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin ({INSTALL_HINT})"
        ));
      }
      return Err(format!("option-missing: {text}"));
    }
  };
  let bin = host.resolve_binary(charliebot_charlie_code::BINARY_NAME).map_err(|_| {
    format!("binary-missing: install charlie-code so it resolves on PATH or in ~/.local/bin ({INSTALL_HINT})")
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
  let fields = charliebot_charlie_code::options::CharlieCodeOption::from_fields(&option.type_fields)
    .expect("mechanism 2 built this option, so its fields parse");
  let api_base = fields.api_base.as_deref().unwrap_or_default().trim_end_matches('/').to_string();
  let url = format!("{api_base}/models");
  // The child env carries the key and the proxy the backend's runs use, so the request reaches the endpoint the
  // same way they do.
  let credential = fields.credential.as_deref().filter(|section| !section.is_empty());
  let api_key = child_env.get(charliebot_charlie_code::API_KEY_ENV).map(String::as_str);
  let proxy_url = fields.proxy_url.as_deref().filter(|proxy| !proxy.is_empty());
  let no_proxy = child_env.get(charliebot_core::proxy_env::NO_PROXY).map(String::as_str);
  let served_ids = match fetch_served_models(&url, api_key, proxy_url, no_proxy) {
    Ok(served_ids) => served_ids,
    Err(ServedModelsError::Unreachable(error)) => {
      return Err(format!("model-not-served: {url} unreachable ({error})"));
    }
    Err(ServedModelsError::AuthRefused(status)) => {
      let checked = match credential {
        Some(section) => format!("check the api_key in credentials.yaml section '{section}'"),
        None => "the option names no credentials.yaml section, so the request carried no key".to_string(),
      };
      return Err(format!("auth-refused: {url} refused the credential (HTTP {status}); {checked}"));
    }
  };
  // The litellm provider prefix is stripped at request time; the endpoint serves the bare id.
  let served_model = option.model.split_once('/').map_or(option.model.as_str(), |(_, bare)| bare);
  if !served_ids.iter().any(|id| id == served_model) {
    // The list prints with quoted ids inside brackets.
    let listed = served_ids.iter().map(|id| format!("'{id}'")).collect::<Vec<_>>().join(", ");
    return Err(format!("model-not-served: {url} lists [{listed}]; fix the model field or the endpoint"));
  }

  println!("OK: {backend_id} -> {} serves {served_model} at {api_base}", bin.display());
  Ok(())
}

/// Run `<bin> --help` under the backend's child env with a 60 s budget; the error is a
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

/// Why the `/models` request failed: the request, the body read or the payload shape was unusable, or the endpoint
/// refused the credential the option's runs carry.
enum ServedModelsError {
  Unreachable(String),
  AuthRefused(reqwest::StatusCode),
}

/// The model ids the endpoint's `/models` lists, sorted, under a 15 s budget. `api_key` travels as the bearer
/// credential when set, and a `proxy_url` routes the request through the backend's proxy with its no-proxy list;
/// without one, reqwest's default proxy behavior stands.
fn fetch_served_models(
  url: &str,
  api_key: Option<&str>,
  proxy_url: Option<&str>,
  no_proxy: Option<&str>,
) -> Result<Vec<String>, ServedModelsError> {
  charliebot_cli_common::http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
  runtime.block_on(async {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(15));
    if let Some(proxy_url) = proxy_url {
      let proxy = reqwest::Proxy::all(proxy_url)
        .map_err(|error| ServedModelsError::Unreachable(error.to_string()))?
        .no_proxy(no_proxy.and_then(reqwest::NoProxy::from_string));
      builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
    let mut request = client.get(url);
    if let Some(api_key) = api_key {
      request = request.bearer_auth(api_key);
    }
    let response = request.send().await.map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
      return Err(ServedModelsError::AuthRefused(status));
    }
    let response = response.error_for_status().map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
    let body = response.bytes().await.map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
    let payload: Value =
      serde_json::from_slice(&body).map_err(|error| ServedModelsError::Unreachable(error.to_string()))?;
    let entries = payload
      .get("data")
      .and_then(Value::as_array)
      .ok_or_else(|| ServedModelsError::Unreachable("the payload has no data list".to_string()))?;
    let mut ids: Vec<String> = entries
      .iter()
      .map(|entry| {
        entry
          .get("id")
          .map(|id| match id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
          })
          .ok_or_else(|| ServedModelsError::Unreachable("a data entry has no id".to_string()))
      })
      .collect::<Result<Vec<_>, _>>()?;
    ids.sort();
    Ok(ids)
  })
}
