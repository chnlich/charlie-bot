//! The `backend` command: `backend resolve` prints the option a call runs on, and `backend run` runs that option once.
//! `backend run --resume <option>:<session>` continues the session a failed run named, on that option.
//!
//! The server picks the option through `POST /api/backends/resolve`. `backend run --launch <dir>` takes an option a
//! launcher resolved earlier, from `<dir>/option.json`, for a host that runs no server; its `<dir>/login` holds the
//! `NAME=VALUE` lines the run's process environment gets.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_agents::backend_run::{self, RunIdentity, RunStart};
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
  /// Continue the session that a failed run named, `<option>:<session>`, as its `resume handle` line prints it. The
  /// run continues that session on that option. A --call, --backend or --launch given with it must resolve to that
  /// option; without one, the handle's option is resolved by its id.
  #[arg(long, value_name = "HANDLE")]
  resume: Option<String>,
}

#[derive(Args)]
#[group(multiple = false)]
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
      let resolved = resolve_response(&args.call, &args.backend)?;
      json_out::print_result(&resolved);
      Ok(())
    }
    BackendCommand::Run(args) => run_command(args, lazy_host),
  }
}

/// The resolve answer the server returns: `{"option": <record>, "claude_config_dir"?: <dir>}`.
fn resolve_response(call: &Option<String>, backend: &Option<String>) -> Result<Value, CliExit> {
  let target = match (call, backend) {
    (Some(class), None) => json!({"call": class}),
    (None, Some(id)) => json!({"backend": id}),
    _ => return Err(CliExit::usage("name exactly one of --call or --backend")),
  };
  client::post(RESOLVE_PATH, target, None, &[])
}

fn response_field(response: &Value, key: &str) -> Result<Value, CliExit> {
  response.get(key).cloned().ok_or_else(|| CliExit::error(format!("the resolve answer has no {key} field: {response}")))
}

fn optional_claude_config_dir(response: &Value) -> Result<Option<String>, CliExit> {
  match response.get("claude_config_dir") {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(dir)) => Ok(Some(dir.clone())),
    Some(other) => Err(CliExit::error(format!("claude_config_dir must be a string, got {other}"))),
  }
}

/// A resume handle as a failed run prints it, `<option>:<session>`, split at the first colon.
struct ResumeHandle<'a> {
  text: &'a str,
  option: &'a str,
  session: &'a str,
}

fn parse_resume_handle(text: &str) -> Result<ResumeHandle<'_>, CliExit> {
  match text.split_once(':') {
    Some((option, session)) if !option.is_empty() && !session.is_empty() => Ok(ResumeHandle { text, option, session }),
    _ => Err(CliExit::usage(format!(
      "resume handle {text:?} is malformed: expected <option>:<session>, as a failed backend run prints it"
    ))),
  }
}

/// The option a resume handle names, resolved by its id the way `--backend` resolves one. A failure names the handle.
fn resolve_handle_option(handle: &ResumeHandle) -> Result<Value, CliExit> {
  name_handle_in_failure(handle, resolve_response(&None, &Some(handle.option.to_string())))
}

/// A failure of resolving a handle's option, with the handle named in its message and the server's exit code kept.
fn name_handle_in_failure(handle: &ResumeHandle, resolved: Result<Value, CliExit>) -> Result<Value, CliExit> {
  resolved.map_err(|error| {
    let reason = reason_of(&error);
    CliExit {
      code: error.code,
      body: json!({"error": format!("resume handle {} does not resolve: {reason}", handle.text)}),
    }
  })
}

/// Refuses a run whose option is not the one its resume handle names; the refusal names both ids.
fn check_resume_option(handle: &ResumeHandle, option_id: &str) -> Result<(), CliExit> {
  if handle.option == option_id {
    return Ok(());
  }
  Err(CliExit::usage(format!(
    "resume handle {} names option {}, but this run resolves to option {option_id}; \
     a resume runs on the handle's option",
    handle.text, handle.option
  )))
}

fn reason_of(error: &CliExit) -> String {
  error.body.get("error").and_then(Value::as_str).map_or_else(|| error.body.to_string(), str::to_string)
}

fn run_command(args: RunArgs, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let prompt = files::read_required_text_file("--task-file", &args.task_file)?;
  let handle = args.resume.as_deref().map(parse_resume_handle).transpose()?;
  let (resolved, login) = match (args.pick, &handle) {
    (RunPick { launch: Some(dir), .. }, _) => launch_record_and_login(&dir)?,
    (RunPick { call: None, backend: None, .. }, Some(handle)) => (resolve_handle_option(handle)?, Env::new()),
    (RunPick { call: None, backend: None, .. }, None) => {
      return Err(CliExit::usage("name one of --call, --backend, --launch, or --resume"));
    }
    (RunPick { call, backend, .. }, _) => (resolve_response(&call, &backend)?, Env::new()),
  };
  let record = response_field(&resolved, "option")?;
  let claude_config_dir = optional_claude_config_dir(&resolved)?;
  let option = option_of(&record, host)?;
  if let Some(handle) = &handle {
    check_resume_option(handle, &option.id)?;
  }
  let cwd =
    std::env::current_dir().map_err(|error| CliExit::error(format!("cannot read the working directory: {error}")))?;
  let events_path = cwd.join("events.ndjson");
  let start = match handle {
    Some(handle) => {
      RunStart::Resume { session_id: handle.session.to_string(), run_id: uuid::Uuid::new_v4().to_string() }
    }
    None => RunStart::New(RunIdentity {
      session_id: uuid::Uuid::new_v4().to_string(),
      run_id: uuid::Uuid::new_v4().to_string(),
    }),
  };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("cannot start the runtime: {error}")))?;
  let report = runtime
    .block_on(backend_run::run_once(
      host,
      &option,
      prompt,
      cwd,
      login,
      start,
      claude_config_dir.as_deref(),
      &events_path,
    ))
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

  /// A launch directory holds the resolve output (`option.json`) and a mode-600 `login`.
  #[test]
  fn a_launch_directory_reads_the_resolve_output_and_login() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
      dir.path().join("option.json"),
      r#"{"option":{"id":"synthetic-a","label":"A","type":"cc-claude","model":"m"},"claude_config_dir":"/tmp/acct"}"#,
    )
    .unwrap();
    let login_path = dir.path().join("login");
    std::fs::write(&login_path, "CLAUDE_CODE_OAUTH_TOKEN=tok\n").unwrap();
    std::fs::set_permissions(&login_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (resolved, login) = launch_record_and_login(dir.path()).unwrap();
    assert_eq!(response_field(&resolved, "option").unwrap()["id"], "synthetic-a");
    assert_eq!(optional_claude_config_dir(&resolved).unwrap().as_deref(), Some("/tmp/acct"));
    assert_eq!(login.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str), Some("tok"));
  }

  /// A resume handle is `<option>:<session>`, split at its first colon; a handle missing either part is refused, and
  /// the refusal names the handle.
  #[test]
  fn a_resume_handle_is_option_colon_session_and_a_malformed_one_is_refused_by_name() {
    let handle = parse_resume_handle("synthetic-a:session-1").unwrap();
    assert_eq!((handle.option, handle.session), ("synthetic-a", "session-1"));
    for text in ["synthetic-a", ":session-1", "synthetic-a:", ""] {
      let message = reason_of(&parse_resume_handle(text).err().unwrap());
      assert!(message.contains(&format!("{text:?}")), "{message}");
    }
  }

  /// A resume runs only on the option its handle names: any other option is refused, and the refusal names both ids.
  #[test]
  fn a_resume_refuses_an_option_other_than_the_handles() {
    let handle = parse_resume_handle("synthetic-a:session-1").unwrap();
    check_resume_option(&handle, "synthetic-a").unwrap();
    let message = reason_of(&check_resume_option(&handle, "synthetic-b").unwrap_err());
    assert!(message.contains("synthetic-a") && message.contains("synthetic-b"), "{message}");
  }

  /// The handle a failed run prints is the text `--resume` takes back: it parses to the same option and session.
  #[test]
  fn the_handle_a_failed_run_prints_parses_back_to_its_option_and_session() {
    let option = BackendOption {
      id: "synthetic-a".into(),
      label: "A".into(),
      model: "m".into(),
      prompt_overlay: None,
      billing: None,
      backend_type: "synthetic".into(),
      type_fields: Default::default(),
      visible: true,
    };
    let message = reason_of(&run_failure(&option, "session-1", "the backend exited with code 1"));
    let (_, printed) = message.rsplit_once("resume handle: ").expect("a failed run with a session prints its handle");
    let handle = parse_resume_handle(printed).unwrap();
    assert_eq!((handle.option, handle.session), ("synthetic-a", "session-1"));
  }

  /// A failure of resolving the handle's option names the handle and keeps the server's exit code.
  #[test]
  fn a_handle_whose_option_does_not_resolve_is_named_in_the_failure() {
    let handle = parse_resume_handle("synthetic-a:session-1").unwrap();
    let server = CliExit { code: 1, body: json!({"error": "backend 'synthetic-a' is not a configured id"}) };
    let failure = name_handle_in_failure(&handle, Err(server)).unwrap_err();
    assert_eq!(failure.code, 1);
    let message = reason_of(&failure);
    assert!(message.contains("synthetic-a:session-1"), "{message}");
    assert!(message.contains("is not a configured id"), "{message}");
  }
}
