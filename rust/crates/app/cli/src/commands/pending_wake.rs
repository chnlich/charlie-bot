//! `charliebot hook pending-wake`: the turn-end check that a web manager turn which ends with its task open leaves a wake.
//!
//! A wake is a Waiting on you item, a bound schedule with a next run, a pending trigger, or an open child that runs a
//! run now or holds a wake itself. The reads run cheapest first and stop at the first wake: the turn input, then the
//! session directories, then the internal API. A turn's first miss blocks with the check's text; a miss after a block
//! in the same turn passes with a notice. The check reads only the turn's own events, so it keeps no state.

use std::path::Path;

use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::route;
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types::{TASK_CLOSE_REQUESTED, TURN_HOOKS, WEB_STATUS};
use charliebot_session_api::api as session_routes;
use charliebot_store::layout;
use charliebot_store::metadata::read_session_metadata;
use charliebot_tasks::turn_hooks::{CheckResult, TurnInput};
use serde::Deserialize;
use serde_json::Value;

use crate::commands::session::home_dir;

/// The entry that checks a web manager turn leaves a wake behind it.
pub const PENDING_WAKE_ENTRY: &str = "pending-wake";

/// The reason of a turn's first miss: the states a line can be in when its turn ends with the task open.
const BLOCK_REASON: &str = concat!(
  "pending-wake check: a turn that ends with the task open leaves the line in one of these states:\n",
  "- When the work can go on now, continue it in this turn or in a delegated task.\n",
  "- When a job, a restart or another session is still waited on, set a trigger that fires within four hours ",
  "and checks the awaited state.\n",
  "- When a wait on the user exists, list each one in Waiting on you.\n",
  "- When the work is delivered, send it with a close request.\n",
  "- When the line cannot go on, list one Waiting on you item that says the line has stopped, why, ",
  "and the reply that restarts it.",
);

/// The notice of a miss after a block in the same turn: the turn left no wake, so the line has stopped.
const NOTICE: &str = "pending-wake check: this turn left no wake, so the line has stopped. A reply restarts it.";

/// The state of a tree row whose run is in progress.
const RUNNING: &str = "running";
/// The task state of a tree row whose task is still open.
const OPEN: &str = "open";

/// Answers the check for one turn of a turn-end run.
pub fn check(input: &TurnInput) -> Result<CheckResult, CliExit> {
  if !applies(input) || wake_found(input)? {
    return Ok(CheckResult::Pass);
  }
  if blocked_before(&input.turn) {
    return Ok(CheckResult::Notice(NOTICE.to_string()));
  }
  Ok(CheckResult::Block(BLOCK_REASON.to_string()))
}

/// The check's scope: a web session whose turn exited cleanly or as a failed continuation, and left no close request.
fn applies(input: &TurnInput) -> bool {
  input.kind.as_deref() == Some("web")
    && input.exit_is_judged()
    && !input.turn.iter().any(|event| event["type"] == TASK_CLOSE_REQUESTED)
}

/// Whether the turn already blocked on this check: one of its `turn_hooks` records holds the block.
fn blocked_before(turn: &[Value]) -> bool {
  turn.iter().any(|event| {
    event["type"] == TURN_HOOKS
      && event["results"].as_array().is_some_and(|results| {
        results.iter().any(|result| result["name"] == PENDING_WAKE_ENTRY && result["result"] == "block")
      })
  })
}

/// Whether the session or an open descendant holds a wake.
fn wake_found(input: &TurnInput) -> Result<bool, CliExit> {
  if waiting_on_user(&input.status)? {
    return Ok(true);
  }
  let sessions = sessions_dir_in(&home_dir()?);
  if dir_wake(&sessions, &input.session_id)? || pending_trigger_count(&input.session_id)? > 0 {
    return Ok(true);
  }
  subtree_holds_wake(&sessions, &input.session_id)
}

/// Whether the turn's stored status lists a Waiting on you item. A turn with no stored status lists none.
fn waiting_on_user(status: &Value) -> Result<bool, CliExit> {
  match status.get("waiting") {
    None | Some(Value::Null) => Ok(false),
    Some(Value::Array(items)) => Ok(!items.is_empty()),
    Some(other) => Err(CliExit::error(format!("the turn input's status.waiting is not a list: {other}"))),
  }
}

/// Whether a session's directory holds a wake: a non-empty Waiting on you list, or a bound schedule with a next run.
fn dir_wake(sessions: &Path, session_id: &str) -> Result<bool, CliExit> {
  let path = layout::metadata_path(&sessions.join(session_id));
  let meta = read_session_metadata(&path)
    .map_err(|error| CliExit::error(format!("read {}: {error:?}", path.display())))?
    .ok_or_else(|| CliExit::error(format!("session {session_id} has no metadata at {}", path.display())))?;
  let waiting = meta
    .extra
    .get(WEB_STATUS)
    .and_then(|status| status.get("waiting"))
    .and_then(Value::as_array)
    .is_some_and(|items| !items.is_empty());
  let scheduled =
    meta.schedule_enabled == Some(true) && meta.schedule_next_run.as_deref().is_some_and(|run| !run.is_empty());
  Ok(waiting || scheduled)
}

/// The number of a session's pending delayed triggers, from the server's own list.
fn pending_trigger_count(session_id: &str) -> Result<usize, CliExit> {
  let endpoint = route::fill(session_routes::SESSION_PENDING_TRIGGERS_PATH, &[session_id]);
  let value = read(&endpoint, None)?;
  let triggers = value.as_array().ok_or_else(|| CliExit::error(format!("{endpoint} did not answer a list")))?;
  Ok(triggers.len())
}

/// One row of the session tree page, with the fields the wake search reads.
#[derive(Deserialize)]
struct TreeRow {
  id: String,
  task_state: String,
  work_state: String,
  open_descendant_count: u64,
}

/// One page of the session tree.
#[derive(Deserialize)]
struct TreePage {
  items: Vec<TreeRow>,
  next_cursor: Option<String>,
}

/// Whether an open child of `parent_id` holds a wake. The children's pages are read to the last page unless a child
/// runs first. Then the directories, the triggers and the open descendants are read, each in turn over the children.
fn subtree_holds_wake(sessions: &Path, parent_id: &str) -> Result<bool, CliExit> {
  let mut open = Vec::new();
  let mut cursor: Option<String> = None;
  loop {
    let page = tree_page(parent_id, cursor.as_deref())?;
    for row in page.items.into_iter().filter(|row| row.task_state == OPEN) {
      if row.work_state == RUNNING {
        return Ok(true);
      }
      open.push(row);
    }
    match page.next_cursor {
      Some(next) => cursor = Some(next),
      None => break,
    }
  }
  for row in &open {
    if dir_wake(sessions, &row.id)? {
      return Ok(true);
    }
  }
  for row in &open {
    if pending_trigger_count(&row.id)? > 0 {
      return Ok(true);
    }
  }
  for row in &open {
    if row.open_descendant_count > 0 && subtree_holds_wake(sessions, &row.id)? {
      return Ok(true);
    }
  }
  Ok(false)
}

/// One page of the children of `parent_id`, from `cursor` on.
fn tree_page(parent_id: &str, cursor: Option<&str>) -> Result<TreePage, CliExit> {
  let mut params = vec![(session_routes::SESSION_TREE_PARENT_ID_QUERY.to_string(), parent_id.to_string())];
  if let Some(cursor) = cursor {
    params.push((session_routes::SESSION_TREE_CURSOR_QUERY.to_string(), cursor.to_string()));
  }
  let value = read(session_routes::SESSION_TREE_PATH, Some(params))?;
  serde_json::from_value(value)
    .map_err(|error| CliExit::error(format!("the session tree page of {parent_id} does not parse: {error}")))
}

/// A GET of the internal API. A failed read exits 1: the turn-end runner reads exit 2 as a block.
fn read(endpoint: &str, params: Option<Vec<(String, String)>>) -> Result<Value, CliExit> {
  client::get(endpoint, params).map_err(|error| CliExit::contract(error.body, 1))
}
