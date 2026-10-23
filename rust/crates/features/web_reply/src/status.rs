//! A web session's status (`src/features/web_reply/status.py`): the Goal, Now and Waiting on you items that
//! `charliebot web status` sets and reads. A set stores the items its JSON object names, records one `web_status`
//! event and tells every ancestor page. A set refuses a Waiting on you item until a cold read of it is recorded. A read
//! lists the open descendants whose status holds Waiting on you items and the open children that hold a Now.

use std::collections::HashSet;
use std::fs;
use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_compat::file_urls;
use charliebot_compat::python_json::{self, LoadError};
use charliebot_core::event_types::{WEB_ASK_CHECK, WEB_STATUS};
use charliebot_core::pyfmt::{JsonStyle, json_dumps};
use charliebot_core::pystr;
use charliebot_registry::FeatureContext;
use charliebot_store::compact::CompactValue;
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
const NOW_BUDGET_CHARS: usize = 200;
const ASK_CHECK_CONTENT: &str = "Cold read of a Waiting on you item";
const ITEM_SHA256: &str = "item_sha256";

/// One Waiting on you item: a decision or an action, its context, and the link to the place where the user answers it
/// (`metadata.WaitingItem`). An absent context stays absent in stored legacy items.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitingItem {
  text: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  context: Option<String>,
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

/// One `WaitingItem` parsed with the `item` model's refusal (`status.parse_item`).
pub fn parse_item(raw: &Value) -> Result<WaitingItem, WebReplyError> {
  let mut problems = Vec::new();
  let item = waiting_item(raw, "", &mut problems);
  if !problems.is_empty() {
    let summary = problems.iter().map(|problem| format!("{}: {}", problem.loc, problem.msg)).collect::<Vec<_>>();
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("Waiting item is invalid: {}", summary.join("; ")),
    ));
  }
  Ok(item.expect("a valid waiting item has text"))
}

/// `json.dumps({...}, sort_keys=True, ensure_ascii=False)`: the item payload that the digest covers.
const ITEM_DIGEST_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: true, item_separator: ", ", key_separator: ": ", indent: None };

/// `status.item_sha256`: the SHA-256 that names an item as it stands, its three fields and then the bytes of the host
/// file its link names, when that path is a file. A link to a missing path or to a directory adds no bytes.
fn item_sha256(item: &WaitingItem) -> Result<String, WebReplyError> {
  let payload = json!({"text": &item.text, "context": &item.context, "link": &item.link});
  let mut bytes = json_dumps(&payload, &ITEM_DIGEST_JSON).into_bytes();
  let host_path = file_urls::host_file(item.link.as_deref()).map_err(anyhow::Error::msg)?;
  if let Some(path) = host_path
    && file_urls::is_file(&path).map_err(|error| WebReplyError::Failed(error.into()))?
  {
    bytes.extend(fs::read(path).map_err(|error| WebReplyError::Failed(error.into()))?);
  }
  Ok(charliebot_store::control_events::sha256_bytes_hex(&bytes))
}

/// The digests of all `web_ask_check` events in the session.
async fn read_item_digests(features: &FeatureContext, session_id: &str) -> Result<HashSet<String>, WebReplyError> {
  let events = reply::load_events(features, session_id).await?;
  Ok(
    events
      .iter()
      .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(WEB_ASK_CHECK))
      .filter_map(|event| event.get(ITEM_SHA256).and_then(CompactValue::as_str).map(str::to_string))
      .collect(),
  )
}

/// Refuse a waiting item with no non-blank context before its digest is checked.
fn require_waiting_context(items: &[WaitingItem]) -> Result<(), WebReplyError> {
  if let Some(item) = items.iter().find(|item| item.context.is_none()) {
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("Waiting item needs context: {}", item.text),
    ));
  }
  Ok(())
}

/// The items whose current digest is absent from the read set.
fn unread_items(items: &[WaitingItem], read_digests: &HashSet<String>) -> Result<Vec<WaitingItem>, WebReplyError> {
  items
    .iter()
    .filter_map(|item| match item_sha256(item) {
      Ok(digest) if read_digests.contains(&digest) => None,
      Ok(_) => Some(Ok(item.clone())),
      Err(error) => Some(Err(error)),
    })
    .collect()
}

/// Record one cold read of `raw_item` and return its digest (`status.record_ask_check`).
pub async fn record_ask_check(
  features: &FeatureContext,
  session_id: &str,
  raw_item: &Value,
  backend: &str,
  answer: &str,
) -> Result<Value, WebReplyError> {
  reply::require_web_session(features, session_id).await?;
  let item = parse_item(raw_item)?;
  let digest = item_sha256(&item)?;
  features
    .store
    .persist_and_broadcast(
      session_id,
      json!({
        "type": WEB_ASK_CHECK,
        "content": ASK_CHECK_CONTENT,
        ITEM_SHA256: &digest,
        "backend": backend,
        "answer": answer,
      }),
    )
    .await?;
  tracing::info!(session = %session_id, backend, "web_ask_check_recorded");
  Ok(json!({ITEM_SHA256: digest}))
}

/// Return the waiting items in `text` whose current digest has no matching ask-check event.
pub async fn unread_waiting(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  reply::require_web_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  reply::refuse_dead_links(scanned_texts(&patch), &reply::file_link_scan_context(features, session_id)?)?;
  let items = patch.waiting.as_deref().unwrap_or_default();
  require_waiting_context(items)?;
  let read_digests = read_item_digests(features, session_id).await?;
  let unread = unread_items(items, &read_digests)?;
  Ok(json!({"unread": unread}))
}

/// `Text` for a present value: the non-blank string it holds, or a problem for a blank string or any other type.
fn text_value(value: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<String> {
  match value {
    Value::String(text) if pystr::strip(text).is_empty() => {
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
  items
    .iter()
    .enumerate()
    .filter_map(|(index, item)| waiting_item(item, &format!("waiting.{index}"), problems))
    .collect()
}

/// One `WaitingItem`: its required non-blank `text`, optional non-blank `context`, optional `link`, then extra keys.
fn waiting_item(item: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<WaitingItem> {
  let Value::Object(fields) = item else {
    problems.push(Problem { loc: loc.to_string(), msg: NOT_A_WAITING_ITEM });
    return None;
  };
  let text = match fields.get("text") {
    None => {
      problems.push(Problem { loc: nested_loc(loc, "text"), msg: FIELD_REQUIRED });
      None
    }
    Some(value) => text_value(value, &nested_loc(loc, "text"), problems),
  };
  let context = match fields.get("context") {
    None | Some(Value::Null) => None,
    Some(value) => text_value(value, &nested_loc(loc, "context"), problems),
  };
  let link = match fields.get("link") {
    None | Some(Value::Null) => None,
    Some(Value::String(link)) => Some(link.clone()),
    Some(_) => {
      problems.push(Problem { loc: nested_loc(loc, "link"), msg: NOT_A_STRING });
      None
    }
  };
  for key in fields.keys().filter(|key| !matches!(key.as_str(), "text" | "context" | "link")) {
    problems.push(Problem { loc: nested_loc(loc, key), msg: EXTRA_FORBIDDEN });
  }
  text.map(|text| WaitingItem { text, context, link })
}

/// A nested Pydantic location, including the root model's empty location.
fn nested_loc(prefix: &str, field: &str) -> String {
  if prefix.is_empty() { field.to_string() } else { format!("{prefix}.{field}") }
}

/// The texts of a patch that the file-link scanner reads, in its order: goal, now, then each waiting item's text,
/// context and link.
fn scanned_texts(patch: &StatusPatch) -> Vec<&str> {
  let mut texts: Vec<&str> = patch.goal.iter().chain(patch.now.iter()).flatten().map(String::as_str).collect();
  for item in patch.waiting.iter().flatten() {
    texts.push(item.text.as_str());
    texts.extend([item.context.as_deref(), item.link.as_deref()].into_iter().flatten());
  }
  texts
}

/// `status.set_status`: store the items the JSON `text` names, record one `web_status` event and notify the
/// ancestors. Waiting items need a non-blank context and a matching cold-read digest. The return adds the Now length;
/// a refusal stores and records nothing.
pub async fn set_status(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  reply::refuse_dead_links(scanned_texts(&patch), &reply::file_link_scan_context(features, session_id)?)?;
  if let Some(items) = patch.waiting.as_deref() {
    require_waiting_context(items)?;
    let read_digests = read_item_digests(features, session_id).await?;
    let unread = unread_items(items, &read_digests)?;
    if !unread.is_empty() {
      let names = unread.iter().map(|item| item.text.as_str()).collect::<Vec<_>>().join("; ");
      return Err(WebReplyError::refused(
        StatusCode::CONFLICT,
        format!("Waiting on you items need a cold read: {names}"),
      ));
    }
  }
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
  let now_chars = status.now.as_ref().map_or(0, |value| value.chars().count());
  let mut result = payload.as_object().expect("a web status serializes as an object").clone();
  result.insert("now_chars".to_string(), json!(now_chars));
  result.insert("now_over_budget".to_string(), json!(now_chars > NOW_BUDGET_CHARS));
  Ok(Value::Object(result))
}

/// The sessions among `session_ids` with a status, an open task and no archived marker.
async fn open_statuses(
  store: &Arc<Store>,
  session_ids: Vec<String>,
  archived: &HashSet<String>,
) -> Result<Vec<(String, SessionMetadata, WebStatus)>, WebReplyError> {
  let mut statuses = Vec::new();
  for session_id in session_ids {
    let Some(meta) = store.get_session(&session_id).await? else {
      continue; // deleted between the task-tree index read and this metadata read
    };
    let Some(status) = stored_status(&meta)? else {
      continue;
    };
    if meta.status == SessionStatus::Archived || archived.contains(&session_id) {
      continue;
    }
    if task_state(store, &session_id).await? != "open" {
      continue;
    }
    statuses.push((session_id, meta, status));
  }
  Ok(statuses)
}

/// `status.read_status`: the session's status, waiting items of open descendants and Now items of open children, newest
/// first. A descendant needs waiting items; a child needs a Now value.
pub async fn read_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let stored = stored_status(&meta)?;
  let archived = derived_archived_ids(&features.store).await?.into_iter().collect::<HashSet<_>>();
  let mut rows: Vec<(UtcDateTime, Value)> = Vec::new();
  for (descendant_id, child, child_status) in
    open_statuses(&features.store, descendant_ids(&features.store, session_id).await?, &archived).await?
  {
    if child_status.waiting.is_empty() {
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
  let child_ids = tree_read(&features.store, {
    let session_id = session_id.to_string();
    move |store| store.child_ids(&session_id)
  })
  .await?;
  let mut children: Vec<(UtcDateTime, Value)> = Vec::new();
  for (child_id, child, child_status) in open_statuses(&features.store, child_ids, &archived).await? {
    let Some(now) = child_status.now.as_ref() else {
      continue;
    };
    let row = json!({
      "session_id": child_id,
      "name": child.name,
      "now": now,
      "updated_at": child_status.updated_at.to_python_string(),
    });
    children.push((child_status.updated_at, row));
  }
  children.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  Ok(json!({
    "session_id": session_id,
    "web_status": stored.as_ref().map(payload_of),
    "descendants": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    "children": children.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
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
  use std::path::PathBuf;
  use std::time::Duration;

  use axum::http::Method;
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
    json!({"waiting": [waiting_item(text)]}).to_string()
  }

  /// A waiting item whose current state can pass the status set's cold-read gate.
  fn waiting_item(text: &str) -> Value {
    json!({"text": text, "context": "The decision is active now. Options are continue or stop; continue is recommended."})
  }

  /// Records a cold read for the waiting item used by `waiting_set`.
  async fn check_waiting_item(home: &Home, session_id: &str, text: &str) {
    record_ask_check(
      &home.features,
      session_id,
      &waiting_item(text),
      "synthetic-haiku",
      "The options are continue or stop.",
    )
    .await
    .expect("the waiting item cold read records");
  }

  /// The status response without its transient Now budget fields.
  fn without_budget(status: &Value) -> Value {
    let mut result = status.as_object().expect("a status response is an object").clone();
    result.remove("now_chars");
    result.remove("now_over_budget");
    Value::Object(result)
  }

  /// The link form the file server accepts for `path`.
  fn file_link(path: &std::path::Path) -> String {
    format!("https://synthetic.invalid{}{}", file_urls::FILE_SERVER_MOUNT, path.display())
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
    check_waiting_item(&home, &sid, "Pick a layout").await;

    let first = set_status(
      &home.features,
      &sid,
      &json!({"goal": "Ship the panel", "waiting": [waiting_item("Pick a layout")]}).to_string(),
    )
    .await
    .expect("the first set stores");
    let second = set_status(&home.features, &sid, VALID_SET).await.expect("the second set stores");

    assert_eq!(second["goal"], json!("Ship the panel"));
    assert_eq!(second["now"], json!("Running"));
    assert_eq!(
      second["waiting"],
      json!([{
        "text": "Pick a layout",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null
      }])
    );
    assert_eq!(second["now_chars"], json!(7));
    assert_eq!(second["now_over_budget"], json!(false));
    assert_ne!(second["updated_at"], first["updated_at"]);
    assert_eq!(stored(&home, &sid).await, without_budget(&second));
    let events = status_events(&home, &sid);
    assert_eq!(
      events.iter().map(|event| event["web_status_changed"].clone()).collect::<Vec<_>>(),
      vec![json!(["goal", "waiting"]), json!(["now"]),]
    );
    assert_eq!(events[1]["content"], json!("Status updated: Now"));
    assert_eq!(events[1][WEB_STATUS], without_budget(&second));
    assert!(events[1][WEB_STATUS].get("now_chars").is_none());
  }

  #[tokio::test]
  async fn a_null_goal_clears_the_goal_and_keeps_the_other_items() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, r#"{"goal": "Ship the panel", "now": "Running"}"#).await.expect("the set stores");

    let stored_status = set_status(&home.features, &sid, r#"{"goal": null}"#).await.expect("the clear stores");

    assert_eq!((&stored_status["goal"], &stored_status["now"]), (&Value::Null, &json!("Running")));
    assert_eq!(stored_status["now_chars"], json!(7));
    assert_eq!(stored_status["now_over_budget"], json!(false));
    assert_eq!(status_events(&home, &sid).last().expect("two events")["web_status_changed"], json!(["goal"]));
  }

  #[tokio::test]
  async fn a_status_set_reports_the_unicode_now_length_and_keeps_budget_fields_out_of_storage() {
    for (chars, over_budget) in [(200, false), (201, true)] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let now = "界".repeat(chars);

      let response = set_status(&home.features, &sid, &json!({"now": now}).to_string())
        .await
        .expect("the Now stores even when over budget");

      assert_eq!(response["now_chars"], json!(chars));
      assert_eq!(response["now_over_budget"], json!(over_budget));
      assert_eq!(stored(&home, &sid).await, without_budget(&response));
      assert!(status_events(&home, &sid)[0][WEB_STATUS].get("now_chars").is_none());
      assert!(status_events(&home, &sid)[0][WEB_STATUS].get("now_over_budget").is_none());
    }
  }

  #[tokio::test]
  async fn a_status_set_without_now_reports_zero_chars() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let response = set_status(&home.features, &sid, r#"{"goal": "Ship"}"#).await.expect("the set stores");

    assert_eq!(response["now_chars"], json!(0));
    assert_eq!(response["now_over_budget"], json!(false));
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
      check_waiting_item(&home, child, "decision").await;
      set_status(&home.features, child, &waiting_set("decision")).await.expect("the child sets");
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
    set_status(&home.features, &empty, r#"{"waiting": []}"#).await.expect("the empty child sets");
    check_waiting_item(&home, &closed, "closed decision").await;
    set_status(&home.features, &closed, &waiting_set("closed decision")).await.expect("the closed child sets");
    check_waiting_item(&home, &archived, "archived decision").await;
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
    assert_eq!(
      body["descendants"][0]["waiting"],
      json!([{
        "text": "decision",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null
      }])
    );
  }

  #[tokio::test]
  async fn the_status_read_lists_open_direct_children_with_a_now_newest_first() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let old = home.child(&root, "old").await;
    let new = home.child(&root, "new").await;
    let deep = home.child(&old, "deep").await;
    let empty = home.child(&root, "empty").await;
    let closed = home.child(&root, "closed").await;
    let archived = home.child(&root, "archived").await;
    set_status(&home.features, &old, r#"{"now": "Old step"}"#).await.expect("the old child sets");
    tokio::time::sleep(Duration::from_millis(2)).await;
    set_status(&home.features, &new, r#"{"now": "New step"}"#).await.expect("the new child sets");
    set_status(&home.features, &deep, r#"{"now": "Deep step"}"#).await.expect("the deep child sets");
    set_status(&home.features, &empty, r#"{"goal": "No Now"}"#).await.expect("the empty child sets");
    set_status(&home.features, &closed, r#"{"now": "Closed step"}"#).await.expect("the closed child sets");
    set_status(&home.features, &archived, r#"{"now": "Archived step"}"#).await.expect("the archived child sets");
    home
      .features
      .store
      .archive_subtree(&closed, charliebot_store::TaskCaller::Operator)
      .await
      .expect("the task closes");
    home.features.store.archive_session(&archived).await.expect("the session archives");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert_eq!(body["children"].as_array().unwrap().len(), 2);
    assert_eq!(
      body["children"].as_array().unwrap().iter().map(|row| row["session_id"].as_str().unwrap()).collect::<Vec<_>>(),
      [new.as_str(), old.as_str()]
    );
    assert_eq!(
      body["children"].as_array().unwrap().iter().map(|row| row["now"].as_str().unwrap()).collect::<Vec<_>>(),
      ["New step", "Old step"]
    );
  }

  #[test]
  fn an_item_digest_names_its_fields_and_linked_file_bytes() {
    let directory = tempfile::tempdir().expect("the digest directory exists");
    let page = directory.path().join("page.html");
    fs::write(&page, "first").expect("the linked page writes");
    let item = parse_item(&json!({
      "text": "Pick a layout",
      "context": "Why now: the panel ships today.",
      "link": file_link(&page),
    }))
    .expect("the waiting item validates");

    let digest = item_sha256(&item).expect("the linked page reads");
    let changed = [
      parse_item(&json!({"text": "Pick a colour", "context": item.context, "link": item.link})).unwrap(),
      parse_item(&json!({"text": item.text, "context": "Why now: the panel ships tomorrow.", "link": item.link}))
        .unwrap(),
      parse_item(&json!({"text": item.text, "context": item.context, "link": null})).unwrap(),
    ];
    let mut changed_digests = changed.iter().map(item_sha256).collect::<Result<HashSet<_>, _>>().unwrap();
    fs::write(&page, "second").expect("the linked page changes");
    changed_digests.insert(item_sha256(&item).expect("the changed linked page reads"));

    assert_eq!(digest.len(), 64);
    assert!(!changed_digests.contains(&digest));
    assert_eq!(changed_digests.len(), 4);
  }

  #[test]
  fn an_item_digest_matches_the_python_payload_for_an_item_without_a_link() {
    let item = parse_item(&json!({
      "text": "Pick a layout",
      "context": "Options: grid or list. Grid fits the panel; ships today é界.",
      "link": null,
    }))
    .expect("the waiting item validates");

    assert_eq!(
      item_sha256(&item).expect("an item without a link digests"),
      "c534de57a0d3cc97bf8ef2de09dfe0365098f087ad13f381715ab9bb43c6e737"
    );
  }

  #[test]
  fn a_link_names_a_host_file_as_python_splits_it() {
    let names = |link: &str| file_urls::host_file(Some(link)).expect("the link splits");

    assert_eq!(names("/tmp/page.html"), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names(" /tmp/page.html"), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names("/tmp/page.html?"), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names("/tmp/page.html#"), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names("///tmp/page.html"), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names(&file_link(std::path::Path::new("/tmp/page.html"))), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(names("/?session=synthetic"), None);
    assert_eq!(names("https://github.com/org/repo/pull/1"), None);
    assert_eq!(names("relative/page.html"), None);
    assert_eq!(file_urls::host_file(None).expect("an absent link splits"), None);
    assert_eq!(file_urls::host_file(Some("")).expect("an empty link splits"), None);
  }

  #[test]
  fn an_item_digest_reads_bare_files_and_ignores_remote_links() {
    let directory = tempfile::tempdir().expect("the digest directory exists");
    let page = directory.path().join("page.html");
    fs::write(&page, "page bytes").expect("the linked page writes");
    let bare = parse_item(&json!({"text": "Pick a layout", "link": page.to_string_lossy()})).unwrap();
    let served = parse_item(&json!({"text": "Pick a layout", "link": file_link(&page)})).unwrap();
    let remote = parse_item(&json!({"text": "Pick a layout", "link": "https://github.com/org/repo/pull/1"})).unwrap();
    let route = parse_item(&json!({"text": "Pick a layout", "link": "/?session=synthetic"})).unwrap();

    assert_eq!(item_sha256(&bare).unwrap().len(), 64);
    assert_ne!(item_sha256(&bare).unwrap(), item_sha256(&served).unwrap());
    assert_eq!(item_sha256(&remote).unwrap().len(), 64);
    assert_eq!(item_sha256(&route).unwrap().len(), 64);
  }

  #[test]
  fn an_item_digest_adds_no_bytes_for_a_missing_file_or_a_directory() {
    let directory = tempfile::tempdir().expect("the digest directory exists");
    let page_directory = directory.path().join("page-directory");
    fs::create_dir(&page_directory).expect("the linked directory is created");
    let missing = parse_item(&json!({
      "text": "Pick a layout",
      "link": file_link(&directory.path().join("missing.html")),
    }))
    .expect("the waiting item validates");
    let linked_directory = parse_item(&json!({"text": "Pick a layout", "link": file_link(&page_directory)}))
      .expect("the waiting item validates");

    let digests = [item_sha256(&missing), item_sha256(&linked_directory)];

    assert!(digests.iter().all(Result::is_ok), "a missing file and a directory add no bytes: {digests:?}");
    for digest in digests {
      assert_eq!(digest.expect("checked above").len(), 64);
    }
  }

  #[tokio::test]
  async fn a_waiting_link_to_a_directory_can_be_cold_read_and_stored() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let directory = home.root().join("linked-directory");
    fs::create_dir(&directory).unwrap();
    let item = json!({
      "text": "Review the directory",
      "context": "The directory lists the generated pages. Options are browse or wait; browse is recommended.",
      "link": format!("{}{}", file_urls::FILE_SERVER_MOUNT, directory.display()),
    });
    let (status, _) = home
      .send(
        Method::POST,
        "/api/internal/web/ask-check",
        Some(json!({
          "session_id": sid,
          "item": item.clone(),
          "backend": "synthetic-haiku",
          "answer": "(1) Browse. (2) Wait.",
        })),
      )
      .await;
    assert_eq!(status, StatusCode::OK);

    let text = json!({"waiting": [item.clone()]}).to_string();
    let (status, response) =
      home.send(Method::POST, "/api/internal/web/status", Some(json!({"session_id": sid, "text": text}))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["waiting"], json!([item.clone()]));
    assert_eq!(stored(&home, &sid).await["waiting"], json!([item]));
  }

  #[tokio::test]
  async fn dead_links_in_every_status_field_refuse_before_any_cold_read_or_write() {
    for (endpoint, uri) in [("set", "/api/internal/web/status"), ("unread", "/api/internal/web/status/unread")] {
      for field in ["goal", "now", "waiting_text", "waiting_context", "waiting_link"] {
        let home = Home::new();
        let sid = home.manager("session", None).await;
        let port = home.features.config.get().expect("the config loads").server.port;
        let session_dir = home.features.store.session_dir(&sid);
        let missing = home.root().join("missing-status-page.html");
        let canonical = format!("http://localhost:{port}{}{}", file_urls::FILE_SERVER_MOUNT, missing.display());
        let local_target = "artifacts/missing-status-page.html";
        let local_link = format!("[the page]({local_target})");
        let local_correct = format!("{}{}", file_urls::FILE_SERVER_MOUNT, session_dir.join(local_target).display());
        let local_detail = format!(
          "Dead file links; nothing was recorded.\n- {local_target}: a local path opens nothing on the web page. \
           Correct link: {local_correct}"
        );
        let canonical_detail =
          format!("Dead file links; nothing was recorded.\n- {canonical}: no file exists at {}.", missing.display());
        let mut item = json!({
          "text": "Decide",
          "context": "The choice is ready. Options are keep or change; keep is recommended.",
          "link": null,
        });
        let (patch, detail) = match field {
          "goal" => (json!({"goal": format!("Review {local_link}")}), local_detail),
          "now" => (json!({"now": format!("Review {local_link}")}), local_detail),
          "waiting_text" => {
            item["text"] = json!(format!("Review {local_link}"));
            (json!({"waiting": [item]}), local_detail)
          }
          "waiting_context" => {
            item["context"] = json!(format!("Review {local_link}"));
            (json!({"waiting": [item]}), local_detail)
          }
          _ => {
            item["link"] = json!(canonical);
            (json!({"waiting": [item]}), canonical_detail)
          }
        };
        let events_before = home.events(&sid).len();

        let (status, response) =
          home.send(Method::POST, uri, Some(json!({"session_id": sid, "text": patch.to_string()}))).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{endpoint} {field}");
        assert_eq!(response["detail"], json!(detail), "{endpoint} {field}");
        assert_eq!(stored(&home, &sid).await, Value::Null, "{endpoint} {field}");
        assert_eq!(home.events(&sid).len(), events_before, "{endpoint} {field}");
      }
    }
  }

  #[tokio::test]
  async fn an_ask_check_records_the_item_digest_and_stays_out_of_the_for_you_layer() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = json!({"text": "Pick a layout", "context": "The panel folds context.", "link": null});
    let answer = "(1) Pick a layout.\n(2) NOT STATED.";

    let response = record_ask_check(&home.features, &sid, &item, "haiku", answer).await.expect("the ask check records");

    let digest = item_sha256(&parse_item(&item).unwrap()).unwrap();
    assert_eq!(response, json!({"item_sha256": digest}));
    let checks = home.events(&sid).into_iter().filter(|event| event["type"] == WEB_ASK_CHECK).collect::<Vec<_>>();
    assert_eq!(checks.len(), 1);
    assert_eq!(
      json!({
        "type": checks[0]["type"],
        "content": checks[0]["content"],
        "item_sha256": checks[0]["item_sha256"],
        "backend": checks[0]["backend"],
        "answer": checks[0]["answer"],
      }),
      json!({
        "type": WEB_ASK_CHECK,
        "content": ASK_CHECK_CONTENT,
        "item_sha256": digest,
        "backend": "haiku",
        "answer": answer,
      })
    );
    let events = home.events(&sid);
    assert!(reply::for_you_entries(&home.features.store, &events).unwrap().is_empty());
    let compact = events.iter().map(charliebot_store::ChatEvent::from_value).collect::<Vec<_>>();
    let messages = home.features.store.events_to_messages(&compact).expect("the ask-check event renders");
    assert!(messages.iter().any(|message| message["role"] == WEB_ASK_CHECK));
  }

  #[tokio::test]
  async fn refused_ask_checks_record_nothing() {
    for (case, item, expected) in [
      ("blank", json!({"text": " "}), 422),
      ("extra", json!({"text": "Decide", "due": "today"}), 422),
      ("missing", json!({}), 422),
    ] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let before = home.events(&sid);

      let error =
        record_ask_check(&home.features, &sid, &item, "haiku", "answer").await.expect_err("the item is refused");

      assert!(matches!(error, WebReplyError::Refused { status, .. } if status.as_u16() == expected), "{case}");
      assert_eq!(home.events(&sid), before, "{case}");
    }
    for case in ["unknown", "thread", "worker"] {
      let home = Home::new();
      let origin = (case == "thread")
        .then(|| ("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"})));
      let root = home.manager("session", origin).await;
      let sid = if case == "worker" { home.worker(&root, "worker").await } else { root };
      let target = if case == "unknown" { "no-such-session" } else { sid.as_str() };
      let before = home.events(&sid);

      let error = record_ask_check(&home.features, target, &json!({"text": "Decide"}), "haiku", "answer")
        .await
        .expect_err("the session is refused");

      let expected = if case == "unknown" { 404 } else { 409 };
      assert!(matches!(error, WebReplyError::Refused { status, .. } if status.as_u16() == expected), "{case}");
      assert_eq!(home.events(&sid), before, "{case}");
    }
  }

  #[tokio::test]
  async fn the_unread_query_tracks_ask_check_and_linked_file_changes_without_storing_status() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let directory = tempfile::tempdir().expect("the status page directory exists");
    let page = directory.path().join("page.html");
    fs::write(&page, "first").expect("the status page writes");
    let item = json!({
      "text": "Decide the sample size",
      "context": "The sweep costs $5 an hour. Options: 1k or 10k; 1k is recommended.",
      "link": file_link(&page),
    });
    let text = json!({"waiting": [item]}).to_string();

    let without_waiting = unread_waiting(&home.features, &sid, r#"{"now": "Checking"}"#)
      .await
      .expect("a status file without waiting items lists nothing");
    assert_eq!(without_waiting, json!({"unread": []}));
    let before = unread_waiting(&home.features, &sid, &text).await.expect("the unread query answers");
    assert_eq!(before, json!({"unread": [item]}));
    let digest = record_ask_check(&home.features, &sid, &item, "haiku", "1k is recommended.")
      .await
      .expect("the ask check records");
    assert_eq!(digest["item_sha256"], item_sha256(&parse_item(&item).unwrap()).unwrap());
    assert_eq!(
      unread_waiting(&home.features, &sid, &text).await.expect("the read item is not unread"),
      json!({"unread": []})
    );
    fs::write(&page, "second").expect("the status page changes");
    assert_eq!(
      unread_waiting(&home.features, &sid, &text).await.expect("the changed item is unread"),
      json!({"unread": [item]})
    );
    assert_eq!(stored(&home, &sid).await, Value::Null);
    assert!(status_events(&home, &sid).is_empty());
    assert_eq!(home.events(&sid).iter().filter(|event| event["type"] == WEB_ASK_CHECK).count(), 1);
  }

  #[tokio::test]
  async fn an_unread_query_refuses_an_item_without_context_and_records_nothing() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let before = home.events(&sid);

    let error = unread_waiting(&home.features, &sid, r#"{"waiting": [{"text": "Decide"}]}"#)
      .await
      .expect_err("an item without context is refused");

    assert!(matches!(error, WebReplyError::Refused { status: StatusCode::UNPROCESSABLE_ENTITY, .. }));
    assert_eq!(home.events(&sid), before);
  }

  #[tokio::test]
  async fn a_status_set_requires_nonblank_context_and_a_matching_current_digest() {
    let without_context = [
      json!({"text": "Approve the deploy"}),
      json!({"text": "Approve the deploy", "context": null}),
      json!({"text": "Approve the deploy", "context": " "}),
    ];
    for raw in without_context {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let error = set_status(&home.features, &sid, &json!({"waiting": [raw]}).to_string())
        .await
        .expect_err("a waiting item without context is refused");
      assert!(matches!(error, WebReplyError::Refused { status: StatusCode::UNPROCESSABLE_ENTITY, .. }));
      assert_eq!(stored(&home, &sid).await, Value::Null);
      assert!(status_events(&home, &sid).is_empty());
      assert!(home.events(&sid).iter().all(|event| event["type"] != WEB_ASK_CHECK));
    }

    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = json!({
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": null,
    });
    let text = json!({"waiting": [item]}).to_string();
    let error = set_status(&home.features, &sid, &text).await.expect_err("an unread item is refused");
    assert!(matches!(error, WebReplyError::Refused { status: StatusCode::CONFLICT, .. }));
    assert!(status_events(&home, &sid).is_empty());

    record_ask_check(&home.features, &sid, &item, "haiku", "deploy is recommended")
      .await
      .expect("the cold read records");
    let response = set_status(&home.features, &sid, &text).await.expect("the read item stores");
    assert_eq!(
      response["waiting"],
      json!([{
        "text": "Approve the deploy",
        "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
        "link": null,
      }])
    );
    assert_eq!(stored(&home, &sid).await, without_budget(&response));
    assert_eq!(status_events(&home, &sid).len(), 1);
  }

  #[tokio::test]
  async fn a_changed_linked_page_makes_a_status_item_unread_again() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let directory = tempfile::tempdir().expect("the status page directory exists");
    let page = directory.path().join("page.html");
    fs::write(&page, "first page").expect("the status page writes");
    let item = json!({
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": file_link(&page),
    });
    record_ask_check(&home.features, &sid, &item, "haiku", "deploy is recommended")
      .await
      .expect("the cold read records");
    fs::write(&page, "changed page").expect("the status page changes");

    let error = set_status(&home.features, &sid, &json!({"waiting": [item]}).to_string())
      .await
      .expect_err("the changed page invalidates the read");

    assert!(matches!(error, WebReplyError::Refused { status: StatusCode::CONFLICT, .. }));
    assert_eq!(stored(&home, &sid).await, Value::Null);
    assert!(status_events(&home, &sid).is_empty());
    assert_eq!(home.events(&sid).iter().filter(|event| event["type"] == WEB_ASK_CHECK).count(), 1);
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

  #[tokio::test]
  async fn repeated_status_reads_reuse_the_archive_verdict_and_task_state_folds() {
    let home = Home::new();
    let root = home.manager("status-root", None).await;
    let child = home.child(&root, "status-child").await;
    home.append(&child, json!({"type": "user", "content": "a pending question"})).await;
    let index_before = home.features.store.task_tree_index_build_count();

    read_status(&home.features, &root).await.expect("the status reads");

    let index_after_first = home.features.store.task_tree_index_build_count();
    let state_after_first = home.features.store.history_work().full_folds;
    assert_eq!(index_after_first - index_before, 1);
    read_status(&home.features, &root).await.expect("the status reads again");
    assert_eq!(home.features.store.task_tree_index_build_count(), index_after_first);
    assert_eq!(home.features.store.history_work().full_folds, state_after_first);
  }
}
