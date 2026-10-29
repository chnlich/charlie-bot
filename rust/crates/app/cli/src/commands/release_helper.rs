//! The release scripts' helper verbs: `charliebot release-helper <name>`.
//!
//! `scripts/release/trial.sh` and `scripts/release/switch-rehearsal.sh` ran these as embedded Python programs, and
//! `scripts/release/sweep-check.py` was the third. Each verb keeps its arguments, its output format and its exit code.
//! Both scripts run the `charliebot` of the release under test, so a release's checks and the server they check come
//! from one commit.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::route;
use charliebot_core::cgroup;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::event_types;
use charliebot_core::logline::{self, Row};
use charliebot_core::proc_identity;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_session_api::api;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::layout;
use charliebot_store::metadata;
use charliebot_store::run_records;
use regex::Regex;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::macros::format_description;

/// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`: websocket frames and their signature list.
const COMPACT_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// `json.dumps` with every option at its default: the seeded run record and a chat message body.
const PYTHON_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// The startup sweep's removal event, whose `names` list the sweep check reads.
const SWEEP_REMOVED: &str = "session_cgroup_sweep_removed";
/// The startup event that states the session cgroup limits are on.
const CGROUP_ENABLED: &str = "session_cgroup_enabled";
/// The event that names each session cgroup the server created.
const CGROUP_CREATED: &str = "session_cgroup_created";
/// The display state of a run that finished successfully.
const RUN_STATE_SUCCESS: &str = "success";

/// The verbs and the number of arguments each takes; `--help` prints the list.
const VERBS: &[(&str, &str)] = &[
  ("assistant-reply", "EVENTS_PATH EXPECTED_WORD"),
  ("backend-option", "CONFIG_PATH"),
  ("cgroup-clean", "CGROUP_PATH TREE"),
  ("cgroup-enabled", "SERVER_LOG"),
  ("child-workers", "SESSIONS_DIR PARENT_ID"),
  ("created-cgroups", "SERVER_LOG"),
  ("credential-key", "CREDENTIALS_PATH SECTION"),
  ("delegation-error", "SESSIONS_DIR MANAGER_ID"),
  ("event", "SERVER_LOG EVENT_NAME"),
  ("event-type-count", "EVENTS_PATH EVENT_TYPE"),
  ("free-port", "MIN MAX"),
  ("json-field", "JSON_PATH FIELD"),
  ("leftovers", "TREE SELF_PID"),
  ("listener-pid", "PORT TREE"),
  ("pid-start", "PID"),
  ("redact-stdin", ""),
  ("run-contract", "RUNS_PATH EVENTS_PATH MODE [FIRST_USER_ID] [DELEGATION_ID]"),
  ("run-page", "RUNS_PATH"),
  ("run-summary", "RUNS_PATH"),
  ("runs-success", "RUNS_PATH"),
  ("seed-run", "SESSIONS_DIR SESSION PID PID_START BACKEND RUN_ID"),
  ("self-test", ""),
  ("session-id", "RESPONSE_PATH"),
  ("session-ids", "SESSIONS_DIR"),
  ("sweep-check", "SERVER_LOG SESSIONS_DIR"),
  ("version-sha", "RESPONSE_PATH"),
  ("websocket", "BASE_URL SESSION CREDENTIALS_PATH OUTPUT_PATH"),
  ("write-message", "PROMPT_PATH OUTPUT_PATH"),
];

/// Dispatch `release-helper <name> [args...]`. The scripts redirect most output and read the exit code, so a failure
/// prints its message to stderr and exits 1, and `--help` exits 0 without touching anything.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let Some(name) = args.first().map(|argument| argument.to_string_lossy().into_owned()) else {
    print_usage();
    exit(0);
  };
  if name == "-h" || name == "--help" || name == "help" {
    print_usage();
    exit(0);
  }
  let rest: Vec<String> = args[1..].iter().map(|argument| argument.to_string_lossy().into_owned()).collect();
  match dispatch(&name, &rest) {
    Ok(code) => exit(code),
    Err(message) => {
      eprintln!("{message}");
      exit(1);
    }
  }
}

fn print_usage() {
  println!("usage: charliebot release-helper <name> [args...]");
  println!();
  for (name, arguments) in VERBS {
    if arguments.is_empty() {
      println!("  {name}");
    } else {
      println!("  {name} {arguments}");
    }
  }
}

fn dispatch(name: &str, args: &[String]) -> Result<i32, String> {
  match name {
    "assistant-reply" => assistant_reply(args),
    "backend-option" => backend_option(args),
    "cgroup-clean" => cgroup_clean(args),
    "cgroup-enabled" => cgroup_enabled(args),
    "child-workers" => child_workers(args),
    "created-cgroups" => created_cgroups(args),
    "credential-key" => credential_key(args),
    "delegation-error" => delegation_error(args),
    "event" => event(args),
    "event-type-count" => event_type_count(args),
    "free-port" => free_port(args),
    "json-field" => json_field(args),
    "leftovers" => leftovers(args),
    "listener-pid" => listener_pid(args),
    "pid-start" => pid_start(args),
    "redact-stdin" => redact_stdin(),
    "run-contract" => run_contract(args),
    "run-page" => run_page(args),
    "run-summary" => run_summary_command(args),
    "runs-success" => runs_success(args),
    "seed-run" => seed_run(args),
    "self-test" => self_test(args),
    "session-id" => session_id(args),
    "session-ids" => session_ids(args),
    "sweep-check" => sweep_check(args),
    "version-sha" => version_sha(args),
    "websocket" => websocket(args),
    "write-message" => write_message(args),
    other => Err(format!("unknown helper command: {other}")),
  }
}

/// The exact arguments of one verb, or the argument-count error Python's helpers printed.
fn arguments<'a, const N: usize>(args: &'a [String], name: &str) -> Result<[&'a str; N], String> {
  if args.len() != N {
    return Err(format!("{name}: wrong number of arguments"));
  }
  Ok(std::array::from_fn(|index| args[index].as_str()))
}

fn load_json(path: &str) -> Result<Value, String> {
  let text = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
  serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))
}

/// CPython truthiness of a value `json.loads` produced.
fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(members) => !members.is_empty(),
  }
}

/// `value or ""`, as `str(...)`.
fn or_default(value: Option<&Value>) -> String {
  match value {
    Some(value) if truthy(value) => pyfmt::python_str(value),
    _ => String::new(),
  }
}

/// `str(run.get("state") or "")`.
fn run_state(run: &Value) -> String {
  or_default(run.get("state"))
}

/// A file's text with invalid UTF-8 replaced, as Python's `errors="replace"` reads it.
fn read_text_lossy(path: &str) -> Result<String, String> {
  let bytes = fs::read(path).map_err(|error| format!("{path}: {error}"))?;
  Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The event log lines that end in a newline; a line the server is still writing is not complete.
fn complete_lines(path: &str) -> Result<Vec<String>, String> {
  if !Path::new(path).is_file() {
    return Ok(Vec::new());
  }
  let text = read_text_lossy(path)?;
  let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
  lines.pop();
  Ok(lines)
}

/// Every non-blank line of a JSON-lines file, parsed; a missing file is empty.
fn event_rows(path: &str) -> Result<Vec<Value>, String> {
  if !Path::new(path).is_file() {
    return Ok(Vec::new());
  }
  let text = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
  let mut rows = Vec::new();
  for line in text.lines() {
    if line.trim().is_empty() {
      continue;
    }
    rows.push(serde_json::from_str(line).map_err(|error| format!("{path}: {error}"))?);
  }
  Ok(rows)
}

fn emit_nul(fields: &[String]) -> Result<(), String> {
  let joined = fields.join("\0");
  let mut stdout = io::stdout().lock();
  stdout.write_all(joined.as_bytes()).and_then(|()| stdout.write_all(b"\0")).map_err(|error| error.to_string())
}

// ---- the trial's checks ---------------------------------------------------------------------------------------------

fn session_id(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "session-id")?;
  match load_json(path)?.get("id") {
    Some(Value::String(id)) if !id.is_empty() => {
      println!("{id}");
      Ok(0)
    }
    _ => Err("session response has no id".to_string()),
  }
}

fn run_summary_command(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "run-summary")?;
  let fields = run_summary_fields(&load_json(path)?)?;
  emit_nul(&fields)?;
  Ok(0)
}

/// The eight NUL-separated summary fields of a runs response.
fn run_summary_fields(value: &Value) -> Result<Vec<String>, String> {
  let items = value.get("items").and_then(Value::as_array).ok_or("run response has no items list")?;
  let item = items.last().cloned().unwrap_or_else(|| json!({}));
  let mut fields = vec![items.len().to_string()];
  for key in ["id", "state", "pid", "repo_path", "worktree_path", "branch_name"] {
    fields.push(or_default(item.get(key)));
  }
  fields.push(items.iter().map(run_state).collect::<Vec<_>>().join(","));
  Ok(fields)
}

/// Whether one page of a runs response holds a queued or running run, then that page's next cursor.
fn run_page_fields(value: &Value) -> Result<(bool, String), String> {
  let items = value.get("items").and_then(Value::as_array).ok_or("run response has no items list")?;
  let active = items.iter().any(|item| matches!(run_state(item).as_str(), "queued" | "running"));
  Ok((active, or_default(value.get("next_cursor"))))
}

fn run_page(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "run-page")?;
  let (active, cursor) = run_page_fields(&load_json(path)?)?;
  println!("{}", i32::from(active));
  println!("{cursor}");
  Ok(0)
}

fn json_field(args: &[String]) -> Result<i32, String> {
  let [path, name] = arguments::<2>(args, "json-field")?;
  match load_json(path)?.get(name) {
    Some(Value::String(value)) if !value.is_empty() => {
      println!("{value}");
      Ok(0)
    }
    _ => Err(format!("JSON field {} is missing or not a non-empty string", pyfmt::str_repr(name))),
  }
}

fn run_contract(args: &[String]) -> Result<i32, String> {
  if !(3..=5).contains(&args.len()) {
    return Err("run-contract: wrong number of arguments".to_string());
  }
  let runs = load_json(&args[0])?;
  let items = runs.get("items").and_then(Value::as_array).ok_or("run response has no items list")?;
  let events = event_rows(&args[1])?;
  let first_user_id = args.get(3).cloned().unwrap_or_default();
  let delegation_id = args.get(4).cloned().unwrap_or_default();
  let (ok, summary) = run_contract_value(items, &events, &args[2], &first_user_id, &delegation_id)?;
  println!("{summary}");
  Ok(if ok { 0 } else { 1 })
}

#[derive(Default)]
struct RunCounts {
  user_message: usize,
  delegation_message: usize,
  worker_task_report: usize,
  nudge: usize,
  status_reminder: usize,
  other: usize,
  mixed: usize,
}

impl RunCounts {
  fn bump(&mut self, class: &str) {
    match class {
      "user_message" => self.user_message += 1,
      "delegation_message" => self.delegation_message += 1,
      "worker_task_report" => self.worker_task_report += 1,
      "nudge" => self.nudge += 1,
      "status_reminder" => self.status_reminder += 1,
      other => panic!("unknown run class {other}"),
    }
  }
}

fn event_object_id(event: &Value) -> String {
  match event.get("id") {
    None | Some(Value::Null) => String::new(),
    Some(value) => pyfmt::python_str(value),
  }
}

/// The run contract of one session's runs against its chat events. Returns whether it holds and the line the trial
/// prints for a human to read.
fn run_contract_value(
  items: &[Value],
  events: &[Value],
  mode: &str,
  first_user_id: &str,
  delegation_id: &str,
) -> Result<(bool, String), String> {
  let mut events_by_id: BTreeMap<String, &Value> = BTreeMap::new();
  for event in events {
    let id = event_object_id(event);
    if !id.is_empty() {
      events_by_id.insert(id, event);
    }
  }
  let expected_user_ids: Vec<String> =
    [first_user_id, delegation_id].into_iter().filter(|id| !id.is_empty()).map(str::to_string).collect();
  for expected in &expected_user_ids {
    let is_user = events_by_id
      .get(expected)
      .is_some_and(|event| event.get("type").and_then(Value::as_str) == Some(event_types::USER));
    if !is_user {
      return Err(format!("expected user input event is missing from {}: {expected}", layout::CHAT_EVENTS_FILE_NAME));
    }
  }

  let mut nudge_by_id: BTreeMap<String, String> = BTreeMap::new();
  let mut report_ids: BTreeSet<String> = BTreeSet::new();
  let mut reminder_ids: BTreeSet<String> = BTreeSet::new();
  for event in events {
    let id = event_object_id(event);
    let kind = event.get("type").and_then(Value::as_str);
    // The server sends one session status reminder after a turn that ended without a status set or confirm.
    if kind == Some(event_types::AGENT_MESSAGE)
      && event.get(event_types::WEB_STATUS_REMINDER).is_some_and(truthy)
      && !id.is_empty()
    {
      reminder_ids.insert(id.clone());
    }
    if kind == Some(event_types::AGENT_MESSAGE)
      && let Some(parent) =
        event.get(event_types::WEB_REPLY).and_then(Value::as_object).and_then(|reply| reply.get(event_types::NUDGE_OF))
    {
      let parent = pyfmt::python_str(parent);
      if expected_user_ids.contains(&parent) {
        nudge_by_id.insert(id.clone(), parent);
      }
    }
    if kind == Some(event_types::CHILD_REPORT) && !id.is_empty() {
      report_ids.insert(id);
    }
  }

  let mut counts = RunCounts::default();
  let mut nudge_counts: Vec<(String, usize)> = expected_user_ids.iter().map(|id| (id.clone(), 0)).collect();
  let mut states = Vec::new();
  for run in items {
    let input_ids: Vec<String> = match run.get(event_types::INPUT_EVENT_IDS) {
      Some(Value::Array(ids)) => ids.iter().map(pyfmt::python_str).collect(),
      _ => Vec::new(),
    };
    let mut run_classes: BTreeSet<&'static str> = BTreeSet::new();
    let mut matched_users: BTreeSet<&str> = BTreeSet::new();
    for input_id in &input_ids {
      if !first_user_id.is_empty() && input_id == first_user_id {
        run_classes.insert("user_message");
      }
      if !delegation_id.is_empty() && input_id == delegation_id {
        run_classes.insert("delegation_message");
      }
      if report_ids.contains(input_id) {
        run_classes.insert("worker_task_report");
      }
      if let Some(parent) = nudge_by_id.get(input_id) {
        run_classes.insert("nudge");
        matched_users.insert(parent);
      }
      // One follow-up message can carry both the web reply nudge and the status reminder; it is one nudge run.
      if reminder_ids.contains(input_id) && !nudge_by_id.contains_key(input_id) {
        run_classes.insert("status_reminder");
      }
    }
    for class in &run_classes {
      counts.bump(class);
    }
    for user in matched_users {
      if let Some(entry) = nudge_counts.iter_mut().find(|(id, _)| id.as_str() == user) {
        entry.1 += 1;
      }
    }
    if run_classes.is_empty() {
      counts.other += 1;
    }
    if run_classes.len() > 1 {
      counts.mixed += 1;
    }
    states.push(run_state(run));
  }

  let all_success = states.iter().all(|state| state == RUN_STATE_SUCCESS);
  // Each turn gets at most one status reminder, so the reminder runs never outnumber the other runs.
  let reminders = counts.status_reminder as i64;
  let reminders_ok = reminders <= items.len() as i64 - reminders;
  let contract_ok = match mode {
    "check2" | "check5" => {
      !first_user_id.is_empty()
        && counts.user_message == 1
        && counts.delegation_message == 0
        && counts.worker_task_report == 0
        && counts.nudge <= 1
        && counts.other == 0
        && counts.mixed == 0
        && reminders_ok
        && all_success
    }
    "check4" => {
      !first_user_id.is_empty()
        && !delegation_id.is_empty()
        && counts.user_message == 1
        && counts.delegation_message == 1
        && counts.worker_task_report == 1
        && counts.other == 0
        && counts.mixed == 0
        && nudge_counts.iter().all(|(_, count)| *count <= 1)
        && reminders_ok
        && all_success
    }
    other => return Err(format!("unknown run contract mode: {other}")),
  };
  let nudge_by_user = if nudge_counts.is_empty() {
    "none".to_string()
  } else {
    nudge_counts.iter().map(|(id, count)| format!("{id}:{count}")).collect::<Vec<_>>().join(",")
  };
  let states_summary = if states.is_empty() { "none".to_string() } else { states.join(",") };
  let summary = format!(
    "run_classes total={} user_message={} delegation_message={} worker_task_report={} nudge={} \
nudge_by_user={} status_reminder={} other={} mixed={} states={} all_success={}",
    items.len(),
    counts.user_message,
    counts.delegation_message,
    counts.worker_task_report,
    counts.nudge,
    nudge_by_user,
    counts.status_reminder,
    counts.other,
    counts.mixed,
    states_summary,
    i32::from(all_success)
  );
  Ok((contract_ok, summary))
}

fn runs_success(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "runs-success")?;
  let value = load_json(path)?;
  let items = value.get("items").and_then(Value::as_array).ok_or("run response has no items list")?;
  let states: Vec<String> = items.iter().map(run_state).collect();
  let ok = !items.is_empty() && states.iter().all(|state| state == RUN_STATE_SUCCESS);
  println!(
    "runs={} states={} all_success={}",
    items.len(),
    if states.is_empty() { "none".to_string() } else { states.join(",") },
    i32::from(ok)
  );
  Ok(if ok { 0 } else { 1 })
}

fn child_workers(args: &[String]) -> Result<i32, String> {
  let [sessions, parent_id] = arguments::<2>(args, "child-workers")?;
  let mut directories = directory_entries(sessions)?;
  directories.sort();
  let mut rows = Vec::new();
  for directory in directories {
    if !directory.is_dir() {
      continue;
    }
    let metadata_path = layout::metadata_path(&directory);
    if !metadata_path.is_file() {
      continue;
    }
    let metadata =
      metadata::read_metadata_value(&metadata_path).map_err(|error| format!("{}: {error}", metadata_path.display()))?;
    if metadata.get("task_parent_id").and_then(Value::as_str) != Some(parent_id) {
      continue;
    }
    if metadata.get("profile").and_then(Value::as_str) != Some("worker") {
      continue;
    }
    let task = metadata.get("task").filter(|value| truthy(value)).cloned().unwrap_or_else(|| json!({}));
    let id = match or_default(metadata.get("id")) {
      id if !id.is_empty() => id,
      _ => directory.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    rows.push(vec![id, or_default(task.get("task_type")), or_default(task.get("repo_path"))]);
  }
  for row in &rows {
    emit_nul(row)?;
  }
  Ok(0)
}

fn directory_entries(directory: &str) -> Result<Vec<PathBuf>, String> {
  let entries = fs::read_dir(directory).map_err(|error| format!("{directory}: {error}"))?;
  entries.map(|entry| entry.map(|entry| entry.path()).map_err(|error| error.to_string())).collect()
}

fn session_ids(args: &[String]) -> Result<i32, String> {
  let [sessions] = arguments::<1>(args, "session-ids")?;
  let mut directories = directory_entries(sessions)?;
  directories.sort();
  for directory in directories {
    let metadata_path = layout::metadata_path(&directory);
    if !directory.is_dir() || !metadata_path.is_file() {
      continue;
    }
    let metadata =
      metadata::read_metadata_value(&metadata_path).map_err(|error| format!("{}: {error}", metadata_path.display()))?;
    if let Some(Value::String(id)) = metadata.get("id")
      && !id.is_empty()
    {
      println!("{id}");
    }
  }
  Ok(0)
}

fn content_text(value: &Value, parts: &mut Vec<String>) {
  match value {
    Value::String(text) => parts.push(text.clone()),
    Value::Array(items) => {
      for item in items {
        content_text(item, parts);
      }
    }
    Value::Object(members) => match members.get("text") {
      Some(Value::String(text)) => parts.push(text.clone()),
      _ => {
        if let Some(content) = members.get("content") {
          content_text(content, parts);
        }
      }
    },
    _ => {}
  }
}

/// `.strip().strip("` ").rstrip(".! ")`.
fn normalize_reply(text: &str) -> &str {
  let text = text.trim().trim_matches(['`', ' ']);
  text.trim_end_matches(['.', '!', ' '])
}

fn assistant_reply(args: &[String]) -> Result<i32, String> {
  let [path, expected] = arguments::<2>(args, "assistant-reply")?;
  let mut found = false;
  for event in event_rows(path)? {
    let message = event.get("message");
    let mut role = event.get("type").and_then(Value::as_str) == Some(event_types::ASSISTANT)
      || event.get("role").and_then(Value::as_str) == Some(event_types::ASSISTANT);
    if message.and_then(Value::as_object).and_then(|message| message.get("role")).and_then(Value::as_str)
      == Some(event_types::ASSISTANT)
    {
      role = true;
    }
    if !role {
      continue;
    }
    let content = match message {
      Some(Value::Object(_)) => message.and_then(|message| message.get("content")),
      _ => event.get("content"),
    };
    let mut parts = Vec::new();
    if let Some(content) = content {
      content_text(content, &mut parts);
    }
    for text in parts {
      let normalized = normalize_reply(&text);
      if normalized == expected && !normalized.contains(' ') && !normalized.contains('\n') {
        found = true;
      }
    }
  }
  println!("{}", i32::from(found));
  Ok(0)
}

fn event_type_count(args: &[String]) -> Result<i32, String> {
  let [path, wanted] = arguments::<2>(args, "event-type-count")?;
  let count =
    event_rows(path)?.iter().filter(|event| event.get("type").and_then(Value::as_str) == Some(wanted)).count();
  println!("{count}");
  Ok(0)
}

fn strings_in<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
  match value {
    Value::String(text) => found.push(text),
    Value::Array(items) => {
      for item in items {
        strings_in(item, found);
      }
    }
    Value::Object(members) => {
      for item in members.values() {
        strings_in(item, found);
      }
    }
    _ => {}
  }
}

fn timeout_error_line(text: &str) -> Option<String> {
  text.lines().map(str::trim).find(|line| line.contains("timed out after") && line.contains("git")).map(str::to_string)
}

/// The chat event logs a delegation's error can be in: the manager's, then each session's, in name order.
fn chat_event_paths(sessions: &Path, manager_id: &str) -> Result<Vec<PathBuf>, String> {
  let mut paths = vec![layout::chat_events_path(&sessions.join(manager_id))];
  let mut own = if sessions.is_dir() { directory_entries(&sessions.to_string_lossy())? } else { Vec::new() };
  own.sort();
  paths.extend(own.iter().map(|directory| layout::chat_events_path(directory)));
  Ok(paths)
}

/// The first git timeout that a chat event log names, read lossily.
fn chat_timeout_text(path: &Path) -> Result<Option<String>, String> {
  let lines =
    chat_events::read_lines(path, LineRule::LossyLines).map_err(|error| format!("{}: {error}", path.display()))?;
  for line in lines {
    let line = line.map_err(|error| format!("{}: {error}", path.display()))?;
    if !line.text.contains("timed out after") {
      continue;
    }
    if let Some(found) = event_timeout(&line.text, &line.decode()) {
      return Ok(Some(found));
    }
  }
  Ok(None)
}

/// The git timeout that one event line names: a line that does not parse is its own candidate, and a parsed line gives
/// each of its strings that names a timeout. The first candidate with a timeout line wins, joined and cut to 400 chars.
fn event_timeout(line: &str, parsed: &Result<Value, serde_json::Error>) -> Option<String> {
  let candidates: Vec<&str> = match parsed {
    Ok(value) => {
      let mut strings = Vec::new();
      strings_in(value, &mut strings);
      strings.into_iter().filter(|text| text.contains("timed out after")).collect()
    }
    Err(_) => vec![line],
  };
  candidates.into_iter().find_map(timeout_error_line).map(|found| {
    let joined = found.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(400).collect()
  })
}

/// The first git timeout the trial server reported for the delegation, as its sessions recorded it.
fn delegation_error_text(sessions: &Path, manager_id: &str) -> Result<Option<String>, String> {
  let chat_paths = chat_event_paths(sessions, manager_id)?;
  let mut nested = Vec::new();
  collect_named_files(sessions, run_records::RUN_EVENTS_NAME, &mut nested)?;
  nested.sort();

  let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
  for path in chat_paths {
    if !seen.insert(path.clone()) || !path.is_file() {
      continue;
    }
    if let Some(found) = chat_timeout_text(&path)? {
      return Ok(Some(found));
    }
  }
  for path in nested {
    if !seen.insert(path.clone()) || !path.is_file() {
      continue;
    }
    let lines = run_records::read_event_lines_lossy(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    for line in &lines {
      if !line.contains("timed out after") {
        continue;
      }
      if let Some(found) = event_timeout(line, &run_records::decode_event_line(line)) {
        return Ok(Some(found));
      }
    }
  }
  Ok(None)
}

fn collect_named_files(root: &Path, name: &str, found: &mut Vec<PathBuf>) -> Result<(), String> {
  if !root.is_dir() {
    return Ok(());
  }
  for entry in fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))? {
    let path = entry.map_err(|error| error.to_string())?.path();
    if path.is_dir() {
      collect_named_files(&path, name, found)?;
    } else if path.file_name().is_some_and(|file_name| file_name == name) {
      found.push(path);
    }
  }
  Ok(())
}

fn delegation_error(args: &[String]) -> Result<i32, String> {
  let [sessions, manager_id] = arguments::<2>(args, "delegation-error")?;
  match delegation_error_text(Path::new(sessions), manager_id)? {
    Some(line) => {
      println!("{line}");
      Ok(0)
    }
    None => Ok(1),
  }
}

/// The rows of a server log: each line `logline` reads, in file order. Other lines are skipped.
fn log_rows(path: &str) -> Result<Vec<Row>, String> {
  let text = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
  Ok(text.lines().filter_map(logline::parse).collect())
}

fn field_or_empty(row: &Row, key: &str) -> String {
  row.field(key).unwrap_or_default().to_string()
}

fn cgroup_enabled(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "cgroup-enabled")?;
  for row in log_rows(path)? {
    if row.event != CGROUP_ENABLED {
      continue;
    }
    emit_nul(&[
      "enabled".to_string(),
      field_or_empty(&row, "memory_max_mb"),
      field_or_empty(&row, "swap_max_mb"),
      field_or_empty(&row, "tasks_max"),
    ])?;
    return Ok(0);
  }
  Ok(1)
}

fn created_cgroups(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "created-cgroups")?;
  for row in log_rows(path)? {
    if let Some(cgroup_path) = row.field("path")
      && row.event == CGROUP_CREATED
    {
      println!("{cgroup_path}");
    }
  }
  Ok(0)
}

// ---- the log check the trial and the rehearsal share ----------------------------------------------------------------

fn session_cgroup_names(sessions: &str) -> Result<BTreeSet<String>, String> {
  let root = Path::new(sessions);
  if !root.is_dir() {
    return Ok(BTreeSet::new());
  }
  let mut names = BTreeSet::new();
  for entry in fs::read_dir(root).map_err(|error| format!("{sessions}: {error}"))? {
    let entry = entry.map_err(|error| error.to_string())?;
    if !entry.path().is_dir() {
      continue;
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    names.insert(cgroup::session_cgroup_name(&name));
  }
  Ok(names)
}

/// The names a `session_cgroup_sweep_removed` line removed, or `None` for any other line.
fn sweep_removed_names(line: &str) -> Result<Option<Vec<String>>, String> {
  let Some(row) = logline::parse(line) else { return Ok(None) };
  if row.event != SWEEP_REMOVED {
    return Ok(None);
  }
  // The server logs `names = ?removed`: the Debug text of the list, which a line holds as a quoted JSON array.
  let encoded = row.field("names").ok_or("the sweep line has no names")?;
  match serde_json::from_str::<Value>(encoded).map_err(|error| error.to_string())? {
    Value::Array(items) => Ok(Some(items.iter().map(pyfmt::python_str).collect())),
    _ => Err("the sweep line's names are not a list".to_string()),
  }
}

fn sweep_check(args: &[String]) -> Result<i32, String> {
  let [log, sessions] = arguments::<2>(args, "sweep-check")?;
  let own = session_cgroup_names(sessions)?;
  let text = read_text_lossy(log)?;
  let mut lines = 0;
  let mut removed = Vec::new();
  // Complete lines only: a line the server is still writing has no newline yet.
  let parts: Vec<&str> = text.split('\n').collect();
  for line in &parts[..parts.len().saturating_sub(1)] {
    if let Some(names) = sweep_removed_names(line)? {
      lines += 1;
      removed.extend(names);
    }
  }
  let mut foreign: Vec<String> = removed.iter().filter(|name| !own.contains(*name)).cloned().collect();
  let mut removed: Vec<String> = removed;
  foreign.sort();
  removed.sort();
  println!(
    "sweep_removed_lines={} removed={} foreign={}",
    lines,
    if removed.is_empty() { "none".to_string() } else { removed.join(",") },
    if foreign.is_empty() { "none".to_string() } else { foreign.join(",") }
  );
  Ok(if foreign.is_empty() { 0 } else { 1 })
}

// ---- the rehearsal's checks -----------------------------------------------------------------------------------------

fn event(args: &[String]) -> Result<i32, String> {
  let [log, wanted] = arguments::<2>(args, "event")?;
  for line in complete_lines(log)? {
    if logline::parse(&line).is_some_and(|row| row.event == wanted) {
      return Ok(0);
    }
  }
  Ok(1)
}

fn version_sha(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "version-sha")?;
  match load_json(path)?.get("sha") {
    Some(Value::String(sha)) if !sha.is_empty() => {
      println!("{sha}");
      Ok(0)
    }
    _ => Err("the version response has no sha".to_string()),
  }
}

/// Whether the process's environment or argv names `needle`.
fn environ_has(pid: u32, needle: &str) -> bool {
  let needle = needle.as_bytes();
  ["environ", "cmdline"].into_iter().any(|name| {
    fs::read(format!("/proc/{pid}/{name}")).is_ok_and(|data| data.windows(needle.len()).any(|window| window == needle))
  })
}

fn listener_pid(args: &[String]) -> Result<i32, String> {
  let [port, tree] = arguments::<2>(args, "listener-pid")?;
  let output = Command::new("ss")
    .arg("-Hltnp")
    .arg(format!("sport = :{port}"))
    .output()
    .map_err(|error| format!("ss: {error}"))?;
  if !output.status.success() {
    return Err(format!("ss exited {}", output.status));
  }
  static PID: OnceLock<Regex> = OnceLock::new();
  let pattern = PID.get_or_init(|| Regex::new(r"pid=(\d+)").expect("the ss pid pattern compiles"));
  let pids: BTreeSet<u32> = pattern
    .captures_iter(&String::from_utf8_lossy(&output.stdout))
    .filter_map(|captures| captures[1].parse().ok())
    .collect();
  let owned: Vec<u32> = pids.into_iter().filter(|pid| environ_has(*pid, tree)).collect();
  if owned.len() == 1 {
    println!("{}", owned[0]);
  }
  Ok(0)
}

fn leftovers(args: &[String]) -> Result<i32, String> {
  let [tree, self_pid] = arguments::<2>(args, "leftovers")?;
  let self_pid: u32 = self_pid.parse().map_err(|error| format!("self pid {self_pid}: {error}"))?;
  let skip: BTreeSet<u32> =
    [std::process::id(), nix::unistd::getppid().as_raw().cast_unsigned(), self_pid].into_iter().collect();
  let mut found = false;
  let entries = fs::read_dir("/proc").map_err(|error| format!("/proc: {error}"))?;
  for entry in entries {
    let entry = entry.map_err(|error| error.to_string())?;
    let name = entry.file_name().to_string_lossy().into_owned();
    let Ok(pid) = name.parse::<u32>() else { continue };
    if skip.contains(&pid) || !environ_has(pid, tree) {
      continue;
    }
    let command = fs::read_to_string(format!("/proc/{pid}/comm"))
      .map(|text| text.trim().to_string())
      .unwrap_or_else(|_| "?".to_string());
    println!("{pid} {command}");
    found = true;
  }
  Ok(if found { 1 } else { 0 })
}

fn cgroup_pids(path: &Path) -> Result<Vec<u32>, String> {
  let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
  text.split_whitespace().map(|pid| pid.parse().map_err(|error| format!("{pid}: {error}"))).collect()
}

/// Removes one rehearsal session cgroup; processes in it are signalled only when they name the tree.
fn cgroup_clean(args: &[String]) -> Result<i32, String> {
  let [path, tree] = arguments::<2>(args, "cgroup-clean")?;
  let directory = Path::new(path);
  if !directory.is_dir() {
    return Ok(0);
  }
  let procs = directory.join("cgroup.procs");
  for pid in cgroup_pids(&procs)? {
    if !environ_has(pid, tree) {
      continue;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL) {
      Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
      Err(error) => return Err(error.to_string()),
    }
  }
  let deadline = Instant::now() + Duration::from_secs(10);
  while !cgroup_pids(&procs)?.is_empty() && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(200));
  }
  if !cgroup_pids(&procs)?.is_empty() {
    println!(
      "populated by processes outside the tree: {}",
      directory.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
    );
    return Ok(1);
  }
  fs::remove_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
  Ok(0)
}

// ---- the preflight reads --------------------------------------------------------------------------------------------

/// The first `backends.options` entry whose `api_base` and `credential` are both set: the operator's own model
/// endpoint, which the trial and the rehearsal run against.
fn backend_choice(config_path: &str) -> Result<Option<(String, String)>, String> {
  let document = config::read_yaml_document(Path::new(config_path)).map_err(|error| error.to_string())?;
  let document = config::yaml_to_json(&document).map_err(|error| error.to_string())?;
  let options = document.get("backends").and_then(|backends| backends.get("options")).and_then(Value::as_array);
  let Some(options) = options else { return Ok(None) };
  for option in options {
    let (Some(api_base), Some(credential)) = (option.get("api_base"), option.get("credential")) else {
      continue;
    };
    if truthy(api_base) && truthy(credential) {
      return Ok(Some((pyfmt::python_str(api_base), pyfmt::python_str(credential))));
    }
  }
  Ok(None)
}

fn backend_option(args: &[String]) -> Result<i32, String> {
  let [path] = arguments::<1>(args, "backend-option")?;
  match backend_choice(path)? {
    Some((api_base, credential)) => {
      println!("{api_base}");
      println!("{credential}");
      Ok(0)
    }
    None => Ok(1),
  }
}

/// `credentials.yaml`'s `section.api_key`, or `None` when the file has no such key.
fn credential_api_key(credentials_path: &str, section: &str) -> Result<Option<String>, String> {
  let home = Path::new(credentials_path).parent().ok_or_else(|| format!("{credentials_path} has no directory"))?;
  let credentials = credentials::load_credentials(home).map_err(|error| error.to_string())?;
  Ok(credentials.get(section, "api_key").map(credentials::CredentialValue::python_str))
}

fn credential_key(args: &[String]) -> Result<i32, String> {
  let [path, section] = arguments::<2>(args, "credential-key")?;
  match credential_api_key(path, section)? {
    Some(value) => {
      println!("{value}");
      Ok(0)
    }
    None => Ok(1),
  }
}

fn free_port(args: &[String]) -> Result<i32, String> {
  let [min, max] = arguments::<2>(args, "free-port")?;
  let min: u16 = min.parse().map_err(|error| format!("{min}: {error}"))?;
  let max: u16 = max.parse().map_err(|error| format!("{max}: {error}"))?;
  for port in min..=max {
    if TcpListener::bind(("127.0.0.1", port)).is_ok() {
      println!("{port}");
      return Ok(0);
    }
  }
  Ok(1)
}

fn pid_start(args: &[String]) -> Result<i32, String> {
  let [pid] = arguments::<1>(args, "pid-start")?;
  let pid: u32 = pid.parse().map_err(|error| format!("{pid}: {error}"))?;
  match proc_identity::read_pid_stat(pid) {
    Some(stat) => {
      println!("{}", stat.pid_start);
      Ok(0)
    }
    None => Ok(1),
  }
}

fn seed_run(args: &[String]) -> Result<i32, String> {
  let [sessions, session, pid, pid_start, backend, run_id] = arguments::<6>(args, "seed-run")?;
  let run_dir = layout::runs_dir(&Path::new(sessions).join(session)).join(run_id);
  if run_dir.exists() {
    return Err(format!("{} already exists", run_dir.display()));
  }
  fs::create_dir_all(&run_dir).map_err(|error| format!("{}: {error}", run_dir.display()))?;
  let pid: i64 = pid.parse().map_err(|error| format!("{pid}: {error}"))?;
  let started_at = OffsetDateTime::now_utc()
    .format(format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]+00:00"))
    .map_err(|error| error.to_string())?;
  let record = json!({
    "id": run_id,
    "session_id": session,
    "kind": "manager_turn",
    "backend": backend,
    "pid": pid,
    "pid_start": pid_start,
    "started_at": started_at,
    "ended_at": Value::Null,
  });
  let metadata = run_dir.join(run_records::RUN_METADATA_NAME);
  run_records::write_run_metadata(&metadata, &record, &pyfmt::PYTHON_JSON_DEFAULT)
    .map_err(|error| format!("{}: {error}", metadata.display()))?;
  Ok(0)
}

// ---- the trial server's WebSocket replay -----------------------------------------------------------------------------

fn write_message(args: &[String]) -> Result<i32, String> {
  let [prompt_path, output_path] = arguments::<2>(args, "write-message")?;
  let prompt = fs::read_to_string(prompt_path).map_err(|error| format!("{prompt_path}: {error}"))?;
  // The message route requires a request_id, which deduplicates a retried send; each trial message is a new send.
  let request_id = format!("trial-{}", uuid::Uuid::new_v4().simple());
  let body = json!({ "content": prompt, "request_id": request_id });
  fs::write(output_path, pyfmt::json_dumps(&body, &PYTHON_JSON)).map_err(|error| format!("{output_path}: {error}"))?;
  Ok(0)
}

fn redact_value(text: &mut String, value: &str) {
  if value.is_empty() {
    return;
  }
  *text = text.replace(value, "[redacted]");
  let characters: Vec<char> = value.chars().collect();
  if characters.len() >= 2
    && characters[0] == characters[characters.len() - 1]
    && (characters[0] == '"' || characters[0] == '\'')
  {
    let inner: String = characters[1..characters.len() - 1].iter().collect();
    if !inner.is_empty() {
      *text = text.replace(&inner, "[redacted]");
    }
  }
}

fn redact_stdin() -> Result<i32, String> {
  let mut text = String::new();
  io::stdin().read_to_string(&mut text).map_err(|error| error.to_string())?;
  for name in ["TRIAL_REDACT_ACCESS_KEY", "TRIAL_REDACT_LIVE_KEY", "TRIAL_REDACT_BACKEND_KEY"] {
    redact_value(&mut text, &std::env::var(name).unwrap_or_default());
  }
  io::stdout().write_all(text.as_bytes()).map_err(|error| error.to_string())?;
  Ok(0)
}

/// `urllib.parse.quote(value, safe="")`: the characters Python leaves bare.
fn quote_component(value: &str) -> String {
  use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
  const UNSAFE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
  percent_encoding::utf8_percent_encode(value, UNSAFE).to_string()
}

fn frame_signature(frame: &Map<String, Value>) -> Value {
  let message = frame.get("message").and_then(Value::as_object);
  let field = |key: &str| -> Value {
    if let Some(value) = frame.get(key) {
      return value.clone();
    }
    message.and_then(|message| message.get(key)).cloned().unwrap_or(Value::Null)
  };
  json!([field("event_index"), field("role"), field("id")])
}

fn write_jsonl(path: &str, frames: &[Value]) -> Result<(), String> {
  let mut text = String::new();
  for frame in frames {
    text.push_str(&pyfmt::json_dumps(frame, &COMPACT_JSON));
    text.push('\n');
  }
  fs::write(path, text).map_err(|error| format!("{path}: {error}"))
}

fn replay(
  base_url: &str,
  session: &str,
  credentials_path: &str,
  output_path: &str,
) -> Result<(Vec<Value>, String), String> {
  let home = Path::new(credentials_path).parent().ok_or_else(|| format!("{credentials_path} has no directory"))?;
  let credentials = credentials::load_credentials(home).map_err(|error| error.to_string())?;
  let token = credentials.access_key().ok_or("the credentials response has no access key")?;
  // The replay cursor rides the handshake query; cursor 0 replays every event.
  let path = route::fill(api::SESSION_WEBSOCKET_PATH, &[session]);
  let url = format!("{base_url}{path}?token={}&cursor=0", quote_component(&token));
  let outcome = replay_frames(&url).map_err(|error| error.replace(&token, "[redacted]"))?;
  let frames = outcome.ok_or("ValueError: replay returned no catchup frames")?;
  let mut signatures = Vec::new();
  for frame in &frames {
    let object = frame.as_object().ok_or("a replay frame is not a JSON object")?;
    signatures.push(frame_signature(object));
  }
  let sequence = serde_json::to_string(&Value::Array(signatures)).map_err(|error| error.to_string())?;
  write_jsonl(output_path, &frames)?;
  Ok((frames, sequence))
}

/// Every frame before `catchup_complete`; `None` when the server sent no frame at all.
fn replay_frames(url: &str) -> Result<Option<Vec<Value>>, String> {
  use futures_util::StreamExt as _;
  use tokio_tungstenite::tungstenite::Message;
  use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

  let runtime =
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())?;
  runtime.block_on(async {
    let parsed = url::Url::parse(url).map_err(|error| error.to_string())?;
    let host = parsed.host_str().ok_or("the websocket URL names no host")?.to_string();
    let port = parsed.port_or_known_default().ok_or("the websocket URL names no port")?;
    let stream = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect((host.as_str(), port)))
      .await
      .map_err(|_| "the websocket connection timed out".to_string())?
      .map_err(|error| error.to_string())?;
    let request = url.into_client_request().map_err(|error| error.to_string())?;
    let (mut socket, _) =
      tokio_tungstenite::client_async_with_config(request, stream, None).await.map_err(|error| error.to_string())?;
    let mut frames = Vec::new();
    loop {
      let message = tokio::time::timeout(Duration::from_secs(15), socket.next())
        .await
        .map_err(|_| "the websocket replay timed out".to_string())?
        .ok_or("the websocket closed before catchup_complete")?
        .map_err(|error| error.to_string())?;
      let text = match message {
        Message::Text(text) => text.to_string(),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())?,
        Message::Ping(_) | Message::Pong(_) => continue,
        Message::Close(_) => return Err("the websocket closed before catchup_complete".to_string()),
        Message::Frame(_) => continue,
      };
      let frame: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
      if frame.get("type").and_then(Value::as_str) == Some("catchup_complete") {
        break;
      }
      frames.push(frame);
    }
    if frames.is_empty() {
      return Ok(None);
    }
    Ok(Some(frames))
  })
}

fn websocket(args: &[String]) -> Result<i32, String> {
  let [base_url, session, credentials_path, output_path] = arguments::<4>(args, "websocket")?;
  match replay(base_url, session, credentials_path, output_path) {
    Ok((frames, sequence)) => {
      println!("rust_frame_sequence={sequence}");
      println!("frames={}", frames.len());
      Ok(0)
    }
    Err(error) => {
      eprintln!("websocket replay failed: {error}");
      Ok(1)
    }
  }
}

// ---- the helpers' own checks -----------------------------------------------------------------------------------------

fn self_test(args: &[String]) -> Result<i32, String> {
  if !args.is_empty() {
    return Err("self-test: wrong number of arguments".to_string());
  }
  let summary = json!({"items": [
    {"id": "run-a", "state": "success", "pid": 101, "repo_path": "/repo", "worktree_path": "/work", "branch_name": "trial"},
    {"id": "run-b", "state": "", "pid": null, "repo_path": "", "worktree_path": "", "branch_name": ""},
  ]});
  let fields = run_summary_fields(&summary)?;
  let expected = ["2", "run-b", "", "", "", "", "", "success,"];
  if fields.iter().map(String::as_str).ne(expected) {
    return Err(format!("empty run-summary fields are not preserved: {fields:?}"));
  }
  println!("PASS self-test empty run-summary fields preserved");

  let nested = json!({"type": "message", "event_index": 3,
    "message": {"event_index": 77, "role": "assistant", "id": "nested-id"}});
  if frame_signature(nested.as_object().expect("a frame object")) != json!([3, "assistant", "nested-id"]) {
    return Err("the frame signature used a nested event_index".to_string());
  }
  println!("PASS self-test frame signature uses only top-level event_index");

  let events = vec![
    json!({"id": "user-1", "type": "user"}),
    json!({"id": "nudge-1", "type": "agent_message", (event_types::WEB_REPLY): {(event_types::NUDGE_OF): "user-1"}}),
  ];
  let runs = vec![
    json!({"id": "run-user", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]}),
    json!({"id": "run-nudge", "state": "success", (event_types::INPUT_EVENT_IDS): ["nudge-1"]}),
  ];
  let (ok, _) = run_contract_value(&runs, &events, "check2", "user-1", "")?;
  if !ok {
    return Err("check 2 rejected one nudge run".to_string());
  }
  println!("PASS self-test check 2 allows one nudge run");

  let runs = vec![
    json!({"id": "run-user-a", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]}),
    json!({"id": "run-user-b", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]}),
  ];
  let (ok, _) = run_contract_value(&runs, &events, "check2", "user-1", "")?;
  if ok {
    return Err("check 2 accepted two runs for one user message".to_string());
  }
  println!("PASS self-test check 2 rejects two runs for one user message");

  let events = vec![
    json!({"id": "user-1", "type": "user"}),
    json!({"id": "reminder-1", "type": "agent_message", (event_types::WEB_STATUS_REMINDER): "reminder-a"}),
    json!({"id": "reminder-2", "type": "agent_message", (event_types::WEB_STATUS_REMINDER): "reminder-b"}),
  ];
  let runs = vec![
    json!({"id": "run-user", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]}),
    json!({"id": "run-reminder", "state": "success", (event_types::INPUT_EVENT_IDS): ["reminder-1"]}),
  ];
  for mode in ["check2", "check5"] {
    let (ok, _) = run_contract_value(&runs, &events, mode, "user-1", "")?;
    if !ok {
      return Err(format!("{mode} rejected one status reminder run"));
    }
  }
  println!("PASS self-test checks 2 and 5 allow one status reminder run");

  let runs = vec![
    json!({"id": "run-user", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]}),
    json!({"id": "run-reminder-a", "state": "success", (event_types::INPUT_EVENT_IDS): ["reminder-1"]}),
    json!({"id": "run-reminder-b", "state": "success", (event_types::INPUT_EVENT_IDS): ["reminder-2"]}),
  ];
  let (ok, _) = run_contract_value(&runs, &events, "check5", "user-1", "")?;
  if ok {
    return Err("check 5 accepted two status reminder runs for one turn".to_string());
  }
  println!("PASS self-test check 5 rejects two status reminder runs for one turn");

  let events = vec![json!({"id": "user-1", "type": "user",
    (event_types::WEB_REPLY): {(event_types::NUDGE_OF): "user-1"}, (event_types::WEB_STATUS_REMINDER): "reminder-a"})];
  let runs = vec![json!({"id": "run-user", "state": "success", (event_types::INPUT_EVENT_IDS): ["user-1"]})];
  for mode in ["check2", "check5"] {
    let (ok, summary) = run_contract_value(&runs, &events, mode, "user-1", "")?;
    if !ok {
      return Err(format!("{mode} did not count a combined nudge and status reminder as one run: {summary}"));
    }
  }
  println!("PASS self-test checks 2 and 5 count a combined nudge and status reminder as one nudge run");

  let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
  let sessions = directory.path();
  let session = sessions.join("manager");
  fs::create_dir_all(layout::data_dir(&session)).map_err(|error| error.to_string())?;
  let line = json!({"id": "report-1", "type": event_types::CHILD_REPORT, "outcome": "failed",
    "summary": "run run-a failed: git worktree add timed out after 0.3s for charliebot/task-x\nsee the worker log"});
  chat_events::append_event(&layout::chat_events_path(&session), &line).map_err(|error| error.to_string())?;
  let reported = delegation_error_text(sessions, "manager")?;
  let wanted = "run run-a failed: git worktree add timed out after 0.3s for charliebot/task-x";
  if reported.as_deref() != Some(wanted) {
    return Err(format!("the delegation error was {reported:?}, want {wanted:?}"));
  }
  println!("PASS self-test delegation error reports the server's git timeout text");
  Ok(0)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn temporary() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
  }

  /// The time of the fixture lines.
  const STAMP: &str = "2026-10-21 10:00:01.000Z";

  #[test]
  fn the_sweep_line_is_read_from_text_and_json_logs() {
    // The server's `names = ?removed`: the Debug text of the list.
    let removed = format!("{:?}", ["charliebot-sess-aaaaaaaa", "charliebot-sess-bbbbbbbb"]);
    let text = logline::render(
      STAMP,
      "info",
      SWEEP_REMOVED,
      &[("count".to_string(), json!(2)), ("names".to_string(), json!(removed))],
    );
    assert_eq!(
      sweep_removed_names(&text).unwrap(),
      Some(vec!["charliebot-sess-aaaaaaaa".to_string(), "charliebot-sess-bbbbbbbb".to_string()])
    );
    let encoded = r#"{"event": "session_cgroup_sweep_removed", "names": "[\"charliebot-sess-cccccccc\"]"}"#;
    assert_eq!(sweep_removed_names(encoded).unwrap(), Some(vec!["charliebot-sess-cccccccc".to_string()]));
    let listed = r#"{"event": "session_cgroup_sweep_removed", "names": ["charliebot-sess-dddddddd"]}"#;
    assert_eq!(sweep_removed_names(listed).unwrap(), Some(vec!["charliebot-sess-dddddddd".to_string()]));
    assert_eq!(sweep_removed_names(&logline::render(STAMP, "info", "server_ready", &[])).unwrap(), None);
  }

  #[test]
  fn a_text_log_line_splits_into_its_event_and_fields() {
    let directory = temporary();
    let path = directory.path().join("server.log");
    let enabled = logline::render(
      STAMP,
      "info",
      CGROUP_ENABLED,
      &[
        ("memory_max_mb".to_string(), json!(1024)),
        ("swap_max_mb".to_string(), json!(0)),
        ("tasks_max".to_string(), json!(256)),
      ],
    );
    let created =
      logline::render(STAMP, "info", CGROUP_CREATED, &[("path".to_string(), json!("/a/charliebot-sess-aaaaaaaa"))]);
    fs::write(&path, format!("{enabled}\n{created}\n")).unwrap();

    let rows = log_rows(&path.to_string_lossy()).unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].event, CGROUP_ENABLED);
    assert_eq!(field_or_empty(&rows[0], "memory_max_mb"), "1024");
    assert_eq!(field_or_empty(&rows[0], "tasks_max"), "256");
    assert_eq!(rows[1].event, CGROUP_CREATED);
    assert_eq!(field_or_empty(&rows[1], "path"), "/a/charliebot-sess-aaaaaaaa");
  }

  #[test]
  fn a_json_log_line_carries_its_event_and_fields_without_level_or_timestamp() {
    let directory = temporary();
    let path = directory.path().join("server.log");
    fs::write(
      &path,
      "{\"event\": \"session_cgroup_created\", \"path\": \"/a/charliebot-sess-bbbbbbbb\", \"level\": \"info\", \
\"timestamp\": \"2026-10-21T10:00:02.000000Z\"}\n",
    )
    .unwrap();

    let rows = log_rows(&path.to_string_lossy()).unwrap();

    assert_eq!(rows[0].event, CGROUP_CREATED);
    assert_eq!(field_or_empty(&rows[0], "path"), "/a/charliebot-sess-bbbbbbbb");
    assert_eq!(rows[0].field("level"), None);
    assert_eq!(rows[0].field("timestamp"), None);
  }

  #[test]
  fn the_backend_choice_takes_the_first_option_with_both_values() {
    let directory = temporary();
    let config = directory.path().join("config.yaml");
    fs::write(
      &config,
      concat!(
        "backends:\n",
        "  options:\n",
        "    - id: no-credential\n      api_base: http://first.invalid\n",
        "    - id: upstream\n      api_base: http://second.invalid\n      credential: upstream\n",
      ),
    )
    .unwrap();

    assert_eq!(
      backend_choice(&config.to_string_lossy()).unwrap(),
      Some(("http://second.invalid".to_string(), "upstream".to_string()))
    );
  }

  #[test]
  fn the_credential_key_is_read_from_the_home_file() {
    let directory = temporary();
    let credentials = directory.path().join("credentials.yaml");
    fs::write(&credentials, "upstream:\n  api_key: k-1\n").unwrap();

    assert_eq!(credential_api_key(&credentials.to_string_lossy(), "upstream").unwrap(), Some("k-1".to_string()));
    assert_eq!(credential_api_key(&credentials.to_string_lossy(), "missing").unwrap(), None);
  }

  #[test]
  fn a_runs_page_is_active_only_while_a_run_is_queued_or_running() {
    let page = json!({"items": [{"state": "success"}, {"state": "running"}], "next_cursor": "c-2"});
    assert_eq!(run_page_fields(&page).unwrap(), (true, "c-2".to_string()));
    let last = json!({"items": [{"state": "success"}, {"state": ""}], "next_cursor": null});
    assert_eq!(run_page_fields(&last).unwrap(), (false, String::new()));
  }

  #[test]
  fn a_reply_is_normalized_before_comparison() {
    assert_eq!(normalize_reply("  `HELLO`  "), "HELLO");
    // The trailing punctuation strip runs after the backtick strip, as Python's `.strip("` ").rstrip(".! ")` does.
    assert_eq!(normalize_reply("`HELLO`."), "HELLO`");
    assert_eq!(normalize_reply("HELLO!"), "HELLO");
    assert_eq!(normalize_reply("two words"), "two words");
  }
}
