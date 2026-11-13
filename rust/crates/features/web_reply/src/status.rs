//! A web session's status: the Goal, Now and Waiting on you items
//! that `charliebot web status` sets and reads. A set stores the items its JSON object names, records one `web_status`
//! event and tells every ancestor page. A set refuses a Waiting on you item until a cold read of it is recorded; the
//! read tiers the item against the approval policy table, and with the table's refusal switch on a set refuses an
//! item tiered t1 or t2. A read lists the open descendants whose status holds Waiting on you items and the open
//! children that hold a Now.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, LazyLock, Mutex};

use axum::http::StatusCode;
use charliebot_compat::file_urls;
use charliebot_compat::file_urls::ScanContext;
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types::{WEB_ASK_CHECK, WEB_REPLY, WEB_STATUS, WEB_STATUS_CONFIRMED};
use charliebot_core::json_loads::{self, LoadError};
use charliebot_core::str_methods;
use charliebot_core::text_repr::{JsonStyle, json_dumps};
use charliebot_registry::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, session_channel};
use charliebot_user_text_checks as user_text_checks;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

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
const ITEMS: [&str; 3] = ["goal", "now", "waiting"];

/// The label of each item in the event's content.
fn label(item: &str) -> &'static str {
  match item {
    "goal" => "Goal",
    "now" => "Now",
    _ => "Waiting on you",
  }
}

/// The non-persisted frame an ancestor's page receives when a descendant sets its status (`SUBTREE_NOTICE_TYPE`).
const SUBTREE_NOTICE_TYPE: &str = "web_status_subtree";

/// The validation messages a status file can break, as the `StatusPatch` and `WaitingItem` readers raise them.
const MUST_NOT_BE_BLANK: &str = "Value error, must not be blank";
const NOT_A_STRING: &str = "Input should be a valid string";
const NOT_A_LIST: &str = "Input should be a valid list";
const NOT_A_WAITING_ITEM: &str = "Input should be a valid dictionary or instance of WaitingItem";
const FIELD_REQUIRED: &str = "Field required";
const EXTRA_FORBIDDEN: &str = "Extra inputs are not permitted";
const NOW_BUDGET_CHARS: usize = 200;
const ASK_CHECK_CONTENT: &str = "Cold read of a Waiting on you item";
const ITEM_SHA256: &str = "item_sha256";
/// The item fields that only the server writes; a set file carries neither.
const SERVER_WRITTEN_ITEM_FIELDS: [&str; 4] = ["since", "since_exact", "tier", "rule"];
/// The message a since field that is not a datetime raises.
const NOT_A_DATETIME: &str = "Input should be a valid datetime";
/// The message a since_exact field that is not a boolean raises.
const NOT_A_BOOLEAN: &str = "Input should be a valid boolean";

/// The session status: Goal, Now and Waiting on you, and the time the last set stored them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebStatus {
  goal: Option<String>,
  now: Option<String>,
  waiting: Vec<WaitingItem>,
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

/// Refuse a set file that names a field only the server writes: a waiting item's `since`, `since_exact`, `tier` and
/// `rule`. Only a list `waiting` is examined, and only its object items.
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
  payload["waiting"] = serde_json::to_value(effective_waiting(status)).expect("waiting items serialize to JSON");
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

/// The items with each one's `tier` and `rule` filled in from the ask-check its digest matches.
/// An item whose check predates the tier keeps neither field.
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

/// The tier and rule each `web_ask_check` event gave its item digest, the latest event per digest winning. A check recorded before the tier existed contributes no entry.
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

/// The table as the server reads it, with a changed read recording its `policy_table_changed` record.
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

/// Record one cold read of `raw_item` and return its digest. The answer's last line
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
  let page_refusals = page_link_refusals(&prospective);
  if !page_refusals.is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, page_link_refusal_detail(&page_refusals)));
  }
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

/// A nested validation location, including the root's empty location.
fn nested_loc(prefix: &str, field: &str) -> String {
  if prefix.is_empty() { field.to_string() } else { format!("{prefix}.{field}") }
}

/// The texts the file-link scanner reads, in order: goal, now, then each waiting item's text and context. A structured
/// Waiting link is checked by the page-link owner.
fn scanned_texts(patch: &StatusPatch) -> Vec<&str> {
  let mut texts: Vec<&str> = patch.goal.iter().chain(patch.now.iter()).flatten().map(String::as_str).collect();
  for item in patch.waiting.iter().flatten() {
    texts.push(item.text.as_str());
    texts.extend([item.context.as_deref()].into_iter().flatten());
  }
  texts
}

/// Store the items the JSON `text` names, record one `web_status` event and notify the ancestors. Waiting items need
/// a non-blank context and a matching cold-read digest, whose tier and rule the stored item carries; with the
/// approval policy table's refusal switch on, an item tiered t1 or t2 refuses the set. A stale plan link refuses the
/// set before the cold read. A thread session's set names the goal alone; one that names Now or Waiting on you is
/// refused. The return adds the Now length; a refusal stores and records nothing.
pub async fn set_status(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  let scan_context = reply::file_link_scan_context(features, session_id)?;
  let meta = reply::require_manager_session(features, session_id).await?;
  let patch = parse_patch(text)?;
  if !reply::is_web_session(&meta) && patch.named() != ["goal"] {
    return Err(WebReplyError::refused(
      StatusCode::CONFLICT,
      "Session is a thread session: only its goal can be set; now and waiting on you stay in the thread status block",
    ));
  }
  reply::refuse_dead_links(scanned_texts(&patch), &scan_context)?;
  require_waiting_context(patch.waiting.as_deref().unwrap_or_default())?;
  let stored = stored_status(&meta)?;
  let base =
    stored.clone().unwrap_or(WebStatus { goal: None, now: None, waiting: Vec::new(), updated_at: UtcDateTime::now() });
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
  let mut ask_tiers = HashMap::new();
  if let Some(items) = patch.waiting.as_deref()
    && !items.is_empty()
  {
    let approval = load_approval_policy(features)?;
    ask_tiers = read_ask_tiers(features, session_id).await?;
    let read_digests: HashSet<String> = ask_tiers.keys().cloned().collect();
    for (index, item) in items.iter().enumerate() {
      let digest = match item_sha256(item) {
        Ok(digest) => digest,
        Err(_) if page_refusals.iter().any(|(refused_index, _)| *refused_index == index) => continue,
        Err(error) => return Err(error),
      };
      if !read_digests.contains(&digest) {
        refusal_details.push(format!("Waiting on you item {:?} needs a cold read", item.text));
        refused = true;
        continue;
      }
      if approval.table.enable_ask_tier_refusal
        && let Some((tier, rule)) = ask_tiers.get(&digest)
        && matches!(tier.as_str(), "t1" | "t2")
      {
        refusal_details.push(format!(
          "Waiting on you item {:?} is tiered {tier} (rule: {}): carry out the step and record it with \
           charliebot digest add instead of waiting for approval",
          item.text,
          rule.as_deref().unwrap_or("none")
        ));
        refused = true;
      }
    }
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
      assign_tiers(assign_since(items, &stored_waiting, links, now)?, &ask_tiers)?
    }
    None => stored_waiting,
  };
  let mut status = base;
  let changed = patch.named();
  let StatusPatch { goal, now: now_item, waiting: _ } = patch;
  if let Some(goal) = goal {
    status.goal = goal;
  }
  if let Some(now_item) = now_item {
    status.now = now_item;
  }
  status.waiting = waiting;
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

/// Confirm that the stored status needs no change and return it.
///
/// A stale plan link in a stored item refuses the confirmation with 409 and names the newest file. A confirmation
/// records one web_status_confirmed event and cold reads nothing.
pub async fn confirm_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let status = stored_status(&meta)?.unwrap_or(WebStatus {
    goal: None,
    now: None,
    waiting: Vec::new(),
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
  statuses: Vec<(String, SessionMetadata, WebStatus)>,
}

/// Read the session's subtree: the direct children, then each descendant's metadata and stored status while that
/// descendant is open (its task state is `open`, and neither it nor an ancestor is archived).
///
/// A session deleted since the task-tree index was read is absent, so it answers no row.
fn read_status_tree(store: &Store, session_id: &str) -> Result<StatusTree, WebReplyError> {
  let children = store.child_ids(session_id)?;
  let mut open = Vec::new();
  let mut statuses = Vec::new();
  for meta in store.open_descendants(session_id)? {
    let descendant_id = meta.id.clone();
    if let Some(status) = stored_status(&meta)? {
      statuses.push((descendant_id.clone(), meta, status));
    }
    open.push(descendant_id);
  }
  Ok(StatusTree { children, open, statuses })
}

/// The derived Waiting on you items that the registry's reader gives: those held by a session in `holders`, or all of
/// them when `holders` is `None`. The status read and the approval view both list them through this function, and no
/// stored status holds them.
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
        tier: row.tier,
        rule: row.rule,
        since: row.since,
        is_derived: true,
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
    "tier": item.tier,
    "rule": item.rule,
    "since": item.since.to_str(),
  })
}

/// The session's status, waiting items of open descendants and Now items of open children, newest
/// first. A descendant needs waiting items; a child needs a Now value. The derived items the session holds ride
/// beside the stored ones, under `derived_waiting`. The session's own task state rides under `own_task_state`: the
/// approval page lists a closed session's stored items nowhere, so a count that opens the page leaves them out.
pub async fn read_status(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  let meta = reply::require_web_session(features, session_id).await?;
  let stored = stored_status(&meta)?;
  let (tree, own_task_state) = tree_read_reply(&features.store, {
    let session_id = session_id.to_string();
    move |store| {
      let tree = read_status_tree(store, &session_id)?;
      let own_task_state = store.task_state(&session_id)?;
      Ok((tree, own_task_state))
    }
  })
  .await?;
  let mut holders = HashSet::from([session_id.to_string()]);
  holders.extend(tree.open.iter().cloned());
  let derived = derived_items(features, Some(&holders)).await?;
  let mut rows: Vec<(UtcDateTime, Value)> = Vec::new();
  for (descendant_id, child, child_status) in &tree.statuses {
    if child_status.waiting.is_empty() {
      continue;
    }
    let payload = status_payload(child_status);
    let row = json!({
      "session_id": descendant_id,
      "name": child.name,
      "updated_at": child_status.updated_at.to_str(),
      "waiting": payload["waiting"],
    });
    rows.push((child_status.updated_at, row));
  }
  rows.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
  let status_of: HashMap<&str, usize> =
    tree.statuses.iter().enumerate().map(|(index, (id, _, _))| (id.as_str(), index)).collect();
  let mut children: Vec<(UtcDateTime, Value)> = Vec::new();
  for child_id in &tree.children {
    let Some(index) = status_of.get(child_id.as_str()) else {
      continue;
    };
    let (_, child, child_status) = &tree.statuses[*index];
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
    "own_task_state": own_task_state,
    (WEB_STATUS): stored.as_ref().map(status_payload),
    "derived_waiting": derived.iter().map(derived_payload).collect::<Vec<_>>(),
    "descendants": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    "children": children.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
  }))
}

/// The status as its JSON serialization renders it.
fn payload_of(status: &WebStatus) -> Value {
  serde_json::to_value(status).expect("a web status serializes to JSON")
}

/// One open session's Waiting on you item with the session that holds it. The start time
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
  pub is_derived: bool,
}

/// The sessions whose stored web status carries waiting items, filtered from one metadata corpus.
///
/// The approval day view asks every session of the home for its status on each load, and the corpus holds one row per
/// session of the home. The filter runs when the corpus moves and the rows are reused while the same corpus holds; the
/// memo keeps the corpus it filtered, so a corpus that replaces it holds a different allocation.
struct WaitingSessions {
  corpus: Arc<Vec<Arc<SessionMetadata>>>,
  rows: Arc<Vec<Arc<SessionMetadata>>>,
}

static WAITING_SESSIONS: LazyLock<Mutex<Option<WaitingSessions>>> = LazyLock::new(|| Mutex::new(None));

/// The corpus rows whose stored status names at least one waiting item, in corpus order.
fn sessions_with_waiting(
  metas: &Arc<Vec<Arc<SessionMetadata>>>,
) -> Result<Arc<Vec<Arc<SessionMetadata>>>, WebReplyError> {
  {
    let memo = WAITING_SESSIONS.lock().expect("the waiting sessions mutex is not poisoned");
    if let Some(entry) = memo.as_ref()
      && Arc::ptr_eq(&entry.corpus, metas)
    {
      return Ok(Arc::clone(&entry.rows));
    }
  }
  let mut rows = Vec::new();
  for meta in metas.iter() {
    if stored_status(meta)?.is_some_and(|status| !status.waiting.is_empty()) {
      rows.push(Arc::clone(meta));
    }
  }
  let rows = Arc::new(rows);
  *WAITING_SESSIONS.lock().expect("the waiting sessions mutex is not poisoned") =
    Some(WaitingSessions { corpus: Arc::clone(metas), rows: Arc::clone(&rows) });
  Ok(rows)
}

/// Every open session's Waiting on you items, the stored ones in metadata-listing order and then the derived ones. The
/// approval day view reads the whole home through this, where the status read reads one task tree.
pub async fn open_waiting_items(features: &FeatureContext) -> Result<Vec<OpenWaitingItem>, WebReplyError> {
  let archived = derived_archived_ids(&features.store).await?.iter().cloned().collect::<HashSet<_>>();
  // The read takes the corpus the listing memo holds instead of a copy of it: the approval day view reads this on
  // every page load, and copying one row per session of the home cost more than the rest of the view together.
  let (metas, _) = tree_read(&features.store, |store| store.load_session_metas_snapshot_versioned(None)).await?;
  let waiting = sessions_with_waiting(&metas)?;
  let mut rows = Vec::new();
  for meta in waiting.iter() {
    let status = stored_status(meta)?.expect("the filter kept only sessions whose status carries waiting items");
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
        is_derived: false,
      });
    }
  }
  rows.extend(derived_items(features, None).await?);
  Ok(rows)
}

/// The registry's reader for every open session's Waiting on you items (`charliebot_registry::OpenWaitingItemsFn`), which the
/// approval view looks up on the registry instead of importing this feature.
pub fn open_waiting_rows<'a>(
  features: &'a FeatureContext,
) -> futures_util::future::BoxFuture<'a, anyhow::Result<Vec<charliebot_registry::registry::OpenWaitingRow>>> {
  Box::pin(async move {
    let rows = open_waiting_items(features).await.map_err(|error| match error {
      WebReplyError::Failed(error) => error,
      WebReplyError::Refused { status, detail } => anyhow::anyhow!("{status}: {detail}"),
    })?;
    Ok(
      rows
        .into_iter()
        .map(|row| charliebot_registry::registry::OpenWaitingRow {
          session_id: row.session_id,
          session_name: row.session_name,
          text: row.text,
          context: row.context,
          link: row.link,
          tier: row.tier,
          rule: row.rule,
          since: row.since,
          is_derived: row.is_derived,
        })
        .collect(),
    )
  })
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

async fn derived_archived_ids(store: &Arc<Store>) -> Result<Arc<Vec<String>>, WebReplyError> {
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
  use crate::test_support::{self, Home, set_derived_waiting};
  use charliebot_registry::registry::OpenWaitingRow;

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
  async fn a_refused_status_set_stores_nothing_and_records_no_event() {
    let home = Home::new();
    let own = home.manager("own", None).await;
    let thread = home
      .manager(
        "thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;
    let worker = home.worker(&own, "worker").await;
    let cases =
      [("unknown", VALID_SET, 404), ("thread", VALID_SET, 409), ("worker", VALID_SET, 409), ("own", "not json", 422)];
    for (case, text, status) in cases {
      let sid = match case {
        "thread" => thread.as_str(),
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
      (r#"{"remaining": "x"}"#, "Status file is invalid: remaining: Extra inputs are not permitted"),
      (
        r#"{"remaining": [{"text": "Move the server", "done": true}]}"#,
        "Status file is invalid: remaining: Extra inputs are not permitted",
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
          tier: Some("t0".to_string()),
          rule: Some("irreversible".to_string()),
        }],
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
        "tier": "t0",
        "rule": "irreversible"
      }])
    );
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

    // A closed task's stored items appear on the approval page nowhere, and the count that opens the page leaves
    // them out through this field.
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
        let mut item = json!({
          "text": "Decide",
          "context": "The choice is ready. Options are keep or change; keep is recommended.",
          "link": null,
        });
        let (patch, expected_detail) = match field {
          "goal" => (json!({"goal": format!("Review {local_link}")}), Some(local_detail)),
          "now" => (json!({"now": format!("Review {local_link}")}), Some(local_detail)),
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
  async fn set_status_returns_all_refusals() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    set_status(&home.features, &sid, r#"{"now": "Running"}"#).await.expect("the baseline status stores");
    let t1 = waiting_item("Run the covered release");
    let t2 = waiting_item("Choose the design");
    let directory = tempfile::tempdir().expect("the page directory exists");
    let markdown = directory.path().join("source.md");
    fs::write(&markdown, "source text").expect("the mapped source writes");
    let bad_link = json!({
      "text": "Open the markdown source",
      "context": "The source is available now.",
      "link": file_link(&markdown),
    });
    for (item, answer) in [
      (&t1, "Inside the policy.\ntier: t1 rule: release-restart"),
      (&t2, "Choose the design.\ntier: t2 rule: none"),
      (&bad_link, "The page is available.\ntier: t0 rule: irreversible"),
    ] {
      record_ask_check(&home.features, &sid, item, "synthetic-haiku", answer)
        .await
        .expect("the item cold read records");
    }
    test_support::write_approval_policy_table(
      &home.root(),
      &test_support::APPROVAL_POLICY_TABLE.replace("enable_ask_tier_refusal: false", "enable_ask_tier_refusal: true"),
    );
    let before = stored(&home, &sid).await;
    let status_event_count = status_events(&home, &sid).len();

    let error = set_status(&home.features, &sid, &json!({"waiting": [t1, t2, bad_link]}).to_string())
      .await
      .expect_err("the set has tier and page-link refusals");

    let detail = match error {
      WebReplyError::Refused { status, detail } => {
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        detail
      }
      other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(detail.contains("Run the covered release") && detail.contains("tiered t1"), "{detail}");
    assert!(detail.contains("Choose the design") && detail.contains("tiered t2"), "{detail}");
    assert!(
      detail.contains("waiting.2 item \"Open the markdown source\"") && detail.contains("non-page file"),
      "{detail}"
    );
    assert_eq!(stored(&home, &sid).await, before);
    assert_eq!(status_events(&home, &sid).len(), status_event_count);
  }

  #[tokio::test]
  async fn set_status_full_list_keeps_s1() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let a = waiting_item("A: publish the result");
    let b = waiting_item("B: choose the design");
    let c = waiting_item("C: sign in to the console");
    for item in [&a, &b, &c] {
      record_ask_check(&home.features, &sid, item, "synthetic-haiku", "The first grade.\ntier: t0 rule: money")
        .await
        .expect("the initial grade records");
    }
    set_status(&home.features, &sid, &json!({"now": "Running", "waiting": [&a, &b, &c]}).to_string())
      .await
      .expect("the original complete list stores");
    let original = stored(&home, &sid).await;
    for (item, answer) in [
      (&a, "The step is t0.\ntier: t0 rule: money"),
      (&b, "The step is t2.\ntier: t2 rule: none"),
      (&c, "The user must act.\ntier: user-act rule: none"),
    ] {
      record_ask_check(&home.features, &sid, item, "synthetic-haiku", answer).await.expect("the current grade records");
    }
    test_support::write_approval_policy_table(
      &home.root(),
      &test_support::APPROVAL_POLICY_TABLE.replace("enable_ask_tier_refusal: false", "enable_ask_tier_refusal: true"),
    );
    let status_event_count = status_events(&home, &sid).len();

    let error = set_status(&home.features, &sid, &json!({"waiting": [&a, &b, &c]}).to_string())
      .await
      .expect_err("the full list has one refused item");

    assert!(matches!(&error, WebReplyError::Refused { status, .. } if *status == StatusCode::CONFLICT));
    assert!(detail(error).contains("B: choose the design"));
    assert_eq!(stored(&home, &sid).await, original);
    assert_eq!(status_events(&home, &sid).len(), status_event_count);

    let updated = set_status(&home.features, &sid, &json!({"waiting": [&a, &c]}).to_string())
      .await
      .expect("the handled B item is removed from the current complete list");
    assert_eq!(
      without_item_times(&updated["waiting"]),
      json!([
        {"text": "A: publish the result", "context": "The decision is active now. Options are continue or stop; continue is recommended.", "link": null, "tier": "t0", "rule": "money"},
        {"text": "C: sign in to the console", "context": "The decision is active now. Options are continue or stop; continue is recommended.", "link": null, "tier": "user-act", "rule": null},
      ])
    );
  }

  #[tokio::test]
  async fn set_status_full_list_keeps_s3() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let a = waiting_item("Keep the first action");
    let b = waiting_item("Repair the page link");
    for item in [&a, &b] {
      record_ask_check(&home.features, &sid, item, "synthetic-haiku", "The step is approved.\ntier: t0 rule: money")
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
    record_ask_check(&home.features, &sid, &fixed_b, "synthetic-haiku", "The step is approved.\ntier: t0 rule: money")
      .await
      .expect("the corrected link gets a current cold read");
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

  /// A thread manager stores its Goal alone, and the stored Goal is the one the Goal hook reads. A set that names Now
  /// or Waiting on you is refused, and stores nothing.
  #[tokio::test]
  async fn a_thread_manager_sets_its_goal_alone() {
    let home = Home::new();
    let thread = home
      .manager(
        "thread",
        Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"}))),
      )
      .await;

    set_status(&home.features, &thread, r#"{"goal": "Ship the thread reply"}"#)
      .await
      .expect("the goal-only set stores");
    let meta = home.meta(&thread).await;
    assert_eq!(stored_goal(&meta).unwrap().as_deref(), Some("Ship the thread reply"));

    for text in [r#"{"now": "Running"}"#, r#"{"goal": "Renamed", "now": "Running"}"#] {
      let error = set_status(&home.features, &thread, text).await.expect_err("the set is refused");
      assert!(matches!(error, WebReplyError::Refused { status: StatusCode::CONFLICT, .. }), "{text}");
    }
    assert_eq!(stored_goal(&home.meta(&thread).await).unwrap().as_deref(), Some("Ship the thread reply"));
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
      tier: Some("user-act".to_string()),
      rule: None,
      since: UtcDateTime::parse("2026-11-01T21:43:00-07:00").expect("the fixture time parses"),
      is_derived: true,
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

    // The approval view lists every open holder's derived item with the stored ones.
    let rows = open_waiting_items(&home.features).await.expect("the approval rows read");
    let listed: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
    assert_eq!(listed, [root, child, unrelated]);
    assert!(rows.iter().all(|row| row.tier.as_deref() == Some("user-act") && row.rule.is_none()));
  }

  #[tokio::test]
  async fn a_waiting_item_set_after_an_approval_read_appears_on_the_next_one() {
    let home = Home::new();
    let first = home.manager("first", None).await;
    let second = home.manager("second", None).await;
    assert!(open_waiting_items(&home.features).await.expect("the empty read lists nothing").is_empty());

    // The set writes metadata, so the corpus the filter ran on moves and the next read filters the new one.
    set_after_reads(&home, &second, planned_item("Decide", Value::Null)).await;

    let rows = open_waiting_items(&home.features).await.expect("the read after the set lists the item");
    let listed: Vec<&str> = rows.iter().map(|row| row.session_id.as_str()).collect();
    assert_eq!(listed, [second.as_str()], "the item the set stored, not the corpus the earlier read filtered");
    assert_ne!(first, second);
  }
}
