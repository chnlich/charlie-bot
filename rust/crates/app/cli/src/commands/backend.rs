//! The `backend` command: `backend resolve` prints the option a call runs on, and `backend run` runs that option once.
//!
//! The server picks the option through `POST /api/backends/resolve`. `backend run --launch <dir>` takes an option a
//! launcher resolved earlier, from `<dir>/option.json`, for a host that runs no server; its `<dir>/login` holds the
//! `NAME=VALUE` lines the run's process environment gets.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_agents::backend_run::{self, RunIdentity};
use charliebot_agents::call_resolve::RESOLVE_PATH;
use charliebot_agents::cli_host::{CliHost, LazyHost};
use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_core::config::{self, BackendOption};
use charliebot_registry::backend::Env;
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "backend", subcommand_required = true, arg_required_else_help = true)]
struct BackendCli {
  #[command(subcommand)]
  command: BackendCommand,
}

#[derive(Subcommand)]
enum BackendCommand {
  /// Print the option a call class or a backend id runs on: its backends.options entry, which holds no key.
  Resolve(ResolveArgs),
  /// Run the option once in the current directory with the task file's text as the prompt.
  Run(RunArgs),
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct ResolveArgs {
  /// The backends.calls class to resolve.
  #[arg(long, value_name = "CLASS")]
  call: Option<String>,
  /// A backends.options or backends.pools id to resolve.
  #[arg(long, value_name = "ID")]
  backend: Option<String>,
}

#[derive(Args)]
struct RunArgs {
  #[command(flatten)]
  pick: RunPick,
  /// The file whose text is the prompt.
  #[arg(long, value_name = "FILE")]
  task_file: String,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct RunPick {
  /// The backends.calls class to run on.
  #[arg(long, value_name = "CLASS")]
  call: Option<String>,
  /// A backends.options or backends.pools id to run on.
  #[arg(long, value_name = "ID")]
  backend: Option<String>,
  /// A launch directory holding the resolved `option.json` and the `login` variables.
  #[arg(long, value_name = "DIR")]
  launch: Option<PathBuf>,
}

/// Run the `backend` subcommand.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let cli: BackendCli = cli_args::parse("charliebot backend", args);
  match cli.command {
    BackendCommand::Resolve(args) => {
      let option = resolve_record(&args.call, &args.backend)?;
      json_out::print_result(&option);
      Ok(())
    }
    BackendCommand::Run(args) => run_command(args, lazy_host),
  }
}

/// The record the server picks for a call class or a backend id.
fn resolve_record(call: &Option<String>, backend: &Option<String>) -> Result<Value, CliExit> {
  let target = match (call, backend) {
    (Some(class), None) => json!({"call": class}),
    (None, Some(id)) => json!({"backend": id}),
    _ => return Err(CliExit::usage("name exactly one of --call or --backend")),
  };
  let response = client::post(RESOLVE_PATH, target, None, &[])?;
  response_field(&response, "option")
}

fn response_field(response: &Value, key: &str) -> Result<Value, CliExit> {
  response.get(key).cloned().ok_or_else(|| CliExit::error(format!("the resolve answer has no {key} field: {response}")))
}

fn run_command(args: RunArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let prompt = files::read_required_text_file("--task-file", &args.task_file)?;
  let (record, login) = match args.pick {
    RunPick { launch: Some(dir), .. } => launch_record_and_login(&dir)?,
    RunPick { call, backend, .. } => (resolve_record(&call, &backend)?, Env::new()),
  };
  let option = option_of(&record, host)?;
  let cwd =
    std::env::current_dir().map_err(|error| CliExit::error(format!("cannot read the working directory: {error}")))?;
  let events_path = cwd.join("events.ndjson");
  let identity = RunIdentity { session_id: uuid::Uuid::new_v4().to_string(), run_id: uuid::Uuid::new_v4().to_string() };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("cannot start the runtime: {error}")))?;
  let report = runtime
    .block_on(backend_run::run_once(host, &option, prompt, cwd, login, identity, &events_path))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  let session = report.session_id.as_deref().unwrap_or("none");
  eprintln!("backend run: {} ({}) session {session}", option.id, option.backend_type);
  match report.exit_code {
    Some(0) if !report.text.is_empty() => {
      println!("{}", report.text);
      Ok(())
    }
    Some(0) => Err(run_failure(&option, session, "the run ended with no final text")),
    Some(code) => {
      let stderr = report.stderr_text.trim();
      let reason = if stderr.is_empty() { format!("the backend exited with code {code}") } else { stderr.to_string() };
      Err(run_failure(&option, session, &reason))
    }
    None => Err(run_failure(&option, session, "no backend process started")),
  }
}

/// The failure of a run: its reason, and the resume handle `<backend>:<session>` when the run had a session.
fn run_failure(option: &BackendOption, session: &str, reason: &str) -> CliExit {
  if session == "none" {
    CliExit::error(reason.to_string())
  } else {
    CliExit::error(format!("{reason}\nresume handle: {}:{session}", option.id))
  }
}

/// The option a record names, read through the loader's own option parse against the registered backend types.
fn option_of(record: &Value, host: &CliHost) -> Result<BackendOption, CliExit> {
  let types = host.registry.config_schema().backend_types;
  let entry = serde_yaml::to_value(record).map_err(|error| CliExit::error(format!("{error}")))?;
  config::parse_backend_record(&entry, &types).map_err(|error| CliExit::error(error.to_string()))
}

/// The option and login a launch directory holds: `option.json` is the resolve answer's record, and `login` is a file
/// of `NAME=VALUE` lines readable only by its owner.
fn launch_record_and_login(dir: &Path) -> Result<(Value, Env), CliExit> {
  let option_path = dir.join("option.json");
  let option_text = files::read_text_mode(&option_path)?;
  let record: Value = serde_json::from_str(&option_text)
    .map_err(|error| CliExit::error(format!("{} is not a JSON option record: {error}", option_path.display())))?;
  let login_path = dir.join("login");
  let login = read_login(&login_path)?;
  Ok((record, login))
}

fn read_login(path: &Path) -> Result<Env, CliExit> {
  use std::os::unix::fs::PermissionsExt;
  let mode = std::fs::metadata(path)
    .map_err(|error| CliExit::error(format!("cannot read the login file {}: {error}", path.display())))?
    .permissions()
    .mode()
    & 0o777;
  if mode != 0o600 {
    return Err(CliExit::error(format!("the login file {} must have mode 600, not {mode:o}", path.display())));
  }
  let text = files::read_text_mode(path)?;
  let mut login = Env::new();
  for line in text.lines().filter(|line| !line.trim().is_empty()) {
    let Some((name, value)) = line.split_once('=') else {
      return Err(CliExit::error(format!("a login line of {} is not NAME=VALUE", path.display())));
    };
    login.insert(name.to_string(), value.to_string());
  }
  Ok(login)
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use super::*;

  /// The login file is read only when its owner alone can read it, and its lines are `NAME=VALUE`.
  #[test]
  fn a_login_file_is_read_only_at_mode_600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("login");
    std::fs::write(&path, "ANTHROPIC_AUTH_TOKEN=secret\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = read_login(&path).unwrap_err();
    assert!(format!("{error:?}").contains("must have mode 600"), "{error:?}");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let login = read_login(&path).unwrap();
    assert_eq!(login.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("secret"));
  }
}
