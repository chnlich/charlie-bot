//! The web session's turn-end checks: the web reply to each user message the turn received (`web-reply`), and the
//! status set or confirm that ends the turn (`web-status`).
//!
//! Both are entries of the turn-end hook list. The `charliebot hook` command answers each one from the shared input
//! (`TurnInput`), so the checks read only the turn's events. A first miss blocks the turn with the reason. A second
//! miss passes: `web-reply` then shows the user the no-reply notice, and `web-status` passes quietly. A continuation
//! that failed is a second miss too: `web-status` then shows its notice, so the failed turn never passes. A turn with a
//! close request, and a turn that exits with an error other than a failed continuation, pass both checks.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::event_types::{
  INPUT_EVENT_ID, OWNER_RUN_ID, TASK_CLOSE_REQUESTED, TURN_HOOKS, WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS,
  WEB_STATUS_CONFIRMED,
};
use charliebot_registry::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::reply_order;
use charliebot_store::{ChatEvent, Store};
use charliebot_tasks::turn_hooks::{CheckResult, TurnInput, received_receipts};
use serde_json::{Value, json};

use crate::reply;

/// The entry that checks each user message of the turn has a web reply.
pub const WEB_REPLY_ENTRY: &str = "web-reply";
/// The entry that checks the turn ran a status set or confirm.
pub const WEB_STATUS_ENTRY: &str = "web-status";

/// The status instruction that ends a status reminder.
const STATUS_SENTENCES: &str = concat!(
  "Run `charliebot web status set` when Goal, Remaining, Now or Waiting on you differs from the state that turn ",
  "read back. ",
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

/// Whether the web checks apply to this turn: a web session whose turn exited cleanly or as a failed continuation, and
/// left no close request.
fn applies(input: &TurnInput) -> bool {
  input.kind.as_deref() == Some("web")
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
  if !applies(input) {
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
  if !applies(input) {
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

/// Shows the no-reply notice `message` for each user message of the turn that no web reply answers, then marks the
/// session unread. A message that already has its notice gets none, so a retried delivery adds nothing. A turn with no
/// such message attaches nothing and resolves to `false`: the turn-end runner then posts the notice itself.
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
    if reply::noticed(loaded.as_slice(), &user_id) {
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
  use super::*;
  use crate::test_support::Home;

  fn input(kind: Option<&str>, exit_code: i64, turn: Vec<Value>) -> TurnInput {
    TurnInput {
      session_id: "web-session".to_string(),
      kind: kind.map(str::to_string),
      exit_code,
      continuation_failed: false,
      status: Value::Null,
      turn,
    }
  }

  /// The input of a turn that is the failed continuation of a block.
  fn failed_continuation(turn: Vec<Value>) -> TurnInput {
    TurnInput { continuation_failed: true, ..input(Some("web"), 1, turn) }
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
    let first = check_web_reply(&input(Some("web"), 0, turn.clone())).unwrap();
    assert!(matches!(first, CheckResult::Block(_)), "{first:?}");
    assert!(text_of(&first).contains("The unanswered message begins: \"the question\"."));

    let mut second_turn = turn;
    second_turn.push(block_record(WEB_REPLY_ENTRY));
    let second = check_web_reply(&input(Some("web"), 0, second_turn)).unwrap();
    assert_eq!(second, CheckResult::Notice(reply::NOTICE_TEXT.to_string()));
  }

  #[test]
  fn a_web_reply_after_the_receipt_answers_the_message() {
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z"), reply_at("r1", "2026-01-01T00:00:05Z")];
    assert_eq!(check_web_reply(&input(Some("web"), 0, turn)).unwrap(), CheckResult::Pass);
  }

  #[test]
  fn a_reply_at_the_receipt_instant_does_not_answer_it() {
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z"), reply_at("r1", "2026-01-01T00:00:01Z")];
    assert!(matches!(check_web_reply(&input(Some("web"), 0, turn)).unwrap(), CheckResult::Block(_)));
  }

  #[test]
  fn a_status_set_or_confirm_passes_the_status_check_and_a_missing_one_blocks_once() {
    let bare = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    assert!(matches!(check_web_status(&input(Some("web"), 0, bare.clone())), CheckResult::Block(_)));

    let mut with_status = bare.clone();
    with_status.push(json!({"type": WEB_STATUS, "id": "s1"}));
    assert_eq!(check_web_status(&input(Some("web"), 0, with_status)), CheckResult::Pass);

    let mut with_confirm = bare.clone();
    with_confirm.push(json!({"type": WEB_STATUS_CONFIRMED, "id": "c1"}));
    assert_eq!(check_web_status(&input(Some("web"), 0, with_confirm)), CheckResult::Pass);

    let mut second = bare;
    second.push(block_record(WEB_STATUS_ENTRY));
    assert_eq!(check_web_status(&input(Some("web"), 0, second)), CheckResult::Pass);
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
  fn the_checks_pass_outside_a_clean_web_turn_and_with_a_close_request() {
    let unanswered = vec![user("u1", "q", "2026-01-01T00:00:01Z")];
    assert_eq!(check_web_reply(&input(Some("slack"), 0, unanswered.clone())).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_reply(&input(Some("web"), 1, unanswered.clone())).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_status(&input(None, 0, unanswered.clone())), CheckResult::Pass);

    let mut closing = unanswered;
    closing.push(json!({"type": TASK_CLOSE_REQUESTED, "id": "close-1"}));
    assert_eq!(check_web_reply(&input(Some("web"), 0, closing.clone())).unwrap(), CheckResult::Pass);
    assert_eq!(check_web_status(&input(Some("web"), 0, closing)), CheckResult::Pass);
  }

  #[tokio::test]
  async fn a_notice_attaches_to_an_unanswered_message_or_reports_that_none_is_there() {
    let home = Home::new();
    let session_id = home.manager("session", None).await;
    let turn = vec![user("u1", "the question", "2026-01-01T00:00:01Z")];

    assert!(show_notice(&home.features, &session_id, &turn, "no reply").await.unwrap());
    assert_eq!(home.events(&session_id).iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).count(), 1);

    // A turn without an unanswered user message attaches nothing, so the runner posts the notice itself.
    assert!(!show_notice(&home.features, &session_id, &[], "no reply").await.unwrap());
    assert_eq!(home.events(&session_id).iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).count(), 1);
  }
}
