//! The web session's turn-end checks: the web reply to each user message the turn received (`web-reply`), the status
//! set or confirm that ends the turn (`web-status`), and the status sets that store the Waiting on you and Remaining
//! lists after the turn's last input (`status-reread`).
//!
//! All three are names in the turn-end check list, and the web contribution answers each one in process from the
//! shared input (`TurnInput`), so the checks read only the turn's events. A first miss blocks the turn with the reason.
//! A second miss passes: `web-reply` then shows the user the no-reply notice, `web-status` passes quietly, and
//! `status-reread` shows the lists it left. A continuation that failed is a second miss too: `web-status` then shows
//! its notice, so the failed turn never passes. A turn with a close request, and a turn that exits with an error other
//! than a failed continuation, pass all three checks.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::event_types::{
  AGENT_MESSAGE, CHILD_REPORT, FROM_SESSION, FROM_SESSION_NAME, INPUT_EVENT_ID, OWNER_RUN_ID, TASK_CLOSE_REQUESTED,
  TURN_HOOKS, WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS, WEB_STATUS_CONFIRMED,
};
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{CheckResult, TurnInput};
use charliebot_store::compact::CompactValue;
use charliebot_store::reply_order;
use charliebot_store::{ChatEvent, Store};
use charliebot_tasks::task_input_prompt::input_text;
use charliebot_tasks::turn_hooks::{last_received_index, received_receipts};
use serde_json::{Value, json};

use crate::reply;

/// The entry that checks each user message of the turn has a web reply.
pub const WEB_REPLY_ENTRY: &str = "web-reply";
/// The entry that checks the turn ran a status set or confirm.
pub const WEB_STATUS_ENTRY: &str = "web-status";
/// The entry that checks a turn which received an input stored a status set after it that names each list the turn must
/// store: Waiting on you, and Remaining for a turn that received a user message or a child report.
pub const STATUS_REREAD_ENTRY: &str = "status-reread";

/// The status instruction that ends a status reminder.
const STATUS_SENTENCES: &str = concat!(
  "Run `charliebot web status set` when Goal, Remaining, Now or Waiting on you differs from the state that turn ",
  "read back, when the turn received an input and Waiting on you holds items, or when the turn received a user ",
  "message or a child report and Remaining holds items. ",
  "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back."
);

/// The reason the `web-status` check blocks a turn that recorded no status set or confirm.
fn status_reminder_text() -> String {
  format!("Session status reminder: your last turn ended without a status set or confirm. {STATUS_SENTENCES}")
}

/// The notice of a `web-status` miss that follows a failed continuation: the turn that answered the reminder ended in
/// an error, so no status was set.
const FAILED_CONTINUATION_STATUS_NOTICE: &str = concat!(
  "Session status check: the continuation of your last turn ended in an error, so no status was set. ",
  "The panel shows the state the last turn read back."
);

/// Whether the web checks judge this turn: the run is not colleague-started, since its requests to write are refused,
/// the turn exited cleanly or as a failed continuation, and it left no close request. The `web-reply` and `web-status`
/// entries read their scope here, as does the `pending-wake` check. The contribution runs these checks only on manager
/// sessions, so each check that reads this scope passes on every other session.
pub fn web_turn_checks_apply(input: &TurnInput) -> bool {
  !input.colleague_read_only
    && input.exit_is_judged()
    && !input.turn.iter().any(|event| event["type"] == TASK_CLOSE_REQUESTED)
}

/// Whether the turn already blocked on the entry `name`: a `turn_hooks` record of this turn holds its block.
fn blocked_before(turn: &[Value], name: &str) -> bool {
  turn.iter().any(|event| {
    event["type"] == TURN_HOOKS
      && event["results"]
        .as_array()
        .is_some_and(|results| results.iter().any(|result| result["name"] == name && result["result"] == "block"))
  })
}

/// The `web-reply` entry: each user message the turn received has a web reply later than its receipt. The first miss
/// blocks with the nudge text of each unanswered message; a miss after that passes with the notice.
pub fn check_web_reply(input: &TurnInput) -> anyhow::Result<CheckResult> {
  if !web_turn_checks_apply(input) {
    return Ok(CheckResult::Pass);
  }
  let unanswered = unanswered_user_messages(&input.turn)?;
  if unanswered.is_empty() {
    return Ok(CheckResult::Pass);
  }
  if blocked_before(&input.turn, WEB_REPLY_ENTRY) {
    return Ok(CheckResult::Notice(reply::NOTICE_TEXT.to_string()));
  }
  let reasons: Vec<String> = unanswered.iter().map(|(_, text)| reply::nudge_text(text)).collect();
  Ok(CheckResult::Block(reasons.join("\n\n")))
}

/// The `web-status` entry: the turn ran a status set or confirm. The first miss blocks with the reminder. A miss after
/// that passes quietly, unless the turn is a failed continuation, which shows the notice that no status was set.
pub fn check_web_status(input: &TurnInput) -> CheckResult {
  if !web_turn_checks_apply(input) {
    return CheckResult::Pass;
  }
  let recorded = input.turn.iter().any(|event| event["type"] == WEB_STATUS || event["type"] == WEB_STATUS_CONFIRMED);
  if recorded {
    return CheckResult::Pass;
  }
  if blocked_before(&input.turn, WEB_STATUS_ENTRY) {
    return if input.continuation_failed {
      CheckResult::Notice(FAILED_CONTINUATION_STATUS_NOTICE.to_string())
    } else {
      CheckResult::Pass
    };
  }
  CheckResult::Block(status_reminder_text())
}

/// The `status-reread` entry: a turn that received an input stores a status set after its last input that names each
/// list the turn must store (`required_lists`). A confirm, a set stored before the last input, and a set that printed
/// its cold read and stored nothing do not count. The first miss blocks with the reason; a miss after that passes with
/// the notice of the lists left. A parent-approved session's Waiting on you list is exempt: its status set refuses a
/// non-empty list, so the reason could not be met.
///
/// This is stage 1 of a staged rollout. Remaining is required only when the turn received a user message, an external-
/// thread user input, or a child report; a turn whose inputs are only agent messages or scheduled triggers keeps the
/// rule it had before Remaining.
pub fn check_status_reread(input: &TurnInput) -> CheckResult {
  if !web_turn_checks_apply(input) {
    return CheckResult::Pass;
  }
  let Some(last) = last_received_index(&input.turn) else {
    return CheckResult::Pass;
  };
  let after = &input.turn[last + 1..];
  let unset: Vec<StatusList> = required_lists(input).into_iter().filter(|list| !names_list(after, list.key)).collect();
  if unset.is_empty() {
    return CheckResult::Pass;
  }
  if blocked_before(&input.turn, STATUS_REREAD_ENTRY) {
    return CheckResult::Notice(left_items_notice(&unset));
  }
  CheckResult::Block(reread_reason(input, last, &unset))
}

/// A non-empty list of the stored status that a turn with an input stores again after the input: its key in the status
/// payload and in a set's `web_status_changed`, its label in the reason and the notice, and its item texts.
struct StatusList {
  key: &'static str,
  label: &'static str,
  items: Vec<String>,
}

/// The lists that a turn with an input must store: each non-empty list of the stored status. A parent-approved
/// session's Waiting on you list is not one of them, and Remaining is one only for a turn that received a user message,
/// an external-thread user input, or a child report.
fn required_lists(input: &TurnInput) -> Vec<StatusList> {
  let mut lists = Vec::new();
  if !input.parent_approved {
    lists.push(StatusList { key: "waiting", label: "Waiting on you", items: waiting_texts(&input.status) });
  }
  if received_user_or_report(input) {
    lists.push(StatusList { key: "remaining", label: "Remaining", items: remaining_texts(&input.status) });
  }
  lists.retain(|list| !list.items.is_empty());
  lists
}

/// Whether the turn received a user message, an external-thread input, or a child report. `reply::answered_user_id`
/// identifies web messages and their reply nudges. `chat_threads` admits external-thread inputs as agent messages from
/// the bound session itself, with a non-empty session name.
fn received_user_or_report(input: &TurnInput) -> bool {
  input.turn.iter().any(|event| {
    event["received_at"].is_string()
      && (reply::answered_user_id(event).is_some()
        || external_thread_user_input(event, &input.session_id)
        || event["type"] == CHILD_REPORT)
  })
}

/// Whether an agent message is an external-thread input admitted from this session's own channel binding.
fn external_thread_user_input(event: &Value, session_id: &str) -> bool {
  event["type"] == AGENT_MESSAGE
    && event[FROM_SESSION].as_str() == Some(session_id)
    && event[FROM_SESSION_NAME].as_str().is_some_and(|name| !name.is_empty())
}

/// Whether a set stored in `events` names the list `key` in its changed fields.
fn names_list(events: &[Value], key: &str) -> bool {
  events.iter().any(|event| {
    event["type"] == WEB_STATUS
      && event["web_status_changed"]
        .as_array()
        .is_some_and(|changed| changed.iter().any(|item| item.as_str() == Some(key)))
  })
}

/// The text of each Waiting on you item of the stored status payload, in list order.
fn waiting_texts(status: &Value) -> Vec<String> {
  status["waiting"]
    .as_array()
    .into_iter()
    .flatten()
    .map(|item| item["text"].as_str().unwrap_or_default().to_string())
    .collect()
}

/// The text of each Remaining milestone of the stored status payload, in list order.
fn remaining_texts(status: &Value) -> Vec<String> {
  status["remaining"]
    .as_array()
    .into_iter()
    .flatten()
    .map(|item| item.as_str().unwrap_or_default().to_string())
    .collect()
}

/// The names of the unset lists as the reason and the notice write them: `the Remaining list`, or
/// `the Waiting on you and Remaining lists`.
fn list_names(unset: &[StatusList]) -> String {
  let labels: Vec<&str> = unset.iter().map(|list| list.label).collect();
  let noun = if unset.len() == 1 { "list" } else { "lists" };
  format!("the {} {noun}", labels.join(" and "))
}

/// The reason the `status-reread` check blocks with: the receipt time of the turn's input at `last`, the opening of the
/// text the prompt renders for it, and the item count of each list left unset.
fn reread_reason(input: &TurnInput, last: usize, unset: &[StatusList]) -> String {
  let event = &input.turn[last];
  let received = event["received_at"].as_str().unwrap_or_default();
  let text = input_text(event, input.last_input_provenance.as_deref());
  let opening = reply::message_opening(&text);
  let counts: Vec<String> = unset
    .iter()
    .map(|list| {
      let noun = if list.items.len() == 1 { "item" } else { "items" };
      format!("{} holds {} {noun}", list.label, list.items.len())
    })
    .collect();
  let stands = if unset.len() == 1 { "as it stands" } else { "as they stand" };
  format!(
    "Session status reminder: your last turn received an input at {received}, which begins \"{opening}\", and {}. \
     Run `charliebot web status show`, then run `charliebot web status set` with {} {stands} after that input.",
    counts.join(" and "),
    list_names(unset),
  )
}

/// The notice of a `status-reread` second miss: the lists the turn left unset, each with its items as stored.
fn left_items_notice(unset: &[StatusList]) -> String {
  let left: Vec<String> =
    unset.iter().map(|list| format!("The {} items left as stored: {}.", list.label, list.items.join("; "))).collect();
  format!(
    "Session status check: your last turn received an input and stored no status set after it that names {}. {}",
    list_names(unset),
    left.join(" ")
  )
}

/// The user messages the turn received that no web reply later than their receipt answers, each with its id and text.
fn unanswered_user_messages(turn: &[Value]) -> anyhow::Result<Vec<(String, String)>> {
  let events: Vec<ChatEvent> = turn.iter().map(CompactValue::from_value).collect();
  let mut seen = HashSet::new();
  let mut unanswered = Vec::new();
  for receipt in received_receipts(turn)? {
    let Some(input) = events
      .iter()
      .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(receipt.input_event_id.as_str()))
    else {
      continue;
    };
    let Some(user_id) = reply::answered_user_id(input) else { continue };
    if reply_order::answered(&receipt, &events, WEB_REPLY)? || !seen.insert(user_id.clone()) {
      continue;
    }
    let text = events
      .iter()
      .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(user_id.as_str()))
      .and_then(|event| event.get("content").and_then(CompactValue::as_str))
      .unwrap_or_default()
      .to_string();
    unanswered.push((user_id, text));
  }
  Ok(unanswered)
}

/// Shows the notice `message` for each user message of the turn that no web reply answers, then marks the session
/// unread. A message that already holds this notice gets none, so a retried delivery adds nothing; another notice
/// on the same message still shows. A turn with no such message attaches nothing and resolves to `false`: the
/// turn-end runner then records the notice itself, as a notice that admits no input.
pub async fn show_notice(
  features: &Arc<FeatureContext>,
  session_id: &str,
  turn: &[Value],
  message: &str,
) -> anyhow::Result<bool> {
  let unanswered = unanswered_user_messages(turn)?;
  if unanswered.is_empty() {
    return Ok(false);
  }
  let loaded = reply::load_events(features, session_id).await?;
  for (user_id, _) in unanswered {
    if reply::noticed(loaded.as_slice(), &user_id, message) {
      continue;
    }
    features
      .store
      .persist_and_broadcast(
        session_id,
        json!({
          "type": WEB_REPLY_NOTICE,
          "content": message,
          WEB_REPLY_NOTICE: {INPUT_EVENT_ID: user_id},
        }),
      )
      .await?;
  }
  features.store.mark_unread(session_id).await?;
  Ok(true)
}

/// Whether the session's newest run left a close request pending. A request that a
/// later run owns was voided by the wake that started that run, so it does not count.
pub(crate) async fn close_request_pending(store: &Arc<Store>, session_id: &str) -> anyhow::Result<bool> {
  let store = Arc::clone(store);
  let session = session_id.to_string();
  tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
    let facts = store.facts_of(&session)?;
    let newest = store.newest_run_record_sync(&session)?;
    let newest_run_id = newest.as_ref().map(|run| run.id.as_str());
    let closed: HashSet<Option<&str>> =
      facts.close_events.iter().map(|event| event.get("request_id").and_then(CompactValue::as_str)).collect();
    Ok(facts.close_requests.iter().any(|request| {
      let request_id = request.get("request_id").and_then(CompactValue::as_str);
      request.get(OWNER_RUN_ID).and_then(CompactValue::as_str) == newest_run_id && !closed.contains(&request_id)
    }))
  })
  .await
  .context("the task-tree read stopped")?
}

#[cfg(test)]
mod tests {
  use charliebot_core::event_types::{COLD_READ_CHECK, SCHEDULED_TRIGGER};
  use charliebot_tasks::task_input_prompt::child_report_text;

  use super::*;
  use crate::test_support::Home;

  fn input(colleague_read_only: bool, exit_code: i64, turn: Vec<Value>) -> TurnInput {
    TurnInput {
      session_id: "web-session".to_string(),
      colleague_read_only,
      parent_approved: false,
      exit_code,
      continuation_failed: false,
      status: Value::Null,
      last_input_provenance: None,
      turn,
    }
  }

  /// The input of a turn that is the failed continuation of a block.
  fn failed_continuation(turn: Vec<Value>) -> TurnInput {
    TurnInput { continuation_failed: true, ..input(false, 1, turn) }
  }

  /// A user message received at `received_at`.
  fn user(id: &str, text: &str, received_at: &str) -> Value {
    json!({"type": "user", "id": id, "content": text, "timestamp": received_at, "received_at": received_at})
  }

  fn reply_at(id: &str, timestamp: &str) -> Value {
    json!({"type": WEB_REPLY, "id": id, "content": "answer", "timestamp": timestamp})
  }

  fn block_record(name: &str) -> Value {
    json!({"type": TURN_HOOKS, "id": "record-1", "results": [{"name": name, "result": "block", "reason": "r"}]})
  }

  fn text_of(result: &CheckResult) -> &str {
    match result {
      CheckResult::Block(text) | CheckResult::Notice(text) => text,
      CheckResult::Pass => panic!("the check passed"),
    }
  }

  #[test]
  fn an_unanswered_user_message_blocks_once_then_passes_with_the_notice() {
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z")];
    let first = check_web_reply(&input(false, 0, turn.clone())).unwrap();
    assert!(matches!(first, CheckResult::Block(_)), "{first:?}");
    assert!(text_of(&first).contains("The unanswered message begins: \"the question\"."));

    let mut second_turn = turn;
    second_turn.push(block_record(WEB_REPLY_ENTRY));
    let second = check_web_reply(&input(false, 0, second_turn)).unwrap();
    assert_eq!(second, CheckResult::Notice(reply::NOTICE_TEXT.to_string()));
  }

  #[test]
  fn a_web_reply_after_the_receipt_answers_the_message() {
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z"), reply_at("r1", "2026-01-01T00:00:05Z")];
    assert_eq!(check_web_reply(&input(false, 0, turn)).unwrap(), CheckResult::Pass);
  }

  #[test]
  fn a_reply_at_the_receipt_instant_does_not_answer_it() {
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z"), reply_at("r1", "2026-01-01T00:00:01Z")];
    assert!(matches!(check_web_reply(&input(false, 0, turn)).unwrap(), CheckResult::Block(_)));
  }

  #[test]
  fn a_status_set_or_confirm_passes_the_status_check_and_a_missing_one_blocks_once() {
    let bare = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    assert!(matches!(check_web_status(&input(false, 0, bare.clone())), CheckResult::Block(_)));

    let mut with_status = bare.clone();
    with_status.push(json!({"type": WEB_STATUS, "id": "s1"}));
    assert_eq!(check_web_status(&input(false, 0, with_status)), CheckResult::Pass);

    let mut with_confirm = bare.clone();
    with_confirm.push(json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"}));
    assert_eq!(check_web_status(&input(false, 0, with_confirm)), CheckResult::Pass);

    let mut second = bare;
    second.push(block_record(WEB_STATUS_ENTRY));
    assert_eq!(check_web_status(&input(false, 0, second)), CheckResult::Pass);
  }

  #[test]
  fn a_failed_continuation_misses_both_checks_again_and_never_passes() {
    let mut turn = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    turn.push(block_record(WEB_REPLY_ENTRY));
    turn.push(block_record(WEB_STATUS_ENTRY));
    let text = text_of(&check_web_status(&failed_continuation(turn.clone()))).to_string();
    assert_eq!(text, FAILED_CONTINUATION_STATUS_NOTICE);
    assert_eq!(
      check_web_reply(&failed_continuation(turn.clone())).unwrap(),
      CheckResult::Notice(reply::NOTICE_TEXT.to_string())
    );

    // A failed continuation that set its status before it ended still passes the status check.
    turn.push(json!({"type": WEB_STATUS, "id": "s1"}));
    assert_eq!(check_web_status(&failed_continuation(turn)), CheckResult::Pass);
  }

  #[test]
  fn the_checks_pass_for_a_colleague_started_turn_a_failed_turn_and_a_close_request() {
    let unanswered = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    assert_eq!(check_web_reply(&input(true, 0, unanswered.clone())).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_status(&input(true, 0, unanswered.clone())), CheckResult::Pass);
    assert_eq!(check_web_reply(&input(false, 1, unanswered.clone())).unwrap(), CheckResult::Pass);

    let mut closing = unanswered;
    closing.push(json!({"type": TASK_CLOSE_REQUESTED, "id": "close-1"}));
    assert_eq!(check_web_reply(&input(false, 0, closing.clone())).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_status(&input(false, 0, closing)), CheckResult::Pass);
  }

  /// A colleague-started turn passes every web write-side check, which the same turn fails as the operator's.
  #[test]
  fn a_colleague_started_turn_passes_every_web_write_check() {
    let turn = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    let status = json!({"goal": "g", "waiting": [{"text": "an answer", "context": "c"}]});
    let mut colleague = input(true, 0, turn.clone());
    colleague.status = status.clone();
    assert_eq!(check_web_reply(&colleague).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_status(&colleague), CheckResult::Pass);
    assert_eq!(check_status_reread(&colleague), CheckResult::Pass);

    let mut operator = input(false, 0, turn);
    operator.status = status;
    assert!(matches!(check_web_reply(&operator).unwrap(), CheckResult::Block(_)));
    assert!(matches!(check_web_status(&operator), CheckResult::Block(_)));
    assert!(matches!(check_status_reread(&operator), CheckResult::Block(_)));
  }

  /// A thread summon alone is no unanswered web message, so it never blocks the web-reply check.
  #[test]
  fn a_thread_summon_alone_never_blocks_the_web_reply_check() {
    let summon = json!({
      "type": "agent_message",
      "id": "s1",
      "content": "a question from the thread",
      "slack": {"channel_id": "C1", "thread_ts": "1.0", "mention_ts": "1.1"},
      "received_at": "2026-01-01T00:00:01Z",
    });
    assert_eq!(check_web_reply(&input(false, 0, vec![summon])).unwrap(), CheckResult::Pass);
  }

  /// A parent-approved session's legacy Waiting on you list is exempt: an input turn that holds only that list passes
  /// with or without a set. Its status set refuses a non-empty Waiting on you list, so the reminder could not ask for
  /// one.
  #[test]
  fn a_parent_approved_session_passes_status_reread_for_an_input_turn() {
    let legacy = json!({"goal": "g", "waiting": [{"text": "legacy item", "context": "c"}]});
    let received = vec![user("u1", "q", "2026-01-01T00:00:01Z")];

    let no_set = TurnInput { parent_approved: true, status: legacy.clone(), ..input(false, 0, received.clone()) };
    assert_eq!(check_status_reread(&no_set), CheckResult::Pass);

    let mut cleared = received.clone();
    cleared.push(json!({"type": WEB_STATUS, "id": "s1", "web_status_changed": ["waiting"]}));
    let empty_list =
      TurnInput { parent_approved: true, status: json!({"goal": "g", "waiting": []}), ..input(false, 0, cleared) };
    assert_eq!(check_status_reread(&empty_list), CheckResult::Pass);
  }

  /// A user-approved session blocks `status-reread` on the same input turn with the reminder, then notices the item it
  /// left as stored.
  #[test]
  fn a_user_approved_session_blocks_status_reread_then_notices_the_left_items() {
    let legacy = json!({"goal": "g", "waiting": [{"text": "legacy item", "context": "c"}]});
    let received = vec![user("u1", "q", "2026-01-01T00:00:01Z")];

    let blocked = TurnInput { status: legacy.clone(), ..input(false, 0, received.clone()) };
    let CheckResult::Block(reason) = check_status_reread(&blocked) else { panic!("the reminder blocks") };
    assert!(reason.starts_with("Session status reminder:") && reason.contains("holds 1 item"), "{reason}");

    let mut second = received;
    second.push(block_record(STATUS_REREAD_ENTRY));
    let noticed = TurnInput { status: legacy, ..input(false, 0, second) };
    let CheckResult::Notice(notice) = check_status_reread(&noticed) else { panic!("the second miss notices") };
    assert!(notice.contains("legacy item"), "{notice}");
  }

  /// A set that names `changed` in its changed fields.
  fn status_set(changed: &[&str]) -> Value {
    json!({"type": WEB_STATUS, "id": "s1", "web_status_changed": changed})
  }

  /// The stored status of a session whose Waiting on you list holds `waiting` and whose Remaining list holds
  /// `remaining`.
  fn stored_lists(waiting: &[&str], remaining: &[&str]) -> Value {
    let items: Vec<Value> = waiting.iter().map(|text| json!({"text": text, "context": "c"})).collect();
    json!({"goal": "g", "waiting": items, "remaining": remaining})
  }

  /// The turn of a user message received at 00:00:01 followed by `after`.
  fn input_turn(after: Vec<Value>) -> Vec<Value> {
    let mut turn = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    turn.extend(after);
    turn
  }

  #[test]
  fn an_input_turn_with_remaining_needs_a_set_that_names_remaining_after_the_input() {
    let status = stored_lists(&[], &["Land the list"]);
    let turn = |after: Vec<Value>| TurnInput { status: status.clone(), ..input(false, 0, input_turn(after)) };

    // A confirm alone blocks with the reason naming Remaining; the second miss notices the list left.
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let CheckResult::Block(reason) = check_status_reread(&turn(vec![confirm.clone()])) else {
      panic!("the confirm blocks")
    };
    assert!(reason.contains("Remaining holds 1 item") && reason.contains("the Remaining list"), "{reason}");
    let mut second = input_turn(vec![confirm]);
    second.push(block_record(STATUS_REREAD_ENTRY));
    let noticed = TurnInput { status: status.clone(), ..input(false, 0, second) };
    let CheckResult::Notice(notice) = check_status_reread(&noticed) else { panic!("the second miss notices") };
    assert!(notice.contains("The Remaining items left as stored: Land the list."), "{notice}");

    // A set that names Remaining after the input passes.
    assert_eq!(check_status_reread(&turn(vec![status_set(&["remaining"])])), CheckResult::Pass);

    // A set that names only Now blocks.
    assert!(matches!(check_status_reread(&turn(vec![status_set(&["now"])])), CheckResult::Block(_)));

    // An exit-3 set prints its cold read and stores nothing: only the record of that read follows the input.
    let record = json!({"type": COLD_READ_CHECK, "id": "k1", "name": "remaining"});
    assert!(matches!(check_status_reread(&turn(vec![record.clone()])), CheckResult::Block(_)));
    assert_eq!(check_status_reread(&turn(vec![record, status_set(&["remaining"])])), CheckResult::Pass);
  }

  #[test]
  fn a_parent_approved_session_exempts_only_its_legacy_waiting_list() {
    let status = stored_lists(&["legacy item"], &["Land the list"]);
    let turn = |after: Vec<Value>| TurnInput {
      parent_approved: true,
      status: status.clone(),
      ..input(false, 0, input_turn(after))
    };

    assert_eq!(check_status_reread(&turn(vec![status_set(&["remaining"])])), CheckResult::Pass);
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let CheckResult::Block(reason) = check_status_reread(&turn(vec![confirm])) else { panic!("the confirm blocks") };
    assert!(reason.contains("Remaining holds 1 item"), "{reason}");
    assert!(!reason.contains("Waiting on you"), "{reason}");
  }

  #[test]
  fn an_input_turn_with_both_lists_names_each_list_its_set_leaves_out() {
    let status = stored_lists(&["Choose the title"], &["Land the list"]);
    let turn = |after: Vec<Value>| TurnInput { status: status.clone(), ..input(false, 0, input_turn(after)) };

    let CheckResult::Block(reason) = check_status_reread(&turn(vec![status_set(&["waiting"])])) else {
      panic!("the set without Remaining blocks")
    };
    assert!(reason.contains("Remaining holds 1 item") && !reason.contains("Waiting on you holds"), "{reason}");

    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let CheckResult::Block(reason) = check_status_reread(&turn(vec![confirm])) else { panic!("the confirm blocks") };
    assert!(reason.contains("Waiting on you holds 1 item and Remaining holds 1 item"), "{reason}");

    assert_eq!(check_status_reread(&turn(vec![status_set(&["waiting", "remaining"])])), CheckResult::Pass);
  }

  /// An agent message that the turn received at 00:00:01.
  fn agent_message_input() -> Value {
    json!({"type": AGENT_MESSAGE, "id": "m1", "content": "a question", "received_at": "2026-01-01T00:00:01Z"})
  }

  /// An external-thread user input admitted as an agent message from the destination session.
  fn external_thread_input() -> Value {
    json!({
      "type": AGENT_MESSAGE,
      "id": "m2",
      "content": "a thread question",
      "received_at": "2026-01-01T00:00:01Z",
      "from_session": "web-session",
      "from_session_name": "Slack",
      "slack": {"channel_id": "C1", "thread_ts": "1.0", "mention_ts": "1.1"},
    })
  }

  /// A scheduled trigger that the turn received at 00:00:01.
  fn scheduled_trigger_input() -> Value {
    json!({"type": SCHEDULED_TRIGGER, "id": "t1", "content": "Nightly sweep", "received_at": "2026-01-01T00:00:01Z"})
  }

  #[test]
  fn an_agent_message_only_turn_passes_a_confirm_with_remaining() {
    let status = stored_lists(&[], &["Land the list"]);
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let turn = TurnInput { status, ..input(false, 0, vec![agent_message_input(), confirm.clone()]) };
    assert_eq!(check_status_reread(&turn), CheckResult::Pass);

    // The Waiting on you requirement still applies to an agent-message turn.
    let waiting = stored_lists(&["Choose the title"], &["Land the list"]);
    let turn = TurnInput { status: waiting, ..input(false, 0, vec![agent_message_input(), confirm]) };
    let CheckResult::Block(reason) = check_status_reread(&turn) else { panic!("the confirm blocks") };
    assert!(reason.contains("Waiting on you holds 1 item") && !reason.contains("Remaining"), "{reason}");
  }

  #[test]
  fn an_external_thread_user_input_needs_remaining() {
    let status = stored_lists(&[], &["Land the list"]);
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let turn = TurnInput { status, ..input(false, 0, vec![external_thread_input(), confirm]) };
    let CheckResult::Block(reason) = check_status_reread(&turn) else { panic!("the confirm blocks") };
    assert!(reason.contains("Remaining holds 1 item"), "{reason}");
  }

  #[test]
  fn a_scheduled_trigger_only_turn_passes_a_confirm_with_remaining() {
    let status = stored_lists(&[], &["Land the list"]);
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let turn = TurnInput { status, ..input(false, 0, vec![scheduled_trigger_input(), confirm]) };
    assert_eq!(check_status_reread(&turn), CheckResult::Pass);
  }

  #[test]
  fn a_turn_with_an_agent_message_and_a_child_report_needs_remaining() {
    let status = stored_lists(&[], &["Land the list"]);
    let confirm = json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"});
    let turn = TurnInput {
      status,
      ..input(false, 0, vec![agent_message_input(), child_report("child-1", "2026-01-01T00:00:01Z"), confirm])
    };
    let CheckResult::Block(reason) = check_status_reread(&turn) else { panic!("the confirm blocks") };
    assert!(reason.contains("Remaining holds 1 item"), "{reason}");
  }

  #[test]
  fn a_turn_without_an_input_passes_with_a_remaining_list() {
    let status = stored_lists(&[], &["Land the list"]);
    let no_input = TurnInput { status, ..input(false, 0, vec![json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"})]) };
    assert_eq!(check_status_reread(&no_input), CheckResult::Pass);
  }

  /// The report that task `child_id` delivers to its parent, received at `received_at`.
  fn child_report(child_id: &str, received_at: &str) -> Value {
    json!({
      "type": CHILD_REPORT,
      "id": "report-1",
      "child_session_id": child_id,
      "outcome": "completed",
      "summary": "Run completed",
      "received_at": received_at,
    })
  }

  #[test]
  fn a_status_reminder_quotes_a_child_report_as_the_prompt_renders_it() {
    let report = child_report("child-1", "2026-01-01T00:00:01Z");
    for provenance in [None, Some("clone of parent-1")] {
      let turn = TurnInput {
        status: json!({"waiting": [{"text": "Review the draft"}]}),
        last_input_provenance: provenance.map(str::to_string),
        turn: vec![report.clone()],
        ..input(false, 0, Vec::new())
      };
      let opening = reply::message_opening(&child_report_text(&report, provenance));
      let CheckResult::Block(reason) = check_status_reread(&turn) else { panic!("the reminder blocks") };
      assert!(reason.contains(&format!("begins \"{opening}\"")), "{reason}");
      assert!(!reason.contains("begins \"\""), "{reason}");
    }
  }

  #[test]
  fn a_status_reminder_quotes_a_user_message_and_a_scheduled_trigger_by_their_content() {
    let trigger = json!({
      "type": SCHEDULED_TRIGGER, "id": "t1", "content": "Nightly sweep", "received_at": "2026-01-01T00:00:01Z",
    });
    let cases = [(user("u1", "the question", "2026-01-01T00:00:01Z"), "the question"), (trigger, "Nightly sweep")];
    for (event, content) in cases {
      let turn = TurnInput {
        status: json!({"waiting": [{"text": "Review the draft"}]}),
        turn: vec![event],
        ..input(false, 0, Vec::new())
      };
      let CheckResult::Block(reason) = check_status_reread(&turn) else { panic!("the reminder blocks") };
      assert!(reason.contains(&format!("begins \"{content}\"")), "{reason}");
    }
  }

  #[tokio::test]
  async fn a_notice_attaches_to_an_unanswered_message_or_reports_that_none_is_there() {
    let home = Home::new();
    let session_id = home.manager("session", None).await;
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z")];

    assert!(show_notice(&home.features, &session_id, &turn, "no reply").await.unwrap());
    assert_eq!(home.events(&session_id).iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).count(), 1);

    // A turn without an unanswered user message attaches nothing, so the runner records the notice itself.
    assert!(!show_notice(&home.features, &session_id, &[], "no reply").await.unwrap());
    assert_eq!(home.events(&session_id).iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).count(), 1);
  }
}
