//! A web session's status: the Goal, Remaining, Now and Waiting on you items
//! that `charliebot web status` sets and reads. A set stores the items its JSON object names, records one `web_status`
//! event and tells every ancestor page. A set refuses a Waiting on you item until a cold read of it is recorded, a
//! kept item until a `settle` cold read of the turn's record follows the turn's last input, and a Remaining list
//! that needs its read until a `remaining` cold read follows the turn's last input. A read lists the open
//! descendants whose status holds Waiting on you items and the open children that hold a Now.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_agents::model_pool::POOL_MEMBER_FIELD;
use charliebot_compat::file_urls;
use charliebot_compat::file_urls::ScanContext;
use charliebot_core::config::LabelStyle;
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types::{BACKEND_LABEL, WEB_REPLY, WEB_STATUS, WEB_STATUS_CONFIRMED};
use charliebot_core::json_loads::{self, LoadError};
use charliebot_core::str_methods;
use charliebot_core::text_repr::{JsonStyle, json_dumps};
use charliebot_registry::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::control_events::SOURCE_SESSION_ID_FIELD;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, session_channel};
use charliebot_tasks::{cold_read, session_turn};
use charliebot_user_text_checks as user_text_checks;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::audit;
use crate::cold_read_record;
use crate::json_input::replace_unrepresentable_numbers;
use crate::metadata::WaitingItem;
use crate::plan_links::{self, PlanLinks};
use crate::reply;
pub use crate::reply::WebReplyError;

/// The owner the web reply package registers its metadata under.
pub const OWNER: &str = WEB_REPLY;

/// The metadata field that holds the status, under the owner.
const STATUS_FIELD: &str = WEB_STATUS;

/// The status items in the order a set lists them and the status line names them.
const ITEMS: [&str; 4] = ["goal", "now", "waiting", "remaining"];

/// The label of each item in the event's content.
fn label(item: &str) -> &'static str {
  match item {
    "goal" => "Goal",
    "now" => "Now",
    "remaining" => "Remaining",
    _ => "Waiting on you",
  }
}

/// The non-persisted frame an ancestor's page receives when a descendant sets its status (`SUBTREE_NOTICE_TYPE`).
const SUBTREE_NOTICE_TYPE: &str = charliebot_core::event_types::WEB_STATUS_SUBTREE;

/// The validation messages a status file can break, as the `StatusPatch` and `WaitingItem` readers raise them.
const MUST_NOT_BE_BLANK: &str = "Value error, must not be blank";
const NOT_A_STRING: &str = "Input should be a valid string";
const NOT_A_LIST: &str = "Input should be a valid list";
const NOT_A_WAITING_ITEM: &str = "Input should be a valid dictionary or instance of WaitingItem";
const FIELD_REQUIRED: &str = "Field required";
const EXTRA_FORBIDDEN: &str = "Extra inputs are not permitted";
const NOW_BUDGET_CHARS: usize = 200;
const ITEM_SHA256: &str = "item_sha256";
/// The digest field of a `remaining` cold-read record: the SHA-256 of the lists the probe read. The store gate does
/// not key on it; a record's presence after the turn's last input is what counts.
const REMAINING_SHA256: &str = "remaining_sha256";
/// The item fields that only the server writes; a set file carries neither.
const SERVER_WRITTEN_ITEM_FIELDS: [&str; 2] = ["since", "since_exact"];
/// The message a since field that is not a datetime raises.
const NOT_A_DATETIME: &str = "Input should be a valid datetime";
/// The message a since_exact field that is not a boolean raises.
const NOT_A_BOOLEAN: &str = "Input should be a valid boolean";

/// The session status: Goal, Now, Waiting on you and Remaining, and the time the last set stored them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebStatus {
  goal: Option<String>,
  now: Option<String>,
  waiting: Vec<WaitingItem>,
  /// The open milestones between the session and its Goal, in the order the work reaches them.
  #[serde(default)]
  remaining: Vec<String>,
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
  remaining: Option<Vec<String>>,
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

/// One validation failure: the location the reader reports (`waiting.0.text`) and its message.
struct Problem {
  loc: String,
  msg: &'static str,
}

/// The status a session stores, or `None` before its first set.
fn stored_status(meta: &SessionMetadata) -> Result<Option<WebStatus>, WebReplyError> {
  match meta.extra.get(STATUS_FIELD) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => Ok(Some(serde_json::from_value(value.clone()).map_err(|error| {
      WebReplyError::Failed(anyhow::anyhow!("session {} holds an unreadable web status: {error}", meta.id))
    })?)),
  }
}

/// The status Goal text the session stores, or `None` before it sets one. An unreadable status is the failure this
/// package raises for it.
pub fn stored_goal(meta: &SessionMetadata) -> anyhow::Result<Option<String>> {
  let status = stored_status(meta).map_err(status_failure)?;
  Ok(status.and_then(|status| status.goal))
}

/// Whether the session stores a Waiting on you item that no click answered. The sidebar's waiting flag and the
/// Status view's done checkboxes read this one rule: an item is answered when a click names its position in the list
/// version the status serves, so a changed list leaves every item unanswered. An unreadable status is the failure
/// this package raises for it.
pub(crate) fn has_unanswered_waiting(store: &Store, meta: &SessionMetadata) -> anyhow::Result<bool> {
  let status = stored_status(meta).map_err(status_failure)?;
  let Some(status) = status.filter(|status| !status.waiting.is_empty()) else {
    return Ok(false);
  };
  // Only a session that holds items reads its click facts, so a session with none costs no history read.
  let version = waiting_version(&effective_waiting(&status));
  let clicked = store.facts_of(&meta.id)?.clicked;
  Ok((1..=status.waiting.len()).any(|position| !item_answered(&version, position, &clicked)))
}

/// The status error as the failure a caller outside this package reports.
fn status_failure(error: WebReplyError) -> anyhow::Error {
  match error {
    WebReplyError::Refused { detail, .. } => anyhow::anyhow!("{detail}"),
    WebReplyError::Failed(error) => error,
  }
}

/// The JSON error's message without the `: line L column C (char P)` suffix the error text adds.
fn json_message(error_text: &str) -> &str {
  error_text.rsplit_once(": line ").map_or(error_text, |(msg, _)| msg)
}

/// The patch that the JSON `text` holds. A refusal is a 422 whose detail an agent reads.
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
  // The fields validate in declaration order, each with its nested errors, then the object's extra keys.
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

/// Refuse a set file that names a field only the server writes: a waiting item's `since` or `since_exact`. Only a list
/// `waiting` is examined, and only its object items.
fn refuse_server_fields(fields: &Map<String, Value>) -> Result<(), WebReplyError> {
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

/// The status's waiting items with `since` and `since_exact` filled in.
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

/// The status as the API and the CLI return it, with each waiting item's start time filled in.
fn status_payload(status: &WebStatus) -> Value {
  let mut payload = payload_of(status);
  let waiting = effective_waiting(status);
  payload["waiting_version"] = json!(waiting_version(&waiting));
  payload["waiting"] = serde_json::to_value(waiting).expect("waiting items serialize to JSON");
  payload
}

/// Refuse `items` when any of their links names an older version of a registered plan or a closed plan, before the
/// cold read runs; return the lookup the since pairing then uses.
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

/// The new items with each one's start time kept from the stored item it continues.
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

/// One `WaitingItem` parsed with the `item` model's refusal.
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

/// The item payload the digest covers: JSON with the keys sorted and non-ASCII text literal.
const ITEM_DIGEST_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: true, item_separator: ", ", key_separator: ": ", indent: None };

/// The first 16 hex characters of the digest of the ordered, effective Waiting on you items.
fn waiting_version(items: &[WaitingItem]) -> String {
  let payload = serde_json::to_value(items).expect("waiting items serialize to JSON");
  let encoded = json_dumps(&payload, &ITEM_DIGEST_JSON);
  charliebot_store::control_events::sha256_bytes_hex(encoded.as_bytes())[..16].to_string()
}

/// Whether `clicked` holds a click on the item at `position` in the list `version`.
fn item_answered(version: &str, position: usize, clicked: &[(String, usize)]) -> bool {
  clicked.iter().any(|(clicked_version, clicked_position)| clicked_version == version && *clicked_position == position)
}

/// Add the fold's click references to the status payload without changing stored waiting items.
fn status_payload_with_clicked(status: &WebStatus, clicked: &[(String, usize)]) -> Value {
  let mut payload = status_payload(status);
  let version = payload["waiting_version"].as_str().expect("a status payload has its waiting version").to_string();
  let waiting = payload["waiting"].as_array_mut().expect("a status payload has a waiting list");
  for (index, item) in waiting.iter_mut().enumerate() {
    item["answered"] = json!(item_answered(&version, index + 1, clicked));
  }
  payload
}

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

/// The item digests that records of the cold-read check `name` wrote, each one a read the set may store the item
/// after.
async fn read_item_digests(
  features: &FeatureContext,
  session_id: &str,
  name: &str,
) -> Result<HashSet<String>, WebReplyError> {
  let events = reply::load_events(features, session_id).await?;
  let mut digests = HashSet::new();
  for event in events.iter() {
    let event: &charliebot_store::ChatEvent = event;
    if cold_read_record::cold_read_record_name(event) == Some(name)
      && let Some(digest) = event.get(ITEM_SHA256).and_then(CompactValue::as_str)
    {
      digests.insert(digest.to_string());
    }
  }
  Ok(digests)
}

/// The item digests that a `settle` record in `turn` wrote after `since`: the turn's last input, or, for a turn with
/// no input event, anywhere in the turn since the previous `master_done`.
fn settle_digests(turn: &[Value], since: Option<UtcDateTime>) -> HashSet<String> {
  let mut digests = HashSet::new();
  for event in turn {
    if cold_read_record::cold_read_record_name(event) != Some(cold_read::SETTLE) {
      continue;
    }
    if let Some(since) = since {
      let written = event["timestamp"].as_str().and_then(|text| UtcDateTime::parse(text).ok());
      if written.is_none_or(|written| written <= since) {
        continue;
      }
    }
    if let Some(digest) = event[ITEM_SHA256].as_str() {
      digests.insert(digest.to_string());
    }
  }
  digests
}

/// The incoming waiting items the stored status already holds: each kept item needs a `settle` record before a set
/// may store it again. An item whose digest cannot be read is left to the page-link refusal that names it.
fn kept_items(items: &[WaitingItem], stored: &[WaitingItem]) -> Vec<WaitingItem> {
  let stored_digests: HashSet<String> = stored.iter().filter_map(|item| item_sha256(item).ok()).collect();
  items.iter().filter(|item| item_sha256(item).is_ok_and(|digest| stored_digests.contains(&digest))).cloned().collect()
}

/// The kept items that still need a `settle` cold read: the incoming items the stored status keeps, whose digest has
/// no `settle` record written after the turn's last input.
fn settle_items(items: &[WaitingItem], stored: &[WaitingItem], turn: &[Value]) -> Vec<WaitingItem> {
  let recorded = settle_digests(turn, session_turn::last_input_at(turn));
  let mut settle = Vec::new();
  for item in kept_items(items, stored) {
    if let Ok(digest) = item_sha256(&item)
      && !recorded.contains(&digest)
    {
      settle.push(item);
    }
  }
  settle
}

/// Whether a `remaining` record was written in `turn` after `since`: the turn's last input, or anywhere in the turn
/// for a turn with no input event. The gate keys on the record's presence, not on the list it read, so a list the
/// agent revised after the read still stores on the next set without a second read.
fn remaining_checked_since(turn: &[Value], since: Option<UtcDateTime>) -> bool {
  turn.iter().any(|event| {
    cold_read_record::cold_read_record_name(event) == Some(cold_read::REMAINING)
      && match since {
        Some(since) => {
          let written = event["timestamp"].as_str().and_then(|text| UtcDateTime::parse(text).ok());
          written.is_some_and(|written| written > since)
        }
        None => true,
      }
  })
}

/// The Remaining cold read one set needs: the prospective list, and the stored list when the set replaced it, a
/// clear to an empty list included. A turn with an input reads a non-empty prospective list; a set that changes the
/// list reads it whatever the turn holds. Neither read runs again while a record follows the turn's last input.
struct RemainingRead {
  prospective: Vec<String>,
  previous: Option<Vec<String>>,
}

/// The Remaining read the set needs, or `None` when no record is missing.
fn remaining_read(patch: &StatusPatch, stored_remaining: &[String], turn: &[Value]) -> Option<RemainingRead> {
  let since = session_turn::last_input_at(turn);
  let changed = patch.remaining.as_ref().is_some_and(|next| *next != stored_remaining);
  let prospective = patch.remaining.clone().unwrap_or_else(|| stored_remaining.to_vec());
  let needed = (since.is_some() && !prospective.is_empty()) || changed;
  if needed && !remaining_checked_since(turn, since) {
    Some(RemainingRead { previous: changed.then(|| stored_remaining.to_vec()), prospective })
  } else {
    None
  }
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

/// Refuse a non-empty `waiting` list when the session's approver is its parent node, so parent-approved sessions
/// escalate blockers and approval requests to their parent instead of writing user-facing Waiting on you items.
fn refuse_parent_approved_waiting(meta: &SessionMetadata, patch: &StatusPatch) -> Result<(), WebReplyError> {
  if meta.parent_approved() && patch.waiting.as_deref().is_some_and(|items| !items.is_empty()) {
    return Err(WebReplyError::Refused {
      status: StatusCode::CONFLICT,
      detail: concat!(
        "Status set refused; nothing was stored: this session's approver is its parent node, so it cannot add ",
        "Waiting on you items. Send blockers or approval requests to your parent via `charliebot session send`, ",
        "or pass `\"waiting\": []` to clear legacy items."
      )
      .into(),
    });
  }
  Ok(())
}

/// The item indexes and reasons for links the browser cannot render.
fn page_link_refusals(items: &[WaitingItem]) -> Vec<(usize, String)> {
  items
    .iter()
    .enumerate()
    .filter_map(|(index, item)| {
      user_text_checks::check_page_link(item.link.as_deref())
        .err()
        .map(|reason| (index, format!("waiting.{index} item {:?} link {:?}: {reason}", item.text, item.link)))
    })
    .collect()
}

/// The details for every page link refused by the status entry.
fn page_link_refusal_detail(refusals: &[(usize, String)]) -> String {
  let details = refusals.iter().map(|(_, detail)| detail.clone()).collect::<Vec<_>>();
  format!("Waiting links must resolve to rendered browser pages; nothing was stored:\n{}", refusal_lines(&details))
}

/// The refusal details in item order.
fn refusal_lines(refusals: &[String]) -> String {
  refusals.iter().map(|detail| format!("- {detail}")).collect::<Vec<_>>().join("\n")
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

/// Record one cold read of a Waiting on you item under the check `name` and return its digest. The read's answer is
/// the item's readability record; a blank answer records nothing.
pub async fn record_item_check(
  features: &FeatureContext,
  session_id: &str,
  name: &str,
  raw_item: &Value,
  backend: &str,
  answer: &str,
) -> Result<Value, WebReplyError> {
  reply::require_manager_session(features, session_id).await?;
  let item = parse_item(raw_item)?;
  let digest = item_sha256(&item)?;
  if answer.trim().is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "the cold-read answer is blank".to_string()));
  }
  cold_read_record::record(
    features,
    session_id,
    cold_read_record::CheckRecord {
      name,
      backend,
      answer: Some(answer),
      digest_field: ITEM_SHA256,
      digest: &digest,
      findings: None,
      run: None,
    },
  )
  .await?;
  Ok(json!({ITEM_SHA256: digest}))
}

/// Record one cold read of the Remaining list. The read's `text` names the lists the probe read; a blank answer
/// records nothing.
pub async fn record_remaining_check(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  backend: &str,
  answer: &str,
) -> Result<Value, WebReplyError> {
  reply::require_manager_session(features, session_id).await?;
  if answer.trim().is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "the cold-read answer is blank".to_string()));
  }
  let digest = charliebot_store::control_events::sha256_bytes_hex(text.as_bytes());
  cold_read_record::record(
    features,
    session_id,
    cold_read_record::CheckRecord {
      name: cold_read::REMAINING,
      backend,
      answer: Some(answer),
      digest_field: REMAINING_SHA256,
      digest: &digest,
      findings: None,
      run: None,
    },
  )
  .await?;
  Ok(json!({REMAINING_SHA256: digest}))
}

/// Record one cold read of one Waiting on you item.
pub async fn record_ask_check(
  features: &FeatureContext,
  session_id: &str,
  raw_item: &Value,
  backend: &str,
  answer: &str,
) -> Result<Value, WebReplyError> {
  record_item_check(features, session_id, cold_read::ASK, raw_item, backend, answer).await
}

/// Return the waiting items in `text` whose current digest has no matching `ask` cold-read record, the kept items that
/// still need a settle cold read, the Remaining list that still needs its cold read, and the turn's record when `settle`
/// or the Remaining read is non-empty. When neither is needed, the answer carries `unread` alone.
pub async fn unread_waiting(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_manager_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  refuse_parent_approved_waiting(&meta, &patch)?;
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  reply::refuse_dead_links(scanned_texts(&patch), &scan_context)?;
  let items = patch.waiting.as_deref().unwrap_or_default();
  require_waiting_context(items)?;
  let stored = stored_status(&meta)?;
  let stored_waiting = stored.as_ref().map(effective_waiting).unwrap_or_default();
  let prospective = match patch.waiting.as_deref() {
    Some(items) => items.to_vec(),
    None => stored_waiting.clone(),
  };
  let page_refusals = page_link_refusals(&prospective);
  if !page_refusals.is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, page_link_refusal_detail(&page_refusals)));
  }
  require_current_plan_links(features, &prospective, &scan_context)?;
  let read_digests = read_item_digests(features, session_id, cold_read::ASK).await?;
  let unread = unread_items(items, &read_digests)?;
  let turn = session_turn::current_turn(features, session_id)?;
  let settle = settle_items(items, &stored_waiting, &turn);
  let stored_remaining = stored.as_ref().map(|status| status.remaining.clone()).unwrap_or_default();
  let remaining = remaining_read(&patch, &stored_remaining, &turn);
  let mut answer = json!({"unread": unread});
  if let Some(read) = &remaining {
    answer["remaining"] = json!(read.prospective);
    if let Some(previous) = &read.previous {
      answer["previous_remaining"] = json!(previous);
    }
    answer["turn_text"] = json!(session_turn::turn_text(&turn));
  }
  if settle.is_empty() {
    return Ok(answer);
  }
  answer["settle"] = json!(settle);
  answer["turn_text"] = json!(session_turn::turn_text(&turn));
  Ok(answer)
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

/// The `waiting` list: each item validated, invalid items reported with their index.
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

/// The `remaining` list: each open milestone a non-blank string, invalid items reported with their index.
fn remaining_items(value: &Value, problems: &mut Vec<Problem>) -> Vec<String> {
  let Value::Array(items) = value else {
    problems.push(Problem { loc: "remaining".to_string(), msg: NOT_A_LIST });
    return Vec::new();
  };
  items
    .iter()
    .enumerate()
    .filter_map(|(index, item)| text_value(item, &format!("remaining.{index}"), problems))
    .collect()
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
  for key in fields.keys().filter(|key| !matches!(key.as_str(), "text" | "context" | "link" | "since" | "since_exact"))
  {
    problems.push(Problem { loc: nested_loc(loc, key), msg: EXTRA_FORBIDDEN });
  }
  text.map(|text| WaitingItem { text, context, link, since, since_exact })
}

/// A nested validation location, including the root's empty location.
fn nested_loc(prefix: &str, field: &str) -> String {
  if prefix.is_empty() { field.to_string() } else { format!("{prefix}.{field}") }
}

/// The texts the file-link scanner reads, in order: goal, now, each waiting item's text and context, then each
/// remaining milestone. A structured Waiting link is checked by the page-link owner.
fn scanned_texts(patch: &StatusPatch) -> Vec<&str> {
  let mut texts: Vec<&str> = patch.goal.iter().chain(patch.now.iter()).flatten().map(String::as_str).collect();
  for item in patch.waiting.iter().flatten() {
    texts.push(item.text.as_str());
    texts.extend([item.context.as_deref()].into_iter().flatten());
  }
  texts.extend(patch.remaining.iter().flatten().map(String::as_str));
  texts
}

/// Whether a status command echoes `remaining_missing: true`: a manager session with a Goal and an empty Remaining, with
/// no close request pending and not archived. Each call computes it; no status stores it.
async fn remaining_missing(
  features: &FeatureContext,
  meta: &SessionMetadata,
  status: &WebStatus,
) -> Result<bool, WebReplyError> {
  if !reply::is_manager_session(meta)
    || status.goal.is_none()
    || !status.remaining.is_empty()
    || meta.status == SessionStatus::Archived
  {
    return Ok(false);
  }
  Ok(!audit::close_request_pending(&features.store, &meta.id).await?)
}

/// Store the items the JSON `text` names, record one `web_status` event and notify the ancestors. Waiting items need
/// a non-blank context and a matching cold-read digest. A stale plan link refuses the set before the cold read. The
/// return adds the Now length and `remaining_missing`; a refusal stores and records nothing.
pub async fn set_status(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  let meta = reply::require_manager_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  refuse_parent_approved_waiting(&meta, &patch)?;
  reply::refuse_dead_links(scanned_texts(&patch), &scan_context)?;
  require_waiting_context(patch.waiting.as_deref().unwrap_or_default())?;
  let stored = stored_status(&meta)?;
  let base = stored.clone().unwrap_or(WebStatus {
    goal: None,
    now: None,
    waiting: Vec::new(),
    remaining: Vec::new(),
    updated_at: UtcDateTime::now(),
  });
  let stored_waiting = effective_waiting(&base);
  let prospective = patch.waiting.clone().unwrap_or_else(|| stored_waiting.clone());
  let page_refusals = page_link_refusals(&prospective);
  let mut refusal_details = page_refusals.iter().map(|(_, detail)| detail.clone()).collect::<Vec<_>>();
  let mut refusal_status =
    if page_refusals.is_empty() { StatusCode::CONFLICT } else { StatusCode::UNPROCESSABLE_ENTITY };
  let mut refused = !page_refusals.is_empty();
  let mut links = if prospective.is_empty() || !page_refusals.is_empty() {
    None
  } else {
    match require_current_plan_links(features, &prospective, &scan_context) {
      Ok(links) => Some(links),
      Err(WebReplyError::Refused { status, detail }) => {
        refusal_status = status;
        refusal_details.push(detail);
        refused = true;
        None
      }
      Err(error @ WebReplyError::Failed(_)) => return Err(error),
    }
  };
  let turn = session_turn::current_turn(features, session_id)?;
  if let Some(items) = patch.waiting.as_deref()
    && !items.is_empty()
  {
    let read_digests = read_item_digests(features, session_id, cold_read::ASK).await?;
    for (index, item) in items.iter().enumerate() {
      let digest = match item_sha256(item) {
        Ok(digest) => digest,
        Err(_) if page_refusals.iter().any(|(refused_index, _)| *refused_index == index) => continue,
        Err(error) => return Err(error),
      };
      if !read_digests.contains(&digest) {
        refusal_details.push(format!("Waiting on you item {:?} needs a cold read", item.text));
        refused = true;
      }
    }
    let recorded = settle_digests(&turn, session_turn::last_input_at(&turn));
    for item in kept_items(items, &stored_waiting) {
      let Ok(digest) = item_sha256(&item) else { continue };
      if !recorded.contains(&digest) {
        refusal_details
          .push(format!("Waiting on you item {:?} was kept, so it needs a cold read of this turn's record", item.text));
        refused = true;
      }
    }
  }
  if remaining_read(&patch, &base.remaining, &turn).is_some() {
    refusal_details.push("Remaining needs a cold read of this turn's record".to_string());
    refused = true;
  }
  if refused {
    return Err(WebReplyError::refused(
      refusal_status,
      format!("Status set refused; nothing was stored:\n{}", refusal_lines(&refusal_details)),
    ));
  }
  let now = UtcDateTime::now();
  let waiting = match patch.waiting.as_deref() {
    Some([]) => Vec::new(),
    Some(items) => {
      let links = links.as_mut().expect("nonempty waiting items have plan-link validation");
      assign_since(items, &stored_waiting, links, now)?
    }
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
  if let Some(remaining) = remaining {
    status.remaining = remaining;
  }
  status.waiting = waiting;
  status.updated_at = now;
  let missing = remaining_missing(features, &meta, &status).await?;
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
    let notice = json!({"type": SUBTREE_NOTICE_TYPE, (SOURCE_SESSION_ID_FIELD): session_id});
    features.store.broadcast(&session_channel(&ancestor_id), notice).await?;
  }
  tracing::info!(session = %session_id, changed = ?changed, "web_status_set");
  let now_chars = status.now.as_ref().map_or(0, |value| value.chars().count());
  let mut result = payload.as_object().expect("a web status serializes as an object").clone();
  result.insert("now_chars".to_string(), json!(now_chars));
  result.insert("now_over_budget".to_string(), json!(now_chars > NOW_BUDGET_CHARS));
  result.insert("remaining_missing".to_string(), json!(missing));
  Ok(Value::Object(result))
}

/// Confirm that the stored status needs no change and return it, with `remaining_missing`.
///
/// A stale plan link in a stored item refuses the confirmation with 409 and names the newest file. A confirmation
/// records one web_status_confirmed event and cold reads nothing.
pub async fn confirm_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_manager_session(features, session_id).await?;
  let status = stored_status(&meta)?.unwrap_or(WebStatus {
    goal: None,
    now: None,
    waiting: Vec::new(),
    remaining: Vec::new(),
    updated_at: UtcDateTime::now(),
  });
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  require_current_plan_links(features, &effective_waiting(&status), &scan_context)?;
  let missing = remaining_missing(features, &meta, &status).await?;
  features
    .store
    .persist_and_broadcast(session_id, json!({"type": WEB_STATUS_CONFIRMED, "content": "Status confirmed"}))
    .await?;
  tracing::info!(session = %session_id, "{}", WEB_STATUS_CONFIRMED);
  let mut payload = status_payload(&status);
  payload["remaining_missing"] = json!(missing);
  Ok(payload)
}

/// What one blocking pass over the session's subtree reads: the direct children, every open descendant, and each open
/// descendant's stored status.
///
/// One pass reads each descendant's metadata once, so the holders, the descendant rows and the children rows all come
/// from the same read. The request pays one blocking round trip for the pass, not one per read.
struct StatusTree {
  /// The direct children, as the task-tree index orders them.
  children: Vec<String>,
  /// The open descendants, in depth-first order.
  open: Vec<String>,
  /// The open descendants that hold a stored status, in the same order.
  statuses: Vec<StatusDescendant>,
}

struct StatusDescendant {
  session_id: String,
  metadata: SessionMetadata,
  status: WebStatus,
  clicked: Vec<(String, usize)>,
  /// The compact `Pool → Member` label the row shows, for a session whose backend is a pool; `None` for one option.
  backend_label: Option<String>,
}

/// Read the session's subtree: the direct children, then each descendant's metadata and stored status while that
/// descendant is open (its task state is `open`, and neither it nor an ancestor is archived).
///
/// A session deleted since the task-tree index was read is absent, so it answers no row.
fn read_status_tree(store: &Store, session_id: &str) -> Result<StatusTree, WebReplyError> {
  let children = store.child_ids(session_id)?;
  let config = store.config();
  let mut open = Vec::new();
  let mut statuses = Vec::new();
  for meta in store.open_descendants(session_id)? {
    let descendant_id = meta.id.clone();
    let backend_label = config.backends.pool(&meta.backend).map(|_| {
      let member = meta.extra(POOL_MEMBER_FIELD).and_then(Value::as_str).unwrap_or_default();
      config.backends.session_model_label(&meta.backend, member, LabelStyle::Compact)
    });
    if let Some(status) = stored_status(&meta)? {
      let clicked = if status.waiting.is_empty() { Vec::new() } else { store.facts_of(&descendant_id)?.clicked };
      statuses.push(StatusDescendant {
        session_id: descendant_id.clone(),
        metadata: meta,
        status,
        clicked,
        backend_label,
      });
    }
    open.push(descendant_id);
  }
  Ok(StatusTree { children, open, statuses })
}

/// The derived Waiting on you items that the registry's reader gives: those held by a session in `holders`, or all of
/// them when `holders` is `None`. The status read lists them through this function, and no stored status holds them.
async fn derived_items(
  features: &FeatureContext,
  holders: Option<&HashSet<String>>,
) -> Result<Vec<OpenWaitingItem>, WebReplyError> {
  let read = features.registry.derived_waiting_items_reader().map_err(WebReplyError::Failed)?;
  let rows = read(features).await.map_err(WebReplyError::Failed)?;
  Ok(
    rows
      .into_iter()
      .filter(|row| holders.is_none_or(|holders| holders.contains(&row.session_id)))
      .map(|row| OpenWaitingItem {
        session_id: row.session_id,
        session_name: row.session_name,
        text: row.text,
        context: row.context,
        link: row.link,
        since: row.since,
      })
      .collect(),
  )
}

/// A derived item as the status read returns it: its holder, its text and the fields a stored item carries.
fn derived_payload(item: &OpenWaitingItem) -> Value {
  json!({
    "session_id": item.session_id,
    "text": item.text,
    "context": item.context,
    "link": item.link,
    "since": item.since.to_str(),
  })
}

/// The session's status, waiting items of open descendants and Now items of open children, newest
/// first. A descendant needs waiting items; a child needs a Now value. The derived items the session holds ride
/// beside the stored ones, under `derived_waiting`. The session's own task state rides under `own_task_state`.
pub async fn read_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_manager_session(features, session_id).await?;
  let stored = stored_status(&meta)?;
  let (tree, own_task_state, own_clicked) = tree_read_reply(&features.store, {
    let session_id = session_id.to_string();
    move |store| {
      let tree = read_status_tree(store, &session_id)?;
      let own_task_state = store.task_state(&session_id)?;
      let own_clicked = store.facts_of(&session_id)?.clicked;
      Ok((tree, own_task_state, own_clicked))
    }
  })
  .await?;
  let mut holders = HashSet::from([session_id.to_string()]);
  holders.extend(tree.open.iter().cloned());
  let derived = derived_items(features, Some(&holders)).await?;
  let mut rows: Vec<(UtcDateTime, Value)> = Vec::new();
  for descendant in &tree.statuses {
    if descendant.status.waiting.is_empty() {
      continue;
    }
    let payload = status_payload_with_clicked(&descendant.status, &descendant.clicked);
    let mut row = json!({
      "session_id": descendant.session_id,
      "name": descendant.metadata.name,
      "updated_at": descendant.status.updated_at.to_str(),
      "waiting_version": payload["waiting_version"],
      "waiting": payload["waiting"],
    });
    if let Some(label) = &descendant.backend_label {
      row[BACKEND_LABEL] = json!(label);
    }
    rows.push((descendant.status.updated_at, row));
  }
  rows.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  let status_of: HashMap<&str, usize> =
    tree.statuses.iter().enumerate().map(|(index, descendant)| (descendant.session_id.as_str(), index)).collect();
  let mut children: Vec<(UtcDateTime, Value)> = Vec::new();
  for child_id in &tree.children {
    let Some(index) = status_of.get(child_id.as_str()) else {
      continue;
    };
    let child = &tree.statuses[*index];
    let Some(now) = child.status.now.as_ref() else {
      continue;
    };
    let mut row = json!({
      "session_id": child_id,
      "name": child.metadata.name,
      "now": now,
      "updated_at": child.status.updated_at.to_str(),
    });
    if let Some(label) = &child.backend_label {
      row[BACKEND_LABEL] = json!(label);
    }
    children.push((child.status.updated_at, row));
  }
  children.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  let own_status = stored.as_ref().map(|status| status_payload_with_clicked(status, &own_clicked));
  Ok(json!({
    "session_id": session_id,
    "own_task_state": own_task_state,
    (WEB_STATUS): own_status,
    "derived_waiting": derived.iter().map(derived_payload).collect::<Vec<_>>(),
    "descendants": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    "children": children.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
  }))
}

/// The status as its JSON serialization renders it.
fn payload_of(status: &WebStatus) -> Value {
  serde_json::to_value(status).expect("a web status serializes to JSON")
}

/// One derived Waiting on you item with the session that holds it.
#[derive(Clone, Debug, Serialize)]
pub struct OpenWaitingItem {
  pub session_id: String,
  pub session_name: String,
  pub text: String,
  pub context: Option<String>,
  pub link: Option<String>,
  pub since: UtcDateTime,
}

/// The task-tree reads run off the runtime's worker threads, never on the async worker.
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

/// The task-tree reads run off the runtime's worker threads, never on the async worker. A pass that raises its own
/// `WebReplyError` keeps it, so a refusal the pass detects reaches the route unchanged.
async fn tree_read_reply<T, F>(store: &Arc<Store>, read: F) -> Result<T, WebReplyError>
where
  T: Send + 'static,
  F: FnOnce(&Store) -> Result<T, WebReplyError> + Send + 'static,
{
  let store = Arc::clone(store);
  let answer = tokio::task::spawn_blocking(move || read(&store)).await;
  answer.map_err(|error| anyhow::anyhow!("a task-tree read stopped: {error}"))?
}

async fn ancestor_ids(store: &Arc<Store>, session_id: &str) -> Result<Vec<String>, WebReplyError> {
  let session_id = session_id.to_string();
  tree_read(store, move |store| store.ancestor_ids(&session_id)).await
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::time::Duration;

  use axum::http::Method;
  use charliebot_core::event_types::COLD_READ_CHECK;
  use charliebot_store::Subscription;
  use serde_json::json;

  use super::*;
  use crate::sidebar::WebReplySidebar;
  use crate::test_support::{Home, set_derived_waiting};
  use charliebot_registry::registry::OpenWaitingRow;
  use charliebot_registry::sidebar::SidebarContribution;
  use charliebot_store::metadata::TaskApprover;
  use charliebot_store::sidebar_state::HAS_WAITING_ITEMS;

  const VALID_SET: &str = r#"{"now": "Running"}"#;

  /// Write one archived close fact, as the completion owner's operator archive writes a root's fact.
  async fn archive_task(features: &charliebot_registry::context::FeatureContext, session_id: &str) {
    let guard = features.store.control_lock().await;
    let event = charliebot_store::control_events::archived_close_event(
      session_id,
      "synthetic-status-archive",
      "archived by the user",
      None,
      None,
      None,
      charliebot_store::control_events::ACTOR_USER,
      &[],
      &[],
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

  /// A waiting item whose current state can pass the status set's cold-read gate.
  fn waiting_item(text: &str) -> Value {
    json!({"text": text, "context": "The decision is active now. Options are continue or stop; continue is recommended."})
  }

  /// Records a cold read for the waiting item used by a status set.
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

  /// Records the settle cold read of the kept waiting item a second status set names.
  async fn check_kept_item(home: &Home, session_id: &str, text: &str) {
    record_item_check(
      &home.features,
      session_id,
      cold_read::SETTLE,
      &waiting_item(text),
      "synthetic-haiku",
      "The record leaves the item open.",
    )
    .await
    .expect("the kept item settle read records");
  }

  /// The ask cold-read records of the session's log.
  fn ask_checks(home: &Home, session_id: &str) -> Vec<Value> {
    home
      .events(session_id)
      .into_iter()
      .filter(|event| event["type"] == COLD_READ_CHECK && event["name"] == cold_read::ASK)
      .collect()
  }

  /// The settle cold-read records of the session's log.
  fn settle_checks(home: &Home, session_id: &str) -> Vec<Value> {
    home
      .events(session_id)
      .into_iter()
      .filter(|event| event["type"] == COLD_READ_CHECK && event["name"] == cold_read::SETTLE)
      .collect()
  }

  fn waiting_click(session_id: &str, version: &str, since: &str, position: usize) -> Value {
    json!({
      "type": "user",
      "content": format!(
        "[Status · Waiting on you]\n1. ▸ Waiting on you · session {session_id} · list {version} · since {since} · #{position}\n   ↳ Done"
      )
    })
  }

  /// The status response without its transient echo fields, which no status stores.
  fn without_echoes(status: &Value) -> Value {
    let mut result = status.as_object().expect("a status response is an object").clone();
    result.remove("now_chars");
    result.remove("now_over_budget");
    result.remove("remaining_missing");
    Value::Object(result)
  }

  /// The link form the file server accepts for `path`.
  fn file_link(path: &std::path::Path) -> String {
    format!("https://synthetic.invalid{}{}", file_urls::FILE_SERVER_MOUNT, path.display())
  }

  /// The bare file-server path an agent writes as an item link.
  fn host_link(path: &std::path::Path) -> String {
    format!("{}{}", file_urls::FILE_SERVER_MOUNT, path.display())
  }

  /// The frames a page has received, read until the page goes quiet.
  async fn drain(page: &mut Subscription) -> Vec<Value> {
    drain_with_timeout(page, Duration::from_millis(20)).await
  }

  async fn drain_with_timeout(page: &mut Subscription, quiet_period: Duration) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(text)) = tokio::time::timeout(quiet_period, page.recv()).await {
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
        "link": null
      }])
    );
    assert_eq!(second["waiting"][0]["since_exact"], json!(true), "a new item starts at the set's time");
    assert_eq!(second["now_chars"], json!(7));
    assert_eq!(second["now_over_budget"], json!(false));
    assert_ne!(second["updated_at"], first["updated_at"]);
    assert_eq!(stored(&home, &sid).await, without_echoes(&second));
    let events = status_events(&home, &sid);
    assert_eq!(
      events.iter().map(|event| event["web_status_changed"].clone()).collect::<Vec<_>>(),
      vec![json!(["goal", "waiting"]), json!(["now"]),]
    );
    assert_eq!(events[1]["content"], json!("Status updated: Now"));
    assert_eq!(events[1][WEB_STATUS], without_echoes(&second));
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
      assert_eq!(stored(&home, &sid).await, without_echoes(&response));
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

  /// The `remaining_missing` echo of a set of `text` to the session.
  async fn set_echo(home: &Home, session_id: &str, text: &str) -> bool {
    set_status(&home.features, session_id, text).await.expect("the set stores")["remaining_missing"]
      .as_bool()
      .expect("the echo is a boolean")
  }

  #[tokio::test]
  async fn a_remaining_list_replaces_on_set_and_survives_a_set_that_leaves_it_out() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    // A set that changes Remaining reads it first; the record, not the list's digest, is what lets the retry store,
    // so a list the agent revised after the read stores as revised.
    let changed = set_status(&home.features, &sid, r#"{"goal": "Ship", "remaining": ["Build", "Read back"]}"#)
      .await
      .expect_err("the first list needs its cold read");
    let refusal = detail(changed);
    assert!(refusal.contains("Remaining needs a cold read of this turn's record"), "{refusal}");
    record_remaining_check(&home.features, &sid, r#"{"remaining": []}"#, "synthetic-haiku", "Yes.")
      .await
      .expect("the remaining read records");
    set_status(&home.features, &sid, r#"{"goal": "Ship", "remaining": ["Build", "Read back"]}"#)
      .await
      .expect("the read list stores");

    let goal_only = set_status(&home.features, &sid, r#"{"goal": "Ship it"}"#).await.expect("the goal stores");
    assert_eq!(goal_only["remaining"], json!(["Build", "Read back"]), "a set that leaves the list out keeps it");

    set_status(&home.features, &sid, r#"{"remaining": ["Read back"]}"#).await.expect("the list replaces");
    assert_eq!(stored(&home, &sid).await["remaining"], json!(["Read back"]));
    assert_eq!(status_events(&home, &sid).last().expect("an event")["web_status_changed"], json!(["remaining"]));

    set_status(&home.features, &sid, r#"{"remaining": []}"#).await.expect("the empty list stores");
    assert_eq!(stored(&home, &sid).await["remaining"], json!([]));
  }

  #[tokio::test]
  async fn a_goal_web_session_without_remaining_echoes_missing_on_set_and_on_confirm() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    assert!(set_echo(&home, &sid, r#"{"goal": "Ship"}"#).await, "a set stores and echoes missing");
    assert_eq!(stored(&home, &sid).await["goal"], json!("Ship"), "the set stores as usual");
    assert!(stored(&home, &sid).await.get("remaining_missing").is_none(), "the echo is not stored");

    let confirmed = confirm_status(&home.features, &sid).await.expect("the confirm records");
    assert_eq!(confirmed["remaining_missing"], json!(true));
    assert_eq!(home.events(&sid).iter().filter(|event| event["type"] == "web_status_confirmed").count(), 1);
    assert_eq!(stored(&home, &sid).await["goal"], json!("Ship"), "a confirm stores nothing");
  }

  #[tokio::test]
  async fn remaining_missing_echoes_false_without_a_goal_with_remaining_after_a_close_or_when_archived() {
    let home = Home::new();
    let no_goal = home.manager("no goal", None).await;
    assert!(!set_echo(&home, &no_goal, r#"{"now": "Running"}"#).await, "no Goal");

    let filled = home.manager("filled", None).await;
    record_remaining_check(&home.features, &filled, r#"{"remaining": []}"#, "synthetic-haiku", "Yes.")
      .await
      .expect("the remaining read records");
    assert!(!set_echo(&home, &filled, r#"{"goal": "Ship", "remaining": ["Build"]}"#).await, "a non-empty Remaining");
    let confirmed = confirm_status(&home.features, &filled).await.expect("the confirm records");
    assert_eq!(confirmed["remaining_missing"], json!(false), "confirm echoes the same rule");

    let closing = home.manager("closing", None).await;
    home.launch_run(&closing, "run-1", &[], "2026-01-01T00:00:00Z");
    home
      .append(
        &closing,
        json!({
          "type": charliebot_core::event_types::TASK_CLOSE_REQUESTED,
          "request_id": "close-1",
          "owner_run_id": "run-1",
          "summary": "delivered",
          "result_refs": [],
          "run_ids": [],
        }),
      )
      .await;
    assert!(!set_echo(&home, &closing, r#"{"goal": "Ship"}"#).await, "a pending close request");

    let archived = home.manager("archived", None).await;
    home.features.store.archive_session(&archived).await.expect("the session archives");
    assert!(!set_echo(&home, &archived, r#"{"goal": "Ship"}"#).await, "an archived session");
  }

  #[tokio::test]
  async fn an_operator_thread_stores_all_four_status_fields_without_a_refusal() {
    let home = Home::new();
    let thread = home
      .manager(
        "thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;
    check_waiting_item(&home, &thread, "Pick a layout").await;
    record_remaining_check(&home.features, &thread, r#"{"remaining": []}"#, "synthetic-haiku", "Yes.")
      .await
      .expect("the remaining read records");
    let text = json!({
      "goal": "Ship",
      "remaining": ["Build"],
      "now": "Running",
      "waiting": [waiting_item("Pick a layout")],
    })
    .to_string();

    assert!(!set_echo(&home, &thread, &text).await, "a Goal with Remaining holds no reminder");
    let stored = stored_status(&home.meta(&thread).await).expect("the status reads").expect("a status is stored");
    assert_eq!(stored.goal.as_deref(), Some("Ship"));
    assert_eq!(stored.remaining, ["Build"]);
    assert_eq!(stored.now.as_deref(), Some("Running"));
    assert_eq!(stored.waiting.len(), 1);
  }

  #[tokio::test]
  async fn a_thread_with_a_goal_and_no_remaining_echoes_the_reminder() {
    let home = Home::new();
    let thread = home
      .manager(
        "thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;
    assert!(set_echo(&home, &thread, r#"{"goal": "Ship"}"#).await, "a thread manager is a manager");
  }

  #[tokio::test]
  async fn a_refused_status_set_stores_nothing_and_records_no_event() {
    let home = Home::new();
    let own = home.manager("own", None).await;
    let worker = home.worker(&own, "worker").await;
    let cases = [("unknown", VALID_SET, 404), ("worker", VALID_SET, 409), ("own", "not json", 422)];
    for (case, text, status) in cases {
      let sid = match case {
        "worker" => worker.as_str(),
        _ => own.as_str(),
      };
      let target = if case == "unknown" { "no-such-session" } else { sid };

      let error = set_status(&home.features, target, text).await.expect_err("the set is refused");

      match error {
        WebReplyError::Refused { status: code, .. } => assert_eq!(code.as_u16(), status, "case {case} {text}"),
        other => panic!("case {case} {text}: expected a refusal, got {other:?}"),
      }
      assert_eq!(stored(&home, sid).await, Value::Null, "case {case} {text}");
      assert!(status_events(&home, sid).is_empty(), "case {case} {text}");
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
      (r#"{"remaining": "x"}"#, "Status file is invalid: remaining: Input should be a valid list"),
      (r#"{"remaining": [" "]}"#, "Status file is invalid: remaining.0: Value error, must not be blank"),
      (
        r#"{"remaining": [{"text": "Build", "done": true}]}"#,
        "Status file is invalid: remaining.0: Input should be a valid string",
      ),
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
  async fn a_child_status_set_notifies_its_parent_and_grandparent_pages() {
    let home = Home::status_only();
    home.seed_manager("root", None);
    home.seed_manager("child", Some("root"));
    let grandchild = "grandchild";
    home.seed_manager(grandchild, Some("child"));
    let mut root_page = home.subscribe("root");
    let mut child_page = home.subscribe("child");

    set_status(&home.features, grandchild, VALID_SET).await.expect("the grandchild sets");

    // Each page also receives its own status message; the ancestor notices are the frames this test compares.
    let notice = json!({"type": SUBTREE_NOTICE_TYPE, "source_session_id": grandchild});
    let notices = |frames: Vec<Value>| -> Vec<Value> {
      frames.into_iter().filter(|frame| frame["type"] == SUBTREE_NOTICE_TYPE).collect()
    };
    assert_eq!(notices(drain_with_timeout(&mut child_page, Duration::from_millis(1)).await), vec![notice.clone()]);
    assert_eq!(notices(drain_with_timeout(&mut root_page, Duration::from_millis(1)).await), vec![notice]);
  }

  #[tokio::test]
  async fn a_root_status_set_notifies_no_ancestor_page() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let mut root_page = home.subscribe(&root);

    set_status(&home.features, &root, VALID_SET).await.expect("the root sets");

    let notices: Vec<Value> =
      drain(&mut root_page).await.into_iter().filter(|frame| frame["type"] == SUBTREE_NOTICE_TYPE).collect();
    assert!(notices.is_empty(), "the root has no ancestor page to notify");
  }

  #[tokio::test]
  async fn the_status_read_lists_open_waiting_descendants_newest_first() {
    let home = Home::status_only();
    let (root, old, new, deep) = ("root", "old", "new", "deep");
    home.seed_manager(root, None);
    home.seed_manager(old, Some(root));
    home.seed_manager(new, Some(root));
    home.seed_manager(deep, Some(old));
    for (session_id, updated_at) in
      [(old, "2026-11-07T00:00:01Z"), (new, "2026-11-07T00:00:02Z"), (deep, "2026-11-07T00:00:03Z")]
    {
      let updated_at = UtcDateTime::parse(updated_at).unwrap();
      let status = WebStatus {
        goal: None,
        now: None,
        waiting: vec![WaitingItem {
          text: "decision".to_string(),
          context: Some(
            "The decision is active now. Options are continue or stop; continue is recommended.".to_string(),
          ),
          link: None,
          since: None,
          since_exact: None,
        }],
        remaining: Vec::new(),
        updated_at,
      };
      home
        .features
        .store
        .update_slot_fields_at(
          session_id,
          OWNER,
          Map::from_iter([(STATUS_FIELD.to_string(), payload_of(&status))]),
          updated_at,
        )
        .await
        .unwrap()
        .unwrap();
    }

    let body = read_status(&home.features, root).await.expect("the read answers");

    assert_eq!(body["web_status"], Value::Null);
    let ids: Vec<&str> =
      body["descendants"].as_array().expect("a list").iter().map(|row| row["session_id"].as_str().unwrap()).collect();
    assert_eq!(ids, [deep, new, old]);
    assert_eq!(
      without_item_times(&body["descendants"][0]["waiting"]),
      json!([{
        "text": "decision",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null,
        "answered": false
      }])
    );
  }

  #[tokio::test]
  async fn status_read_adds_versions_and_position_answers_for_own_and_descendant_items() {
    let home = Home::status_only();
    let (root, child) = ("root", "child");
    home.seed_manager(root, None);
    home.seed_manager(child, Some(root));
    let duplicate_items = json!([
      {
        "text": "Decide the layout",
        "context": "The decision is active now.",
        "link": null,
        "since": "2026-11-06T16:50:57.752710Z",
        "since_exact": true,
      },
      {
        "text": "Decide the layout",
        "context": "The decision is active now.",
        "link": null,
        "since": "2026-11-06T16:51:57.752710Z",
        "since_exact": true,
      },
    ]);
    write_stored_status(
      &home,
      root,
      json!({
        "goal": "Ship the panel",
        "now": "Reviewing",
        "waiting": duplicate_items,
        "updated_at": "2026-11-06T17:00:00Z",
      }),
    )
    .await;
    write_stored_status(
      &home,
      child,
      json!({
        "goal": null,
        "now": null,
        "waiting": [{
          "text": "Review the child result",
          "context": "The result is ready.",
          "link": null,
          "since": "2026-11-06T16:52:57.752710Z",
          "since_exact": true,
        }],
        "updated_at": "2026-11-06T17:01:00Z",
      }),
    )
    .await;

    let before = read_status(&home.features, root).await.expect("the status reads");
    let own_version = before["web_status"]["waiting_version"].as_str().unwrap().to_string();
    let own_since = before["web_status"]["waiting"][1]["since"].as_str().unwrap().to_string();
    let child_version = before["descendants"][0]["waiting_version"].as_str().unwrap().to_string();
    let child_since = before["descendants"][0]["waiting"][0]["since"].as_str().unwrap().to_string();
    assert_eq!(own_version.len(), 16);
    assert!(own_version.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    assert_eq!(before["web_status"]["waiting"][0]["answered"], json!(false));

    home.append(root, waiting_click(root, &own_version, &own_since, 2)).await;
    home.append(child, waiting_click(child, &child_version, &child_since, 1)).await;
    let after = read_status(&home.features, root).await.expect("the status reads after the clicks");

    assert_eq!(after["web_status"]["waiting_version"], json!(own_version));
    assert_eq!(
      after["web_status"]["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["answered"].as_bool().unwrap())
        .collect::<Vec<_>>(),
      [false, true],
      "identical text still resolves to the clicked 1-based position"
    );
    assert_eq!(after["descendants"][0]["waiting_version"], json!(child_version));
    assert_eq!(after["descendants"][0]["waiting"][0]["answered"], json!(true));
  }

  #[tokio::test]
  async fn answered_survives_now_and_confirm_but_tracks_the_current_list_version() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let original = waiting_item("Pick a layout");
    check_waiting_item(&home, &sid, "Pick a layout").await;
    set_status(&home.features, &sid, &json!({"waiting": [original.clone()]}).to_string())
      .await
      .expect("the first set stores");
    let initial = read_status(&home.features, &sid).await.expect("the status reads");
    let initial_version = initial["web_status"]["waiting_version"].as_str().unwrap().to_string();
    let since = initial["web_status"]["waiting"][0]["since"].as_str().unwrap().to_string();
    home.append(&sid, waiting_click(&sid, &initial_version, &since, 1)).await;
    assert_eq!(read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"], json!(true));

    set_status(&home.features, &sid, VALID_SET).await.expect("the Now-only set stores");
    let after_now = read_status(&home.features, &sid).await.expect("the status reads after Now");
    assert_eq!(after_now["web_status"]["waiting_version"], json!(initial_version));
    assert_eq!(after_now["web_status"]["waiting"][0]["answered"], json!(true));

    confirm_status(&home.features, &sid).await.expect("the status confirms");
    assert_eq!(
      read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"],
      json!(true),
      "confirm leaves clicked facts available"
    );

    let changed = json!({
      "text": "Pick a layout",
      "context": "The user has not reviewed the revised layout yet.",
      "link": null,
    });
    record_ask_check(&home.features, &sid, &changed, "synthetic-haiku", "The revised layout is ready.")
      .await
      .expect("the changed item cold read records");
    set_status(&home.features, &sid, &json!({"waiting": [changed.clone()]}).to_string())
      .await
      .expect("the context edit stores");
    let after_edit = read_status(&home.features, &sid).await.expect("the status reads after the edit");
    let changed_version = after_edit["web_status"]["waiting_version"].as_str().unwrap().to_string();
    let changed_since = after_edit["web_status"]["waiting"][0]["since"].as_str().unwrap().to_string();
    assert_ne!(changed_version, initial_version);
    assert_eq!(after_edit["web_status"]["waiting"][0]["answered"], json!(false));

    home.append(&sid, waiting_click(&sid, &initial_version, &since, 1)).await;
    assert_eq!(
      read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"],
      json!(false),
      "a stale-version click does not mark the current item"
    );
    home.append(&sid, waiting_click(&sid, &changed_version, &changed_since, 1)).await;
    assert_eq!(read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"], json!(true));
    home.append(&sid, waiting_click(&sid, &initial_version, &since, 1)).await;
    assert_eq!(
      read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"],
      json!(true),
      "a late stale-version click does not clear the current mark"
    );

    record_ask_check(&home.features, &sid, &original, "synthetic-haiku", "The options are continue or stop.")
      .await
      .expect("the restored item cold read records");
    set_status(&home.features, &sid, &json!({"waiting": [original]}).to_string())
      .await
      .expect("the original list stores again");
    let restored = read_status(&home.features, &sid).await.expect("the status reads after restoration");
    assert_eq!(restored["web_status"]["waiting_version"], json!(initial_version));
    assert_eq!(restored["web_status"]["waiting"][0]["answered"], json!(true));
  }

  #[tokio::test]
  async fn the_sidebar_waiting_flag_counts_only_items_that_no_click_answered() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    for text in ["Approve the deploy", "Pick a layout"] {
      check_waiting_item(&home, &sid, text).await;
    }
    set_status(
      &home.features,
      &sid,
      &json!({"waiting": [waiting_item("Approve the deploy"), waiting_item("Pick a layout")]}).to_string(),
    )
    .await
    .expect("the set stores");
    let reading = read_status(&home.features, &sid).await.expect("the status reads");
    let version = reading["web_status"]["waiting_version"].as_str().unwrap().to_string();
    let since: Vec<String> = reading["web_status"]["waiting"]
      .as_array()
      .unwrap()
      .iter()
      .map(|item| item["since"].as_str().unwrap().to_string())
      .collect();
    let flag =
      || WebReplySidebar.row_flags(&home.features.store, &sid).into_iter().find(|(name, _)| *name == HAS_WAITING_ITEMS);
    assert_eq!(flag(), Some((HAS_WAITING_ITEMS, true)), "two unmarked items show the mark");

    home.append(&sid, waiting_click(&sid, &version, &since[0], 1)).await;
    assert_eq!(flag(), Some((HAS_WAITING_ITEMS, true)), "one unmarked item keeps the mark");

    home.append(&sid, waiting_click(&sid, &version, &since[1], 2)).await;
    assert_eq!(flag(), Some((HAS_WAITING_ITEMS, false)), "every item answered lifts the mark");

    let rewritten = json!({
      "text": "Approve the deploy now",
      "context": "The decision is active now. Options are deploy or wait; deploy is recommended.",
      "link": null,
    });
    record_ask_check(&home.features, &sid, &rewritten, "synthetic-haiku", "Deploy now.")
      .await
      .expect("the rewritten item cold read records");
    check_kept_item(&home, &sid, "Pick a layout").await;
    set_status(&home.features, &sid, &json!({"waiting": [rewritten, waiting_item("Pick a layout")]}).to_string())
      .await
      .expect("the rewritten list stores");
    assert_eq!(flag(), Some((HAS_WAITING_ITEMS, true)), "a changed list leaves every item unanswered");
  }

  #[tokio::test]
  async fn a_legacy_item_keeps_its_answer_when_now_stores_its_effective_since() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    write_stored_status(
      &home,
      &sid,
      json!({
        "goal": null,
        "now": null,
        "waiting": [{
          "text": "Review the old item",
          "context": "The item predates per-item start times.",
          "link": null,
        }],
        "updated_at": "2026-11-01T12:00:00Z",
      }),
    )
    .await;
    let before = read_status(&home.features, &sid).await.expect("the legacy status reads");
    let version = before["web_status"]["waiting_version"].as_str().unwrap().to_string();
    let since = before["web_status"]["waiting"][0]["since"].as_str().unwrap().to_string();
    assert_eq!(since, "2026-11-01T12:00:00Z");
    home.append(&sid, waiting_click(&sid, &version, &since, 1)).await;
    assert_eq!(read_status(&home.features, &sid).await.unwrap()["web_status"]["waiting"][0]["answered"], json!(true));

    set_status(&home.features, &sid, VALID_SET).await.expect("the Now-only set stores");
    let after = read_status(&home.features, &sid).await.expect("the status reads after Now");
    assert_eq!(after["web_status"]["waiting_version"], json!(version));
    assert_eq!(after["web_status"]["waiting"][0]["since"], json!(since));
    assert_eq!(after["web_status"]["waiting"][0]["since_exact"], json!(false));
    assert_eq!(after["web_status"]["waiting"][0]["answered"], json!(true));
  }

  #[tokio::test]
  async fn the_status_read_omits_empty_waiting_closed_and_archived_descendants() {
    let home = Home::status_only();
    let root = home.manager("root", None).await;
    let empty = home.child(&root, "empty").await;
    let closed = home.child(&root, "closed").await;
    let archived = home.child(&root, "archived").await;
    let empty_status = json!({
      "goal": null,
      "now": null,
      "waiting": [],
      "updated_at": UtcDateTime::now().to_str(),
    });
    let waiting_status = json!({
      "goal": null,
      "now": null,
      "waiting": [{
        "text": "hidden decision",
        "context": "The decision is active now. Options are continue or stop; continue is recommended.",
        "link": null,
      }],
      "updated_at": UtcDateTime::now().to_str(),
    });
    write_stored_status(&home, &empty, empty_status).await;
    write_stored_status(&home, &closed, waiting_status.clone()).await;
    write_stored_status(&home, &archived, waiting_status).await;
    archive_task(&home.features, &closed).await;
    home.features.store.archive_session(&archived).await.expect("the session archives");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert!(body["descendants"].as_array().unwrap().is_empty());
  }

  #[tokio::test]
  async fn the_status_read_names_the_session_s_own_task_state() {
    let home = Home::status_only();
    let root = home.manager("root", None).await;

    let body = read_status(&home.features, &root).await.expect("the read answers");
    assert_eq!(body["own_task_state"], json!("open"));

    // A closed task's stored items are left out of the status read.
    archive_task(&home.features, &root).await;
    let body = read_status(&home.features, &root).await.expect("the read answers");
    assert_eq!(body["own_task_state"], json!("archived"));
  }

  #[tokio::test]
  async fn the_status_read_lists_open_direct_children_with_a_now_newest_first() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let old = home.child(&root, "old").await;
    let new = home.child(&root, "new").await;
    let deep = home.child(&old, "deep").await;
    set_status(&home.features, &old, r#"{"now": "Old step"}"#).await.expect("the old child sets");
    tokio::time::sleep(Duration::from_millis(2)).await;
    set_status(&home.features, &new, r#"{"now": "New step"}"#).await.expect("the new child sets");
    set_status(&home.features, &deep, r#"{"now": "Deep step"}"#).await.expect("the deep child sets");

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
    // A child on one option carries no backend label: the row's own name already stands for it.
    assert!(body["children"].as_array().unwrap().iter().all(|row| row.get("backend_label").is_none()));
    assert!(body["children"].as_array().unwrap().iter().all(|row| row.get("backend").is_none()));
    assert!(body["children"].as_array().unwrap().iter().all(|row| row.get("pool_member").is_none()));
  }

  #[tokio::test]
  async fn the_status_read_shows_no_member_for_a_recorded_member_off_a_pool() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let child = home.child(&root, "child").await;
    set_status(&home.features, &child, r#"{"now": "Child step"}"#).await.expect("the child sets");
    // The child's backend is one option, not a pool, so the member recorded on it is not shown.
    let mut metadata =
      home.features.store.read_metadata_fresh(&child).expect("the metadata reads").expect("the child exists");
    metadata.set_extra(POOL_MEMBER_FIELD, json!("retired"));
    fs::write(
      home.features.store.metadata_path(&child),
      metadata.to_json_string(&home.features.registry.metadata_slots(), true),
    )
    .expect("the member is recorded");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert_eq!(body["children"].as_array().unwrap().len(), 1);
    assert!(body["children"][0].get("backend_label").is_none());
  }

  /// Record a session's backend and the member it ran on, as the store's metadata file carries them.
  fn record_backend(home: &Home, session_id: &str, backend: &str, member: Option<&str>) {
    let mut metadata =
      home.features.store.read_metadata_fresh(session_id).expect("the metadata reads").expect("the session exists");
    metadata.backend = backend.to_string();
    metadata.set_extra(POOL_MEMBER_FIELD, member.map_or(Value::Null, |member| json!(member)));
    fs::write(
      home.features.store.metadata_path(session_id),
      metadata.to_json_string(&home.features.registry.metadata_slots(), true),
    )
    .expect("the backend is recorded");
  }

  #[tokio::test]
  async fn the_status_read_names_a_pool_child_with_its_compact_label() {
    let home = Home::with_pool("pool-a", "Pool A", "member-a", "CC · Member A");
    let root = home.manager("root", None).await;
    let child = home.child(&root, "child").await;
    record_backend(&home, &child, "pool-a", Some("member-a"));
    write_stored_status(
      &home,
      &child,
      json!({
        "goal": null,
        "now": "Child step",
        "waiting": [{"text": "Pick a layout", "context": "The layout is open now.", "link": null}],
        "updated_at": UtcDateTime::now().to_str(),
      }),
    )
    .await;

    let body = read_status(&home.features, &root).await.expect("the read answers");
    assert_eq!(body["children"][0]["backend_label"], "Pool A → Member A");
    assert_eq!(body["descendants"][0]["backend_label"], "Pool A → Member A");
  }

  #[tokio::test]
  async fn the_status_read_reads_the_pool_label_for_a_member_the_pool_does_not_list() {
    let home = Home::with_pool("pool-a", "Pool A", "member-a", "CC · Member A");
    let root = home.manager("root", None).await;
    let child = home.child(&root, "child").await;
    record_backend(&home, &child, "pool-a", Some("member-b"));
    set_status(&home.features, &child, r#"{"now": "Child step"}"#).await.expect("the child sets");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert_eq!(body["children"][0]["backend_label"], "Pool A");
  }

  #[tokio::test]
  async fn the_status_read_omits_direct_children_without_an_open_now() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let empty = home.child(&root, "empty").await;
    let closed = home.child(&root, "closed").await;
    let archived = home.child(&root, "archived").await;
    set_status(&home.features, &empty, r#"{"goal": "No Now"}"#).await.expect("the empty child sets");
    set_status(&home.features, &closed, r#"{"now": "Closed step"}"#).await.expect("the closed child sets");
    set_status(&home.features, &archived, r#"{"now": "Archived step"}"#).await.expect("the archived child sets");
    archive_task(&home.features, &closed).await;
    home.features.store.archive_session(&archived).await.expect("the session archives");

    let body = read_status(&home.features, &root).await.expect("the read answers");

    assert!(body["children"].as_array().unwrap().is_empty());
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
      "link": file_link(&directory),
    });
    let (status, _) = home
      .send(
        Method::POST,
        "/api/internal/web/cold-read-check",
        Some(json!({
          "session_id": sid,
          "name": "ask",
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
    let expected = item.clone();
    assert_eq!(without_item_times(&response["waiting"]), json!([expected.clone()]));
    assert_eq!(without_item_times(&stored(&home, &sid).await["waiting"]), json!([expected]));
  }

  #[tokio::test]
  async fn dead_links_in_every_status_field_refuse_before_any_cold_read_or_write() {
    for (endpoint, uri) in [("set", "/api/internal/web/status"), ("unread", "/api/internal/web/status/unread")] {
      for field in ["goal", "now", "remaining", "waiting_text", "waiting_context", "waiting_link"] {
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
        let mut item = json!({
          "text": "Decide",
          "context": "The choice is ready. Options are keep or change; keep is recommended.",
          "link": null,
        });
        let (patch, expected_detail) = match field {
          "goal" => (json!({"goal": format!("Review {local_link}")}), Some(local_detail)),
          "now" => (json!({"now": format!("Review {local_link}")}), Some(local_detail)),
          "remaining" => (json!({"remaining": [format!("Review {local_link}")]}), Some(local_detail)),
          "waiting_text" => {
            item["text"] = json!(format!("Review {local_link}"));
            (json!({"waiting": [item]}), Some(local_detail))
          }
          "waiting_context" => {
            item["context"] = json!(format!("Review {local_link}"));
            (json!({"waiting": [item]}), Some(local_detail))
          }
          _ => {
            item["link"] = json!(canonical);
            (json!({"waiting": [item]}), None)
          }
        };
        let events_before = home.events(&sid).len();

        let (status, response) =
          home.send(Method::POST, uri, Some(json!({"session_id": sid, "text": patch.to_string()}))).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{endpoint} {field}");
        let detail = response["detail"].as_str().expect("a refusal detail");
        if let Some(expected_detail) = expected_detail {
          assert_eq!(detail, expected_detail, "{endpoint} {field}");
        } else {
          assert!(detail.contains(&canonical) && detail.contains(&missing.display().to_string()), "{detail}");
          assert!(detail.contains("mapped link") && detail.contains("No such file or directory"), "{detail}");
          if endpoint == "set" {
            assert!(detail.contains("needs a cold read"), "{detail}");
          }
        }
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
    let checks = ask_checks(&home, &sid);
    assert_eq!(checks.len(), 1);
    assert_eq!(
      json!({
        "type": checks[0]["type"],
        "name": checks[0]["name"],
        "content": checks[0]["content"],
        "item_sha256": checks[0]["item_sha256"],
        "backend": checks[0]["backend"],
        "answer": checks[0]["answer"],
      }),
      json!({
        "type": COLD_READ_CHECK,
        "name": cold_read::ASK,
        "content": "Cold read of a Waiting on you item",
        "item_sha256": digest,
        "backend": "haiku",
        "answer": answer,
      })
    );
    let events = home.events(&sid);
    assert!(reply::for_you_entries(&home.features.store, &events).unwrap().is_empty());
    let compact = events.iter().map(charliebot_store::ChatEvent::from_value).collect::<Vec<_>>();
    let messages = home.features.store.events_to_messages(&compact).expect("the ask cold-read record renders");
    assert!(messages.iter().any(|message| message["role"] == COLD_READ_CHECK && message["name"] == cold_read::ASK));
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
    for case in ["unknown", "worker"] {
      let home = Home::new();
      let root = home.manager("session", None).await;
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
  async fn a_blank_cold_read_answer_records_nothing() {
    for answer in ["", "  \n"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let before = home.events(&sid);

      let error = record_ask_check(&home.features, &sid, &waiting_item("Decide"), "haiku", answer)
        .await
        .expect_err("the blank answer is refused");

      assert!(matches!(error, WebReplyError::Refused { status: StatusCode::UNPROCESSABLE_ENTITY, .. }), "{answer:?}");
      assert_eq!(home.events(&sid), before, "{answer:?}");

      let remaining_error = record_remaining_check(&home.features, &sid, r#"{"remaining": []}"#, "haiku", answer)
        .await
        .expect_err("the blank remaining answer is refused");
      assert!(
        matches!(remaining_error, WebReplyError::Refused { status: StatusCode::UNPROCESSABLE_ENTITY, .. }),
        "{answer:?}"
      );
      assert_eq!(home.events(&sid), before, "{answer:?}");
    }
  }

  #[tokio::test]
  async fn set_status_returns_all_refusals() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, r#"{"now": "Running"}"#).await.expect("the baseline status stores");
    let uncold_read = waiting_item("Run the covered release");
    let directory = tempfile::tempdir().expect("the page directory exists");
    let markdown = directory.path().join("source.md");
    fs::write(&markdown, "source text").expect("the mapped source writes");
    let bad_link = json!({
      "text": "Open the markdown source",
      "context": "The source is available now.",
      "link": file_link(&markdown),
    });
    record_ask_check(&home.features, &sid, &bad_link, "synthetic-haiku", "The page is available.")
      .await
      .expect("the item cold read records");
    let before = stored(&home, &sid).await;
    let status_event_count = status_events(&home, &sid).len();

    let error = set_status(&home.features, &sid, &json!({"waiting": [uncold_read, bad_link]}).to_string())
      .await
      .expect_err("the set has cold-read and page-link refusals");

    let detail = match error {
      WebReplyError::Refused { status, detail } => {
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        detail
      }
      other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(detail.contains("Run the covered release") && detail.contains("needs a cold read"), "{detail}");
    assert!(
      detail.contains("waiting.1 item \"Open the markdown source\"") && detail.contains("non-page file"),
      "{detail}"
    );
    assert_eq!(stored(&home, &sid).await, before);
    assert_eq!(status_events(&home, &sid).len(), status_event_count);
  }

  #[tokio::test]
  async fn set_status_full_list_keeps_s3() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let a = waiting_item("Keep the first action");
    let b = waiting_item("Repair the page link");
    for item in [&a, &b] {
      record_ask_check(&home.features, &sid, item, "synthetic-haiku", "The step is approved.")
        .await
        .expect("the original grade records");
    }
    set_status(&home.features, &sid, &json!({"now": "Running", "waiting": [&a, &b]}).to_string())
      .await
      .expect("the original complete list stores");
    let original = stored(&home, &sid).await;
    let status_event_count = status_events(&home, &sid).len();
    let bad_b = json!({
      "text": b["text"],
      "context": b["context"],
      "link": "relative/missing.html",
    });

    let error = set_status(&home.features, &sid, &json!({"waiting": [&a, bad_b]}).to_string())
      .await
      .expect_err("the bad link refuses the complete list");

    assert!(matches!(&error, WebReplyError::Refused { status, .. } if *status == StatusCode::UNPROCESSABLE_ENTITY));
    let detail = detail(error);
    assert!(detail.contains("Repair the page link") && detail.contains("absolute URL"), "{detail}");
    assert_eq!(stored(&home, &sid).await, original);
    assert_eq!(status_events(&home, &sid).len(), status_event_count);

    let fixed_b = json!({
      "text": b["text"],
      "context": b["context"],
      "link": "https://example.invalid/page",
    });
    record_ask_check(&home.features, &sid, &fixed_b, "synthetic-haiku", "The step is approved.")
      .await
      .expect("the corrected link gets a current cold read");
    check_kept_item(&home, &sid, "Keep the first action").await;
    let updated = set_status(&home.features, &sid, &json!({"waiting": [&a, &fixed_b]}).to_string())
      .await
      .expect("the corrected complete list stores");
    assert_eq!(
      updated["waiting"].as_array().unwrap().iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(),
      ["Keep the first action", "Repair the page link"]
    );
  }

  #[tokio::test]
  async fn link_single_renderability_owner() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, r#"{"now": "Running"}"#).await.expect("the baseline status stores");
    let directory = tempfile::tempdir().expect("the source directory exists");
    let source = directory.path().join("changes.diff");
    fs::write(&source, "diff --git a/a b/a").expect("the source writes");
    let item = json!({
      "text": "Review the patch",
      "context": "The patch is ready.",
      "link": file_link(&source),
    });
    let request = json!({"session_id": sid, "text": json!({"waiting": [item]}).to_string()});
    let before = stored(&home, &sid).await;
    let status_event_count = status_events(&home, &sid).len();

    let (unread_status, unread) =
      home.send(Method::POST, "/api/internal/web/status/unread", Some(request.clone())).await;
    let (set_status_code, set) = home.send(Method::POST, "/api/internal/web/status", Some(request)).await;

    assert_eq!(unread_status, StatusCode::UNPROCESSABLE_ENTITY, "{unread}");
    assert_eq!(set_status_code, StatusCode::UNPROCESSABLE_ENTITY, "{set}");
    let unread_detail = unread["detail"].as_str().expect("the unread refusal has a detail");
    let set_detail = set["detail"].as_str().expect("the set refusal has a detail");
    assert!(unread_detail.contains("non-page file") && unread_detail.contains("waiting.0"));
    assert!(set_detail.contains("non-page file") && set_detail.contains("waiting.0"));
    assert!(set_detail.contains("needs a cold read"));
    assert_eq!(stored(&home, &sid).await, before);
    assert_eq!(status_events(&home, &sid).len(), status_event_count);
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
    assert_eq!(ask_checks(&home, &sid).len(), 1);
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
      assert!(ask_checks(&home, &sid).is_empty());
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
      without_item_times(&response["waiting"]),
      json!([{
        "text": "Approve the deploy",
        "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
        "link": null,
      }])
    );
    assert_eq!(stored(&home, &sid).await, without_echoes(&response));
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
    assert_eq!(ask_checks(&home, &sid).len(), 1);
  }

  #[tokio::test]
  async fn the_status_read_refuses_an_unknown_or_worker_session() {
    for (case, status) in [("unknown", 404), ("worker", 409)] {
      let home = Home::new();
      let root = home.manager("session", None).await;
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
  async fn the_status_read_serves_a_thread_manager() {
    let home = Home::new();
    let thread = home
      .manager(
        "thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;

    read_status(&home.features, &thread).await.expect("a thread manager reads its status");
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

  /// The waiting items without the start-time fields that the server writes.
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

  /// A session artifact page whose bytes are the file name.
  fn session_artifact(home: &Home, session_id: &str, name: &str) -> PathBuf {
    let path = sessions_dir(home).join(session_id).join("artifacts").join(name);
    fs::create_dir_all(path.parent().expect("the artifact directory")).expect("the artifact directory writes");
    fs::write(&path, name).expect("the artifact page writes");
    path
  }

  /// One plan-registry version row.
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

  /// One plan-registry plan row.
  fn plan(plan_id: i64, versions: Value, closed: Value) -> Value {
    json!({"id": plan_id, "title": format!("Plan {plan_id}"), "versions": versions, "takeoff": null, "closed": closed})
  }

  /// Writes `plans` as the session's plan registry.
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

  /// A status file whose one waiting item names `text`, `context` and `link`.
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

    for (endpoint, uri) in [("set", "/api/internal/web/status"), ("unread", "/api/internal/web/status/unread")] {
      for field in ["text", "context", "link"] {
        let mut item = json!({
          "text": "Decide plan 1",
          "context": "The plan waits. Options are approve or revise; approve is recommended.",
          "link": null,
        });
        let plan_link = if field == "link" { file_link(&v9) } else { host_link(&v9) };
        item[field] = json!(plan_link);
        let text = json!({"waiting": [item]}).to_string();

        let (status, response) = home.send(Method::POST, uri, Some(json!({"session_id": sid, "text": text}))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{endpoint} {field}");
        let detail = response["detail"].as_str().expect("a refusal detail");
        assert!(detail.contains("Stale plan links"), "{endpoint} {field}: {detail}");
        assert!(detail.contains(plan_link.as_str()), "{endpoint} {field}: {detail}");
        assert!(detail.contains(&v11.display().to_string()), "{endpoint} {field}: {detail}");
        if endpoint == "set" {
          assert!(detail.contains("needs a cold read"), "{endpoint} {field}: {detail}");
        }
        assert_eq!(stored(&home, &sid).await, Value::Null, "{endpoint} {field}");
        assert!(status_events(&home, &sid).is_empty(), "{endpoint} {field}");
        assert!(ask_checks(&home, &sid).is_empty(), "{endpoint} {field}");
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
      let link = if local_path { v9.display().to_string() } else { file_link(&v9) };
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
          "link": file_link(&page),
        }]}),
      )
      .await;
      let payload = if endpoint == "confirm" {
        json!({"session_id": sid})
      } else {
        json!({"session_id": sid, "text": waiting_with_link("Decide plan 2", json!(file_link(&page)))})
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
      json!({"waiting": [{"text": "Decide", "context": "The line is active now. Options are continue or stop; continue is recommended.", "link": file_link(&page)}]}),
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

  /// Records a cold read of `item`, then stores the one-item status file that names it.
  /// Answers the stored status.
  async fn set_after_reads(home: &Home, session_id: &str, item: Value) -> Value {
    record_ask_check(&home.features, session_id, &item, "synthetic-haiku", "continue is recommended.")
      .await
      .expect("the item cold read records");
    record_item_check(
      &home.features,
      session_id,
      cold_read::SETTLE,
      &item,
      "synthetic-haiku",
      "The record leaves the item open.",
    )
    .await
    .expect("the settle cold read records");
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
    let link = file_link(&page);

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

    let first = set_after_reads(&home, &sid, planned_item("Decide plan A", json!(file_link(&v2)))).await;
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
    let second = set_after_reads(&home, &sid, planned_item("Decide plan B", json!(file_link(&v3)))).await;

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
    write_stored_status(
      &home,
      &child,
      json!({
        "goal": null,
        "now": null,
        "waiting": [{
          "text": "Answer the pending question",
          "context": "The question is open.",
          "link": null,
          "since": "2026-11-01T12:00:00Z",
          "since_exact": true,
        }],
        "updated_at": "2026-11-01T12:00:00Z",
      }),
    )
    .await;
    let index_before = home.features.store.task_tree_index_build_count();

    read_status(&home.features, &root).await.expect("the status reads");

    let index_after_first = home.features.store.task_tree_index_build_count();
    let state_after_first = home.features.store.history_work().full_folds;
    assert_eq!(index_after_first - index_before, 1);
    read_status(&home.features, &root).await.expect("the status reads again");
    assert_eq!(home.features.store.task_tree_index_build_count(), index_after_first);
    assert_eq!(home.features.store.history_work().full_folds, state_after_first);
  }

  /// The derived item one holder carries, as the host gate's reader gives it.
  fn derived_row(session_id: &str) -> OpenWaitingRow {
    OpenWaitingRow {
      session_id: session_id.to_string(),
      session_name: "Synthetic holder".to_string(),
      text: "Approve the ssh login to synthetic-login-host on /host-auth".to_string(),
      context: Some("Synthetic login check".to_string()),
      link: Some("/host-auth".to_string()),
      since: UtcDateTime::parse("2026-11-01T21:43:00-07:00").expect("the fixture time parses"),
    }
  }

  #[tokio::test]
  async fn a_derived_item_rides_the_status_of_its_open_holders_and_no_set_stores_it() {
    let home = Home::status_only();
    let (root, child, unrelated) = ("root", "child", "unrelated");
    home.seed_manager(root, None);
    home.seed_manager(child, Some(root));
    home.seed_manager(unrelated, None);
    set_derived_waiting(&home.root(), vec![derived_row(root), derived_row(child), derived_row(unrelated)]);

    let read = read_status(&home.features, root).await.expect("the status reads");
    let held = |read: &Value| -> Vec<String> {
      read["derived_waiting"]
        .as_array()
        .expect("the status lists its derived items")
        .iter()
        .map(|item| item["session_id"].as_str().expect("an item names its holder").to_string())
        .collect()
    };
    assert_eq!(
      held(&read),
      [root.to_string(), child.to_string()],
      "the holder and its open descendants, not the other session"
    );
    assert_eq!(held(&read_status(&home.features, child).await.expect("the child reads")), [child.to_string()]);

    // A set stores its own items and leaves the derived ones where the reader gives them.
    set_status(&home.features, root, VALID_SET).await.expect("the set stores");
    assert_eq!(stored(&home, root).await["waiting"], json!([]));
    let after = read_status(&home.features, root).await.expect("the status reads after the set");
    assert_eq!(held(&after), [root.to_string(), child.to_string()]);
    assert_eq!(after[WEB_STATUS]["waiting"], json!([]));
  }

  /// Creates a manager child under `parent` with an explicit `approver` on its task spec.
  async fn child_with_approver(home: &Home, parent: &str, name: &str, approver: TaskApprover) -> String {
    let request = charliebot_store::CreateTask {
      request_id: format!("child-{name}"),
      task_parent_id: Some(parent.to_string()),
      profile: charliebot_store::metadata::TaskProfile::Manager,
      task: Some(charliebot_store::metadata::TaskSpec { approver, ..Default::default() }),
      name: Some(name.to_string()),
      backend: None,
      group: None,
      session_id: None,
    };
    home
      .features
      .store
      .create_task(request, charliebot_store::TaskCaller::Operator, Map::new())
      .await
      .expect("the child task is created")
      .id
  }

  #[tokio::test]
  async fn parent_approved_session_refuses_non_empty_waiting_with_409_and_stores_nothing() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let child = child_with_approver(&home, &root, "parent-approved", TaskApprover::Parent).await;
    let expected_detail = concat!(
      "Status set refused; nothing was stored: this session's approver is its parent node, so it cannot add ",
      "Waiting on you items. Send blockers or approval requests to your parent via `charliebot session send`, ",
      "or pass `\"waiting\": []` to clear legacy items."
    );
    let unread_patch = json!({"waiting": [waiting_item("Approve merge")]}).to_string();
    let dead_link_patch = json!({
      "now": "Waiting",
      "waiting": [{"text": "Review [missing](artifacts/missing.html)", "context": "Ready now."}],
    })
    .to_string();

    for (endpoint, uri) in [("unread", "/api/internal/web/status/unread"), ("set", "/api/internal/web/status")] {
      for (case, patch) in [("unread_item", &unread_patch), ("dead_link", &dead_link_patch)] {
        let (status, response) = home.send(Method::POST, uri, Some(json!({"session_id": child, "text": patch}))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{endpoint} {case}");
        assert_eq!(response["detail"], json!(expected_detail), "{endpoint} {case}");
        assert_eq!(stored(&home, &child).await, Value::Null, "{endpoint} {case}");
        assert!(status_events(&home, &child).is_empty(), "{endpoint} {case}");
      }
    }
  }

  #[tokio::test]
  async fn parent_approved_session_allows_clearing_legacy_waiting_with_empty_list() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let child = child_with_approver(&home, &root, "parent-approved", TaskApprover::Parent).await;
    write_stored_status(
      &home,
      &child,
      json!({
        "now": "Legacy",
        "waiting": [waiting_item("Legacy item")],
        "updated_at": "2026-11-01T00:00:00Z",
      }),
    )
    .await;

    let unread =
      unread_waiting(&home.features, &child, r#"{"waiting": []}"#).await.expect("clearing waiting passes unread check");
    assert_eq!(unread, json!({"unread": []}));

    let cleared =
      set_status(&home.features, &child, r#"{"waiting": []}"#).await.expect("clearing legacy waiting stores");
    assert_eq!(cleared["waiting"], json!([]));
    assert_eq!(stored(&home, &child).await["waiting"], json!([]));
    assert_eq!(stored(&home, &child).await["now"], json!("Legacy"));
    assert_eq!(status_events(&home, &child).len(), 1);
  }

  #[tokio::test]
  async fn parent_approved_session_allows_updating_now_alone() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let child = child_with_approver(&home, &root, "parent-approved", TaskApprover::Parent).await;

    let unread = unread_waiting(&home.features, &child, VALID_SET).await.expect("updating now passes unread check");
    assert_eq!(unread, json!({"unread": []}));

    let saved = set_status(&home.features, &child, VALID_SET).await.expect("updating now stores");
    assert_eq!(saved["now"], json!("Running"));
    assert_eq!(stored(&home, &child).await["now"], json!("Running"));
    assert_eq!(stored(&home, &child).await["waiting"], json!([]));
    assert_eq!(status_events(&home, &child).len(), 1);
  }

  #[tokio::test]
  async fn user_approved_and_default_sessions_keep_allowing_waiting_items() {
    let home = Home::new();
    let root = home.manager("root", None).await;
    let default_child = home.child(&root, "default-child").await;
    let user_child = child_with_approver(&home, &root, "user-child", TaskApprover::User).await;

    for sid in [&root, &default_child, &user_child] {
      let item = planned_item("Approve deploy", Value::Null);
      let payload = json!({"waiting": [item.clone()]}).to_string();

      assert_eq!(
        unread_waiting(&home.features, sid, &payload).await.expect("unread checks"),
        json!({"unread": [item]})
      );
      record_ask_check(&home.features, sid, &item, "synthetic-haiku", "continue is recommended.")
        .await
        .expect("the waiting item cold read records");
      let saved = set_status(&home.features, sid, &payload).await.expect("waiting stores");

      assert_eq!(without_item_times(&saved["waiting"]), json!([item]));
      assert_eq!(without_item_times(&stored(&home, sid).await["waiting"]), json!([item]));
    }
  }

  // -------------------------------------------------------------------------
  // settle: the kept items are read against the turn's record
  // -------------------------------------------------------------------------

  /// A `settle` record for `item` at `timestamp`, as the cold-read route writes it.
  async fn append_settle_record(home: &Home, session_id: &str, item: &Value, timestamp: &str) {
    let digest = item_sha256(&parse_item(item).expect("the item validates")).expect("the item digests");
    home
      .append(
        session_id,
        json!({
          "type": COLD_READ_CHECK,
          "name": cold_read::SETTLE,
          "content": "Cold read of the kept Waiting on you items",
          ITEM_SHA256: digest,
          "backend": "synthetic-haiku",
          "answer": "The record leaves the item open.",
          "timestamp": timestamp,
        }),
      )
      .await;
  }

  /// Stores the one waiting item under test with its ask record, then answers the status the session stores.
  async fn store_one_item(home: &Home, session_id: &str, item: &Value) {
    record_ask_check(&home.features, session_id, item, "synthetic-haiku", "The item is ready.")
      .await
      .expect("the item cold read records");
    set_status(&home.features, session_id, &json!({"waiting": [item]}).to_string()).await.expect("the item stores");
  }

  #[tokio::test]
  async fn the_unread_query_names_each_kept_item_and_carries_the_bounded_turn_record() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = waiting_item("Decide the layout");
    store_one_item(&home, &sid, &item).await;
    home
      .append(
        &sid,
        json!({"type": "assistant", "timestamp": "2026-01-01T00:00:01Z",
          "message": {"content": [{"type": "text", "text": "The layout is chosen."}]}}),
      )
      .await;

    let unread = unread_waiting(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
      .await
      .expect("the unread query answers");

    assert_eq!(unread["unread"], json!([]));
    let settle = unread["settle"].as_array().expect("the answer names the kept items");
    assert_eq!(settle.len(), 1);
    assert_eq!(settle[0]["text"], json!("Decide the layout"));
    let turn_text = unread["turn_text"].as_str().expect("the answer carries the turn record");
    assert!(turn_text.contains("agent: The layout is chosen."), "{turn_text}");
    assert!(!turn_text.contains("cold_read_check"), "the record excludes the cold-read records: {turn_text}");
    assert!(!turn_text.contains("Status updated"), "the record carries the status payload, not its line: {turn_text}");
  }

  #[tokio::test]
  async fn a_settle_record_after_the_last_input_clears_the_kept_item() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = waiting_item("Decide the layout");
    store_one_item(&home, &sid, &item).await;
    append_settle_record(&home, &sid, &item, "2026-01-01T00:00:05Z").await;

    let unread = unread_waiting(&home.features, &sid, &json!({"waiting": [item]}).to_string())
      .await
      .expect("the unread query answers");

    assert_eq!(unread, json!({"unread": []}), "a read kept item is neither unread nor a settle item");
  }

  #[tokio::test]
  async fn a_settle_record_before_the_turns_last_input_leaves_the_item_kept() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = waiting_item("Decide the layout");
    store_one_item(&home, &sid, &item).await;
    home.launch_run(&sid, "run-1", &["u1"], "2026-01-01T00:00:10Z");
    home
      .append(&sid, json!({"type": "user", "id": "u1", "content": "What now?", "timestamp": "2026-01-01T00:00:09Z"}))
      .await;
    append_settle_record(&home, &sid, &item, "2026-01-01T00:00:05Z").await;

    let before = unread_waiting(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(before["settle"].as_array().map(Vec::len), Some(1), "a read from before the input does not settle it");

    append_settle_record(&home, &sid, &item, "2026-01-01T00:00:11Z").await;
    let after = unread_waiting(&home.features, &sid, &json!({"waiting": [item]}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(after, json!({"unread": []}));
  }

  #[tokio::test]
  async fn a_set_that_names_a_kept_item_refuses_until_its_settle_record_exists() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let item = waiting_item("Decide the layout");
    store_one_item(&home, &sid, &item).await;
    let events = status_events(&home, &sid).len();

    let error = set_status(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
      .await
      .expect_err("a kept item needs its settle read");
    let detail = detail(error);
    assert!(detail.contains("was kept, so it needs a cold read of this turn's record"), "{detail}");
    assert_eq!(status_events(&home, &sid).len(), events, "the refusal stores nothing");

    append_settle_record(&home, &sid, &item, "2026-01-01T00:00:05Z").await;
    assert_eq!(settle_checks(&home, &sid).len(), 1, "the settle record exists before the second set");
    set_status(&home.features, &sid, &json!({"waiting": [item]}).to_string()).await.expect("the read item stores");
  }

  /// The session-api router's answer to one GET request.
  async fn session_api_get(home: &Home, uri: &str) -> Value {
    use tower::ServiceExt as _;
    let request = axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).expect("the request builds");
    let response = charliebot_session_api::routes(&home.features)
      .oneshot(request)
      .await
      .expect("the session-api router is infallible");
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    serde_json::from_slice(&bytes).expect("the body is JSON")
  }

  #[tokio::test]
  async fn the_turn_record_and_the_settle_list_stay_out_of_every_browser_route() {
    let home = Home::new();
    // The status poll's body memo is process-global, so this test's session name must differ from every other
    // test's: a shared deterministic session id would let one home's answer serve the other's request.
    let sid = home.manager("cold-read-isolation", None).await;
    let item = waiting_item("Decide the layout");
    store_one_item(&home, &sid, &item).await;
    let secret = "the-unstated-settling-fact";
    home
      .append(
        &sid,
        json!({"type": "assistant", "timestamp": "2026-01-01T00:00:01Z",
          "message": {"content": [{"type": "text", "text": secret}]}}),
      )
      .await;

    let (status, unread) = home
      .send(
        Method::POST,
        "/api/internal/web/status/unread",
        Some(json!({"session_id": sid, "text": json!({"waiting": [item]}).to_string()})),
      )
      .await;
    assert_eq!(status, StatusCode::OK);
    assert!(unread["settle"].is_array(), "{unread}");
    assert!(unread["turn_text"].as_str().is_some_and(|text| text.contains(secret)), "{unread}");

    let (status, internal) = home.send(Method::GET, &format!("/api/internal/web/status?session_id={sid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let bodies = vec![
      internal,
      session_api_get(&home, &format!("/api/sessions/{sid}/bootstrap")).await,
      session_api_get(&home, &format!("/api/sessions/{sid}/events?before=100&limit=40")).await,
      session_api_get(&home, &format!("/api/sessions/{sid}")).await,
      session_api_get(&home, "/api/sessions/").await,
      session_api_get(&home, &format!("/api/sessions/status?ids={sid}")).await,
      session_api_get(&home, "/api/sessions/search?q=layout").await,
    ];
    for body in bodies {
      // The conversation itself is public to the page; the assembled turn record and the settle list are not.
      let text = body.to_string();
      assert!(!text.contains("turn_text"), "{text}");
      assert!(!text.contains("\"settle\""), "{text}");
    }
    // The live frames the socket sends for each stored event carry neither either.
    for (index, event) in home.events(&sid).iter().enumerate() {
      let frames = charliebot_store::streaming::socket_frames(event, index, Vec::new()).expect("the frames build");
      let text = Value::Array(frames).to_string();
      assert!(!text.contains("turn_text"), "{text}");
      assert!(!text.contains("\"settle\""), "{text}");
    }
  }

  /// Appends one `remaining` cold-read record with `timestamp`, as the CLI's cold-read-check call writes one.
  async fn append_remaining_record(home: &Home, session_id: &str, timestamp: &str) {
    home
      .append(
        session_id,
        json!({
          "type": COLD_READ_CHECK,
          "name": cold_read::REMAINING,
          "content": "Cold read of the Remaining list",
          REMAINING_SHA256: "digest",
          "backend": "synthetic-haiku",
          "answer": "Yes.",
          "timestamp": timestamp,
        }),
      )
      .await;
  }

  /// The remaining cold-read records of the session's log.
  fn remaining_checks(home: &Home, session_id: &str) -> Vec<Value> {
    home
      .events(session_id)
      .into_iter()
      .filter(|event| event["type"] == COLD_READ_CHECK && event["name"] == cold_read::REMAINING)
      .collect()
  }

  /// The stored status with a Remaining list, the running turn whose input woke it, and the user event the run
  /// received.
  async fn a_manager_with_remaining_and_an_input(home: &Home, session_id: &str, run_id: &str) {
    write_stored_status(
      home,
      session_id,
      json!({
        "goal": "Ship",
        "now": "Running",
        "remaining": ["Land the list"],
        "waiting": [],
        "updated_at": "2026-01-01T00:00:00Z",
      }),
    )
    .await;
    home.launch_run(session_id, run_id, &["u1"], "2026-01-01T00:00:10Z");
    home
      .append(
        session_id,
        json!({"type": "user", "id": "u1", "content": "How is it going?", "timestamp": "2026-01-01T00:00:09Z"}),
      )
      .await;
  }

  #[tokio::test]
  async fn a_partial_set_on_an_input_turn_with_stored_remaining_reads_remaining_until_it_records() {
    let home = Home::new();
    let sid = home.manager("remaining-gate", None).await;
    a_manager_with_remaining_and_an_input(&home, &sid, "run-1").await;

    let unread = unread_waiting(&home.features, &sid, &json!({"now": "Running"}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(unread["unread"], json!([]));
    assert_eq!(unread["remaining"], json!(["Land the list"]), "the prospective list rides the answer");
    assert!(unread.get("previous_remaining").is_none(), "an unchanged list names no previous list");
    assert!(unread["turn_text"].as_str().is_some_and(|text| text.contains("How is it going?")), "{unread}");

    let error = set_status(&home.features, &sid, &json!({"now": "Running"}).to_string())
      .await
      .expect_err("the set refuses until the read records");
    assert!(detail(error).contains("Remaining needs a cold read of this turn's record"));
    assert!(status_events(&home, &sid).is_empty(), "the refusal stores nothing");

    append_remaining_record(&home, &sid, "2026-01-01T00:00:11Z").await;
    let stored_status =
      set_status(&home.features, &sid, &json!({"now": "Running"}).to_string()).await.expect("the recorded read stores");
    assert_eq!(without_echoes(&stored_status)["remaining"], json!(["Land the list"]));
  }

  #[tokio::test]
  async fn a_remaining_change_on_a_turn_without_input_reads_once_and_stores_a_revised_list() {
    let home = Home::new();
    let sid = home.manager("remaining-change", None).await;

    let unread = unread_waiting(&home.features, &sid, &json!({"remaining": ["Land the list"]}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(unread["remaining"], json!(["Land the list"]));
    assert_eq!(unread["previous_remaining"], json!([]), "a first list replaces the empty stored one");

    let error = set_status(&home.features, &sid, &json!({"remaining": ["Land the list"]}).to_string())
      .await
      .expect_err("the changed list refuses until the read records");
    assert!(detail(error).contains("Remaining needs a cold read of this turn's record"));

    append_remaining_record(&home, &sid, "2026-01-01T00:00:01Z").await;
    set_status(&home.features, &sid, &json!({"remaining": ["Revised milestone"]}).to_string())
      .await
      .expect("the record, not the list's digest, stores the revised list");
    assert_eq!(stored(&home, &sid).await["remaining"], json!(["Revised milestone"]));
    assert_eq!(remaining_checks(&home, &sid).len(), 1, "the revised list runs no second read");
  }

  #[tokio::test]
  async fn clearing_a_non_empty_remaining_reads_it_once_before_it_stores() {
    let home = Home::new();
    let sid = home.manager("remaining-clear", None).await;
    write_stored_status(
      &home,
      &sid,
      json!({
        "goal": "Ship",
        "now": "Running",
        "remaining": ["Land the list"],
        "waiting": [],
        "updated_at": "2026-01-01T00:00:00Z",
      }),
    )
    .await;

    let unread = unread_waiting(&home.features, &sid, &json!({"remaining": []}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(unread["remaining"], json!([]));
    assert_eq!(unread["previous_remaining"], json!(["Land the list"]), "a clear names the list it clears");

    let error = set_status(&home.features, &sid, &json!({"remaining": []}).to_string())
      .await
      .expect_err("the clear refuses until the read records");
    assert!(detail(error).contains("Remaining needs a cold read of this turn's record"));

    append_remaining_record(&home, &sid, "2026-01-01T00:00:01Z").await;
    set_status(&home.features, &sid, &json!({"remaining": []}).to_string()).await.expect("the clear stores");
    assert_eq!(stored(&home, &sid).await["remaining"], json!([]));
  }

  #[tokio::test]
  async fn a_mid_turn_input_re_arms_the_remaining_read() {
    let home = Home::new();
    let sid = home.manager("remaining-rearm", None).await;
    a_manager_with_remaining_and_an_input(&home, &sid, "run-1").await;
    append_remaining_record(&home, &sid, "2026-01-01T00:00:11Z").await;
    set_status(&home.features, &sid, &json!({"now": "Running"}).to_string()).await.expect("the read set stores");

    // A further input mid-turn re-arms the read: the recorded read of the first input no longer covers the turn.
    home.end_run(&sid, "run-1", "2026-01-01T00:00:15Z");
    home.launch_run(&sid, "run-2", &["u2"], "2026-01-01T00:00:20Z");
    home
      .append(
        &sid,
        json!({"type": "user", "id": "u2", "content": "A milestone landed", "timestamp": "2026-01-01T00:00:19Z"}),
      )
      .await;

    let unread = unread_waiting(&home.features, &sid, &json!({"now": "Running"}).to_string())
      .await
      .expect("the unread query answers");
    assert_eq!(unread["remaining"], json!(["Land the list"]), "the new input re-arms the read");
    let error = set_status(&home.features, &sid, &json!({"now": "Running"}).to_string())
      .await
      .expect_err("the set refuses until the turn's own read records");
    assert!(detail(error).contains("Remaining needs a cold read of this turn's record"));
  }

  #[tokio::test]
  async fn confirm_records_no_remaining_read() {
    let home = Home::new();
    let sid = home.manager("remaining-confirm", None).await;
    a_manager_with_remaining_and_an_input(&home, &sid, "run-1").await;

    let confirmed = confirm_status(&home.features, &sid).await.expect("the confirm records");

    assert_eq!(confirmed["remaining"], json!(["Land the list"]));
    assert!(remaining_checks(&home, &sid).is_empty(), "a confirm cold reads nothing");
    assert_eq!(home.events(&sid).iter().filter(|event| event["type"] == "web_status_confirmed").count(), 1);
  }

  #[tokio::test]
  async fn an_unchanged_remaining_without_an_input_keeps_the_exact_unread_shape() {
    let home = Home::new();
    let sid = home.manager("remaining-shape", None).await;
    write_stored_status(
      &home,
      &sid,
      json!({
        "goal": "Ship",
        "now": "Running",
        "remaining": ["Land the list"],
        "waiting": [],
        "updated_at": "2026-01-01T00:00:00Z",
      }),
    )
    .await;

    let unread = unread_waiting(&home.features, &sid, &json!({"now": "Running"}).to_string())
      .await
      .expect("the unread query answers");

    assert_eq!(unread, json!({"unread": []}), "no input turn and no change reads nothing and adds no fields");
  }

  #[tokio::test]
  async fn a_kept_item_and_the_remaining_list_refuse_together_and_store_after_one_round_of_records() {
    let home = Home::new();
    let sid = home.manager("remaining-settle", None).await;
    write_stored_status(
      &home,
      &sid,
      json!({
        "goal": "Ship",
        "now": "Running",
        "remaining": ["Land the list"],
        "waiting": [],
        "updated_at": "2026-01-01T00:00:00Z",
      }),
    )
    .await;
    let item = waiting_item("Decide the layout");
    record_ask_check(&home.features, &sid, &item, "synthetic-haiku", "The item is ready.")
      .await
      .expect("the item cold read records");
    set_status(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string()).await.expect("the item stores");
    home.launch_run(&sid, "run-1", &["u1"], "2026-01-01T00:00:10Z");
    home
      .append(&sid, json!({"type": "user", "id": "u1", "content": "What now?", "timestamp": "2026-01-01T00:00:09Z"}))
      .await;

    let unread = unread_waiting(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
      .await
      .expect("the unread query answers");
    assert!(unread["settle"].as_array().is_some_and(|settle| settle.len() == 1), "{unread}");
    assert_eq!(unread["remaining"], json!(["Land the list"]), "one answer carries both contexts");

    let error = set_status(&home.features, &sid, &json!({"waiting": [item.clone()]}).to_string())
      .await
      .expect_err("both gates refuse");
    let refusal = detail(error);
    assert!(refusal.contains("was kept, so it needs a cold read of this turn's record"), "{refusal}");
    assert!(refusal.contains("Remaining needs a cold read of this turn's record"), "{refusal}");

    append_settle_record(&home, &sid, &item, "2026-01-01T00:00:11Z").await;
    append_remaining_record(&home, &sid, "2026-01-01T00:00:12Z").await;
    // The agent removes the settled item and leaves Remaining as it stood: the recorded read stores the set without
    // a second probe.
    set_status(&home.features, &sid, &json!({"waiting": []}).to_string()).await.expect("the set stores");
    assert_eq!(stored(&home, &sid).await["waiting"], json!([]));
    assert_eq!(stored(&home, &sid).await["remaining"], json!(["Land the list"]));
    assert_eq!(remaining_checks(&home, &sid).len(), 1, "the retry runs no second remaining read");
  }
}
