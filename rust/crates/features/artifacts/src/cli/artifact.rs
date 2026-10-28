//! The `charliebot artifact` verbs (`src/features/artifacts/cli.py`).
//!
//! `check` runs a genre's mechanical assertions, prints one line per outcome, and — once every assertion passed and
//! `--assertions-only` was not given — runs the cold read. The cold read follows the session's profile: a manager
//! session delegates a cold-read child over `/api/internal/delegate` and prints the started line; a worker session or
//! one with no session identity runs the probe in the foreground and prints the report body. A `waiting` page has no
//! cold read here: `web status set` runs it with the item, so `check` refuses that genre without `--assertions-only`.
//! `wrap` assembles a genre page and can pre-render its math.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_agents::cli_host::{CliHost, LazyHost};
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths;
use charliebot_store::run_identity::SESSION_METADATA_NAME;
use charliebot_tasks::cold_read;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::macros::format_description;

use crate::artifact_check;

/// The cold-read delegation's task type.
const COLD_READ_TASK_TYPE: &str = "cold-read";

#[derive(Parser)]
#[command(name = "artifact", about = "Artifact checks")]
struct ArtifactCli {
  #[command(subcommand)]
  command: ArtifactCommand,
}

#[derive(Subcommand)]
enum ArtifactCommand {
  /// Run a genre's assertions (and cold read) on a local file
  Check(CheckArgs),
  /// Assemble a genre page from a content fragment
  Wrap(super::wrap::WrapArgs),
}

#[derive(Args)]
struct CheckArgs {
  /// Artifact path as an ordinary filesystem path (absolute or cwd-relative)
  file: String,
  /// Genre the page claims to follow
  #[arg(long, value_parser = artifact_check::ARTIFACT_GENRES)]
  genre: String,
  /// Chat message that triggered the page (question 6 verbatim); required for every genre unless --assertions-only
  #[arg(long)]
  trigger: Option<String>,
  /// Run the assertions alone, skipping the cold read
  #[arg(long)]
  assertions_only: bool,
}

/// The process inputs a check reads besides its arguments: the profile home, the cwd that may name a session, the
/// server-written session variable, and the checkout whose `prompts/` the assertions and the probe read.
struct CheckInputs {
  pub home: PathBuf,
  pub cwd: PathBuf,
  pub session_env: Option<String>,
  pub repo_root: PathBuf,
}

/// One check's printed bytes and exit code.
struct CheckOutput {
  pub stdout: String,
  pub exit: i32,
}

/// A check that finished, or one whose cold read failed after the assertion lines were printed.
enum CheckOutcome {
  Done(CheckOutput),
  Failed { stdout: String, exit: CliExit },
}

/// Parse the artifact arguments and run one verb. Parser errors print and exit on their own (argparse-compatible:
/// help 0, usage error 2).
pub fn main(args: &[OsString], host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(ArtifactCli::command());
  let argv = std::iter::once(OsString::from("charliebot artifact")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ArtifactCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    ArtifactCommand::Check(args) => match check(&args, host)? {
      // The assertion lines print before a later failure, as Python's per-line prints do.
      CheckOutcome::Failed { stdout, exit } => {
        print!("{stdout}");
        flush_stdout()?;
        Err(exit)
      }
      CheckOutcome::Done(output) => {
        print!("{}", output.stdout);
        flush_stdout()?;
        std::process::exit(output.exit);
      }
    },
    ArtifactCommand::Wrap(wrap_args) => {
      print!("{}", super::wrap::run(&wrap_args, args)?);
      flush_stdout()
    }
  }
}

fn flush_stdout() -> Result<(), CliExit> {
  use std::io::Write as _;
  std::io::stdout().flush().map_err(|error| CliExit::error(error.to_string()))
}

impl CheckInputs {
  /// The running process's inputs.
  fn from_process(host: &CliHost) -> Result<CheckInputs, CliExit> {
    let cwd = std::env::current_dir().map_err(|error| CliExit::error(error.to_string()))?;
    Ok(CheckInputs {
      home: host.cfg.charliebot_home.clone(),
      cwd,
      session_env: std::env::var(charliebot_cli_common::resolve::SESSION_ID_ENV_VAR)
        .ok()
        .filter(|value| !value.is_empty()),
      repo_root: host.repo_root.clone(),
    })
  }
}

/// `_run_check`: the assertion lines, then the cold read the session's shape selects.
fn check(args: &CheckArgs, lazy_host: &LazyHost) -> Result<CheckOutcome, CliExit> {
  if args.genre == "waiting" && !args.assertions_only {
    return Err(CliExit::usage(
      "--genre waiting has no cold read here: `charliebot web status set` runs the cold read of a waiting page, \
       reading the item with its page. Pass --assertions-only to run the assertions alone",
    ));
  }
  if args.trigger.is_none() && !args.assertions_only {
    return Err(CliExit::usage(format!("--genre {} requires --trigger unless --assertions-only is given", args.genre)));
  }
  let artifact = resolve_path(Path::new(&args.file))?;
  if !artifact.is_file() {
    return Err(CliExit::error(format!("artifact not found: {}", args.file)));
  }
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let inputs = CheckInputs::from_process(host)?;
  check_with_host(args, &artifact, host, &inputs)
}

fn check_with_host(
  args: &CheckArgs,
  artifact: &Path,
  host: &CliHost,
  inputs: &CheckInputs,
) -> Result<CheckOutcome, CliExit> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("could not start the assertion runtime: {error}")))?;
  let outcomes = runtime
    .block_on(artifact_check::run_assertions(&args.genre, artifact.to_path_buf(), &host.cfg, inputs.repo_root.clone()))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;

  let mut stdout = String::new();
  let mut failed = 0;
  for outcome in &outcomes {
    if outcome.passed {
      stdout.push_str(&format!("ok {}", outcome.name));
      if !outcome.detail.is_empty() {
        stdout.push(' ');
        stdout.push_str(&outcome.detail);
      }
    } else {
      failed += 1;
      stdout.push_str(&format!("FAIL {}: {}", outcome.name, outcome.detail));
    }
    stdout.push('\n');
  }
  if failed > 0 {
    return Ok(CheckOutcome::Done(CheckOutput { stdout, exit: 1 }));
  }
  if args.assertions_only {
    return Ok(CheckOutcome::Done(CheckOutput { stdout, exit: 0 }));
  }

  if let Some(session_id) = session_identity(&host.cfg, inputs) {
    let profile = match session_profile(&host.cfg, &session_id) {
      Ok(profile) => profile,
      Err(exit) => return Ok(CheckOutcome::Failed { stdout, exit }),
    };
    if profile == "manager" {
      return match delegate_cold_read(args, artifact, &session_id, inputs) {
        Ok(line) => {
          stdout.push_str(&line);
          Ok(CheckOutcome::Done(CheckOutput { stdout, exit: 0 }))
        }
        Err(exit) => Ok(CheckOutcome::Failed { stdout, exit }),
      };
    }
  }
  let trigger = args.trigger.as_deref().expect("a non-assertions check requires --trigger");
  let result = match run_probe(&runtime, host, artifact, trigger) {
    Ok(result) => result,
    Err(exit) => return Ok(CheckOutcome::Failed { stdout, exit }),
  };
  stdout.push_str(&cold_read::format_report(
    &paths::display(artifact),
    &result.page_sha256,
    &result.attempts,
    match &result.backend_id {
      Some(backend_id) => cold_read::ColdReadOutcome::Answered {
        backend_id,
        answer: result.answer.as_deref().expect("an answered probe carries its answer"),
      },
      None => cold_read::ColdReadOutcome::AllFailed,
    },
  ));
  stdout.push('\n');
  Ok(CheckOutcome::Done(CheckOutput { stdout, exit: if result.backend_id.is_some() { 0 } else { 1 } }))
}

/// `Path(...).resolve()`: an absolute path, with an existing path's symlinks resolved.
fn resolve_path(path: &Path) -> Result<PathBuf, CliExit> {
  match std::fs::canonicalize(path) {
    Ok(resolved) => Ok(resolved),
    Err(_) => std::path::absolute(path).map_err(|error| CliExit::error(error.to_string())),
  }
}

/// `_session_identity`: the server-written variable, else the cwd's name when the cwd is a session directory.
fn session_identity(config: &CharlieBotConfig, inputs: &CheckInputs) -> Option<String> {
  if let Some(session) = &inputs.session_env {
    return Some(session.clone());
  }
  let cwd = resolve_path(&inputs.cwd).ok()?;
  let sessions_dir = resolve_path(&config.sessions_dir()).ok()?;
  if cwd.parent() != Some(sessions_dir.as_path()) {
    return None;
  }
  cwd.file_name().map(|name| name.to_string_lossy().into_owned())
}

/// `_session_profile`: the requesting session's `profile` from its metadata.
fn session_profile(config: &CharlieBotConfig, session_id: &str) -> Result<String, CliExit> {
  let path = config.sessions_dir().join(session_id).join(SESSION_METADATA_NAME);
  let unreadable = |detail: String| {
    CliExit::error(format!("cannot read the requesting session's profile from {}: {detail}", paths::display(&path)))
  };
  let text = std::fs::read_to_string(&path).map_err(|error| unreadable(error.to_string()))?;
  let metadata: Value = serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
  match metadata.get("profile") {
    Some(Value::String(profile)) => Ok(profile.clone()),
    _ => Err(unreadable("the metadata carries no profile".to_string())),
  }
}

/// `_delegate_cold_read`: POST the cold-read child and return the started line.
fn delegate_cold_read(
  args: &CheckArgs,
  artifact: &Path,
  session_id: &str,
  inputs: &CheckInputs,
) -> Result<String, CliExit> {
  let page_bytes = std::fs::read(artifact).map_err(|error| CliExit::error(error.to_string()))?;
  let digest = format!("{:x}", Sha256::digest(&page_bytes));
  let timestamp = time::OffsetDateTime::now_utc()
    .format(&format_description!("[year][month][day]T[hour][minute][second]Z"))
    .map_err(|error| CliExit::error(format!("could not format the request timestamp: {error}")))?;
  let request_id = format!("cold-read:{session_id}:{}:{timestamp}", &digest[..12]);
  let description = format!("cold read of {}", paths::display(artifact));
  let payload = json!({
    "session_id": session_id,
    "description": description,
    "task_type": COLD_READ_TASK_TYPE,
    "cold_read": {
      "page_path": paths::display(artifact),
      "genre": args.genre,
      "trigger": args.trigger,
    },
    "request_id": request_id,
  });
  let home = inputs.home.clone();
  let parent = session_id.to_string();
  let readback_description = description.clone();
  let readback_request_id = request_id.clone();
  let readback = move || {
    Ok(charliebot_store::task_child::find_local_task_child(
      &home,
      &parent,
      &readback_description,
      Some(COLD_READ_TASK_TYPE),
      Some(&readback_request_id),
    ))
  };
  let result = client::post("/api/internal/delegate", payload, Some(Box::new(readback)), &[])?;
  let child = result
    .get("session_id")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("the delegate response carries no session_id"))?;
  let run = result
    .get("run_id")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("the delegate response carries no run_id"))?;
  Ok(format!("cold read started: session {child} run {run}\n"))
}

/// The probe's outcome, the page bytes' sha256 included.
struct ProbeResult {
  attempts: Vec<(String, String)>,
  backend_id: Option<String>,
  answer: Option<String>,
  page_sha256: String,
}

/// `artifact_check.run_probe`: the page text and the eight questions to the light backends in rotation order.
fn run_probe(
  runtime: &tokio::runtime::Runtime,
  host: &CliHost,
  artifact: &Path,
  trigger: &str,
) -> Result<ProbeResult, CliExit> {
  let (system_prompt, questions) =
    cold_read::load_texts(&host.repo_root).map_err(|error| CliExit::error(format!("{error:#}")))?;
  let page_bytes = std::fs::read(artifact).map_err(|error| CliExit::error(error.to_string()))?;
  let page_sha256 = cold_read::page_sha256(&page_bytes);
  let page_text =
    String::from_utf8(page_bytes).map_err(|error| CliExit::error(format!("the artifact is not UTF-8: {error}")))?;
  let prompt = cold_read::probe_input(&page_text, &questions, trigger);
  let probe = runtime
    .block_on(cold_read::run_light_probe(host, &prompt, &system_prompt))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  Ok(ProbeResult { attempts: probe.attempts, backend_id: probe.backend_id, answer: probe.answer, page_sha256 })
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use super::*;
  use crate::test_support::{plan_page, repo_root};
  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_registry::Registry;

  fn test_host(home: &Path, repo_root: &Path) -> CliHost {
    let mut registry = Registry::new();
    registry.register_package("src.features.artifacts", crate::register).unwrap();
    let cfg = registry.load_config(home).unwrap();
    CliHost {
      cfg,
      registry: Arc::new(registry),
      repo_root: repo_root.to_path_buf(),
      base_env: AgentBaseEnvironment { values: std::env::vars().collect() },
      pool: BackendPool::default(),
    }
  }

  #[test]
  fn test_cli_assertions_only_passes_a_page_over_the_height_target() {
    // Python: tests/core/test_artifact_check.py. The fake Chrome is the chrome-stub helper; its measured height is
    // injected, as `test_support::plan_session` does, so no renderer runs.
    let home = tempfile::tempdir().unwrap();
    let chrome = crate::test_support::chrome_program(home.path(), json!([{"exit": 0}]));
    crate::headless_render::set_test_height(&chrome, 2600);
    std::fs::write(home.path().join("config.yaml"), format!("headless_chrome_bin: {}\n", chrome.display())).unwrap();
    let artifact = home.path().join("page.html");
    std::fs::write(&artifact, plan_page("Ship the fix.")).unwrap();
    let args = CheckArgs {
      file: artifact.display().to_string(),
      genre: "plan".to_string(),
      trigger: None,
      assertions_only: true,
    };
    let host = test_host(home.path(), &repo_root());
    let inputs = CheckInputs {
      home: home.path().to_path_buf(),
      cwd: home.path().to_path_buf(),
      session_env: None,
      repo_root: repo_root(),
    };

    let CheckOutcome::Done(output) = check_with_host(&args, &artifact, &host, &inputs).unwrap() else {
      panic!("the check finished");
    };

    assert_eq!(output.exit, 0, "{}", output.stdout);
    assert!(output.stdout.contains("ok page-height 2600 px: 600 px over the 2000 px target"), "{}", output.stdout);
  }

  #[test]
  fn a_manager_or_worker_profile_is_read_from_the_session_metadata() {
    let home = tempfile::tempdir().unwrap();
    let session = home.path().join("sessions").join("sess-1");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join(SESSION_METADATA_NAME), json!({ "id": "sess-1", "profile": "manager" }).to_string())
      .unwrap();
    let host = test_host(home.path(), &repo_root());
    assert_eq!(session_profile(&host.cfg, "sess-1").unwrap(), "manager");
  }

  #[test]
  fn the_session_identity_falls_back_to_a_cwd_named_by_the_sessions_dir() {
    let home = tempfile::tempdir().unwrap();
    let session = home.path().join("sessions").join("sess-2");
    std::fs::create_dir_all(&session).unwrap();
    let host = test_host(home.path(), &repo_root());
    let inputs = CheckInputs {
      home: home.path().to_path_buf(),
      cwd: session.clone(),
      session_env: None,
      repo_root: home.path().to_path_buf(),
    };
    assert_eq!(session_identity(&host.cfg, &inputs).as_deref(), Some("sess-2"));
    let outside = CheckInputs { cwd: home.path().to_path_buf(), ..inputs };
    assert_eq!(session_identity(&host.cfg, &outside), None);
  }

  #[test]
  fn a_waiting_check_without_assertions_only_is_refused_before_any_host_or_cold_read() {
    let args = CheckArgs {
      file: "page.html".to_string(),
      genre: "waiting".to_string(),
      trigger: Some("the trigger".to_string()),
      assertions_only: false,
    };
    // A cold read composes the host first, so a host factory that fails proves the refusal comes before it.
    let lazy_host = LazyHost::new(|| anyhow::bail!("the host must not be composed for a refused check"));
    let exit = check(&args, &lazy_host).err().expect("a waiting check without --assertions-only is refused");
    assert_eq!(exit.code, 2);
    assert!(exit.body["error"].as_str().unwrap().contains("charliebot web status set"), "{}", exit.body);
  }
}
