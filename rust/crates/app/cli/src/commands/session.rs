//! The `session` verbs (`src/runtime/cli/session.py`): create, tree, retry, archive, acknowledge,
//! cancel, reparent, send, dialog.
//!
//! `create` builds session metadata only (no first message); with `--parent` it becomes the v2
//! task create whose (parent, request_id) binds one stable node. `archive` and `cancel` keep the
//! sent-but-lost readback: a lost response reads this request's own close fact from the target
//! session's chat events, and a deferred close answers as the server's archive answer does, from the
//! session's metadata. `reparent` reads back its own `task_reparented` fact the same way.
//! `send` relays a message as an `agent_message` event and maps the 422 over-limit refusal to
//! exit 2. `dialog` prints one session's chat messages as plain text, oldest first, for piping
//! into `rg`.

use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::files;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_cli_common::route;
use charliebot_core::config;
use charliebot_core::event_types;
use charliebot_core::run_token::SESSION_ID_ENV;
use charliebot_core::text_repr::{self, JsonStyle};
use charliebot_session_api::api as session_routes;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::metadata::{UtcDateTime, read_session_metadata};
use charliebot_store::{control_events, layout};
use charliebot_tasks::completion::{CLOSED_EVENT_ID_FIELD, STOPPED_RUNS_FIELD, pending_archive_answer};
use charliebot_tasks::routes as task_routes;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, Value, json};

/// Exit code when the server refuses the send (over-limit message): mirrors argparse's
/// usage-error exit code, matching schedule-trigger's 422 mapping.
const EXIT_SEND_REFUSED: i32 = 2;

/// The events endpoint's clamp; the fewest requests a full dialog history admits.
const DIALOG_PAGE_LIMIT: i64 = 200;

/// `constants.SESSION_PAGE_LIMIT_*`: the bounds the tree parser and the runs paging share.
const SESSION_PAGE_LIMIT_DEFAULT: i64 = 100;
const SESSION_PAGE_LIMIT_MAX: i64 = 500;

/// `json.dumps(..., ensure_ascii=False)` with every other option at its default.
const JSON_ENSURE_ASCII_FALSE: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

#[derive(Parser)]
#[command(name = "session", about = "CharlieBot session mutations")]
struct SessionCli {
  #[command(subcommand)]
  command: SessionCommand,
}

#[derive(Subcommand)]
enum SessionCommand {
  /// Create a session (metadata only, no first message)
  Create(CreateArgs),
  /// Page the task tree
  Tree(TreeArgs),
  /// Create the retry run of one recorded run
  Retry(RetryArgs),
  /// Archive a task or its subtree
  Archive(ArchiveArgs),
  /// Acknowledge exact task inputs the operator handled out-of-band
  Acknowledge(AcknowledgeArgs),
  /// Cancel a task; stop runs and end delegation for workers
  Cancel(CancelArgs),
  /// Move a task under a new parent; an agent moves tasks inside its own subtree only
  Reparent(ReparentArgs),
  /// Set who approves a task: `user` holds its self-archive until the user opens the session, `parent` closes it at run end
  SetApprover(SetApproverArgs),
  /// Relay a message to another session as an agent_message
  Send(SendArgs),
  /// Print one session's chat messages as plain text, oldest first (for rg)
  Dialog(DialogArgs),
}

#[derive(Args)]
struct CreateArgs {
  /// Session/task name (optional)
  #[arg(long)]
  name: Option<String>,
  /// Backend id (optional)
  #[arg(long)]
  backend: Option<String>,
  /// Group name to assign after creation (optional)
  #[arg(long)]
  group: Option<String>,
  /// Parent task id (optional; v2 task create)
  #[arg(long)]
  parent: Option<String>,
  /// Task profile (v2 task create)
  #[arg(long, value_parser = ["manager", "worker"])]
  profile: Option<String>,
  /// Path to the task object JSON (v2 task create)
  #[arg(long)]
  task_file: Option<String>,
  /// Request id binding the stable node id (defaults to a fresh UUID)
  #[arg(long)]
  request_id: Option<String>,
  /// Who approves the task (v2 task create; defaults to `user`)
  #[arg(long, value_parser = ["user", "parent"])]
  approver: Option<String>,
}

#[derive(Args)]
struct TreeArgs {
  /// Parent task id (default: the roots)
  #[arg(long)]
  root: Option<String>,
  /// Include archived/collapsed rows
  #[arg(long)]
  include_archived: bool,
  /// Page size, 1..500
  #[arg(long, default_value_t = SESSION_PAGE_LIMIT_DEFAULT)]
  limit: i64,
  /// next_cursor from the previous page
  #[arg(long)]
  cursor: Option<String>,
}

#[derive(Args)]
struct RetryArgs {
  /// Task id
  session_id: String,
  /// The run id being retried
  #[arg(long)]
  run: String,
  /// Request id binding the retry run (defaults to a fresh UUID)
  #[arg(long)]
  request_id: Option<String>,
}

#[derive(Args)]
struct ArchiveArgs {
  /// Task id
  session_id: String,
  /// Confirm the target is outside the caller's subtree; an agent needs it to archive such a task
  #[arg(long)]
  archive_non_child: bool,
  /// Path to the archive request's JSON body (summary, result_refs, run_ids); required to archive the
  /// caller's own task, optional otherwise (the summary then defaults to "archived by <caller task id>")
  #[arg(long)]
  result_file: Option<String>,
  /// Request id binding the archive (defaults to a fresh UUID)
  #[arg(long)]
  request_id: Option<String>,
}

#[derive(Args)]
struct AcknowledgeArgs {
  /// Task id
  session_id: String,
  /// Comma-separated input event ids being resolved (each must currently be pending)
  #[arg(long)]
  input_ids: String,
  /// Optional note recorded with the acknowledgement
  #[arg(long, default_value = "")]
  note: String,
  /// Request id binding the acknowledgement (defaults to a fresh UUID)
  #[arg(long)]
  request_id: Option<String>,
}

#[derive(Args)]
struct CancelArgs {
  /// Task id
  session_id: String,
  /// Cancellation reason
  #[arg(long)]
  reason: String,
  /// Request id binding the cancel (defaults to a fresh UUID)
  #[arg(long)]
  request_id: Option<String>,
}

#[derive(Args)]
struct ReparentArgs {
  /// Task id
  session_id: String,
  /// The new parent task id
  #[arg(long)]
  parent: String,
}

#[derive(Args)]
struct SetApproverArgs {
  /// Task id
  session_id: String,
  /// The approver: `user` or `parent`
  #[arg(value_parser = ["user", "parent"])]
  approver: String,
}

#[derive(Args)]
struct SendArgs {
  /// Target session id
  target: String,
  /// Message text; long content belongs in a file
  #[arg(long, group = "source")]
  message: Option<String>,
  /// Read the message text from this file
  #[arg(long, group = "source")]
  file: Option<String>,
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
}

#[derive(Args)]
struct DialogArgs {
  /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)
  #[arg(long)]
  session: Option<String>,
  /// Print only the last N complete turns plus the turn in progress
  #[arg(long)]
  turns: Option<i64>,
}

/// Parse the session arguments and run one verb. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = send_source_group(with_infer_long_args(SessionCli::command()));
  let argv = std::iter::once(OsString::from("charliebot session")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = SessionCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    SessionCommand::Create(args) => create(&args),
    SessionCommand::Tree(args) => tree(&args),
    SessionCommand::Retry(args) => retry(&args),
    SessionCommand::Archive(args) => archive(&args),
    SessionCommand::Acknowledge(args) => acknowledge(&args),
    SessionCommand::Cancel(args) => cancel(&args),
    SessionCommand::Reparent(args) => reparent(&args),
    SessionCommand::SetApprover(args) => set_approver(&args),
    SessionCommand::Send(args) => send(&args),
    SessionCommand::Dialog(args) => dialog(&args),
  }
}

/// The `send` source is argparse's required mutually exclusive group: `--message` or `--file`,
/// exactly one.
fn send_source_group(command: clap::Command) -> clap::Command {
  let mut command = command;
  if let Some(send) = command.find_subcommand_mut("send") {
    let group = clap::ArgGroup::new("source").required(true).multiple(false).args(["message", "file"]);
    *send = send.clone().group(group);
  }
  command
}

fn print_result(value: &Value) {
  print!("{}", json_out::render(value, Some(2)));
}

fn home_dir() -> Result<std::path::PathBuf, CliExit> {
  charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))
}

fn parse_json(flag_name: &str, text: &str) -> Result<Value, CliExit> {
  serde_json::from_str(text).map_err(|error| CliExit::error(format!("{flag_name}: {error}")))
}

/// A response field the verb reads directly; a missing one is a broken server contract.
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, CliExit> {
  value.get(key).ok_or_else(|| CliExit::error(format!("the response lacks {key}")))
}

fn create(args: &CreateArgs) -> Result<(), CliExit> {
  let mut payload = Map::new();
  if let Some(name) = &args.name {
    payload.insert("name".to_string(), json!(name));
  }
  if let Some(backend) = &args.backend {
    payload.insert("backend".to_string(), json!(backend));
  }
  if args.parent.is_some() || args.profile.is_some() || args.task_file.is_some() {
    let parent =
      args.parent.as_deref().ok_or_else(|| CliExit::usage("v2 task create requires both --parent and --profile"))?;
    let profile = args
      .profile
      .as_deref()
      .filter(|profile| !profile.is_empty())
      .ok_or_else(|| CliExit::usage("v2 task create requires both --parent and --profile"))?;
    if let Some(task_file) = &args.task_file {
      let text = files::read_required_text_file("--task-file", task_file)?;
      payload.insert("task".to_string(), parse_json("--task-file", &text)?);
    }
    payload.insert("task_parent_id".to_string(), json!(parent));
    payload.insert("profile".to_string(), json!(profile));
    payload
      .insert("request_id".to_string(), json!(args.request_id.clone().unwrap_or_else(control_events::new_request_id)));
    if let Some(approver) = &args.approver {
      payload.insert("approver".to_string(), json!(approver));
    }
  }
  let mut result = client::post(session_routes::SESSIONS_PATH, Value::Object(payload), None, &[])?;
  if let Some(group) = &args.group {
    let id =
      field(&result, "id")?.as_str().ok_or_else(|| CliExit::error("the create response's id is not a string"))?;
    result =
      client::post(&route::fill(session_routes::SESSION_GROUP_PATH, &[id]), json!({ "group": group }), None, &[])?;
  }
  print_result(&result);
  Ok(())
}

fn tree(args: &TreeArgs) -> Result<(), CliExit> {
  let mut params = vec![
    ("limit".to_string(), args.limit.to_string()),
    ("include_archived".to_string(), args.include_archived.to_string()),
  ];
  if let Some(root) = &args.root {
    params.push(("parent_id".to_string(), root.clone()));
  }
  if let Some(cursor) = &args.cursor {
    params.push(("cursor".to_string(), cursor.clone()));
  }
  let result = client::get(session_routes::SESSION_TREE_PATH, Some(params))?;
  print_result(&result);
  Ok(())
}

fn retry(args: &RetryArgs) -> Result<(), CliExit> {
  let payload = json!({
    "request_id": args.request_id.clone().unwrap_or_else(control_events::new_request_id),
    "run_id": args.run,
  });
  let result =
    client::post(&route::fill(task_routes::SESSION_RETRY_PATH, &[args.session_id.as_str()]), payload, None, &[])?;
  print_result(&result);
  Ok(())
}

fn archive(args: &ArchiveArgs) -> Result<(), CliExit> {
  let mut body: Map<String, Value> = match &args.result_file {
    Some(result_file) => {
      let text = files::read_required_text_file("--result-file", result_file)?;
      match parse_json("--result-file", &text)? {
        Value::Object(body) => body,
        _ => return Err(CliExit::usage("--result-file must carry the archive request's JSON object")),
      }
    }
    None => Map::new(),
  };
  if args.archive_non_child {
    body.insert(task_routes::ARCHIVE_NON_CHILD_FIELD.to_string(), json!(true));
  }
  let request_id = args
    .request_id
    .clone()
    .or_else(|| {
      body.get("request_id").filter(|value| text_repr::is_truthy(value)).and_then(Value::as_str).map(str::to_string)
    })
    .unwrap_or_else(control_events::new_request_id);
  body.insert("request_id".to_string(), json!(request_id));

  let home = home_dir()?;
  let task_id = args.session_id.clone();
  let readback = move || find_local_task_close(&home, &task_id, &request_id);
  let result = client::post(
    &route::fill(task_routes::SESSION_ARCHIVE_PATH, &[args.session_id.as_str()]),
    Value::Object(body),
    Some(Box::new(readback)),
    &[],
  )?;
  print_result(&result);
  Ok(())
}

fn acknowledge(args: &AcknowledgeArgs) -> Result<(), CliExit> {
  let input_ids: Vec<String> =
    args.input_ids.split(',').map(str::trim).filter(|part| !part.is_empty()).map(str::to_string).collect();
  if input_ids.is_empty() {
    return Err(CliExit::usage("--input-ids must name at least one input event id"));
  }
  let payload = json!({
    "request_id": args.request_id.clone().unwrap_or_else(control_events::new_request_id),
    "input_ids": input_ids,
    "note": args.note,
  });
  let result = client::post(
    &route::fill(task_routes::TASK_INPUTS_ACKNOWLEDGE_PATH, &[args.session_id.as_str()]),
    payload,
    None,
    &[],
  )?;
  print_result(&result);
  Ok(())
}

fn cancel(args: &CancelArgs) -> Result<(), CliExit> {
  let request_id = args.request_id.clone().unwrap_or_else(control_events::new_request_id);
  let payload = json!({ "request_id": request_id, "reason": args.reason });

  let home = home_dir()?;
  let task_id = args.session_id.clone();
  let readback = move || find_local_task_close(&home, &task_id, &request_id);
  let result = client::post(
    &route::fill(task_routes::SESSION_CANCEL_PATH, &[args.session_id.as_str()]),
    payload,
    Some(Box::new(readback)),
    &[],
  )?;
  print_result(&result);

  let stopped_runs = result.get(STOPPED_RUNS_FIELD).and_then(Value::as_array).cloned().unwrap_or_default();
  let unresolved: Vec<&Value> =
    stopped_runs.iter().filter(|item| item.get("outcome").is_none_or(Value::is_null)).collect();
  if unresolved.is_empty() {
    return Ok(());
  }
  let run_ids: Vec<&str> = unresolved
    .iter()
    .map(|item| field(item, "run_id")?.as_str().ok_or_else(|| CliExit::error("a stopped run's run_id is not a string")))
    .collect::<Result<_, _>>()?;
  let mut run_rows: Map<String, Value> = Map::new();
  let mut cursor: Option<String> = None;
  while run_ids.iter().any(|run_id| !run_rows.contains_key(*run_id)) {
    let mut params = vec![("limit".to_string(), SESSION_PAGE_LIMIT_MAX.to_string())];
    if let Some(cursor) = &cursor {
      params.push(("cursor".to_string(), cursor.clone()));
    }
    let page = client::get(&route::fill(session_routes::SESSION_RUNS_PATH, &[args.session_id.as_str()]), Some(params))?;
    for row in field(&page, "items")?.as_array().cloned().unwrap_or_default() {
      let id = field(&row, "id")?.as_str().ok_or_else(|| CliExit::error("a run row's id is not a string"))?;
      if run_ids.contains(&id) {
        run_rows.insert(id.to_string(), row);
      }
    }
    cursor = match field(&page, "next_cursor")? {
      Value::Null => break,
      value => Some(text_repr::value_str(value)),
    };
  }
  let mut lines = String::new();
  for run_id in &run_ids {
    let row = run_rows.get(*run_id).ok_or_else(|| CliExit::error(format!("the runs listing lacks run {run_id}")))?;
    let pid = text_repr::value_str(field(row, "pid")?);
    lines.push_str(&format!("run {run_id} pid {pid} did not exit\n"));
  }
  eprint!("{lines}");
  std::process::exit(1);
}

fn set_approver(args: &SetApproverArgs) -> Result<(), CliExit> {
  let result = client::patch(
    &route::fill(session_routes::SESSION_PATH, &[args.session_id.as_str()]),
    json!({ "approver": args.approver }),
    None,
    &[],
  )?;
  print_result(&result);
  Ok(())
}

fn reparent(args: &ReparentArgs) -> Result<(), CliExit> {
  let home = home_dir()?;
  let task_id = args.session_id.clone();
  let parent_id = args.parent.clone();
  let caller_session_id = std::env::var(SESSION_ID_ENV).ok().filter(|value| !value.is_empty());
  let sent_at = UtcDateTime::now();
  let readback =
    move || Ok(find_local_task_reparent(&home, &task_id, &parent_id, caller_session_id.as_deref(), sent_at));
  let result = client::patch(
    &route::fill(session_routes::SESSION_PATH, &[args.session_id.as_str()]),
    json!({ "task_parent_id": args.parent }),
    Some(Box::new(readback)),
    &[],
  )?;
  print_result(&result);
  Ok(())
}

fn send(args: &SendArgs) -> Result<(), CliExit> {
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let content = match (&args.message, &args.file) {
    (Some(message), _) => message.clone(),
    (None, Some(file)) => files::read_required_text_file("--file", file)?,
    (None, None) => {
      // The clap source group makes one of the two required; the branch mirrors the Python verb's.
      return Err(CliExit::usage("one of --message or --file is required"));
    }
  };
  let result = client::post(
    task_routes::SESSION_MESSAGE_PATH,
    json!({
      "session_id": session_id,
      (task_routes::SESSION_MESSAGE_TARGET_SESSION_ID_FIELD): args.target,
      "content": content,
    }),
    None,
    &[(422, EXIT_SEND_REFUSED)],
  )?;
  print_result(&result);
  Ok(())
}

/// `find_local_task_close`: the close fact THIS request's own identity binds to — the event whose
/// id derives from (task id, request id) via `stable_close_event_id`. A missing file or an
/// unreadable line proves nothing (`None`), never a borrowed answer; the closed fact outranks the
/// deferral. A deferral answers as the server's archive answer does, which names the approver from
/// the session's metadata; metadata that does not decode fails loud rather than choosing an approver.
fn find_local_task_close(home: &Path, task_id: &str, request_id: &str) -> Result<Option<Value>, CliExit> {
  let session_dir = config::sessions_dir_in(home).join(task_id);
  match scan_task_close(&layout::chat_events_path(&session_dir), task_id, request_id) {
    Some(LocalClose::Closed(closed)) => Ok(Some(closed)),
    Some(LocalClose::Requested) => {
      let metadata_path = layout::metadata_path(&session_dir);
      let meta = read_session_metadata(&metadata_path).map_err(|error| {
        CliExit::error(format!("cannot read the task's approver from {}: {error}", metadata_path.display()))
      })?;
      Ok(Some(pending_archive_answer(meta.as_ref(), task_id, request_id)))
    }
    None => Ok(None),
  }
}

/// What a session's chat events record for one request: its closed fact, or only its deferral.
enum LocalClose {
  Closed(Value),
  Requested,
}

/// Scan the events in file order. A closed fact returns at once, so it outranks a deferral written
/// before or after it.
fn scan_task_close(events_path: &Path, task_id: &str, request_id: &str) -> Option<LocalClose> {
  let closed_id = control_events::stable_close_event_id(task_id, request_id);
  let mut requested = false;
  for line in chat_events::read_lines(events_path, LineRule::Lines).ok()? {
    let event = line.ok()?.decode().ok()?;
    let object = event.as_object()?;
    let event_type = object.get("type").and_then(Value::as_str);
    if event_type == Some(event_types::TASK_CLOSED)
      && object.get("id").and_then(Value::as_str) == Some(closed_id.as_str())
    {
      let outcome = object
        .get("outcome")
        .filter(|outcome| text_repr::is_truthy(outcome))
        .map(text_repr::value_str)
        .unwrap_or_else(|| "completed".to_string());
      return Some(LocalClose::Closed(json!({
        "session_id": task_id,
        "task_state": outcome,
        CLOSED_EVENT_ID_FIELD: closed_id,
      })));
    }
    if event_type == Some(event_types::TASK_CLOSE_REQUESTED)
      && object.get("request_id").and_then(Value::as_str) == Some(request_id)
    {
      requested = true;
    }
  }
  requested.then_some(LocalClose::Requested)
}

/// The `task_reparented` fact THIS request wrote: the same caller session (`None` for an operator
/// caller), the same new parent, and a timestamp no earlier than the send. A parent that already
/// sat there proves nothing, so the readback reads the fact and never the metadata. A missing file
/// or an unreadable line proves nothing (`None`).
fn find_local_task_reparent(
  home: &Path,
  task_id: &str,
  parent_id: &str,
  caller_session_id: Option<&str>,
  sent_at: UtcDateTime,
) -> Option<Value> {
  let session_dir = config::sessions_dir_in(home).join(task_id);
  let events_path = layout::chat_events_path(&session_dir);
  for line in chat_events::read_lines(&events_path, LineRule::Lines).ok()? {
    let event = line.ok()?.decode().ok()?;
    let object = event.as_object()?;
    let written_after_send = object
      .get("timestamp")
      .and_then(Value::as_str)
      .and_then(|timestamp| UtcDateTime::parse(timestamp).ok())
      .is_some_and(|timestamp| timestamp >= sent_at);
    if object.get("type").and_then(Value::as_str) == Some(event_types::TASK_REPARENTED)
      && object.get("task_parent_id").and_then(Value::as_str) == Some(parent_id)
      && object.get("caller_session_id").and_then(Value::as_str) == caller_session_id
      && written_after_send
    {
      let event_id = object.get("id").cloned().unwrap_or(Value::Null);
      return Some(json!({ "session_id": task_id, "task_parent_id": parent_id, "reparented_event_id": event_id }));
    }
  }
  None
}

fn dialog(args: &DialogArgs) -> Result<(), CliExit> {
  if args.turns.is_some_and(|turns| turns < 1) {
    return Err(CliExit::usage("--turns must be an integer of at least 1"));
  }
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  // The bootstrap payload is the newest page the chat UI opens with; its oldest_message_ordinal is
  // the cursor backward paging starts from, in the events endpoint's own cursor space.
  let boot = client::get(&route::fill(session_routes::SESSION_BOOTSTRAP_PATH, &[session_id.as_str()]), None)?;
  let mut page_starts = vec![
    field(&boot, "oldest_message_ordinal")?
      .as_i64()
      .ok_or_else(|| CliExit::error("oldest_message_ordinal must be an integer"))?,
  ];
  let mut pages: Vec<Vec<Value>> = vec![field(&boot, "messages")?.as_array().cloned().unwrap_or_default()];
  let mut cursor = page_starts[0];
  let mut has_more = text_repr::is_truthy(field(&boot, "has_more")?);
  let mut separators = count_separators(&pages[0]);
  // --turns stops once the fetched pages hold one separator more than the requested turn count —
  // everything after the (N+1)-th last separator is then the last N complete turns plus the turn
  // in progress.
  let turn_budget = args.turns.map(|turns| turns + 1);
  while has_more && turn_budget.is_none_or(|budget| (separators as i64) < budget) {
    let page = client::get(
      &route::fill(session_routes::SESSION_EVENTS_PATH, &[session_id.as_str()]),
      Some(vec![("before".to_string(), cursor.to_string()), ("limit".to_string(), DIALOG_PAGE_LIMIT.to_string())]),
    )?;
    let messages = field(&page, "messages")?.as_array().cloned().unwrap_or_default();
    separators += count_separators(&messages);
    pages.push(messages);
    let next_before =
      field(&page, "next_before")?.as_i64().ok_or_else(|| CliExit::error("next_before must be an integer"))?;
    page_starts.push(next_before);
    cursor = next_before;
    has_more = text_repr::is_truthy(field(&page, "has_more")?);
  }

  let mut cut = 0usize;
  if let Some(turns) = args.turns {
    let ascending: Vec<&Value> = pages.iter().rev().flatten().collect();
    let separator_positions: Vec<usize> = ascending
      .iter()
      .enumerate()
      .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("separator"))
      .map(|(index, _)| index)
      .collect();
    // Fewer than N+1 separators means the whole fetched history is N turns or less: print all of
    // it, cut 0 included.
    if separator_positions.len() > turns as usize {
      cut = separator_positions[separator_positions.len() - turns as usize - 1] + 1;
    }
  }

  let mut printed = 0usize;
  let mut blocks: Vec<String> = Vec::new();
  let mut first: Option<(i64, usize, Value)> = None;
  for (page_start, page) in page_starts.iter().rev().zip(pages.iter().rev()) {
    for (position, message) in page.iter().enumerate() {
      if printed < cut {
        printed += 1;
        continue;
      }
      let Some(block) = format_message_block(message) else { continue };
      // Python re-takes the header's anchor while the first block's event index is None.
      if first.as_ref().is_none_or(|(_, _, event_index)| event_index.is_null()) {
        first = Some((*page_start, position, message.get("event_index").cloned().unwrap_or(Value::Null)));
      }
      blocks.push(block);
      printed += 1;
    }
  }
  if blocks.is_empty() {
    return Ok(());
  }
  if let Some(turns) = args.turns {
    let unit = earlier_unit(&boot)?;
    let (page_start, position, event_index) =
      first.expect("the first printed message is recorded with the first block");
    blocks.insert(
      0,
      format!(
        "last {turns} turns · from event {} · {} earlier {unit}",
        text_repr::value_str(&event_index),
        page_start + position as i64
      ),
    );
  }
  println!("{}", blocks.join("\n\n"));
  Ok(())
}

/// `_earlier_unit`: the bootstrap session's `archive_offset` says which cursor space the pages
/// use; without it the unit is a guess, so the command stops instead of printing one.
fn earlier_unit(boot: &Value) -> Result<&'static str, CliExit> {
  let offset = boot.get("session").and_then(|session| session.get("archive_offset")).and_then(Value::as_i64);
  match offset {
    Some(offset) => Ok(if offset > 0 { "events" } else { "messages" }),
    None => Err(CliExit::error(
      "bootstrap payload lacks the session's archive_offset; refusing to guess whether --turns counts messages or events",
    )),
  }
}

fn count_separators(page: &[Value]) -> usize {
  page.iter().filter(|message| message.get("role").and_then(Value::as_str) == Some("separator")).count()
}

/// One block per chat message with a body or at least one tool call; `None` when the message has
/// neither (a turn divider prints nothing).
fn format_message_block(message: &Value) -> Option<String> {
  let body = match message.get("content") {
    Some(Value::String(text)) => text.clone(),
    Some(Value::Null) | None => String::new(),
    Some(other) => text_repr::json_dumps(other, &JSON_ENSURE_ASCII_FALSE),
  };
  let tools = message.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
  if body.is_empty() && tools.is_empty() {
    return None;
  }
  let role = text_repr::value_str(message.get("role").unwrap_or(&Value::Null));
  let timestamp = message
    .get("timestamp")
    .filter(|timestamp| text_repr::is_truthy(timestamp))
    .map(text_repr::value_str)
    .unwrap_or_default();
  let event_index = text_repr::value_str(message.get("event_index").unwrap_or(&Value::Null));
  let mut lines = vec![format!("## {role} · {timestamp} · event {event_index}")];
  if !body.is_empty() {
    lines.push(body);
  }
  lines.extend(tools.iter().map(|tool| {
    let name = text_repr::value_str(tool.get("name").unwrap_or(&Value::Null));
    format!("$ {name} {}", tool_input_first_line(tool))
  }));
  Some(lines.join("\n"))
}

/// One tool call's natural one-line form: Bash carries its command, every other tool a compact
/// JSON rendering of the input; only the first line survives.
fn tool_input_first_line(tool: &Value) -> String {
  let rendered = match tool.get("input") {
    Some(Value::Object(input)) => {
      let is_bash = matches!(tool.get("name").and_then(Value::as_str), Some("Bash") | Some("bash"));
      match (is_bash, input.get("command")) {
        (true, Some(Value::String(command))) => command.clone(),
        _ => text_repr::json_dumps(&Value::Object(input.clone()), &JSON_ENSURE_ASCII_FALSE),
      }
    }
    Some(Value::String(text)) => text.clone(),
    _ => String::new(),
  };
  match rendered.split_once('\n') {
    Some((first, _)) => first.to_string(),
    None => rendered,
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use charliebot_tasks::completion::PENDING_UNTIL_USER_VIEW;
  use tempfile::TempDir;

  use super::*;

  const TASK_ID: &str = "4f1c2a0e-7b1d-5c3e-9a2f-1d2e3f4a5b6c";
  const REQUEST_ID: &str = "archive-request-1";

  /// A CHARLIEBOT_HOME whose one session holds the given metadata document and chat events.
  fn home_with_session(metadata: &Value, events: &[Value]) -> TempDir {
    let home = tempfile::tempdir().expect("a temp home");
    let session_dir = config::sessions_dir_in(home.path()).join(TASK_ID);
    fs::create_dir_all(layout::data_dir(&session_dir)).expect("the session's data directory is created");
    fs::write(layout::metadata_path(&session_dir), metadata.to_string()).expect("the metadata is written");
    let lines: Vec<String> = events.iter().map(Value::to_string).collect();
    fs::write(layout::chat_events_path(&session_dir), lines.join("\n") + "\n").expect("the events are written");
    home
  }

  /// A session metadata document whose task record names `approver` and whose last reply is unread.
  fn metadata(approver: &str) -> Value {
    json!({
      "id": TASK_ID,
      "name": "manager",
      "profile": "manager",
      "has_unread": true,
      "task": {"approver": approver},
    })
  }

  fn close_requested() -> Value {
    json!({"type": event_types::TASK_CLOSE_REQUESTED, "id": "close-request-1", "request_id": REQUEST_ID})
  }

  #[test]
  fn a_lost_archive_of_a_user_approved_task_reads_the_user_view_pending_answer() {
    let home = home_with_session(&metadata("user"), &[close_requested()]);

    let readback = find_local_task_close(home.path(), TASK_ID, REQUEST_ID);

    assert_eq!(readback, Ok(Some(json!({"archived": [], "pending": PENDING_UNTIL_USER_VIEW}))));
  }

  #[test]
  fn a_lost_archive_of_a_parent_approved_task_keeps_pending_run_finish() {
    let home = home_with_session(&metadata("parent"), &[close_requested()]);

    let readback = find_local_task_close(home.path(), TASK_ID, REQUEST_ID);

    assert_eq!(
      readback,
      Ok(Some(json!({"session_id": TASK_ID, "request_id": REQUEST_ID, "status": "pending_run_finish"})))
    );
  }

  #[test]
  fn a_closed_fact_outranks_the_deferral_written_before_it() {
    let closed_id = control_events::stable_close_event_id(TASK_ID, REQUEST_ID);
    let closed = json!({"type": event_types::TASK_CLOSED, "id": closed_id});
    let home = home_with_session(&metadata("user"), &[close_requested(), closed]);

    let readback = find_local_task_close(home.path(), TASK_ID, REQUEST_ID);

    assert_eq!(
      readback,
      Ok(Some(json!({"session_id": TASK_ID, "task_state": "completed", "closed_event_id": closed_id})))
    );
  }

  #[test]
  fn an_approver_the_metadata_cannot_decode_fails_loud() {
    let home = home_with_session(&metadata("owner"), &[close_requested()]);

    let readback = find_local_task_close(home.path(), TASK_ID, REQUEST_ID);

    assert!(readback.is_err(), "an unknown approver must not pick either answer: {readback:?}");
  }
}
