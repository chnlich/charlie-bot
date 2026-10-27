//! The `charliebot plan` verbs (`src/features/artifacts/plan_cli.py`): present, amend, approve, close, diff, list.
//!
//! Every registering verb resolves the session, posts to `/api/internal/plan/<verb>` with a readback that answers a
//! sent-but-lost call from the session's own plan registry, and adds the verb's next-step reminder. `diff` and `list`
//! read the registry through `GET /api/sessions/<sid>/plans`; `diff` then compares the two version files on disk.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_core::home;
use charliebot_core::paths;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

use crate::plan_diff;
use crate::plan_paths;
use crate::plans;

/// The next step `present` prints: a new lineage takes an Improve round before its checks.
const PRESENT_REMINDER: &str = "Run one Improve round on this version, then verify and the cold read. \
The parent node or the root line approves the plan after both. Contract: the plan-approval skill.";
/// The next step `amend` prints: a later version takes only the checks.
const AMEND_REMINDER: &str = "A verify round and the cold read check this version before approval. \
Contract: the plan-approval skill.";
/// The revision triggers `--trigger` accepts (`constants.PLAN_AMEND_TRIGGERS`).
const AMEND_TRIGGERS: [&str; 2] = ["auto_amend", "feedback"];
/// The close modes `--as` accepts (`constants.PLAN_CLOSE_MODES`).
const CLOSE_MODES: [&str; 3] = ["superseded", "abandoned", "completed"];

#[derive(Parser)]
#[command(name = "plan", about = "Plan registry verbs")]
struct PlanCli {
  #[command(subcommand)]
  command: PlanCommand,
}

#[derive(Subcommand)]
enum PlanCommand {
  /// Register a new plan lineage
  Present(PresentArgs),
  /// Append the next version to a plan lineage
  Amend(AmendArgs),
  /// Record the plan's approval
  Approve(ApproveArgs),
  /// Terminate a plan lineage
  Close(CloseArgs),
  /// Print the local diff of one version against its predecessor
  Diff(DiffArgs),
  /// Print the session's plan registry
  List(ListArgs),
}

/// The code target a registration records.
#[derive(Args)]
struct BaseArgs {
  /// Code repo the plan targets
  #[arg(long)]
  base_repo: Option<String>,
  /// Code branch the plan targets
  #[arg(long)]
  base_branch: Option<String>,
  /// Code sha the plan targets
  #[arg(long)]
  base_sha: Option<String>,
}

#[derive(Args)]
struct PresentArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Artifact path relative to the session dir
  #[arg(long)]
  file: String,
  /// Plan title
  #[arg(long)]
  title: String,
  #[command(flatten)]
  base: BaseArgs,
}

#[derive(Args)]
struct AmendArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Artifact path relative to the session dir
  #[arg(long)]
  file: String,
  /// One line saying why this version differs from its predecessor; rides on the version record
  #[arg(long)]
  note: String,
  /// Target plan id (required when ambiguous)
  #[arg(long)]
  plan: Option<i64>,
  /// Revision trigger (default feedback)
  #[arg(long, value_parser = AMEND_TRIGGERS, default_value = "feedback")]
  trigger: String,
  #[command(flatten)]
  base: BaseArgs,
}

#[derive(Args)]
struct ApproveArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Target plan id (required when ambiguous)
  #[arg(long)]
  plan: Option<i64>,
}

#[derive(Args)]
struct CloseArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Target plan id
  #[arg(long)]
  plan: i64,
  /// How the lineage ends
  #[arg(long = "as", value_parser = CLOSE_MODES)]
  close_as: String,
}

#[derive(Args)]
struct DiffArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Target plan id (required when the session holds several)
  #[arg(long)]
  plan: Option<i64>,
  /// Version to diff against its predecessor (default: latest)
  #[arg(long)]
  v: Option<i64>,
}

#[derive(Args)]
struct ListArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
}

/// Parse the plan arguments and run one verb. Parser errors print and exit on their own (argparse-compatible:
/// help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(PlanCli::command());
  let argv = std::iter::once(OsString::from("charliebot plan")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = PlanCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let (session, verb) = match cli.command {
    PlanCommand::Present(args) => (args.session.clone(), Verb::Present(args)),
    PlanCommand::Amend(args) => (args.session.clone(), Verb::Amend(args)),
    PlanCommand::Approve(args) => (args.session.clone(), Verb::Approve(args)),
    PlanCommand::Close(args) => (args.session.clone(), Verb::Close(args)),
    PlanCommand::Diff(args) => (args.session.clone(), Verb::Diff(args)),
    PlanCommand::List(args) => (args.session.clone(), Verb::List),
  };
  let session_id = resolve::resolve_from_env(session.as_deref())?;
  match verb {
    Verb::List => print_result(&client::get(&format!("/api/sessions/{session_id}/plans"), None)?),
    Verb::Diff(args) => print_result(&build_diff(&session_id, &args)?),
    Verb::Present(args) => {
      let (title, file) = (args.title.clone(), args.file.clone());
      register(&session_id, "present", present_payload(&session_id, &args), move |plans| {
        readback_present(plans, &title, &file)
      })?;
    }
    Verb::Amend(args) => {
      let (plan, file) = (args.plan, args.file.clone());
      register(&session_id, "amend", amend_payload(&session_id, &args), move |plans| {
        readback_amend(plans, plan, &file)
      })?;
    }
    Verb::Approve(args) => {
      let plan = args.plan;
      register(&session_id, "approve", approve_payload(&session_id, &args), move |plans| {
        readback_approve(plans, plan)
      })?;
    }
    Verb::Close(args) => {
      let (plan, close_as) = (args.plan, args.close_as.clone());
      register(&session_id, "close", close_payload(&session_id, &args), move |plans| {
        readback_close(plans, plan, &close_as)
      })?;
    }
  }
  Ok(())
}

enum Verb {
  Present(PresentArgs),
  Amend(AmendArgs),
  Approve(ApproveArgs),
  Close(CloseArgs),
  Diff(DiffArgs),
  List,
}

fn print_result(value: &Value) {
  print!("{}", json_out::render(value, Some(2)));
}

fn present_payload(session_id: &str, args: &PresentArgs) -> Value {
  json!({
    "session_id": session_id,
    "file": args.file,
    "title": args.title,
    "base_repo": args.base.base_repo,
    "base_branch": args.base.base_branch,
    "base_sha": args.base.base_sha,
  })
}

fn amend_payload(session_id: &str, args: &AmendArgs) -> Value {
  json!({
    "session_id": session_id,
    "file": args.file,
    "plan_id": args.plan,
    "trigger": args.trigger,
    "note": args.note,
    "base_repo": args.base.base_repo,
    "base_branch": args.base.base_branch,
    "base_sha": args.base.base_sha,
  })
}

fn approve_payload(session_id: &str, args: &ApproveArgs) -> Value {
  json!({ "session_id": session_id, "plan_id": args.plan })
}

fn close_payload(session_id: &str, args: &CloseArgs) -> Value {
  json!({ "session_id": session_id, "plan_id": args.plan, "close_as": args.close_as })
}

/// Post one registering verb, answering a sent-but-lost call from the session's own registry, and add the verb's
/// reminder when the server's response carries none.
fn register(
  session_id: &str,
  verb: &str,
  payload: Value,
  readback: impl FnOnce(&[Value]) -> Option<Value> + 'static,
) -> Result<(), CliExit> {
  let endpoint = format!("/api/internal/plan/{verb}");
  let session = session_id.to_string();
  let readback = move || readback_plan(&session, readback);
  let mut result = client::post(&endpoint, payload, Some(Box::new(readback)), &[])?;
  let reminder = match verb {
    "present" => Some(PRESENT_REMINDER),
    "amend" => Some(AMEND_REMINDER),
    _ => None,
  };
  if let Some(reminder) = reminder
    && let Value::Object(members) = &mut result
    && !members.contains_key("reminder")
  {
    members.insert("reminder".to_string(), json!(reminder));
  }
  print_result(&result);
  Ok(())
}

/// The registry listing the readbacks match against; an unreadable listing proves nothing.
fn listing_plans(session_id: &str) -> Option<Vec<Value>> {
  let listing = client::get(&format!("/api/sessions/{session_id}/plans"), None).ok()?;
  match listing.get("plans") {
    Some(Value::Array(plans)) => Some(plans.clone()),
    _ => None,
  }
}

fn readback_plan(session_id: &str, readback: impl FnOnce(&[Value]) -> Option<Value>) -> Option<Value> {
  readback(&listing_plans(session_id)?)
}

/// `_same_file`: path-string equality tolerant of normalization (`./` prefixes, duplicate slashes).
fn same_file(left: Option<&str>, right: Option<&str>) -> bool {
  match (left, right) {
    (Some(left), Some(right)) => plan_paths::posix_normpath(left) == plan_paths::posix_normpath(right),
    _ => false,
  }
}

/// `_latest_version`: the version with the greatest `v`; a plan with no versions is a broken registry.
fn latest_version(plan: &Value) -> &Value {
  plan["versions"]
    .as_array()
    .and_then(|versions| versions.iter().max_by_key(|version| version["v"].as_i64().unwrap_or(0)))
    .expect("a registered plan carries at least one version")
}

/// `_readback_verdict`: the response shape present/amend/approve return.
fn readback_verdict(plan: &Value) -> Value {
  json!({ "plan": plan["id"], "v": latest_version(plan)["v"], "state": plan["state"] })
}

fn readback_present(plans: &[Value], title: &str, file: &str) -> Option<Value> {
  plans.iter().find_map(|plan| {
    let found_title = plan.get("title").and_then(Value::as_str);
    let files: Vec<&str> = plan["versions"]
      .as_array()
      .map(|versions| versions.iter().filter_map(|version| version.get("file").and_then(Value::as_str)).collect())
      .unwrap_or_default();
    (found_title == Some(title) && files.into_iter().any(|candidate| same_file(Some(candidate), Some(file))))
      .then(|| readback_verdict(plan))
  })
}

fn readback_amend(plans: &[Value], plan_id: Option<i64>, file: &str) -> Option<Value> {
  plans.iter().find_map(|plan| {
    if plan_id.is_some_and(|plan_id| plan.get("id").and_then(Value::as_i64) != Some(plan_id)) {
      return None;
    }
    let latest_file = latest_version(plan).get("file").and_then(Value::as_str);
    same_file(latest_file, Some(file)).then(|| readback_verdict(plan))
  })
}

fn readback_approve(plans: &[Value], plan_id: Option<i64>) -> Option<Value> {
  let candidates: Vec<&Value> = plans
    .iter()
    .filter(|plan| {
      plan.get("takeoff").is_some_and(|takeoff| !takeoff.is_null())
        && plan.get("closed").is_none_or(Value::is_null)
        && plan_id.is_none_or(|plan_id| plan.get("id").and_then(Value::as_i64) == Some(plan_id))
    })
    .collect();
  match candidates.as_slice() {
    [plan] => Some(readback_verdict(plan)),
    _ => None,
  }
}

fn readback_close(plans: &[Value], plan_id: i64, close_as: &str) -> Option<Value> {
  plans.iter().find_map(|plan| {
    let closed_as = plan.get("closed").and_then(|closed| closed.get("as")).and_then(Value::as_str);
    (plan.get("id").and_then(Value::as_i64) == Some(plan_id) && closed_as == Some(close_as))
      .then(|| json!({ "plan": plan["id"], "state": plan["state"] }))
  })
}

/// `_build_diff`: the registry through the list endpoint, the two version files off disk, the diff local.
fn build_diff(session_id: &str, args: &DiffArgs) -> Result<Value, CliExit> {
  let listing = client::get(&format!("/api/sessions/{session_id}/plans"), None)?;
  let session_dir = session_dir(session_id)?;
  let plans: Vec<Value> = match listing.get("plans") {
    Some(Value::Array(plans)) => plans.clone(),
    _ => Vec::new(),
  };
  let (plan, from_version, to_version) = resolve_diff_plan(&plans, args).map_err(CliExit::error)?;
  let old_html = read_version_file(&session_dir, from_version).map_err(CliExit::error)?;
  let new_html = read_version_file(&session_dir, to_version).map_err(CliExit::error)?;
  let text = plan_diff::diff_text(&old_html, &new_html).map_err(CliExit::error)?;
  Ok(json!({
    "plan": plan["id"],
    "from": from_version["v"],
    "to": to_version["v"],
    "note": to_version.get("note").cloned().unwrap_or(Value::Null),
    "text": text,
  }))
}

/// `resolve_diff_plan`: the (plan, predecessor, target) triple; the message names the problem.
fn resolve_diff_plan<'a>(plans: &'a [Value], args: &DiffArgs) -> Result<(&'a Value, &'a Value, &'a Value), String> {
  let plan = match args.plan {
    Some(plan_id) => plans::require_plan(plans, plan_id)?,
    None => {
      if plans.is_empty() {
        return Err("session has no plans; nothing to diff".to_string());
      }
      if plans.len() > 1 {
        let ids = plans.iter().map(|plan| plan["id"].to_string()).collect::<Vec<_>>().join(", ");
        return Err(format!("diff requires --plan (multiple plans: {ids})"));
      }
      &plans[0]
    }
  };
  let versions = plan["versions"].as_array().ok_or_else(|| "the plan's versions are not a list".to_string())?;
  let target = match args.v {
    Some(v) => v,
    None => versions.iter().filter_map(|version| version["v"].as_i64()).max().unwrap_or(0),
  };
  let to_version = versions
    .iter()
    .find(|version| version["v"].as_i64() == Some(target))
    .ok_or_else(|| format!("plan {} has no version {target}", plan["id"]))?;
  if target < 2 {
    return Err(format!("version {target} has no predecessor; diff needs v >= 2"));
  }
  let from_version = versions
    .iter()
    .find(|version| version["v"].as_i64() == Some(target - 1))
    .ok_or_else(|| format!("predecessor version {} not found in plan {}", target - 1, plan["id"]))?;
  Ok((plan, from_version, to_version))
}

/// The profile home's `sessions` directory.
fn session_dir(session_id: &str) -> Result<PathBuf, CliExit> {
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  Ok(home.join("sessions").join(session_id))
}

/// `_read_version_file`: the version's artifact, a registry path relative to the session dir, read as text.
fn read_version_file(session_dir: &Path, version: &Value) -> Result<String, String> {
  let file = version.get("file").and_then(Value::as_str).unwrap_or_default();
  let path = session_dir.join(file);
  if !path.is_file() {
    return Err(format!("version file not found: {}", paths::display(&path)));
  }
  let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
  Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn plan_doc(id: i64, versions: Value) -> Value {
    json!({ "id": id, "title": "P1", "versions": versions, "takeoff": null, "closed": null, "state": "awaiting approval" })
  }

  #[test]
  fn the_present_readback_matches_the_title_and_a_version_file() {
    let plans = vec![plan_doc(1, json!([{ "v": 1, "file": "./artifacts//plan_01.html" }]))];
    assert_eq!(
      readback_present(&plans, "P1", "artifacts/plan_01.html"),
      Some(json!({ "plan": 1, "v": 1, "state": "awaiting approval" }))
    );
    assert_eq!(readback_present(&plans, "P2", "artifacts/plan_01.html"), None);
  }

  #[test]
  fn the_approve_readback_needs_exactly_one_open_takeoff() {
    let open = plan_doc(1, json!([{ "v": 1, "file": "a.html" }]));
    let approved = json!({ "id": 1, "title": "P1", "versions": [{ "v": 1, "file": "a.html" }],
      "takeoff": { "v": 1 }, "closed": null, "state": "approved" });
    assert_eq!(readback_approve(std::slice::from_ref(&open), None), None);
    assert_eq!(
      readback_approve(std::slice::from_ref(&approved), None),
      Some(json!({ "plan": 1, "v": 1, "state": "approved" }))
    );
    assert_eq!(readback_approve(&[approved.clone(), approved], None), None);
  }

  #[test]
  fn the_diff_resolution_names_a_missing_predecessor() {
    let plans = vec![plan_doc(1, json!([{ "v": 2, "file": "a.html" }]))];
    let args = DiffArgs { session: None, plan: None, v: None };
    assert_eq!(resolve_diff_plan(&plans, &args).unwrap_err(), "predecessor version 1 not found in plan 1");
  }

  #[test]
  fn the_diff_resolution_requires_a_plan_when_the_session_holds_several() {
    let plans = vec![plan_doc(1, json!([])), plan_doc(2, json!([]))];
    let args = DiffArgs { session: None, plan: None, v: None };
    assert_eq!(resolve_diff_plan(&plans, &args).unwrap_err(), "diff requires --plan (multiple plans: 1, 2)");
  }
}
