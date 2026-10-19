//! A web session's status (`src/features/web_reply/status.py`): the Goal, Now and Waiting on you items that
//! `charliebot web status` sets and reads. A set stores the items its JSON object names, records one `web_status`
//! event and tells every ancestor page. A read lists the open descendants whose status holds Waiting on you items.

use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_compat::python_json::{self, LoadError};
use charliebot_core::event_types::WEB_STATUS;
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, session_channel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::json_input::replace_unrepresentable_numbers;
use crate::reply::{self, WebReplyError};

/// The owner the web reply package registers its metadata under (`status.OWNER`).
pub const OWNER: &str = "web_reply";

/// The metadata field that holds the status, under the owner.
const STATUS_FIELD: &str = "web_status";

/// The status items in the order a set lists them and the status line names them (`status.ITEMS`).
const ITEMS: [&str; 3] = ["goal", "now", "waiting"];

/// The label of each item in the event's content (`status.LABELS`).
fn label(item: &str) -> &'static str {
  match item {
    "goal" => "Goal",
    "now" => "Now",
    _ => "Waiting on you",
  }
}

/// The non-persisted frame an ancestor's page receives when a descendant sets its status (`SUBTREE_NOTICE_TYPE`).
const SUBTREE_NOTICE_TYPE: &str = "web_status_subtree";

/// The pydantic messages a status file can break, as `StatusPatch` and `WaitingItem` raise them.
const MUST_NOT_BE_BLANK: &str = "Value error, must not be blank";
const NOT_A_STRING: &str = "Input should be a valid string";
const NOT_A_LIST: &str = "Input should be a valid list";
const NOT_A_WAITING_ITEM: &str = "Input should be a valid dictionary or instance of WaitingItem";
const FIELD_REQUIRED: &str = "Field required";
const EXTRA_FORBIDDEN: &str = "Extra inputs are not permitted";

/// One Waiting on you item: a decision or an action, and the link to the place where the user answers it
/// (`metadata.WaitingItem`). Its fields serialize in the model's order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitingItem {
  text: String,
  link: Option<String>,
}

/// The session status: Goal, Now and Waiting on you, and the time the last set stored them (`metadata.WebStatus`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebStatus {
  goal: Option<String>,
  now: Option<String>,
  waiting: Vec<WaitingItem>,
  updated_at: UtcDateTime,
}

/// The validated `StatusPatch`. An item the object leaves out is `None`; an item it names is `Some`, and a null goal
/// or now is `Some(None)`, which clears the stored value.
#[derive(Debug)]
pub struct StatusPatch {
  goal: Option<Option<String>>,
  now: Option<Option<String>>,
  waiting: Option<Vec<WaitingItem>>,
}

impl StatusPatch {
  /// The items the patch names, in `ITEMS` order.
  fn named(&self) -> Vec<&'static str> {
    let mut named = Vec::new();
    if self.goal.is_some() {
      named.push("goal");
    }
    if self.now.is_some() {
      named.push("now");
    }
    if self.waiting.is_some() {
      named.push("waiting");
    }
    named
  }
}

/// One validation failure: the location pydantic reports (`waiting.0.text`) and its message.
struct Problem {
  loc: String,
  msg: &'static str,
}

/// The status a session stores, or `None` before its first set (`status._stored`).
fn stored_status(meta: &SessionMetadata) -> Result<Option<WebStatus>, WebReplyError> {
  match meta.extra.get(STATUS_FIELD) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => Ok(Some(serde_json::from_value(value.clone()).map_err(|error| {
      WebReplyError::Failed(anyhow::anyhow!("session {} holds an unreadable web status: {error}", meta.id))
    })?)),
  }
}

/// CPython's `JSONDecodeError.msg`: the message without the `: line L column C (char P)` that the error text adds.
fn json_message(error_text: &str) -> &str {
  error_text.rsplit_once(": line ").map_or(error_text, |(msg, _)| msg)
}

/// The patch that the JSON `text` holds (`status.parse_patch`). A refusal is a 422 whose detail an agent reads.
pub fn parse_patch(text: &str) -> Result<StatusPatch, WebReplyError> {
  let raw = match python_json::loads_text(text) {
    Ok(raw) => raw,
    Err(LoadError::Value(message)) => {
      return Err(WebReplyError::refused(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("Status file is not JSON: {}", json_message(&message)),
      ));
    }
    Err(LoadError::Unrepresentable(message)) => {
      serde_json::from_str(&replace_unrepresentable_numbers(text)).map_err(|_| {
        WebReplyError::Failed(anyhow::anyhow!("the status file holds a value the port cannot read: {message}"))
      })?
    }
  };
  let Value::Object(fields) = raw else {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Status file is not a JSON object"));
  };
  let mut problems = Vec::new();
  // pydantic validates the fields in declaration order, each with its nested errors, then the object's extra keys.
  let goal = fields.get("goal").map(|value| nullable_text(value, "goal", &mut problems));
  let now = fields.get("now").map(|value| nullable_text(value, "now", &mut problems));
  let waiting = fields.get("waiting").map(|value| waiting_items(value, &mut problems));
  for key in fields.keys().filter(|key| !ITEMS.contains(&key.as_str())) {
    problems.push(Problem { loc: key.clone(), msg: EXTRA_FORBIDDEN });
  }
  if !problems.is_empty() {
    let summary = problems.iter().map(|problem| format!("{}: {}", problem.loc, problem.msg)).collect::<Vec<_>>();
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("Status file is invalid: {}", summary.join("; ")),
    ));
  }
  let patch = StatusPatch { goal, now, waiting };
  if patch.named().is_empty() {
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      "Status file names no item: give goal, now or waiting",
    ));
  }
  Ok(patch)
}

/// `Text` for a present value: the non-blank string it holds, or a problem for a blank string or any other type.
fn text_value(value: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<String> {
  match value {
    Value::String(text) if reply::py_strip(text).is_empty() => {
      problems.push(Problem { loc: loc.to_string(), msg: MUST_NOT_BE_BLANK });
      None
    }
    Value::String(text) => Some(text.clone()),
    _ => {
      problems.push(Problem { loc: loc.to_string(), msg: NOT_A_STRING });
      None
    }
  }
}

/// `Text | None` for a present `goal` or `now`: `None` for a null, else the text.
fn nullable_text(value: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<String> {
  match value {
    Value::Null => None,
    _ => text_value(value, loc, problems),
  }
}

/// `list[WaitingItem]` for a present `waiting` value: each item validated, invalid items reported with their index.
fn waiting_items(value: &Value, problems: &mut Vec<Problem>) -> Vec<WaitingItem> {
  let Value::Array(items) = value else {
    problems.push(Problem { loc: "waiting".to_string(), msg: NOT_A_LIST });
    return Vec::new();
  };
  items.iter().enumerate().filter_map(|(index, item)| waiting_item(item, index, problems)).collect()
}

/// One `WaitingItem`: its required non-blank `text`, its optional `link`, then its extra keys.
fn waiting_item(item: &Value, index: usize, problems: &mut Vec<Problem>) -> Option<WaitingItem> {
  let loc = format!("waiting.{index}");
  let Value::Object(fields) = item else {
    problems.push(Problem { loc, msg: NOT_A_WAITING_ITEM });
    return None;
  };
  let text = match fields.get("text") {
    None => {
      problems.push(Problem { loc: format!("{loc}.text"), msg: FIELD_REQUIRED });
      None
    }
    Some(value) => text_value(value, &format!("{loc}.text"), problems),
  };
  let link = match fields.get("link") {
    None | Some(Value::Null) => None,
    Some(Value::String(link)) => Some(link.clone()),
    Some(_) => {
      problems.push(Problem { loc: format!("{loc}.link"), msg: NOT_A_STRING });
      None
    }
  };
  for key in fields.keys().filter(|key| !matches!(key.as_str(), "text" | "link")) {
    problems.push(Problem { loc: format!("{loc}.{key}"), msg: EXTRA_FORBIDDEN });
  }
  text.map(|text| WaitingItem { text, link })
}

/// `status.set_status`: store the items the JSON `text` names, record one `web_status` event and notify the
/// ancestors. Returns the stored status. A refusal stores and records nothing.
pub async fn set_status(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  let now = UtcDateTime::now();
  let mut status =
    stored_status(&meta)?.unwrap_or(WebStatus { goal: None, now: None, waiting: Vec::new(), updated_at: now });
  let changed = patch.named();
  let StatusPatch { goal, now: now_item, waiting } = patch;
  if let Some(goal) = goal {
    status.goal = goal;
  }
  if let Some(now_item) = now_item {
    status.now = now_item;
  }
  if let Some(waiting) = waiting {
    status.waiting = waiting;
  }
  status.updated_at = now;
  let payload = payload_of(&status);
  {
    let guard = features.store.control_lock().await;
    let values = Map::from_iter([(STATUS_FIELD.to_string(), payload.clone())]);
    features.store.update_task_slot_fields_locked(&guard, session_id, OWNER, values).await?;
  }
  let labels = changed.iter().map(|item| label(item)).collect::<Vec<_>>();
  features
    .store
    .persist_and_broadcast(
      session_id,
      json!({
        "type": WEB_STATUS,
        "content": format!("Status updated: {}", labels.join(", ")),
        WEB_STATUS: payload.clone(),
        "web_status_changed": changed,
      }),
    )
    .await?;
  for ancestor_id in ancestor_ids(&features.store, session_id).await? {
    let notice = json!({"type": SUBTREE_NOTICE_TYPE, "source_session_id": session_id});
    features.store.broadcast(&session_channel(&ancestor_id), notice).await?;
  }
  tracing::info!(session = %session_id, changed = ?changed, "web_status_set");
  Ok(payload)
}

/// `status.read_status`: the session's stored status and the Waiting on you items of its open descendants, newest
/// first. A descendant qualifies when its status holds a waiting item, its session is not archived and its task is open.
pub async fn read_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let stored = stored_status(&meta)?;
  let archived = derived_archived_ids(&features.store).await?;
  let mut rows: Vec<(UtcDateTime, Value)> = Vec::new();
  for descendant_id in descendant_ids(&features.store, session_id).await? {
    let Some(child) = features.store.get_session(&descendant_id).await? else {
      continue; // deleted between the index read and this one
    };
    let Some(child_status) = stored_status(&child)? else {
      continue;
    };
    if child_status.waiting.is_empty() {
      continue;
    }
    if child.status == SessionStatus::Archived || archived.contains(&descendant_id) {
      continue;
    }
    if task_state(&features.store, &descendant_id).await? != "open" {
      continue;
    }
    let row = json!({
      "session_id": descendant_id,
      "name": child.name,
      "updated_at": child_status.updated_at.to_python_string(),
      "waiting": child_status.waiting,
    });
    rows.push((child_status.updated_at, row));
  }
  rows.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  Ok(json!({
    "session_id": session_id,
    "web_status": stored.as_ref().map(payload_of),
    "descendants": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
  }))
}

/// The status as `model_dump(mode="json")` renders it.
fn payload_of(status: &WebStatus) -> Value {
  serde_json::to_value(status).expect("a web status serializes to JSON")
}

/// The task-tree reads run off the runtime's worker threads, as the Python tree reads run in `asyncio.to_thread`.
async fn tree_read<T, F>(store: &Arc<Store>, read: F) -> Result<T, WebReplyError>
where
  T: Send + 'static,
  F: FnOnce(&Store) -> charliebot_store::StoreResult<T> + Send + 'static,
{
  let store = Arc::clone(store);
  let answer = tokio::task::spawn_blocking(move || read(&store)).await;
  let answer = answer.map_err(|error| anyhow::anyhow!("a task-tree read stopped: {error}"))?;
  Ok(answer?)
}

async fn ancestor_ids(store: &Arc<Store>, session_id: &str) -> Result<Vec<String>, WebReplyError> {
  let session_id = session_id.to_string();
  tree_read(store, move |store| store.ancestor_ids(&session_id)).await
}

async fn descendant_ids(store: &Arc<Store>, session_id: &str) -> Result<Vec<String>, WebReplyError> {
  let session_id = session_id.to_string();
  tree_read(store, move |store| store.descendant_ids(&session_id)).await
}

async fn derived_archived_ids(store: &Arc<Store>) -> Result<Vec<String>, WebReplyError> {
  tree_read(store, |store| store.derived_archived_ids()).await
}

async fn task_state(store: &Arc<Store>, session_id: &str) -> Result<String, WebReplyError> {
  let session_id = session_id.to_string();
  tree_read(store, move |store| store.task_state(&session_id)).await
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use charliebot_store::Subscription;
  use serde_json::json;

  use super::*;
  use crate::test_support::Home;

  const VALID_SET: &str = r#"{"now": "Running"}"#;

  /// The status the session's metadata stores, as the session's slot holds it.
  async fn stored(home: &Home, session_id: &str) -> Value {
    home.meta(session_id).await.extra.get(STATUS_FIELD).cloned().unwrap_or(Value::Null)
  }

  /// The `web_status` events of the session's log, in log order.
  fn status_events(home: &Home, session_id: &str) -> Vec<Value> {
    home.events(session_id).into_iter().filter(|event| event["type"] == WEB_STATUS).collect()
  }

  /// The refusal detail of a status write or patch.
  fn detail(error: WebReplyError) -> String {
    match error {
      WebReplyError::Refused { detail, .. } => detail,
      other => panic!("expected a refusal, got {other:?}"),
    }
  }

  /// A waiting item whose text is `text`, as a status file names it.
  fn waiting_set(text: &str) -> String {
    json!({"waiting": [{"text": text}]}).to_string()
  }

  /// The frames a page has received, read until the page goes quiet.
  async fn drain(page: &mut Subscription) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(text)) = tokio::time::timeout(Duration::from_millis(20), page.recv()).await {
      frames.push(serde_json::from_str::<Value>(&text).expect("a frame is JSON"));
    }
    frames
  }

  #[tokio::test]
  async fn a_status_set_of_now_alone_keeps_the_other_items_and_records_one_event() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let first = set_status(
      &home.features,
      &sid,
      r#"{"goal": "Ship the panel", "waiting": [{"text": "Pick a layout", "link": "/p"}]}"#,
    )
    .await
    .expect("the first set stores");
    let second = set_status(&home.features, &sid, VALID_SET).await.expect("the second set stores");

    assert_eq!(second["goal"], json!("Ship the panel"));
    assert_eq!(second["now"], json!("Running"));
    assert_eq!(second["waiting"], json!([{"text": "Pick a layout", "link": "/p"}]));
    assert_ne!(second["updated_at"], first["updated_at"]);
    assert_eq!(stored(&home, &sid).await, second);
    let events = status_events(&home, &sid);
    assert_eq!(
      events.iter().map(|event| event["web_status_changed"].clone()).collect::<Vec<_>>(),
      vec![json!(["goal", "waiting"]), json!(["now"]),]
    );
    assert_eq!(events[1]["content"], json!("Status updated: Now"));
    assert_eq!(events[1][WEB_STATUS], second);
  }

  #[tokio::test]
  async fn a_null_goal_clears_the_goal_and_keeps_the_other_items() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, r#"{"goal": "Ship the panel", "now": "Running"}"#).await.expect("the set stores");

    let stored_status = set_status(&home.features, &sid, r#"{"goal": null}"#).await.expect("the clear stores");

    assert_eq!((&stored_status["goal"], &stored_status["now"]), (&Value::Null, &json!("Running")));
    assert_eq!(status_events(&home, &sid).last().expect("two events")["web_status_changed"], json!(["goal"]));
  }

  #[tokio::test]
  async fn a_refused_status_set_stores_nothing_and_records_no_event() {
    let cases = [
      ("unknown", VALID_SET, 404),
      ("thread", VALID_SET, 409),
      ("worker", VALID_SET, 409),
      ("own", "not json", 422),
      ("own", "[1, 2]", 422),
      ("own", r#"{"colour": "red"}"#, 422),
      ("own", "{}", 422),
      ("own", r#"{"goal": " "}"#, 422),
      ("own", r#"{"waiting": [{"text": " "}]}"#, 422),
      ("own", r#"{"waiting": [{"text": "Decide", "due": "today"}]}"#, 422),
    ];
    for (case, text, status) in cases {
      let home = Home::new();
      let origin = (case == "thread")
        .then(|| ("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"})));
      let root = home.manager("session", origin).await;
      let sid = if case == "worker" { home.worker(&root, "worker").await } else { root };
      let target = if case == "unknown" { "no-such-session" } else { sid.as_str() };

      let error = set_status(&home.features, target, text).await.expect_err("the set is refused");

      match error {
        WebReplyError::Refused { status: code, .. } => assert_eq!(code.as_u16(), status, "case {case} {text}"),
        other => panic!("case {case} {text}: expected a refusal, got {other:?}"),
      }
      assert_eq!(stored(&home, &sid).await, Value::Null, "case {case} {text}");
      assert!(status_events(&home, &sid).is_empty(), "case {case} {text}");
    }
  }

  #[test]
  fn each_422_detail_reads_as_pydantic_and_the_status_module_words_it() {
    let cases = [
      ("not json", "Status file is not JSON: Expecting value"),
      (r#"{"now": "a""#, "Status file is not JSON: Expecting ',' delimiter"),
      ("[1, 2]", "Status file is not a JSON object"),
      ("{}", "Status file names no item: give goal, now or waiting"),
      (r#"{"colour": "red"}"#, "Status file is invalid: colour: Extra inputs are not permitted"),
      (r#"{"goal": " "}"#, "Status file is invalid: goal: Value error, must not be blank"),
      (r#"{"now": true}"#, "Status file is invalid: now: Input should be a valid string"),
      (r#"{"waiting": "x"}"#, "Status file is invalid: waiting: Input should be a valid list"),
      (
        r#"{"waiting": [5]}"#,
        "Status file is invalid: waiting.0: Input should be a valid dictionary or instance of WaitingItem",
      ),
      (r#"{"waiting": [{}]}"#, "Status file is invalid: waiting.0.text: Field required"),
      (
        r#"{"waiting": [{"text": "a", "link": 5}]}"#,
        "Status file is invalid: waiting.0.link: Input should be a valid string",
      ),
      (
        r#"{"waiting": [{"text": "Decide", "due": "today"}]}"#,
        "Status file is invalid: waiting.0.due: Extra inputs are not permitted",
      ),
      (
        r#"{"colour": 1, "goal": 5, "waiting": [{"text": 5, "zzz": 1}]}"#,
        "Status file is invalid: goal: Input should be a valid string; waiting.0.text: Input should be a valid string; waiting.0.zzz: Extra inputs are not permitted; colour: Extra inputs are not permitted",
      ),
      (r#"{"goal": 123456789012345678901234567890}"#, "Status file is invalid: goal: Input should be a valid string"),
    ];
    for (text, expected) in cases {
      let error = parse_patch(text).expect_err("the status file is refused");
      assert_eq!(detail(error), expected, "text {text}");
    }
  }

  #[tokio::test]
  async fn a_child_status_set_notifies_its_parent_and_grandparent_pages_and_a_root_set_notifies_none() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let child = home.child(&root, "child").await;
    let grandchild = home.child(&child, "grandchild").await;
    let mut root_page = home.subscribe(&root);
    let mut child_page = home.subscribe(&child);

    set_status(&home.features, &grandchild, VALID_SET).await.expect("the grandchild sets");
    set_status(&home.features, &root, VALID_SET).await.expect("the root sets");

    // Each page also receives its own status message; the ancestor notices are the frames this test compares.
    let notice = json!({"type": SUBTREE_NOTICE_TYPE, "source_session_id": grandchild});
    let notices = |frames: Vec<Value>| -> Vec<Value> {
      frames.into_iter().filter(|frame| frame["type"] == SUBTREE_NOTICE_TYPE).collect()
    };
    assert_eq!(notices(drain(&mut child_page).await), vec![notice.clone()]);
    assert_eq!(notices(drain(&mut root_page).await), vec![notice]);
  }

  #[tokio::test]
  async fn the_status_read_lists_open_waiting_descendants_newest_first() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let old = home.child(&root, "old").await;
    let new = home.child(&root, "new").await;
    let deep = home.child(&old, "deep").await;
    let empty = home.child(&root, "empty").await;
    let closed = home.child(&root, "closed").await;
    let archived = home.child(&root, "archived").await;
    for child in [&old, &new, &deep] {
      set_status(&home.features, child, &waiting_set("decision")).await.expect("the child sets");
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
    set_status(&home.features, &empty, r#"{"waiting": []}"#).await.expect("the empty child sets");
    set_status(&home.features, &closed, &waiting_set("closed decision")).await.expect("the closed child sets");
    set_status(&home.features, &archived, &waiting_set("archived decision")).await.expect("the archived child sets");
    home
      .features
      .store
      .archive_subtree(&closed, charliebot_store::TaskCaller::Operator)
      .await
      .expect("the task closes");
    home.features.store.archive_session(&archived).await.expect("the session archives");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert_eq!(body["web_status"], Value::Null);
    let ids: Vec<&str> =
      body["descendants"].as_array().expect("a list").iter().map(|row| row["session_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![deep.as_str(), new.as_str(), old.as_str()]);
    assert_eq!(body["descendants"][0]["waiting"], json!([{"text": "decision", "link": null}]));
  }

  #[tokio::test]
  async fn the_status_read_refuses_an_unknown_thread_or_worker_session() {
    for (case, status) in [("unknown", 404), ("thread", 409), ("worker", 409)] {
      let home = Home::new();
      let origin = (case == "thread")
        .then(|| ("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"})));
      let root = home.manager("session", origin).await;
      let sid = if case == "worker" { home.worker(&root, "worker").await } else { root };
      let target = if case == "unknown" { "no-such-session" } else { sid.as_str() };

      let error = read_status(&home.features, target).await.expect_err("the read is refused");

      match error {
        WebReplyError::Refused { status: code, .. } => assert_eq!(code.as_u16(), status, "case {case}"),
        other => panic!("case {case}: expected a refusal, got {other:?}"),
      }
    }
  }

  #[tokio::test]
  async fn a_status_event_leaves_the_unread_mark_unchanged() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    assert!(!home.meta(&sid).await.has_unread);

    set_status(&home.features, &sid, VALID_SET).await.expect("the set stores");
    assert!(!home.meta(&sid).await.has_unread);

    home.features.store.mark_unread(&sid).await.expect("the session is marked");
    set_status(&home.features, &sid, r#"{"now": "Done"}"#).await.expect("the set stores");
    assert!(home.meta(&sid).await.has_unread);
  }
}
