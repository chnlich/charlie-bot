//! The response-body reads of the switch. They cover one field of an API answer, the newest run's fields, a run-state
//! page and the assistant-message count of a bootstrap. The rest read the delegate request body, a launch log match, a
//! run's error text and a run's first command.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::event_types;
use charliebot_core::logline;
use charliebot_core::text_repr;
use charliebot_store::layout;
use charliebot_store::metadata::{BASE_BRANCH_FIELD, KEEP_WORKTREE_FIELD, REPO_PATH_FIELD, TASK_TYPE_FIELD, TaskType};
use charliebot_store::run_logs;
use charliebot_store::run_records::{
  EventReadError, RUN_EVENTS_NAME, parse_event_line, read_event_values, read_run_metadata,
};
use charliebot_tasks::routes as task_routes;
use serde_json::{Value, json};

use crate::common::{USAGE, exact, exit_with, failed, metadata_read_failure, read_json};

/// `json-field BODY NAME`: the top-level field's value as plain text.
pub fn json_field(args: &[OsString]) -> Result<(), CliExit> {
  let [body, name] = exact::<2>(args)?;
  let document = read_json(Path::new(&body)).map_err(failed)?;
  let value = document.get(&name).ok_or_else(|| CliExit::error(format!("the response has no field {name:?}")))?;
  println!("{}", text_repr::value_str(value));
  Ok(())
}

/// `newest-run-fields BODY FIELD...`: the named fields of the newest run, joined by the unit separator (0x1f). A field
/// is empty while the session has no run and when the record holds it as null.
pub fn newest_run_fields(args: &[OsString]) -> Result<(), CliExit> {
  let Some((body, fields)) = args.split_first() else {
    return Err(CliExit::usage(USAGE));
  };
  let document = read_json(Path::new(body.to_str().ok_or_else(|| CliExit::usage(USAGE))?)).map_err(failed)?;
  let names = fields
    .iter()
    .map(|field| field.to_str().map(str::to_string).ok_or_else(|| CliExit::usage(USAGE)))
    .collect::<Result<Vec<_>, _>>()?;
  println!("{}", newest_run_values(&document, &names)?.join("\u{1f}"));
  Ok(())
}

/// The newest run's named fields: empty per field while the page holds no run, and for a null field.
fn newest_run_values(page: &Value, fields: &[String]) -> Result<Vec<String>, CliExit> {
  let items: Option<&Vec<Value>> = match page.get("items") {
    Some(value) if text_repr::is_truthy(value) => {
      Some(value.as_array().ok_or_else(|| CliExit::error("the runs page's items is not a list"))?)
    }
    _ => None,
  };
  let mut values = Vec::new();
  for field in fields {
    let Some(run) = items.and_then(|items| items.first()) else {
      values.push(String::new());
      continue;
    };
    let value = run.get(field).ok_or_else(|| CliExit::error(format!("the newest run has no field {field:?}")))?;
    values.push(if value.is_null() { String::new() } else { text_repr::value_str(value) });
  }
  Ok(values)
}

/// `launch-has LAUNCH_LOG EVENT [KEY=VALUE...]`: exit 0 when a complete line of the log is an EVENT record whose
/// fields include every KEY=VALUE, and 1 otherwise. The log holds the server's lines in either form `logline` reads;
/// a line of another event never matches.
pub fn launch_has(args: &[OsString]) -> Result<(), CliExit> {
  let (path, event, pairs) = match args {
    [path, event, pairs @ ..] => (path, event, pairs),
    _ => return Err(CliExit::usage(USAGE)),
  };
  let path = path.to_str().ok_or_else(|| CliExit::usage(USAGE))?;
  let event = event.to_str().ok_or_else(|| CliExit::usage(USAGE))?;
  let mut wanted = BTreeMap::new();
  for pair in pairs {
    let pair = pair.to_str().ok_or_else(|| CliExit::usage(USAGE))?;
    let (key, value) =
      pair.split_once('=').ok_or_else(|| CliExit::error(format!("the launch match {pair:?} is not KEY=VALUE")))?;
    wanted.insert(key.to_string(), value.to_string());
  }
  let matched = matches(Path::new(path), event, &wanted).map_err(failed)?;
  exit_with(if matched { 0 } else { 1 })
}

/// Whether one launch log holds a complete EVENT record with every wanted field.
fn matches(path: &Path, event: &str, wanted: &BTreeMap<String, String>) -> anyhow::Result<bool> {
  let bytes = fs::read(path)?;
  let text = String::from_utf8_lossy(&bytes);
  // Only complete lines: a line the server is still writing has no newline yet.
  let mut lines: Vec<&str> = text.split('\n').collect();
  lines.pop();
  for line in lines {
    let Some(row) = logline::parse(line) else { continue };
    if row.event != event {
      continue;
    }
    if wanted.iter().all(|(key, value)| row.field(key) == Some(value.as_str())) {
      return Ok(true);
    }
  }
  Ok(false)
}

/// `assistant-messages BODY`: how many assistant messages the bootstrap holds, the open draft included. A tool call is
/// part of its turn's assistant message, so a tool call counts here too.
pub fn assistant_messages(args: &[OsString]) -> Result<(), CliExit> {
  let [body] = exact::<1>(args)?;
  let document = read_json(Path::new(&body)).map_err(failed)?;
  println!("{}", assistant_count(&document)?);
  Ok(())
}

/// The bootstrap's assistant messages, with the open draft when it is an assistant message.
fn assistant_count(bootstrap: &Value) -> Result<usize, CliExit> {
  let messages = bootstrap
    .get("messages")
    .and_then(Value::as_array)
    .ok_or_else(|| CliExit::error("the bootstrap has no messages list"))?;
  let count = messages.iter().try_fold(0usize, |count, message| {
    let role = message.get("role").and_then(Value::as_str).ok_or_else(|| CliExit::error("a message has no role"))?;
    Ok::<_, CliExit>(count + usize::from(role == "assistant"))
  })?;
  let draft = bootstrap.get("pending_draft").cloned().unwrap_or(Value::Null);
  let draft_counts = if text_repr::is_truthy(&draft) {
    let role = draft.get("role").and_then(Value::as_str).ok_or_else(|| CliExit::error("the draft has no role"))?;
    usize::from(role == "assistant")
  } else {
    0
  };
  Ok(count + draft_counts)
}

/// `run-states BODY`: one `RUN_ID STATE` line per run of the runs page. A next cursor means runs the caller would
/// miss, so it is an error, as the switch's archive step treats it.
pub fn run_states(args: &[OsString]) -> Result<(), CliExit> {
  let [body] = exact::<1>(args)?;
  let page = read_json(Path::new(&body)).map_err(failed)?;
  for line in run_state_lines(&page)? {
    println!("{line}");
  }
  Ok(())
}

/// The page's runs as `RUN_ID STATE` lines, refusing a page with a next cursor.
fn run_state_lines(page: &Value) -> Result<Vec<String>, CliExit> {
  let cursor = page.get("next_cursor").ok_or_else(|| CliExit::error("the runs page has no next_cursor"))?;
  if !cursor.is_null() {
    return Err(CliExit::error("the session has more than 500 runs"));
  }
  let items =
    page.get("items").and_then(Value::as_array).ok_or_else(|| CliExit::error("the runs page has no items list"))?;
  items
    .iter()
    .map(|run| {
      let id = run.get("id").ok_or_else(|| CliExit::error("a run row has no id"))?;
      let state = run.get("state").ok_or_else(|| CliExit::error("a run row has no state"))?;
      Ok(format!("{} {}", text_repr::value_str(id), text_repr::value_str(state)))
    })
    .collect()
}

/// `delegation-body SESSION REPO GOAL`: the delegate request as JSON. The base branch is main, and keep_worktree keeps
/// the worktree after the run so the judgment can find it.
pub fn delegation_body(args: &[OsString]) -> Result<(), CliExit> {
  let [session, repo, goal] = exact::<3>(args)?;
  println!("{}", delegation_request(&session, &repo, &goal));
  Ok(())
}

/// The delegate request body, in the field order the switch sends. Every field name comes from its owner: the route
/// that validates the body names the delegate fields, and the store names the task record fields.
fn delegation_request(session: &str, repo: &str, goal: &str) -> String {
  let body = json!({
    (task_routes::DELEGATE_SESSION_ID_FIELD): session,
    (task_routes::DELEGATE_DESCRIPTION_FIELD): goal,
    (REPO_PATH_FIELD): repo,
    (BASE_BRANCH_FIELD): "main",
    (TASK_TYPE_FIELD): TaskType::ScriptRun.as_str(),
    (KEEP_WORKTREE_FIELD): true,
    (task_routes::DELEGATE_REQUEST_ID_FIELD): format!("switch-delegation:{session}"),
  });
  text_repr::json_dumps(&body, &text_repr::DEFAULT_JSON)
}

/// The directory of one run of a session under the home.
fn run_dir(home: &str, session_id: &str, run_id: &str) -> PathBuf {
  let session = config::sessions_dir_in(Path::new(home)).join(session_id);
  layout::runs_dir(&session).join(run_id)
}

/// `run-error HOME SESSION RUN`: the message of the last error event in the run's event log, with its whitespace
/// collapsed; empty when the run logged no error.
pub fn run_error(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let path = run_dir(&home, &session_id, &run_id).join(RUN_EVENTS_NAME);
  let events = match read_event_values(&path) {
    Ok(events) => events,
    Err(EventReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => return Err(CliExit::error(format!("{}: {error}", path.display()))),
  };
  println!("{}", last_error_message(&events).unwrap_or_default());
  Ok(())
}

/// The last error event's message, with its whitespace collapsed; `None` when no error event exists.
fn last_error_message(events: &[Value]) -> Option<String> {
  let last = events.iter().rev().find(|event| event.get("type").and_then(Value::as_str) == Some(event_types::ERROR))?;
  Some(last.get("message").and_then(Value::as_str).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `run-first-command HOME SESSION RUN`: the run's first command as three fields joined by the unit separator (0x1f).
/// The fields are the cwd of the log's first init event, the kind of its first tool result (see `first_command`), and
/// the run's backend from its metadata. The log is read under the skip contract, so a line the backend is still
/// writing does not fail the read. A log not written yet leaves the first two fields empty.
pub fn run_first_command(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let dir = run_dir(&home, &session_id, &run_id);
  let events_path = dir.join(RUN_EVENTS_NAME);
  let events: Vec<Value> = match run_logs::read_bytes(&events_path) {
    Ok(Some(bytes)) => bytes.split(|byte| *byte == b'\n').filter_map(parse_event_line).collect(),
    Ok(None) => Vec::new(),
    Err(error) => return Err(failed(format!("read {}: {error}", events_path.display()))),
  };
  let metadata_path = layout::metadata_path(&dir);
  let metadata =
    read_run_metadata(&metadata_path).map_err(|error| failed(metadata_read_failure(&metadata_path, error)))?;
  let backend = metadata
    .get("backend")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error(format!("{} has no backend", metadata_path.display())))?;
  let (cwd, kind) = first_command(&events);
  println!("{cwd}\u{1f}{kind}\u{1f}{backend}");
  Ok(())
}

/// The cwd of the log's first init event, and the kind of its first tool result. The kinds are `command-ok` for a
/// Bash command that did not error, `command-failed` for a Bash command that errored, and `not-command` for any other
/// tool. A tool result is either a user message's `tool_result` block or a top-level `tool_result` event. The block
/// names its tool by the `tool_use_id` of an earlier assistant `tool_use` block, and the event names its tool in
/// `tool_name`. Each part is empty while the log holds no such event.
fn first_command(events: &[Value]) -> (String, String) {
  let mut cwd: Option<String> = None;
  let mut kind: Option<&'static str> = None;
  let mut tool_names: BTreeMap<String, String> = BTreeMap::new();
  for event in events {
    match event.get("type").and_then(Value::as_str) {
      Some(event_types::SYSTEM) if cwd.is_none() && event.get("subtype").and_then(Value::as_str) == Some("init") => {
        cwd = Some(event.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string());
      }
      Some(event_types::ASSISTANT) => {
        for block in content_blocks(event) {
          let id = block.get("id").and_then(Value::as_str);
          let name = block.get("name").and_then(Value::as_str);
          if block.get("type").and_then(Value::as_str) == Some(event_types::TOOL_USE)
            && let (Some(id), Some(name)) = (id, name)
          {
            tool_names.insert(id.to_string(), name.to_string());
          }
        }
      }
      Some(event_types::USER) => {
        for block in content_blocks(event) {
          if block.get("type").and_then(Value::as_str) == Some(event_types::TOOL_RESULT) {
            let id = block.get(event_types::TOOL_USE_ID).and_then(Value::as_str);
            let name = id.and_then(|id| tool_names.get(id)).map_or("", String::as_str);
            record_first_result(&mut kind, name, is_error(block));
          }
        }
      }
      Some(event_types::TOOL_RESULT) => {
        let name = event.get(event_types::TOOL_NAME).and_then(Value::as_str).unwrap_or_default();
        record_first_result(&mut kind, name, is_error(event));
      }
      _ => {}
    }
  }
  (cwd.unwrap_or_default(), kind.unwrap_or_default().to_string())
}

/// Keeps the kind of the first tool result it is given; later results change nothing.
fn record_first_result(kind: &mut Option<&'static str>, tool: &str, is_error: bool) {
  if kind.is_none() {
    *kind = Some(match (tool, is_error) {
      ("Bash", false) => "command-ok",
      ("Bash", true) => "command-failed",
      _ => "not-command",
    });
  }
}

/// Whether a tool result reports an error. An absent `is_error` is no error, as the aggregator reads it.
fn is_error(result: &Value) -> bool {
  result.get("is_error").is_some_and(text_repr::is_truthy)
}

/// The content blocks of an event's message; empty when the message holds no list.
fn content_blocks(event: &Value) -> &[Value] {
  event.pointer(event_types::MESSAGE_CONTENT_POINTER).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_store::run_records::decode_event_line;

  #[test]
  fn the_newest_run_fields_keep_null_empty_and_missing_runs_empty() {
    let page = json!({"items": [{"id": "r1", "state": "running", "worktree_path": null}]});
    let values = newest_run_values(&page, &["id".to_string(), "state".to_string(), "worktree_path".to_string()])
      .expect("the fields");
    assert_eq!(values, ["r1", "running", ""]);

    let empty = json!({"items": []});
    assert_eq!(newest_run_values(&empty, &["id".to_string()]).expect("the fields"), [""]);
  }

  #[test]
  fn the_message_count_includes_the_open_assistant_draft() {
    let bootstrap = json!({
      "messages": [{"role": "user"}, {"role": "assistant"}, {"role": "tool"}],
      "pending_draft": {"role": "assistant"},
    });
    assert_eq!(assistant_count(&bootstrap).expect("the count"), 2);
    assert_eq!(assistant_count(&json!({"messages": [], "pending_draft": null})).expect("the count"), 0);
  }

  #[test]
  fn a_partial_line_and_another_event_do_not_match() {
    let dir = std::env::temp_dir().join(format!("switch-helper-launch-{}", std::process::id()));
    fs::write(
      &dir,
      "noise\n{\"event\": \"task_recovery_resume\", \"run_id\": \"r\"}\n{\"event\": \"server_ready\"}\n{\"event\": \"tsk",
    )
    .expect("the launch log");

    let none = BTreeMap::new();
    assert!(matches(&dir, "server_ready", &none).expect("the log reads"));
    assert!(
      matches(&dir, "task_recovery_resume", &BTreeMap::from([("run_id".to_string(), "r".to_string())]))
        .expect("the log reads")
    );
    assert!(
      !matches(&dir, "task_recovery_resume", &BTreeMap::from([("run_id".to_string(), "x".to_string())]))
        .expect("the log reads")
    );
    assert!(!matches(&dir, "tsk", &none).expect("the log reads"));
  }

  #[test]
  fn a_text_launch_line_matches_by_event_and_quoted_fields_and_skips_a_partial_line() {
    let dir = std::env::temp_dir().join(format!("switch-helper-launch-text-{}", std::process::id()));
    fs::write(
      &dir,
      concat!(
        "2026-10-29 07:11:32.244Z [info     ] server/main.rs:307             server_ready                   ready_in_ms=1\n",
        "2026-10-29 07:11:33.000Z [info     ] tasks/recovery.rs:542          task_recovery_resume           run_id='r s'\n",
        // A release before the location column writes the text form without it, which a rollback switch restores.
        "2026-10-29 07:11:35.000Z [info     ] task_recovery_resume           run_id=past\n",
        "2026-10-26 02:16:37 [info     ] task_recovery_resume           run_id=old\n",
        "2026-10-29 07:11:34.000Z [info     ] task_recovery_resume           run_id=late",
      ),
    )
    .expect("the launch log");

    assert!(matches(&dir, "server_ready", &BTreeMap::new()).expect("the log reads"));
    let run = |id: &str| BTreeMap::from([("run_id".to_string(), id.to_string())]);
    assert!(matches(&dir, "task_recovery_resume", &run("r s")).expect("the log reads"));
    assert!(matches(&dir, "task_recovery_resume", &run("past")).expect("the log reads"));
    assert!(!matches(&dir, "task_recovery_resume", &run("old")).expect("the log reads"));
    assert!(!matches(&dir, "task_recovery_resume", &run("late")).expect("the log reads"));
  }

  #[test]
  fn a_next_cursor_refuses_the_run_states() {
    let error = run_state_lines(&json!({"items": [], "next_cursor": "more"})).expect_err("the refusal");
    assert_eq!(error.code, 1);
  }

  #[test]
  fn the_last_error_message_collapses_whitespace() {
    let text =
      "{\"type\": \"error\", \"message\": \"first\"}\n{\"type\": \"error\", \"message\": \"  boxed \\n  text \"}\n";
    let events: Vec<Value> = text.lines().map(|line| decode_event_line(line).expect("the events")).collect();
    assert_eq!(last_error_message(&events).as_deref(), Some("boxed text"));
    assert_eq!(last_error_message(&[]), None);
  }

  fn events_of(lines: &[&str]) -> Vec<Value> {
    lines.iter().filter_map(|line| parse_event_line(line.as_bytes())).collect()
  }

  #[test]
  fn the_first_command_reads_a_claude_run_log_and_skips_a_partial_line() {
    // Lines of a claude-code run as its event log holds them. The last line is still being written.
    let events = events_of(&[
      r#"{"type":"system","subtype":"init","cwd":"/wt/task-1"}"#,
      r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git rev-parse --short HEAD"}}]}}"#,
      r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"abc1234","is_error":false}]}}"#,
      r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Rep"#,
    ]);
    assert_eq!(first_command(&events), ("/wt/task-1".to_string(), "command-ok".to_string()));
  }

  #[test]
  fn the_first_tool_result_decides_the_kind_and_a_later_one_changes_nothing() {
    let init = r#"{"type":"system","subtype":"init","cwd":"/wt"}"#;
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"r1","name":"Read","input":{}}]}}"#;
    let read_result =
      r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"r1","content":"x"}]}}"#;
    let failed_bash = r#"{"type":"tool_result","tool_name":"Bash","content":"fatal","is_error":true}"#;
    let bash_ok = r#"{"type":"tool_result","tool_name":"Bash","content":"abc"}"#;

    let kind = |lines: &[&str]| first_command(&events_of(lines)).1;
    assert_eq!(kind(&[init, read, read_result]), "not-command");
    assert_eq!(kind(&[init, failed_bash, bash_ok]), "command-failed");
    // A top-level result carries no is_error when the backend does not report one, which reads as no error.
    assert_eq!(kind(&[init, bash_ok, read_result]), "command-ok");
    assert_eq!(first_command(&events_of(&[])), (String::new(), String::new()));
    assert_eq!(first_command(&events_of(&[init])), ("/wt".to_string(), String::new()));
  }
}
