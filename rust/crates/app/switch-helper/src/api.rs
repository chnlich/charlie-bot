//! The response-body reads of the switch: one field of an API answer, the newest run's fields, a run-state page, the
//! assistant-message count of a bootstrap, the delegate request body, a launch log match and a run's error text.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::event_types;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_store::layout;
use charliebot_store::run_records::{EventReadError, RUN_EVENTS_NAME, read_event_values};
use serde_json::{Value, json};

use crate::common::{USAGE, exact, exit_with, failed, read_json};

/// `json.dumps(value)` with every option at its default: one line, `", "` and `": "` separators, ASCII escapes.
const PYTHON_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// `json-field BODY NAME`: the top-level field's value as Python's `print` writes it.
pub fn json_field(args: &[OsString]) -> Result<(), CliExit> {
  let [body, name] = exact::<2>(args)?;
  let document = read_json(Path::new(&body)).map_err(failed)?;
  let value = document.get(&name).ok_or_else(|| CliExit::error(format!("the response has no field {name:?}")))?;
  println!("{}", pyfmt::python_str(value));
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
    Some(value) if pyfmt::python_truthy(value) => {
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
    values.push(if value.is_null() { String::new() } else { pyfmt::python_str(value) });
  }
  Ok(values)
}

/// `launch-has LAUNCH_LOG EVENT [KEY=VALUE...]`: exit 0 when a complete line of the log is an EVENT record whose
/// fields include every KEY=VALUE, and 1 otherwise. The log holds the Rust server's JSON lines; a line of another event
/// never matches.
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
    if !line.starts_with("{\"") {
      continue;
    }
    let record = serde_json::from_str::<Value>(line)?;
    let fields = record.as_object().ok_or_else(|| anyhow::anyhow!("a launch line is not an object"))?;
    if fields.get("event").and_then(Value::as_str) != Some(event) {
      continue;
    }
    if wanted.iter().all(|(key, value)| fields.get(key).is_some_and(|field| pyfmt::python_str(field) == *value)) {
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
  let draft_counts = if pyfmt::python_truthy(&draft) {
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
      Ok(format!("{} {}", pyfmt::python_str(id), pyfmt::python_str(state)))
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

/// The delegate request body, in the field order the switch's Python wrote.
fn delegation_request(session: &str, repo: &str, goal: &str) -> String {
  let body = json!({
    "session_id": session,
    "description": goal,
    "repo_path": repo,
    "base_branch": "main",
    "task_type": "script-run",
    "keep_worktree": true,
    "request_id": format!("switch-delegation:{session}"),
  });
  pyfmt::json_dumps(&body, &PYTHON_JSON)
}

/// `run-error HOME SESSION RUN`: the message of the last error event in the run's event log, with its whitespace
/// collapsed; empty when the run logged no error.
pub fn run_error(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let session = config::sessions_dir_in(Path::new(&home)).join(&session_id);
  let path = layout::runs_dir(&session).join(&run_id).join(RUN_EVENTS_NAME);
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
}
