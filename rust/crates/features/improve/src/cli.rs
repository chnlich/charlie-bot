//! The `charliebot improve` and `charliebot improve-stop` verbs (`src/features/improve/cli.py`,
//! `src/features/improve/stop_cli.py`).
//!
//! `improve` reads the goal file (and the optional plan file), posts them to
//! `/api/internal/improve`, and returns immediately; the iteration loop runs on the server. The
//! verb's readback proves a launch whose response was lost: the newest loop directory whose
//! `goal.md` equals the sent goal, plus that loop's one worker child.

use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::{self, with_infer_long_args};
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde_json::{Map, Value, json};

use crate::sequence;

const SESSION_HELP: &str =
  "Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the master environment)";

const IMPROVE_EPILOG: &str = r#"The CLI creates a live goal at loops/{loop_id}/goal.md. Each
iteration re-reads it, so editing the file steers the remaining
iterations. The launch response includes loop_id and goal_path.

Runtime authorization is derived from the task tree and the chat event log;
see skills/plan-approval/SKILL.md for the full contract.

Invalid iterations (a missing, malformed, or zero-commit-no-verdict
report) are quarantined from loop context and still consume an
iteration slot.
"#;

#[derive(Parser)]
#[command(
  name = "improve",
  about = "Run an iterative improvement loop via CharlieBot workers",
  after_help = IMPROVE_EPILOG
)]
struct ImproveCli {
  #[arg(long, help = SESSION_HELP)]
  session: Option<String>,
  #[arg(long, required = true, help = "Path to the git repo workers should operate on")]
  repo: String,
  #[arg(long, default_value_t = 3, help = "Number of iterations to run")]
  iterations: i64,
  #[arg(
    long,
    required = true,
    help = "Path to a file containing the improvement goal (read and sent as the goal payload)"
  )]
  goal_file: String,
  #[arg(long, help = "Optional path to a file containing the improvement plan (read and sent as the plan payload)")]
  plan_file: Option<String>,
  #[arg(long, required = true, help = "Base branch for iteration worktrees")]
  base_branch: String,
  #[arg(long, help = "Configured backend option id from $CHARLIEBOT_HOME/config.yaml")]
  backend: Option<String>,
  #[arg(
    long,
    alias = "branch-prefix",
    help = "Single branch all iterations commit to (e.g. 'improve/optimize-step-time')"
  )]
  work_branch: Option<String>,
  #[arg(long, help = "Merge work_branch into base_branch after all iterations complete")]
  merge_back: bool,
}

#[derive(Parser)]
#[command(name = "improve-stop", about = "Stop this session's active improve loop after the current iteration")]
struct StopCli {
  #[arg(long, help = SESSION_HELP)]
  session: Option<String>,
}

/// `charliebot improve`: read the goal and optional plan, launch the loop, print the launch answer.
pub fn improve_main(args: &[OsString]) -> Result<(), CliExit> {
  let mut command = with_infer_long_args(ImproveCli::command());
  let argv = std::iter::once(OsString::from("charliebot improve")).chain(args.iter().cloned());
  let matches = match command.clone().try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ImproveCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");

  args::validate_repo_path(&mut command, &cli.repo);
  let session_id = resolve::resolve_from_env(cli.session.as_deref())?;
  let goal = files::read_required_text_file("--goal-file", &cli.goal_file)?;
  let plan = match &cli.plan_file {
    Some(path) => Some(files::read_required_text_file("--plan-file", path)?),
    None => None,
  };

  let mut payload = Map::new();
  payload.insert("session_id".to_string(), json!(session_id));
  payload.insert("repo_path".to_string(), json!(cli.repo));
  payload.insert("base_branch".to_string(), json!(cli.base_branch));
  payload.insert("backend".to_string(), optional_string(&cli.backend));
  payload.insert("iterations".to_string(), json!(cli.iterations));
  payload.insert("goal".to_string(), json!(goal));
  payload.insert("work_branch".to_string(), optional_string(&cli.work_branch));
  payload.insert("merge_back".to_string(), json!(cli.merge_back));
  if let Some(plan_text) = &plan {
    payload.insert("plan".to_string(), json!(plan_text));
  }

  let home = charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let readback_session = session_id.clone();
  let readback_goal = goal.clone();
  let readback_plan = plan.clone();
  let iterations = cli.iterations;
  let readback = move || {
    Ok(readback_launch(&home, &readback_session, &readback_goal, readback_plan.as_deref(), iterations))
  };
  let result = client::post("/api/internal/improve", Value::Object(payload), Some(Box::new(readback)), &[])?;

  print!("{}", json_out::render(&result, Some(2)));
  Ok(())
}

/// `charliebot improve-stop`: stop the session's running loop and report the server's ack.
pub fn stop_main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(StopCli::command());
  let argv = std::iter::once(OsString::from("charliebot improve-stop")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = StopCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");

  let session_id = resolve::resolve_from_env(cli.session.as_deref())?;
  client::post("/api/internal/improve/stop", json!({ "session_id": session_id }), None, &[])?;
  println!("stopped");
  Ok(())
}

/// The sent-but-lost readback: the loop's live goal file and its sequence's worker child prove the
/// launch landed. Returns the endpoint's response shape so the printed answer stays identical.
///
/// Every missing piece returns `None`, so the caller reports `outcome_unknown`.
fn readback_launch(home: &Path, session_id: &str, goal: &str, plan: Option<&str>, iterations: i64) -> Option<Value> {
  let loops_dir = home.join("sessions").join(session_id).join("loops");
  if !loops_dir.is_dir() {
    return None;
  }
  let mut candidates: Vec<i64> = Vec::new();
  for entry in std::fs::read_dir(&loops_dir).ok()? {
    let entry = entry.ok()?;
    let name = entry.file_name();
    let Some(name) = name.to_str() else { continue };
    if name.is_empty() || !name.chars().all(|digit| digit.is_ascii_digit()) {
      continue;
    }
    let Ok(loop_id) = name.parse::<i64>() else { continue };
    match files::read_text_mode(&entry.path().join("goal.md")) {
      Ok(text) if text == goal => candidates.push(loop_id),
      _ => continue,
    }
  }
  let loop_id = candidates.into_iter().max()?;

  // The session's metadata must have been written: the loop's child is admitted under it.
  let metadata_text = std::fs::read_to_string(home.join("sessions").join(session_id).join("metadata.json")).ok()?;
  charliebot_store::metadata::SessionMetadata::from_json(&metadata_text, None).ok()?;

  let loop_dir = loops_dir.join(loop_id.to_string());
  let mut response = Map::new();
  response.insert("status".to_string(), json!("started"));
  response.insert("session_id".to_string(), json!(session_id));
  response.insert("iterations".to_string(), json!(iterations));
  response.insert("loop_id".to_string(), json!(loop_id));
  response.insert("goal_path".to_string(), json!(loop_dir.join("goal.md").to_string_lossy().into_owned()));
  if plan.is_some() {
    response.insert("plan_path".to_string(), json!(loop_dir.join("plan.md").to_string_lossy().into_owned()));
  }
  // The v2 sequence's one worker child binds by its stable request id; its presence (and only the
  // matching parent's) proves the v2 admission.
  let child = charliebot_store::task_child::find_local_task_child(
    home,
    session_id,
    goal,
    None,
    Some(&sequence::improve_child_request_id(loop_id)),
  )?;
  response.insert("child_session_id".to_string(), child.get("session_id")?.clone());
  Some(Value::Object(response))
}

fn optional_string(value: &Option<String>) -> Value {
  value.as_ref().map_or(Value::Null, |value| json!(value))
}
