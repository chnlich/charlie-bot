//! The `pending-wake` turn-end check: a web manager turn that ends with its task open leaves a wake.
//!
//! A wake is a Waiting on you item, a bound scheduled task with a next fire, a pending trigger, or an open child that
//! runs a run now or holds a wake itself. The reads run cheapest first and stop at the first wake: the turn input, then
//! the session metadata, then the bound scheduled tasks (one read per check, owned by `features/cron`), then the
//! triggers and the session tree. A turn's first miss blocks with the check's text; a miss after a block in the same
//! turn passes with a notice. The check reads only the turn's own events, so it keeps no state.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::event_types::{TURN_HOOKS, WEB_STATUS};
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{CheckResult, TurnInput};
use charliebot_session_api::tree::tree_page;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::trigger_files::TriggerStatus;
use charliebot_triggers::triggers::TriggerManager;
use serde::Deserialize;
use serde_json::Value;

use crate::audit::web_turn_checks_apply;

/// The check's name in the turn-end check list.
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
/// The rows of one session-tree page that the wake search reads.
const TREE_PAGE_LIMIT: usize = 100;

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

/// Answers the check for one turn of a turn-end run.
pub async fn check_pending_wake(features: &Arc<FeatureContext>, input: &TurnInput) -> anyhow::Result<CheckResult> {
  if !web_turn_checks_apply(input) || wake_found(features, input).await? {
    return Ok(CheckResult::Pass);
  }
  if blocked_before(&input.turn) {
    return Ok(CheckResult::Notice(NOTICE.to_string()));
  }
  Ok(CheckResult::Block(BLOCK_REASON.to_string()))
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
async fn wake_found(features: &Arc<FeatureContext>, input: &TurnInput) -> anyhow::Result<bool> {
  if waiting_on_user(&input.status)? {
    return Ok(true);
  }
  if dir_wake(features, &input.session_id)? {
    return Ok(true);
  }
  let scheduled = (features.registry.bound_next_fire_reader()?)(features, UtcDateTime::now())?;
  if scheduled.contains(&input.session_id) || pending_trigger_count(features, &input.session_id).await? > 0 {
    return Ok(true);
  }
  subtree_holds_wake(features, &input.session_id, &scheduled).await
}

/// Whether the turn's stored status lists a Waiting on you item. A turn with no stored status lists none.
fn waiting_on_user(status: &Value) -> anyhow::Result<bool> {
  match status.get("waiting") {
    None | Some(Value::Null) => Ok(false),
    Some(Value::Array(items)) => Ok(!items.is_empty()),
    Some(other) => anyhow::bail!("the turn input's status.waiting is not a list: {other}"),
  }
}

/// Whether a session's metadata holds a wake: a non-empty Waiting on you list.
fn dir_wake(features: &FeatureContext, session_id: &str) -> anyhow::Result<bool> {
  let meta = features
    .store
    .read_metadata_fresh(session_id)
    .map_err(|error| anyhow::anyhow!("read the metadata of session {session_id}: {error:?}"))?
    .with_context(|| format!("session {session_id} has no metadata"))?;
  let waiting = meta
    .extra
    .get(WEB_STATUS)
    .and_then(|status| status.get("waiting"))
    .and_then(Value::as_array)
    .is_some_and(|items| !items.is_empty());
  Ok(waiting)
}

/// The number of a session's pending delayed triggers.
async fn pending_trigger_count(features: &FeatureContext, session_id: &str) -> anyhow::Result<usize> {
  let manager = features.startup.require::<Arc<TriggerManager>>()?;
  let triggers = manager.list_triggers(session_id).await?;
  Ok(triggers.iter().filter(|trigger| trigger.status == TriggerStatus::Pending).count())
}

/// Whether an open child of `parent_id` holds a wake. The children's pages are read to the last page unless a child
/// runs first. Then the metadata and the bound scheduled tasks, the triggers and the open descendants are read, each in
/// turn over the children.
async fn subtree_holds_wake(
  features: &Arc<FeatureContext>,
  parent_id: &str,
  scheduled: &HashSet<String>,
) -> anyhow::Result<bool> {
  let mut open = Vec::new();
  let mut cursor: Option<String> = None;
  loop {
    let page = subtree_page(features, parent_id, cursor.take()).await?;
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
    if dir_wake(features, &row.id)? || scheduled.contains(&row.id) {
      return Ok(true);
    }
  }
  for row in &open {
    if pending_trigger_count(features, &row.id).await? > 0 {
      return Ok(true);
    }
  }
  for row in &open {
    if row.open_descendant_count > 0 && Box::pin(subtree_holds_wake(features, &row.id, scheduled)).await? {
      return Ok(true);
    }
  }
  Ok(false)
}

/// One page of the children of `parent_id`, from `cursor` on, read from the session tree as the server's tree route
/// reads it.
async fn subtree_page(
  features: &Arc<FeatureContext>,
  parent_id: &str,
  cursor: Option<String>,
) -> anyhow::Result<TreePage> {
  let read = Arc::clone(features);
  let parent = parent_id.to_string();
  let bytes = tokio::task::spawn_blocking(move || {
    tree_page(&read.store, Some(parent.as_str()), false, TREE_PAGE_LIMIT, cursor.as_deref())
  })
  .await
  .context("join the session tree read")?
  .map_err(|error| anyhow::anyhow!("read the session tree of {parent_id}: {error:?}"))?;
  serde_json::from_slice(&bytes).with_context(|| format!("the session tree page of {parent_id} does not parse"))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use charliebot_core::event_types::TASK_CLOSE_REQUESTED;
  use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
  use serde_json::json;

  use super::*;
  use crate::test_support::Home;

  fn turn(session_id: &str, kind: Option<&str>, exit_code: i64, status: Value, turn: Vec<Value>) -> TurnInput {
    TurnInput {
      session_id: session_id.to_string(),
      kind: kind.map(str::to_string),
      exit_code,
      continuation_failed: false,
      status,
      last_input_provenance: None,
      turn,
    }
  }

  /// A turn whose only event is the user message it answers.
  fn user_turn() -> Vec<Value> {
    vec![json!({"type": "user", "id": "u1", "content": "go on", "received_at": "2026-11-19T10:00:00Z"})]
  }

  /// A web turn with no Waiting on you item: the input every wake case starts from.
  fn web_turn(session_id: &str) -> TurnInput {
    turn(session_id, Some("web"), 0, json!({"goal": "a goal", "waiting": []}), user_turn())
  }

  /// The first miss of the check, as the turn-end runner records it.
  fn block_record() -> Value {
    json!({
      "type": TURN_HOOKS,
      "id": "record-1",
      "results": [{"name": PENDING_WAKE_ENTRY, "result": "block", "reason": BLOCK_REASON}],
    })
  }

  async fn answer(home: &Home, input: &TurnInput) -> CheckResult {
    check_pending_wake(&home.features, input).await.expect("the check answers")
  }

  /// Rewrites the metadata of `session_id` after `update`.
  async fn update_metadata(home: &Home, session_id: &str, update: impl FnOnce(&mut SessionMetadata)) {
    let mut meta = home.meta(session_id).await;
    update(&mut meta);
    let text = meta.to_json_string(&home.features.registry.metadata_slots(), true);
    fs::write(home.features.store.metadata_path(session_id), text).expect("the metadata writes");
  }

  /// A home whose startup state holds the trigger manager, as the server's does.
  fn new_home() -> Home {
    let home = Home::new();
    home
      .features
      .startup
      .insert(Arc::new(charliebot_triggers::triggers::TriggerManager::new(Arc::clone(&home.features))))
      .expect("the trigger manager registers");
    home
  }

  /// Records a run of `session_id` whose process is this test's own, so the run reads as running.
  fn running_run(home: &Home, session_id: &str) {
    let process = charliebot_core::proc_identity::read_pid_stat(std::process::id()).expect("the test process exists");
    let run_path = home.features.store.run_metadata_path(session_id, "run-live");
    fs::create_dir_all(run_path.parent().expect("the run directory")).expect("the run directory");
    let run = json!({
      "id": "run-live",
      "session_id": session_id,
      "kind": "work",
      "input_event_ids": [],
      "started_at": UtcDateTime::now().to_str(),
      "pid": std::process::id(),
      "pid_start": process.pid_start,
    });
    fs::write(run_path, run.to_string()).expect("the run record");
  }

  /// Writes one pending trigger for `session_id`.
  fn add_pending_trigger(home: &Home, session_id: &str, trigger_id: &str) {
    let directory = home.features.store.session_dir(session_id).join("triggers");
    fs::create_dir_all(&directory).expect("the triggers directory");
    let record = json!({
      "id": trigger_id,
      "session_id": session_id,
      "fire_at": "2026-11-22T00:00:00Z",
      "message": "wake",
      "status": "pending",
    });
    fs::write(directory.join(format!("{trigger_id}.json")), record.to_string()).expect("the trigger record");
  }

  #[tokio::test]
  async fn a_turn_that_leaves_no_wake_is_blocked_with_the_check_text() {
    let home = new_home();
    let root = home.manager("root", None).await;
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Block(BLOCK_REASON.to_string()));
  }

  #[tokio::test]
  async fn a_miss_after_a_block_in_the_same_turn_passes_with_the_notice_only() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let mut events = user_turn();
    events.push(block_record());
    let input = turn(&root, Some("web"), 0, json!({"goal": "a goal", "waiting": []}), events);
    assert_eq!(answer(&home, &input).await, CheckResult::Notice(NOTICE.to_string()));
  }

  #[tokio::test]
  async fn a_pending_trigger_of_the_session_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    add_pending_trigger(&home, &root, "t0");
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_running_worker_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let worker = home.worker(&root, "worker").await;
    running_run(&home, &worker);
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_worker_holding_a_trigger_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let worker = home.worker(&root, "worker").await;
    add_pending_trigger(&home, &worker, "t0");
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_worker_listing_waiting_on_you_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let worker = home.worker(&root, "worker").await;
    update_metadata(&home, &worker, |meta| {
      meta.set_extra(WEB_STATUS, json!({"waiting": [{"text": "an answer"}]}));
    })
    .await;
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn an_open_child_manager_holding_a_running_worker_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let manager = home.child(&root, "manager").await;
    let grandchild = home.worker(&manager, "grandchild").await;
    running_run(&home, &grandchild);
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_waiting_on_you_item_of_the_turn_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let input = turn(&root, Some("web"), 0, json!({"goal": "a goal", "waiting": [{"text": "an answer"}]}), user_turn());
    assert_eq!(answer(&home, &input).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_close_request_a_thread_and_a_turn_that_exited_1_pass() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let mut close = user_turn();
    close.push(json!({"type": TASK_CLOSE_REQUESTED, "id": "c1"}));
    let cases = [
      ("a close request in this turn", turn(&root, Some("web"), 0, json!({"waiting": []}), close)),
      ("a Slack thread", turn(&root, Some("slack"), 0, json!({"waiting": []}), user_turn())),
      ("a turn that exited 1", turn(&root, Some("web"), 1, json!({"waiting": []}), user_turn())),
    ];
    for (label, input) in cases {
      assert_eq!(answer(&home, &input).await, CheckResult::Pass, "{label}");
    }
  }

  #[tokio::test]
  async fn a_failed_continuation_that_leaves_no_wake_gets_the_second_miss_notice() {
    let home = new_home();
    let root = home.manager("root", None).await;
    let mut events = user_turn();
    events.push(json!({
      "type": TURN_HOOKS,
      "id": "record-1",
      "results": [{"name": PENDING_WAKE_ENTRY, "result": "block", "reason": "pending-wake check: blocked"}],
    }));
    let mut input = turn(&root, Some("web"), 1, json!({"goal": "a goal", "waiting": []}), events);
    input.continuation_failed = true;
    assert_eq!(answer(&home, &input).await, CheckResult::Notice(NOTICE.to_string()));
  }

  #[tokio::test]
  async fn a_running_101st_child_on_the_last_page_passes() {
    let home = new_home();
    let root = home.manager("root", None).await;
    for index in 0..101 {
      home.seed_worker(&format!("child-{index:03}"), Some(&root));
    }
    running_run(&home, "child-100");
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Pass);
  }

  #[tokio::test]
  async fn an_open_idle_101st_child_on_the_last_page_blocks() {
    let home = new_home();
    let root = home.manager("root", None).await;
    for index in 0..101 {
      home.seed_worker(&format!("child-{index:03}"), Some(&root));
    }
    assert_eq!(answer(&home, &web_turn(&root)).await, CheckResult::Block(BLOCK_REASON.to_string()));
  }
}
