//! A web session's status (`src/features/web_reply/status.py`): the Goal, Now, Waiting on you and Remaining items
//! that `charliebot web status` sets and reads. A set stores the items its JSON object names, records one `web_status`
//! event and tells every ancestor page. A set refuses a Waiting on you item until a cold read of it is recorded; the
//! read tiers the item against the approval policy table, and with the table's refusal switch on a set refuses an
//! item tiered t1 or t2. A read lists the open descendants whose status holds Waiting on you items and the open
//! children that hold a Now.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_compat::file_urls;
use charliebot_compat::file_urls::ScanContext;
use charliebot_compat::json_loads::{self, LoadError};
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types::{WEB_ASK_CHECK, WEB_REPLY, WEB_STATUS, WEB_STATUS_CONFIRMED};
use charliebot_core::str_methods;
use charliebot_core::text_repr::{JsonStyle, json_dumps};
use charliebot_registry::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, session_channel};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::json_input::replace_unrepresentable_numbers;
use crate::metadata::{RemainingItem, WaitingItem};
use crate::plan_links::{self, PlanLinks};
use crate::reply;
pub use crate::reply::WebReplyError;

/// The owner the web reply package registers its metadata under (`status.OWNER`).
pub const OWNER: &str = WEB_REPLY;

/// The metadata field that holds the status, under the owner.
const STATUS_FIELD: &str = WEB_STATUS;

/// The status items in the order a set lists them and the status line names them (`status.ITEMS`).
const ITEMS: [&str; 4] = ["goal", "now", "waiting", "remaining"];

/// The label of each item in the event's content (`status.LABELS`).
fn label(item: &str) -> &'static str {
  match item {
    "goal" => "Goal",
    "now" => "Now",
    "remaining" => "Remaining",
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
const NOT_A_REMAINING_ITEM: &str = "Input should be a valid dictionary or instance of RemainingItem";
const FIELD_REQUIRED: &str = "Field required";
const EXTRA_FORBIDDEN: &str = "Extra inputs are not permitted";
const NOW_BUDGET_CHARS: usize = 200;
const ASK_CHECK_CONTENT: &str = "Cold read of a Waiting on you item";
const ITEM_SHA256: &str = "item_sha256";
/// The item fields that only the server writes; a set file carries neither (`status._refuse_server_fields`).
const SERVER_WRITTEN_ITEM_FIELDS: [&str; 4] = ["since", "since_exact", "tier", "rule"];
/// The pydantic message a since field that is not a datetime raises.
const NOT_A_DATETIME: &str = "Input should be a valid datetime";
/// The pydantic message a since_exact field that is not a boolean raises.
const NOT_A_BOOLEAN: &str = "Input should be a valid boolean";

/// The session status: Goal, Now, Waiting on you and Remaining, and the time the last set stored them
/// (`metadata.WebStatus`). A status without Remaining serializes without either Remaining key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebStatus {
  goal: Option<String>,
  now: Option<String>,
  waiting: Vec<WaitingItem>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  remaining: Vec<RemainingItem>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  remaining_updated_at: Option<UtcDateTime>,
  #[serde(default = "UtcDateTime::now")]
  updated_at: UtcDateTime,
}

/// The validated `StatusPatch`. An item the object leaves out is `None`; an item it names is `Some`, and a null goal
/// or now is `Some(None)`, which clears the stored value.
#[derive(Debug)]
pub struct StatusPatch {
  goal: Option<Option<String>>,
  now: Option<Option<String>>,
  waiting: Option<Vec<WaitingItem>>,
  remaining: Option<Vec<RemainingItem>>,
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
    if self.remaining.is_some() {
      named.push("remaining");
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
  let raw = match json_loads::loads_text(text) {
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
  refuse_server_fields(&fields)?;
  let mut problems = Vec::new();
  // pydantic validates the fields in declaration order, each with its nested errors, then the object's extra keys.
  let goal = fields.get("goal").map(|value| nullable_text(value, "goal", &mut problems));
  let now = fields.get("now").map(|value| nullable_text(value, "now", &mut problems));
  let waiting = fields.get("waiting").map(|value| waiting_items(value, &mut problems));
  let remaining = fields.get("remaining").map(|value| remaining_items(value, &mut problems));
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
  let patch = StatusPatch { goal, now, waiting, remaining };
  if patch.named().is_empty() {
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      "Status file names no item: give goal, now, waiting or remaining",
    ));
  }
  Ok(patch)
}

/// Refuse a set file that names a field only the server writes (`status._refuse_server_fields`): the status's
/// `remaining_updated_at`, or a waiting item's `since` and `since_exact`. Only a list `waiting` is examined, and only
/// its object items.
fn refuse_server_fields(fields: &Map<String, Value>) -> Result<(), WebReplyError> {
  if fields.contains_key("remaining_updated_at") {
    return Err(WebReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      "Status field remaining_updated_at is written by the server; remove it from the file",
    ));
  }
  let Some(Value::Array(waiting)) = fields.get("waiting") else {
    return Ok(());
  };
  for raw_item in waiting {
    let Value::Object(item) = raw_item else { continue };
    let written: Vec<&str> =
      SERVER_WRITTEN_ITEM_FIELDS.iter().copied().filter(|name| item.contains_key(*name)).collect();
    if !written.is_empty() {
      return Err(WebReplyError::refused(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("Waiting item fields {} are written by the server; remove them from the file", written.join(", ")),
      ));
    }
  }
  Ok(())
}

/// The status's waiting items with `since` and `since_exact` filled in (`status.effective_waiting`).
///
/// An item stored before the fields existed has no start time of its own; the status's `updated_at` is the lower
/// bound, so the item reads with `since_exact` false.
fn effective_waiting(status: &WebStatus) -> Vec<WaitingItem> {
  status
    .waiting
    .iter()
    .map(|item| match (item.since, item.since_exact) {
      (Some(_), Some(_)) => item.clone(),
      _ => WaitingItem {
        since: Some(item.since.unwrap_or(status.updated_at)),
        since_exact: Some(item.since_exact.unwrap_or(false)),
        ..item.clone()
      },
    })
    .collect()
}

/// The status as the API and the CLI return it, with each waiting item's start time filled in
/// (`status.status_payload`).
fn status_payload(status: &WebStatus) -> Value {
  let mut payload = payload_of(status);
  payload["waiting"] = serde_json::to_value(effective_waiting(status)).expect("waiting items serialize to JSON");
  payload
}

/// Refuse `items` when any of their links names an older version of a registered plan or a closed plan, before the
/// cold read runs; return the lookup the since pairing then uses (`status._require_current_plan_links`).
fn require_current_plan_links(
  features: &FeatureContext,
  items: &[WaitingItem],
  scan_context: &ScanContext,
) -> Result<PlanLinks, WebReplyError> {
  let read_plans = features.registry.plan_registry_reader().map_err(WebReplyError::Failed)?;
  let mut links = PlanLinks::new(sessions_dir_in(&scan_context.charliebot_home), read_plans);
  let stale = links.stale_links(items, scan_context)?;
  if !stale.is_empty() {
    return Err(WebReplyError::refused(StatusCode::CONFLICT, plan_links::stale_detail(&stale)));
  }
  Ok(links)
}

/// The new items with each one's start time kept from the stored item it continues (`status._assign_since`).
///
/// An item whose text equals a stored item's text keeps that item's time. The remaining items pair in order within
/// groups of one link target: a registered plan lineage, another link, or no link at all.
fn assign_since(
  new_items: &[WaitingItem],
  stored_items: &[WaitingItem],
  links: &mut PlanLinks,
  now: UtcDateTime,
) -> Result<Vec<WaitingItem>, WebReplyError> {
  let mut available: Vec<Option<&WaitingItem>> = stored_items.iter().map(Some).collect();
  let mut assigned: Vec<Option<WaitingItem>> = vec![None; new_items.len()];
  for (index, item) in new_items.iter().enumerate() {
    let Some(match_slot) = available.iter().position(|stored| stored.is_some_and(|stored| stored.text == item.text))
    else {
      continue;
    };
    let stored = available[match_slot].take().expect("the matched slot holds a stored item");
    assigned[index] = Some(WaitingItem { since: stored.since, since_exact: stored.since_exact, ..item.clone() });
  }
  let mut new_groups: HashMap<String, Vec<usize>> = HashMap::new();
  let mut stored_groups: HashMap<String, Vec<usize>> = HashMap::new();
  for (index, item) in new_items.iter().enumerate() {
    if assigned[index].is_none() {
      new_groups.entry(links.target(item)?).or_default().push(index);
    }
  }
  for (slot, stored) in available.iter().enumerate() {
    if let Some(stored) = stored {
      stored_groups.entry(links.target(stored)?).or_default().push(slot);
    }
  }
  for indices in new_groups.values() {
    let target = links.target(&new_items[indices[0]])?;
    let paired = stored_groups.get(&target).map(Vec::as_slice).unwrap_or_default();
    for (position, index) in indices.iter().enumerate() {
      assigned[*index] = Some(match paired.get(position).and_then(|slot| available[*slot]) {
        Some(stored) => {
          WaitingItem { since: stored.since, since_exact: stored.since_exact, ..new_items[*index].clone() }
        }
        None => WaitingItem { since: Some(now), since_exact: Some(true), ..new_items[*index].clone() },
      });
    }
  }
  let result: Vec<WaitingItem> = assigned.into_iter().flatten().collect();
  assert_eq!(result.len(), new_items.len(), "every new item is assigned a start time");
  Ok(result)
}

/// The items with each one's `tier` and `rule` filled in from the ask-check its digest matches
/// (`status._assign_tiers`). An item whose check predates the tier keeps neither field.
fn assign_tiers(
  items: Vec<WaitingItem>,
  ask_tiers: &HashMap<String, (String, Option<String>)>,
) -> Result<Vec<WaitingItem>, WebReplyError> {
  items
    .into_iter()
    .map(|item| {
      let digest = item_sha256(&item)?;
      Ok(match ask_tiers.get(&digest) {
        Some((tier, rule)) => WaitingItem { tier: Some(tier.clone()), rule: rule.clone(), ..item },
        None => item,
      })
    })
    .collect()
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

/// The tier and rule each `web_ask_check` event gave its item digest, the latest event per digest winning
/// (`status._read_ask_tiers`). A check recorded before the tier existed contributes no entry.
async fn read_ask_tiers(
  features: &FeatureContext,
  session_id: &str,
) -> Result<HashMap<String, (String, Option<String>)>, WebReplyError> {
  let events = reply::load_events(features, session_id).await?;
  let mut tiers = HashMap::new();
  for event in events.iter().filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(WEB_ASK_CHECK)) {
    let Some(digest) = event.get(ITEM_SHA256).and_then(CompactValue::as_str) else {
      continue;
    };
    let Some(tier) = event.get("tier").and_then(CompactValue::as_str) else {
      continue;
    };
    let rule = event.get("rule").and_then(CompactValue::as_str).map(str::to_string);
    tiers.insert(digest.to_string(), (tier.to_string(), rule));
  }
  Ok(tiers)
}

/// The table as the server reads it, with a changed read recording its `policy_table_changed` record
/// (`status._load_approval_policy`).
fn load_approval_policy(
  features: &FeatureContext,
) -> Result<charliebot_core::approval_policy::LoadedTable, WebReplyError> {
  let config = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  charliebot_core::approval_policy::load_and_record(&config.charliebot_home)
    .map_err(|error| WebReplyError::Failed(error.into()))
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

/// Record one cold read of `raw_item` and return its digest (`status.record_ask_check`). The answer's last line
/// tiers the item against the approval policy table; an answer without a parsable tier line records nothing.
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
  let approval = load_approval_policy(features)?;
  let tier = charliebot_core::approval_policy::parse_ask_tier(answer, &approval.table)
    .map_err(|error| WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
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
        "tier": tier.tier,
        "rule": tier.rule,
      }),
    )
    .await?;
  tracing::info!(session = %session_id, backend, tier = %tier.tier, "web_ask_check_recorded");
  Ok(json!({ITEM_SHA256: digest}))
}

/// Return the waiting items in `text` whose current digest has no matching ask-check event.
pub async fn unread_waiting(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  reply::refuse_dead_links(scanned_texts(&patch), &scan_context)?;
  let items = patch.waiting.as_deref().unwrap_or_default();
  require_waiting_context(items)?;
  let stored = stored_status(&meta)?;
  let prospective = match patch.waiting.as_deref() {
    Some(items) => items.to_vec(),
    None => stored.map(|status| status.waiting).unwrap_or_default(),
  };
  require_current_plan_links(features, &prospective, &scan_context)?;
  let read_digests: HashSet<String> = read_ask_tiers(features, session_id).await?.into_keys().collect();
  let unread = unread_items(items, &read_digests)?;
  Ok(json!({"unread": unread}))
}

/// `Text` for a present value: the non-blank string it holds, or a problem for a blank string or any other type.
fn text_value(value: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<String> {
  match value {
    Value::String(text) if str_methods::strip(text).is_empty() => {
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

/// `list[RemainingItem]` for a present `remaining` value: each item validated, invalid items reported with their
/// index.
fn remaining_items(value: &Value, problems: &mut Vec<Problem>) -> Vec<RemainingItem> {
  let Value::Array(items) = value else {
    problems.push(Problem { loc: "remaining".to_string(), msg: NOT_A_LIST });
    return Vec::new();
  };
  items
    .iter()
    .enumerate()
    .filter_map(|(index, item)| remaining_item(item, &format!("remaining.{index}"), problems))
    .collect()
}

/// One `RemainingItem`: its required non-blank `text`, its required boolean `done`, then extra keys.
fn remaining_item(item: &Value, loc: &str, problems: &mut Vec<Problem>) -> Option<RemainingItem> {
  let Value::Object(fields) = item else {
    problems.push(Problem { loc: loc.to_string(), msg: NOT_A_REMAINING_ITEM });
    return None;
  };
  let text = match fields.get("text") {
    None => {
      problems.push(Problem { loc: nested_loc(loc, "text"), msg: FIELD_REQUIRED });
      None
    }
    Some(value) => text_value(value, &nested_loc(loc, "text"), problems),
  };
  let done = match fields.get("done") {
    None => {
      problems.push(Problem { loc: nested_loc(loc, "done"), msg: FIELD_REQUIRED });
      None
    }
    Some(Value::Bool(value)) => Some(*value),
    Some(_) => {
      problems.push(Problem { loc: nested_loc(loc, "done"), msg: NOT_A_BOOLEAN });
      None
    }
  };
  for key in fields.keys().filter(|key| !matches!(key.as_str(), "text" | "done")) {
    problems.push(Problem { loc: nested_loc(loc, key), msg: EXTRA_FORBIDDEN });
  }
  text.zip(done).map(|(text, done)| RemainingItem { text, done })
}

/// One `WaitingItem`: its required non-blank `text`, optional non-blank `context`, optional `link`, the server-written
/// start-time fields, then extra keys.
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
  let since = match fields.get("since") {
    None | Some(Value::Null) => None,
    Some(Value::String(value)) => match UtcDateTime::parse(value) {
      Ok(value) => Some(value),
      Err(_) => {
        problems.push(Problem { loc: nested_loc(loc, "since"), msg: NOT_A_DATETIME });
        None
      }
    },
    Some(_) => {
      problems.push(Problem { loc: nested_loc(loc, "since"), msg: NOT_A_DATETIME });
      None
    }
  };
  let since_exact = match fields.get("since_exact") {
    None | Some(Value::Null) => None,
    Some(Value::Bool(value)) => Some(*value),
    Some(_) => {
      problems.push(Problem { loc: nested_loc(loc, "since_exact"), msg: NOT_A_BOOLEAN });
      None
    }
  };
  let tier = optional_item_string(fields, "tier", loc, problems);
  let rule = optional_item_string(fields, "rule", loc, problems);
  for key in fields
    .keys()
    .filter(|key| !matches!(key.as_str(), "text" | "context" | "link" | "since" | "since_exact" | "tier" | "rule"))
  {
    problems.push(Problem { loc: nested_loc(loc, key), msg: EXTRA_FORBIDDEN });
  }
  text.map(|text| WaitingItem { text, context, link, since, since_exact, tier, rule })
}

/// One server-written string field of a waiting item: absent, null, or a string.
fn optional_item_string(
  fields: &Map<String, Value>,
  name: &str,
  loc: &str,
  problems: &mut Vec<Problem>,
) -> Option<String> {
  match fields.get(name) {
    None | Some(Value::Null) => None,
    Some(Value::String(value)) => Some(value.clone()),
    Some(_) => {
      problems.push(Problem { loc: nested_loc(loc, name), msg: NOT_A_STRING });
      None
    }
  }
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
/// ancestors. Waiting items need a non-blank context and a matching cold-read digest, whose tier and rule the stored
/// item carries; with the approval policy table's refusal switch on, an item tiered t1 or t2 refuses the set. A
/// stale plan link refuses the set before the cold read. The return adds the Now length; a refusal stores and
/// records nothing.
pub async fn set_status(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  let meta = reply::require_web_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  reply::refuse_dead_links(scanned_texts(&patch), &scan_context)?;
  require_waiting_context(patch.waiting.as_deref().unwrap_or_default())?;
  let stored = stored_status(&meta)?;
  let base = stored.clone().unwrap_or(WebStatus {
    goal: None,
    now: None,
    waiting: Vec::new(),
    remaining: Vec::new(),
    remaining_updated_at: None,
    updated_at: UtcDateTime::now(),
  });
  let stored_waiting = effective_waiting(&base);
  let prospective = patch.waiting.clone().unwrap_or_else(|| stored_waiting.clone());
  let mut links = require_current_plan_links(features, &prospective, &scan_context)?;
  let approval = load_approval_policy(features)?;
  let mut ask_tiers = HashMap::new();
  if let Some(items) = patch.waiting.as_deref() {
    ask_tiers = read_ask_tiers(features, session_id).await?;
    let read_digests: HashSet<String> = ask_tiers.keys().cloned().collect();
    let unread = unread_items(items, &read_digests)?;
    if !unread.is_empty() {
      let names = unread.iter().map(|item| item.text.as_str()).collect::<Vec<_>>().join("; ");
      return Err(WebReplyError::refused(
        StatusCode::CONFLICT,
        format!("Waiting on you items need a cold read: {names}"),
      ));
    }
    if approval.table.enable_ask_tier_refusal {
      for item in items {
        let digest = item_sha256(item)?;
        if let Some((tier, rule)) = ask_tiers.get(&digest)
          && matches!(tier.as_str(), "t1" | "t2")
        {
          return Err(WebReplyError::refused(
            StatusCode::CONFLICT,
            format!(
              "Waiting on you item {:?} is tiered {tier} (rule: {}): carry out the step and record it with \
               charliebot digest add instead of waiting for approval",
              item.text,
              rule.as_deref().unwrap_or("none")
            ),
          ));
        }
      }
    }
  }
  let now = UtcDateTime::now();
  let waiting = match patch.waiting.as_deref() {
    Some(items) => assign_tiers(assign_since(items, &stored_waiting, &mut links, now)?, &ask_tiers)?,
    None => stored_waiting,
  };
  let mut status = base;
  let changed = patch.named();
  let StatusPatch { goal, now: now_item, waiting: _, remaining } = patch;
  if let Some(goal) = goal {
    status.goal = goal;
  }
  if let Some(now_item) = now_item {
    status.now = now_item;
  }
  status.waiting = waiting;
  // A named Remaining stores its list and this set's time; an empty list removes both, and an unnamed one keeps both.
  if let Some(items) = remaining {
    status.remaining_updated_at = (!items.is_empty()).then_some(now);
    status.remaining = items;
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

/// `status.confirm_status`: confirm that the stored status needs no change and return it.
///
/// A stale plan link in a stored item refuses the confirmation with 409 and names the newest file. A confirmation
/// records one web_status_confirmed event and cold reads nothing.
pub async fn confirm_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let status = stored_status(&meta)?.unwrap_or(WebStatus {
    goal: None,
    now: None,
    waiting: Vec::new(),
    remaining: Vec::new(),
    remaining_updated_at: None,
    updated_at: UtcDateTime::now(),
  });
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  require_current_plan_links(features, &effective_waiting(&status), &scan_context)?;
  features
    .store
    .persist_and_broadcast(session_id, json!({"type": WEB_STATUS_CONFIRMED, "content": "Status confirmed"}))
    .await?;
  tracing::info!(session = %session_id, "{}", WEB_STATUS_CONFIRMED);
  Ok(status_payload(&status))
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
    let payload = status_payload(&child_status);
    let row = json!({
      "session_id": descendant_id,
      "name": child.name,
      "updated_at": child_status.updated_at.to_str(),
      "waiting": payload["waiting"],
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
      "updated_at": child_status.updated_at.to_str(),
    });
    children.push((child_status.updated_at, row));
  }
  children.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  Ok(json!({
    "session_id": session_id,
    (WEB_STATUS): stored.as_ref().map(status_payload),
    "descendants": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    "children": children.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
  }))
}

/// The status as `model_dump(mode="json")` renders it.
fn payload_of(status: &WebStatus) -> Value {
  serde_json::to_value(status).expect("a web status serializes to JSON")
}

/// One open session's Waiting on you item with the session that holds it (`status.OpenWaitingItem`). The start time
/// is resolved, as the status read resolves it.
#[derive(Clone, Debug, Serialize)]
pub struct OpenWaitingItem {
  pub session_id: String,
  pub session_name: String,
  pub text: String,
  pub context: Option<String>,
  pub link: Option<String>,
  pub tier: Option<String>,
  pub rule: Option<String>,
  pub since: UtcDateTime,
}

/// Every open session's Waiting on you items, in metadata-listing order (`status.open_waiting_items`). The approval
/// day view reads the whole home through this, where the status read reads one task tree.
pub async fn open_waiting_items(features: &FeatureContext) -> Result<Vec<OpenWaitingItem>, WebReplyError> {
  let archived = derived_archived_ids(&features.store).await?.into_iter().collect::<HashSet<_>>();
  let metas = tree_read(&features.store, |store| store.load_session_metas(None)).await?;
  let mut rows = Vec::new();
  for meta in metas {
    let Some(status) = stored_status(&meta)? else { continue };
    if status.waiting.is_empty() {
      continue;
    }
    if meta.status == SessionStatus::Archived || archived.contains(&meta.id) {
      continue;
    }
    if task_state(&features.store, &meta.id).await? != "open" {
      continue;
    }
    for item in effective_waiting(&status) {
      rows.push(OpenWaitingItem {
        session_id: meta.id.clone(),
        session_name: meta.name.clone(),
        text: item.text,
        context: item.context,
        link: item.link,
        tier: item.tier,
        rule: item.rule,
        since: item.since.expect("effective_waiting fills the start time"),
      });
    }
  }
  Ok(rows)
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
  use crate::test_support::{self, Home};

  const VALID_SET: &str = r#"{"now": "Running"}"#;

  /// Write one archived close fact, as the completion owner's operator archive writes a root's fact.
  async fn archive_task(features: &charliebot_registry::context::FeatureContext, session_id: &str) {
    let guard = features.store.control_lock().await;
    let event = charliebot_store::control_events::build_control_event(
      charliebot_core::event_types::TASK_CLOSED,
      "user",
      session_id,
      None,
      None,
      [
        ("request_id".to_string(), json!("synthetic-status-archive")),
        ("outcome".to_string(), json!("archived")),
        ("summary".to_string(), json!("archived by the user")),
        ("result_refs".to_string(), json!([])),
        ("run_ids".to_string(), json!([])),
        ("report_to".to_string(), serde_json::Value::Null),
      ],
    );
    features.store.append_control_fact(&guard, session_id, event).await.expect("the archive fact appends");
    drop(guard);
    features.store.invalidate_tree_index();
  }

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
      "The options are continue or stop.\ntier: t0 rule: irreversible",
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

  /// The bare file-server path an agent writes as an item link (`_host_file_link`).
  fn host_link(path: &std::path::Path) -> String {
    format!("{}{}", file_urls::FILE_SERVER_MOUNT, path.display())
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
      without_item_times(&second["waiting"]),
      json!([{
        "text": "Pick a layout",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null,
        "tier": "t0",
        "rule": "irreversible"
      }])
    );
    assert_eq!(second["waiting"][0]["since_exact"], json!(true), "a new item starts at the set's time");
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
  async fn a_status_set_with_remaining_stores_the_list_its_update_time_and_the_event() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let text = json!({"remaining": [
      {"text": "Move the server to Rust", "done": true},
      {"text": "Delete the Python server", "done": false},
    ]})
    .to_string();

    let response = set_status(&home.features, &sid, &text).await.expect("the set stores");

    assert_eq!(
      response["remaining"],
      json!([
        {"text": "Move the server to Rust", "done": true},
        {"text": "Delete the Python server", "done": false},
      ])
    );
    assert_eq!(response["remaining_updated_at"], response["updated_at"], "the set's time stamps the list");
    assert_eq!(stored(&home, &sid).await, without_budget(&response));
    let events = status_events(&home, &sid);
    assert_eq!(events[0]["content"], json!("Status updated: Remaining"));
    assert_eq!(events[0]["web_status_changed"], json!(["remaining"]));
    assert_eq!(events[0][WEB_STATUS]["remaining"], response["remaining"]);
    assert_eq!(events[0][WEB_STATUS]["remaining_updated_at"], response["remaining_updated_at"]);

    let (code, read) = home.send(Method::GET, &format!("/api/internal/web/status?session_id={sid}"), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(read["web_status"]["remaining"], response["remaining"]);
    assert_eq!(read["web_status"]["remaining_updated_at"], response["remaining_updated_at"]);
  }

  #[tokio::test]
  async fn a_status_set_without_remaining_keeps_the_stored_list_and_its_update_time() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = set_status(&home.features, &sid, r#"{"remaining": [{"text": "Move the server", "done": true}]}"#)
      .await
      .expect("the first set stores");
    tokio::time::sleep(Duration::from_millis(2)).await;

    let second = set_status(&home.features, &sid, VALID_SET).await.expect("the second set stores");

    assert_eq!(second["remaining"], first["remaining"]);
    assert_eq!(second["remaining_updated_at"], first["remaining_updated_at"], "an unnamed Remaining keeps its time");
    assert_ne!(second["updated_at"], first["updated_at"]);
  }

  #[tokio::test]
  async fn an_empty_remaining_list_removes_the_list_and_its_update_time() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(
      &home.features,
      &sid,
      r#"{"goal": "Ship the panel", "remaining": [{"text": "Move the server", "done": false}]}"#,
    )
    .await
    .expect("the first set stores");

    let cleared = set_status(&home.features, &sid, r#"{"remaining": []}"#).await.expect("the empty list stores");

    assert_eq!(cleared["goal"], json!("Ship the panel"), "the other items stay");
    assert!(cleared.get("remaining").is_none(), "a status without Remaining omits the key");
    assert!(cleared.get("remaining_updated_at").is_none());
    let stored_after = stored(&home, &sid).await;
    assert!(stored_after.get("remaining").is_none());
    assert!(stored_after.get("remaining_updated_at").is_none());
    assert_eq!(status_events(&home, &sid).last().expect("two events")["content"], json!("Status updated: Remaining"));
  }

  #[tokio::test]
  async fn a_status_set_with_an_invalid_remaining_item_is_refused_and_stores_nothing() {
    let cases = [
      r#"{"remaining": "x"}"#,
      r#"{"remaining": [5]}"#,
      r#"{"remaining": [{}]}"#,
      r#"{"remaining": [{"text": " ", "done": true}]}"#,
      r#"{"remaining": [{"text": "a"}]}"#,
      r#"{"remaining": [{"text": "a", "done": "yes"}]}"#,
      r#"{"remaining": [{"text": "a", "done": null}]}"#,
      r#"{"remaining": [{"text": "a", "done": true, "due": "today"}]}"#,
      r#"{"remaining": [], "remaining_updated_at": "2026-11-02T03:10:00Z"}"#,
    ];
    for text in cases {
      let home = Home::new();
      let sid = home.manager("session", None).await;

      let error = set_status(&home.features, &sid, text).await.expect_err("the set is refused");

      match error {
        WebReplyError::Refused { status, .. } => {
          assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}")
        }
        other => panic!("{text}: expected a refusal, got {other:?}"),
      }
      assert_eq!(stored(&home, &sid).await, Value::Null, "{text}");
      assert!(status_events(&home, &sid).is_empty(), "{text}");
    }
  }

  #[tokio::test]
  async fn a_status_stored_without_remaining_reads_back_unchanged() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let legacy =
      json!({"goal": "Ship the panel", "now": "Running", "waiting": [], "updated_at": "2026-10-24T00:00:00Z"});
    write_stored_status(&home, &sid, legacy.clone()).await;

    let (code, read) = home.send(Method::GET, &format!("/api/internal/web/status?session_id={sid}"), None).await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(read["web_status"], legacy, "neither Remaining key appears");
    let written = set_status(&home.features, &sid, r#"{"goal": "Ship the panel"}"#).await.expect("the set stores");
    assert!(written.get("remaining").is_none());
    assert!(written.get("remaining_updated_at").is_none());
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
      ("{}", "Status file names no item: give goal, now, waiting or remaining"),
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
      (r#"{"remaining": "x"}"#, "Status file is invalid: remaining: Input should be a valid list"),
      (
        r#"{"remaining": [5]}"#,
        "Status file is invalid: remaining.0: Input should be a valid dictionary or instance of RemainingItem",
      ),
      (
        r#"{"remaining": [{}]}"#,
        "Status file is invalid: remaining.0.text: Field required; remaining.0.done: Field required",
      ),
      (
        r#"{"remaining": [{"text": " ", "done": true}]}"#,
        "Status file is invalid: remaining.0.text: Value error, must not be blank",
      ),
      (
        r#"{"remaining": [{"text": "a", "done": "yes"}]}"#,
        "Status file is invalid: remaining.0.done: Input should be a valid boolean",
      ),
      (
        r#"{"remaining": [{"text": "a", "done": true, "due": "today"}]}"#,
        "Status file is invalid: remaining.0.due: Extra inputs are not permitted",
      ),
      (
        r#"{"remaining": [{}, {"text": "a"}]}"#,
        "Status file is invalid: remaining.0.text: Field required; remaining.0.done: Field required; remaining.1.done: Field required",
      ),
      (
        r#"{"remaining": [], "remaining_updated_at": "2026-11-02T03:10:00Z"}"#,
        "Status field remaining_updated_at is written by the server; remove it from the file",
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
    archive_task(&home.features, &closed).await;
    home.features.store.archive_session(&archived).await.expect("the session archives");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert_eq!(body["web_status"], Value::Null);
    let ids: Vec<&str> =
      body["descendants"].as_array().expect("a list").iter().map(|row| row["session_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![deep.as_str(), new.as_str(), old.as_str()]);
    assert_eq!(
      without_item_times(&body["descendants"][0]["waiting"]),
      json!([{
        "text": "decision",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null,
        "tier": "t0",
        "rule": "irreversible"
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
    archive_task(&home.features, &closed).await;
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
  fn an_item_digest_matches_the_payload_for_an_item_without_a_link() {
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
  fn a_link_names_a_host_file_as_it_splits() {
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
          "answer": "(1) Browse. (2) Wait.\ntier: t2 rule: none",
        })),
      )
      .await;
    assert_eq!(status, StatusCode::OK);

    let text = json!({"waiting": [item.clone()]}).to_string();
    let (status, response) =
      home.send(Method::POST, "/api/internal/web/status", Some(json!({"session_id": sid, "text": text}))).await;

    assert_eq!(status, StatusCode::OK);
    let mut expected = item.clone();
    expected["tier"] = json!("t2");
    expected["rule"] = Value::Null;
    assert_eq!(without_item_times(&response["waiting"]), json!([expected.clone()]));
    assert_eq!(without_item_times(&stored(&home, &sid).await["waiting"]), json!([expected]));
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
    let answer = "(1) Pick a layout.\n(2) NOT STATED.\ntier: t2 rule: none";

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
        "tier": checks[0]["tier"],
        "rule": checks[0]["rule"],
      }),
      json!({
        "type": WEB_ASK_CHECK,
        "content": ASK_CHECK_CONTENT,
        "item_sha256": digest,
        "backend": "haiku",
        "answer": answer,
        "tier": "t2",
        "rule": Value::Null,
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
  async fn an_ask_check_stores_the_tier_and_rule_each_tier_value_gives() {
    for (answer, tier, rule) in [
      ("Tiered against the table.\ntier: t0 rule: irreversible", "t0", Some("irreversible")),
      (
        "Outside the cap.\ntier: t0 rule: out-of-scope:shared-repo-landing",
        "t0",
        Some("out-of-scope:shared-repo-landing"),
      ),
      ("Inside the policy.\ntier: t1 rule: release-restart", "t1", Some("release-restart")),
      ("The agent decides.\ntier: t2 rule: none", "t2", None),
      ("Only the user knows.\ntier: user-act rule: none", "user-act", None),
      ("Review the delivery.\ntier: review rule: none", "review", None),
    ] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let item = waiting_item("Decide");

      record_ask_check(&home.features, &sid, &item, "haiku", answer).await.expect("the ask check records");

      let checks = home.events(&sid).into_iter().filter(|event| event["type"] == WEB_ASK_CHECK).collect::<Vec<_>>();
      assert_eq!(checks.len(), 1, "{answer}");
      assert_eq!(checks[0]["tier"], tier, "{answer}");
      assert_eq!(checks[0]["rule"], rule.map(Value::from).unwrap_or(Value::Null), "{answer}");
    }
  }

  #[tokio::test]
  async fn an_answer_without_a_parsable_tier_line_records_nothing() {
    for answer in ["", "(1) Decide the sample size.", "tier: t0 rule: irreversible\nthen a trailing note"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let before = home.events(&sid);

      let error = record_ask_check(&home.features, &sid, &waiting_item("Decide"), "haiku", answer)
        .await
        .expect_err("the answer is refused");

      assert!(matches!(error, WebReplyError::Refused { status: StatusCode::UNPROCESSABLE_ENTITY, .. }), "{answer:?}");
      assert!(detail(error).contains("no parsable tier line"), "{answer:?}");
      assert_eq!(home.events(&sid), before, "{answer:?}");
    }
  }

  #[tokio::test]
  async fn the_refusal_switch_refuses_a_t1_item_and_admits_a_t0_one() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let t1 = json!({
      "text": "Restart the server",
      "context": "The release is ready now. Options are restart or wait; restart is recommended.",
    });
    let t0 = json!({
      "text": "Publish the page",
      "context": "The draft is ready now. Options are publish or hold; publish is recommended.",
    });
    let t1_answer = "Inside the policy.\ntier: t1 rule: release-restart";
    let t0_answer = "The table's money test names it.\ntier: t0 rule: money";
    record_ask_check(&home.features, &sid, &t1, "haiku", t1_answer).await.expect("the t1 cold read records");
    record_ask_check(&home.features, &sid, &t0, "haiku", t0_answer).await.expect("the t0 cold read records");

    // Record-only mode, the state at release: both items store.
    let response = set_status(&home.features, &sid, &json!({"waiting": [t1.clone(), t0.clone()]}).to_string())
      .await
      .expect("refusal off stores both items");
    assert_eq!(response["waiting"][0]["tier"], json!("t1"));
    assert_eq!(response["waiting"][1]["tier"], json!("t0"));

    // The switch on: the t1 item refuses the set with its rule, and the t0 item still stores.
    test_support::write_approval_policy_table(
      &home.root(),
      &test_support::APPROVAL_POLICY_TABLE.replace("enable_ask_tier_refusal: false", "enable_ask_tier_refusal: true"),
    );
    let error = set_status(&home.features, &sid, &json!({"waiting": [t1.clone()]}).to_string())
      .await
      .expect_err("the t1 item is refused");
    let detail = detail(error);
    assert!(detail.contains("tiered t1 (rule: release-restart)"), "{detail}");
    assert!(detail.contains("charliebot digest add"), "{detail}");
    set_status(&home.features, &sid, &json!({"waiting": [t0.clone()]}).to_string()).await.expect("the t0 item stores");
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
    let mut unread_item = item.clone();
    unread_item["rule"] = Value::Null;
    assert_eq!(before, json!({"unread": [unread_item]}));
    let digest = record_ask_check(&home.features, &sid, &item, "haiku", "1k is recommended.\ntier: t0 rule: money")
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
      json!({"unread": [unread_item]})
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

    record_ask_check(&home.features, &sid, &item, "haiku", "deploy is recommended\ntier: t0 rule: money")
      .await
      .expect("the cold read records");
    let response = set_status(&home.features, &sid, &text).await.expect("the read item stores");
    assert_eq!(
      without_item_times(&response["waiting"]),
      json!([{
        "text": "Approve the deploy",
        "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
        "link": null,
        "tier": "t0",
        "rule": "money",
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
    record_ask_check(&home.features, &sid, &item, "haiku", "deploy is recommended\ntier: t0 rule: money")
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

  // -------------------------------------------------------------------------
  // Stale plan links: an old version or a closed lineage is refused in set and confirm
  // -------------------------------------------------------------------------

  /// The waiting items without the start-time fields that the server writes (`_without_item_times`).
  fn without_item_times(items: &Value) -> Value {
    Value::Array(
      items
        .as_array()
        .expect("a waiting list")
        .iter()
        .map(|item| {
          let mut item = item.as_object().expect("a waiting item object").clone();
          item.remove("since");
          item.remove("since_exact");
          Value::Object(item)
        })
        .collect(),
    )
  }

  /// The sessions directory the plan-link lookup reads the plan registries from.
  fn sessions_dir(home: &Home) -> PathBuf {
    home.root().join("sessions")
  }

  /// A session artifact page whose bytes are the file name (`_session_artifact`).
  fn session_artifact(home: &Home, session_id: &str, name: &str) -> PathBuf {
    let path = sessions_dir(home).join(session_id).join("artifacts").join(name);
    fs::create_dir_all(path.parent().expect("the artifact directory")).expect("the artifact directory writes");
    fs::write(&path, name).expect("the artifact page writes");
    path
  }

  /// One plan-registry version row (`_plan_version`).
  fn plan_version(v: i64, relative: &str) -> Value {
    json!({
      "v": v,
      "file": relative,
      "created_at": "2026-10-24T00:00:00+00:00",
      "trigger": "initial",
      "base": null,
      "note": null,
    })
  }

  /// One plan-registry plan row (`_plan`).
  fn plan(plan_id: i64, versions: Value, closed: Value) -> Value {
    json!({"id": plan_id, "title": format!("Plan {plan_id}"), "versions": versions, "takeoff": null, "closed": closed})
  }

  /// Writes `plans` as the session's plan registry (`_write_plans`).
  fn write_plans(home: &Home, session_id: &str, plans: Value) {
    let path = sessions_dir(home).join(session_id).join("plans.json");
    fs::create_dir_all(path.parent().expect("the session directory")).expect("the session directory writes");
    fs::write(&path, json!({"plans": plans}).to_string()).expect("the plan registry writes");
  }

  /// Writes `web_status` as the session's stored status field, as a prior set left it.
  async fn write_stored_status(home: &Home, session_id: &str, web_status: Value) {
    let guard = home.features.store.control_lock().await;
    let mut values = Map::new();
    values.insert(STATUS_FIELD.to_string(), web_status);
    home
      .features
      .store
      .update_task_slot_fields_locked(&guard, session_id, OWNER, values)
      .await
      .expect("the stored status writes");
  }

  /// A status file whose one waiting item names `text`, `context` and `link` (`_waiting_with_link`).
  fn waiting_with_link(text: &str, link: Value) -> String {
    json!({"waiting": [{
      "text": text,
      "context": "The line is active now. Options are continue or stop; continue is recommended.",
      "link": link,
    }]})
    .to_string()
  }

  #[tokio::test]
  async fn a_status_set_refuses_an_old_plan_link_before_the_cold_read() {
    for (endpoint, uri) in [("set", "/api/internal/web/status"), ("unread", "/api/internal/web/status/unread")] {
      for field in ["text", "context", "link"] {
        let home = Home::new();
        let sid = home.manager("session", None).await;
        let plan_sid = home.manager("plan-session", None).await;
        let v9 = session_artifact(&home, &plan_sid, "plan_01_v9.html");
        let v11 = session_artifact(&home, &plan_sid, "plan_01_v11.html");
        write_plans(
          &home,
          &plan_sid,
          json!([plan(
            1,
            json!([plan_version(9, "artifacts/plan_01_v9.html"), plan_version(11, "artifacts/plan_01_v11.html")]),
            Value::Null,
          )]),
        );
        let mut item = json!({
          "text": "Decide plan 1",
          "context": "The plan waits. Options are approve or revise; approve is recommended.",
          "link": null,
        });
        item[field] = json!(host_link(&v9));
        let text = json!({"waiting": [item]}).to_string();

        let (status, response) = home.send(Method::POST, uri, Some(json!({"session_id": sid, "text": text}))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{endpoint} {field}");
        let detail = response["detail"].as_str().expect("a refusal detail");
        assert!(detail.starts_with("Stale plan links"), "{endpoint} {field}: {detail}");
        assert!(detail.contains(host_link(&v9).as_str()), "{endpoint} {field}: {detail}");
        assert!(detail.contains(&v11.display().to_string()), "{endpoint} {field}: {detail}");
        assert_eq!(stored(&home, &sid).await, Value::Null, "{endpoint} {field}");
        assert!(status_events(&home, &sid).is_empty(), "{endpoint} {field}");
        assert!(home.events(&sid).iter().all(|event| event["type"] != WEB_ASK_CHECK), "{endpoint} {field}");
      }
    }
  }

  #[tokio::test]
  async fn status_confirm_refuses_a_stale_stored_plan_link() {
    for local_path in [false, true] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let v9 = session_artifact(&home, &sid, "plan_01_v9.html");
      let v11 = session_artifact(&home, &sid, "plan_01_v11.html");
      write_plans(
        &home,
        &sid,
        json!([plan(
          1,
          json!([plan_version(9, "artifacts/plan_01_v9.html"), plan_version(11, "artifacts/plan_01_v11.html")]),
          Value::Null,
        )]),
      );
      let link = if local_path { v9.display().to_string() } else { host_link(&v9) };
      write_stored_status(
        &home,
        &sid,
        json!({"waiting": [{
          "text": "Decide plan 1",
          "context": "The plan waits. Options are approve or revise; approve is recommended.",
          "link": link,
        }]}),
      )
      .await;

      let (status, response) =
        home.send(Method::POST, "/api/internal/web/status/confirm", Some(json!({"session_id": sid}))).await;

      assert_eq!(status, StatusCode::CONFLICT, "local path {local_path}");
      let detail = response["detail"].as_str().expect("a refusal detail");
      assert!(detail.contains(link.as_str()), "local path {local_path}: {detail}");
      assert!(detail.contains(&v11.display().to_string()), "local path {local_path}: {detail}");
      assert!(home.events(&sid).iter().all(|event| event["type"] != "web_status_confirmed"));
    }
  }

  #[tokio::test]
  async fn status_refuses_a_link_to_a_closed_lineage() {
    for (endpoint, uri) in [
      ("set", "/api/internal/web/status"),
      ("unread", "/api/internal/web/status/unread"),
      ("confirm", "/api/internal/web/status/confirm"),
    ] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let page = session_artifact(&home, &sid, "plan_02_v1.html");
      write_plans(
        &home,
        &sid,
        json!([plan(2, json!([plan_version(1, "artifacts/plan_02_v1.html")]), json!({"as": "completed"}))]),
      );
      write_stored_status(
        &home,
        &sid,
        json!({"waiting": [{
          "text": "Decide plan 2",
          "context": "The plan is closed. Options are reopen or stop; reopen is recommended.",
          "link": host_link(&page),
        }]}),
      )
      .await;
      let payload = if endpoint == "confirm" {
        json!({"session_id": sid})
      } else {
        json!({"session_id": sid, "text": waiting_with_link("Decide plan 2", json!(host_link(&page)))})
      };

      let (status, response) = home.send(Method::POST, uri, Some(payload)).await;

      assert_eq!(status, StatusCode::CONFLICT, "{endpoint}");
      assert!(response["detail"].as_str().expect("a refusal detail").contains("closed"), "{endpoint}");
      assert!(home.events(&sid).iter().all(|event| event["type"] != "web_status_confirmed"), "{endpoint}");
    }
  }

  #[tokio::test]
  async fn status_confirmation_surfaces_an_unreadable_plan_registry() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let page = session_artifact(&home, &sid, "plan.html");
    fs::create_dir_all(sessions_dir(&home).join(&sid)).expect("the session directory writes");
    fs::write(sessions_dir(&home).join(&sid).join("plans.json"), "invalid JSON").expect("the broken registry writes");
    write_stored_status(
      &home,
      &sid,
      json!({"waiting": [{"text": "Decide", "context": "The line is active now. Options are continue or stop; continue is recommended.", "link": host_link(&page)}]}),
    )
    .await;

    let (status, _response) =
      home.send(Method::POST, "/api/internal/web/status/confirm", Some(json!({"session_id": sid}))).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "the unreadable registry fails the confirmation");
    assert!(home.events(&sid).iter().all(|event| event["type"] != "web_status_confirmed"));
  }

  #[tokio::test]
  async fn status_confirm_returns_the_stored_status_with_start_times_and_records_one_event() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let stored_at = "2026-10-24T00:00:00Z";
    write_stored_status(
      &home,
      &sid,
      json!({
        "waiting": [{
          "text": "Decide the deploy",
          "context": "The release is ready. Options are deploy or wait; deploy is recommended.",
        }],
        "updated_at": stored_at,
      }),
    )
    .await;

    let (status, response) =
      home.send(Method::POST, "/api/internal/web/status/confirm", Some(json!({"session_id": sid}))).await;

    assert_eq!(status, StatusCode::OK);
    let waited = &response["waiting"][0];
    assert_eq!(waited["since"], json!(stored_at), "the status time is the item's lower bound");
    assert_eq!(waited["since_exact"], json!(false));
    assert_eq!(response["updated_at"], json!(stored_at));
    let confirmed =
      home.events(&sid).into_iter().filter(|event| event["type"] == "web_status_confirmed").collect::<Vec<_>>();
    assert_eq!(confirmed.len(), 1);
  }

  // -------------------------------------------------------------------------
  // since: a stored item keeps its start time across a rewording or a plan version
  // -------------------------------------------------------------------------

  /// A waiting item whose text is `text` and whose link is `link`, as a status file names it.
  fn planned_item(text: &str, link: Value) -> Value {
    json!({
      "text": text,
      "context": "The line is active now. Options are continue or stop; continue is recommended.",
      "link": link,
    })
  }

  /// Records a cold read of `item`, then stores the one-item status file that names it
  /// (`_set_after_reads`). Answers the stored status.
  async fn set_after_reads(home: &Home, session_id: &str, item: Value) -> Value {
    record_ask_check(
      &home.features,
      session_id,
      &item,
      "synthetic-haiku",
      "continue is recommended.\ntier: t2 rule: none",
    )
    .await
    .expect("the item cold read records");
    set_status(&home.features, session_id, &json!({"waiting": [item]}).to_string()).await.expect("the set stores")
  }

  #[tokio::test]
  async fn since_keeps_the_start_time_of_the_same_text_and_a_reworded_item() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let first = set_after_reads(&home, &sid, planned_item("Decide A", Value::Null)).await;
    let since = first["waiting"][0]["since"].as_str().expect("a start time").to_string();
    assert_eq!(first["waiting"][0]["since_exact"], json!(true));
    let second = set_after_reads(&home, &sid, planned_item("Decide A", Value::Null)).await;
    let third = set_after_reads(&home, &sid, planned_item("Decide B", Value::Null)).await;

    assert_eq!(second["waiting"][0]["since"], json!(since), "the same text keeps its start time");
    assert_eq!(third["waiting"][0]["since"], json!(since), "a reworded item keeps its start time");
    assert_eq!(third["waiting"][0]["since_exact"], json!(true));
  }

  #[tokio::test]
  async fn since_keeps_the_start_time_of_a_reworded_item_with_the_same_link() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let page = home.root().join("linked-page.html");
    fs::write(&page, "page").expect("the linked page writes");
    let link = host_link(&page);

    let first = set_after_reads(&home, &sid, planned_item("Review A", json!(link))).await;
    let second = set_after_reads(&home, &sid, planned_item("Review B", json!(link))).await;

    assert_eq!(
      second["waiting"][0]["since"], first["waiting"][0]["since"],
      "the same link pairs the reworded item with its stored item"
    );
  }

  #[tokio::test]
  async fn since_keeps_the_start_time_when_a_plan_link_moves_to_its_new_version() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let v2 = session_artifact(&home, &sid, "plan_03_v2.html");
    let v3 = session_artifact(&home, &sid, "plan_03_v3.html");
    write_plans(&home, &sid, json!([plan(3, json!([plan_version(2, "artifacts/plan_03_v2.html")]), Value::Null)]));

    let first = set_after_reads(&home, &sid, planned_item("Decide plan A", json!(host_link(&v2)))).await;
    let since = first["waiting"][0]["since"].as_str().expect("a start time").to_string();
    write_plans(
      &home,
      &sid,
      json!([plan(
        3,
        json!([plan_version(2, "artifacts/plan_03_v2.html"), plan_version(3, "artifacts/plan_03_v3.html")]),
        Value::Null,
      )]),
    );
    let second = set_after_reads(&home, &sid, planned_item("Decide plan B", json!(host_link(&v3)))).await;

    assert_eq!(second["waiting"][0]["since"], json!(since), "one plan lineage pairs its versions");
  }

  #[tokio::test]
  async fn legacy_items_read_with_a_lower_bound_and_keep_it_after_a_set() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let stored_at = "2026-10-24T00:00:00Z";
    write_stored_status(
      &home,
      &sid,
      json!({
        "waiting": [{
          "text": "Legacy decision",
          "context": "The line is active now. Options are continue or stop; continue is recommended.",
        }],
        "updated_at": stored_at,
      }),
    )
    .await;

    let (read_status, read) = home.send(Method::GET, &format!("/api/internal/web/status?session_id={sid}"), None).await;
    assert_eq!(read_status, StatusCode::OK);
    assert_eq!(read["web_status"]["waiting"][0]["since"], json!(stored_at));
    assert_eq!(read["web_status"]["waiting"][0]["since_exact"], json!(false));

    let written = set_after_reads(&home, &sid, planned_item("Legacy decision", Value::Null)).await;

    assert_eq!(written["waiting"][0]["since"], json!(stored_at));
    assert_eq!(written["waiting"][0]["since_exact"], json!(false));
  }

  #[tokio::test]
  async fn a_set_file_that_carries_a_start_time_field_is_refused() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = json!({
      "text": "Decide",
      "context": "The line is active. Options are continue or stop; continue is recommended.",
      "since": "2026-10-24T00:00:00Z",
      "since_exact": true,
    });

    let (status, response) = home
      .send(
        Method::POST,
        "/api/internal/web/status",
        Some(json!({"session_id": sid, "text": json!({"waiting": [item]}).to_string()})),
      )
      .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(response["detail"].as_str().expect("a refusal detail").contains("are written by the server"));
    assert_eq!(stored(&home, &sid).await, Value::Null);
    assert!(status_events(&home, &sid).is_empty());
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
