//! The round-end audit of a web session: the status reminder, the reply nudge and
//! the reply notice.
//!
//! The status reminder covers every finished turn of an open task, background turns included. A turn that ran
//! `charliebot web status set` or `charliebot web status confirm` needs no reminder, and neither does a turn that a
//! status reminder woke. The reply nudge covers each user message of a turn that no web reply answers. When one
//! turn misses both, one combined reminder covers both and wakes one turn. A turn that leaves a close request pending
//! gets neither a reminder nor a nudge, since a run the server starts would void that request.
//!
//! Each step leaves a log marker (the status reminder, the nudge, the notice), so each happens at most once per turn
//! or user message.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::event_types::{
  AGENT_MESSAGE, EventFields, INPUT_EVENT_ID, MASTER_DONE, NUDGE_OF, WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS,
  WEB_STATUS_CONFIRMED, WEB_STATUS_REMINDER,
};
use charliebot_registry::FeatureContext;
use charliebot_store::MessageProvenance;
use charliebot_store::compact::CompactValue;
use charliebot_store::reply_order::{self, Receipt};
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use serde_json::{Value, json};

use crate::reply;

/// The status instruction that ends a status reminder.
const STATUS_SENTENCES: &str = concat!(
  "Run `charliebot web status set` when Goal, Now or Waiting on you differs from the state that turn read back. ",
  "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back."
);

/// The reminder a turn gets when it recorded no status set or confirm.
fn status_reminder_text() -> String {
  format!("Session status reminder: your last turn ended without a status set or confirm. {STATUS_SENTENCES}")
}

/// The one reminder that a turn gets when it missed both its web reply and its status, for the unanswered user message
/// `message`.
fn combined_reminder_text(message: &str) -> String {
  format!(
    "Web reply and session status reminder: your last turn woke on a user message and ended without a web reply and \
     without a status set or confirm. {} {STATUS_SENTENCES}",
    reply::answer_request(message)
  )
}
/// The sender name a status reminder carries in the chat log.
pub const STATUS_REMINDER_SENDER_NAME: &str = "Session status";

/// Whether `event` is a reminder that a turn ended without a status set or confirm.
pub fn is_status_reminder(event: &impl EventFields) -> bool {
  event.field(WEB_STATUS_REMINDER).is_some()
}

/// Audit one finished turn of a web session for its status and its web reply.
///
/// The status reminder needs a session that is not archived, an open task, no pending close request, a turn without a
/// status event and a turn that no status reminder woke. The reply steps are [`reply_steps`]'s. A turn that needs both
/// gets one combined reminder and one wake. A turn that leaves a close request pending gets neither a reminder nor a
/// nudge: a run that the server starts would void the request.
pub async fn audit_turn(
  features: &Arc<FeatureContext>,
  session_id: &str,
  done: &Value,
  archived: bool,
) -> anyhow::Result<()> {
  let input_ids = features.store.master_done_input_event_ids(done);
  let loaded = reply::load_events(features, session_id).await?;
  let receipts = reply_order::turn_receipts(&features.store, session_id, done, &loaded)?;
  let pending_close = close_request_pending(features, session_id).await?;
  let status_needed =
    !archived && status_reminder_needed(features, session_id, done, &input_ids, &loaded, pending_close).await?;
  let steps = reply_steps(&loaded, &receipts)?;
  let turn_id = done.get("id").and_then(Value::as_str);
  // A turn that needs a status reminder posts it first. Its first nudge rides the reminder, so one wake covers both.
  let mut nudge_on_reminder = None;
  if status_needed {
    nudge_on_reminder = steps.iter().find(|step| step.action == ReplyAction::Nudge).cloned();
    match &nudge_on_reminder {
      Some(step) => {
        let text = combined_reminder_text(&step.user_text);
        post_status_reminder(features, session_id, &text, turn_id, Some(&step.user_event_id)).await?;
      }
      None => post_status_reminder(features, session_id, &status_reminder_text(), turn_id, None).await?,
    }
  }
  for step in steps {
    match step.action {
      ReplyAction::Nudge
        if nudge_on_reminder.as_ref().is_some_and(|first| first.user_event_id == step.user_event_id) => {}
      ReplyAction::Nudge if !pending_close => nudge(features, session_id, &step.user_event_id, &step.user_text).await?,
      ReplyAction::Nudge => {}
      ReplyAction::Notice => notice(features, session_id, &step.user_event_id).await?,
    }
  }
  Ok(())
}

/// Audit one finished turn's web reply alone: a nudge for each unanswered user message, then a notice for each nudge
/// that went unanswered. The production hook audits the turn through [`audit_turn`]; the tests
/// drive this reply-only step directly.
#[cfg(test)]
pub async fn audit_reply(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<()> {
  let loaded = reply::load_events(features, session_id).await?;
  let receipts = reply_order::turn_receipts(&features.store, session_id, done, &loaded)?;
  for step in reply_steps(&loaded, &receipts)? {
    match step.action {
      ReplyAction::Nudge => nudge(features, session_id, &step.user_event_id, &step.user_text).await?,
      ReplyAction::Notice => notice(features, session_id, &step.user_event_id).await?,
    }
  }
  Ok(())
}

/// The reply step that one user message of a turn needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyAction {
  /// The nudge that wakes a new turn for an unanswered user message.
  Nudge,
  /// The notice that marks the session unread for a user message that a nudge left unanswered.
  Notice,
}

/// A reply step and the user message it answers.
#[derive(Clone)]
struct ReplyStep {
  action: ReplyAction,
  user_event_id: String,
  /// The unanswered message's text, which the reminder names by its opening.
  user_text: String,
}

/// The reply steps of one turn, in receipt order. Each received input, a user message or a nudge, is judged on the user
/// message it answers: a user message that no web reply later than its receipt answers gets a nudge, unless a nudge was
/// already posted for it; a nudge whose user message no web reply later than the nudge's receipt answers gets a notice,
/// unless one was posted. A user message named by several receipts gets one step.
fn reply_steps(
  loaded: &[charliebot_store::SharedChatEvent],
  receipts: &[Receipt],
) -> Result<Vec<ReplyStep>, charliebot_store::StoreError> {
  let mut steps: Vec<ReplyStep> = Vec::new();
  for receipt in receipts {
    let event = loaded
      .iter()
      .map(charliebot_store::SharedChatEvent::as_ref)
      .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(receipt.input_event_id.as_str()))
      .ok_or_else(|| {
        charliebot_store::StoreError::new(format!(
          "run {} claims the input {} that the session log does not hold",
          receipt.run_id, receipt.input_event_id
        ))
      })?;
    let Some(user_event_id) = reply::answered_user_id(event) else {
      continue;
    };
    if reply::replied(receipt, loaded)? || steps.iter().any(|step| step.user_event_id == user_event_id) {
      continue;
    }
    let user_text = loaded
      .iter()
      .map(charliebot_store::SharedChatEvent::as_ref)
      .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(user_event_id.as_str()))
      .and_then(|event| event.get("content").and_then(CompactValue::as_str))
      .unwrap_or_default()
      .to_string();
    let action = if reply::nudge_of(event).is_some() {
      (!reply::noticed(loaded, &user_event_id)).then_some(ReplyAction::Notice)
    } else {
      (!reply::nudged(loaded, &user_event_id)).then_some(ReplyAction::Nudge)
    };
    if let Some(action) = action {
      steps.push(ReplyStep { action, user_event_id, user_text });
    }
  }
  Ok(steps)
}

/// The events one turn recorded, from the previous round end to `done`.
///
/// The done event is already in the log when the hook runs. A caller that passes an unpersisted done has only this
/// log to judge, so the whole log stands for the turn.
fn turn_events<'a>(
  loaded: &'a [charliebot_store::SharedChatEvent],
  done: &Value,
) -> &'a [charliebot_store::SharedChatEvent] {
  let Some(done_id) = done.get("id").and_then(Value::as_str) else {
    return loaded;
  };
  for (index, event) in loaded.iter().enumerate() {
    if event.get("id").and_then(CompactValue::as_str) != Some(done_id) {
      continue;
    }
    let mut start = 0;
    for earlier in (0..index).rev() {
      if loaded[earlier].get("type").and_then(CompactValue::as_str) == Some(MASTER_DONE) {
        start = earlier + 1;
        break;
      }
    }
    return &loaded[start..index];
  }
  loaded
}

/// Whether one turn recorded a status set or a status confirm.
fn status_recorded(turn_events: &[charliebot_store::SharedChatEvent]) -> bool {
  turn_events.iter().any(|event| {
    matches!(event.get("type").and_then(CompactValue::as_str), Some(WEB_STATUS) | Some(WEB_STATUS_CONFIRMED))
  })
}

/// Whether a status reminder for the turn `turn_id` is already in the log.
fn reminder_already_posted(loaded: &[charliebot_store::SharedChatEvent], turn_id: Option<&str>) -> bool {
  let Some(turn_id) = turn_id else { return false };
  loaded.iter().any(|event| event.get(WEB_STATUS_REMINDER).and_then(CompactValue::as_str) == Some(turn_id))
}

/// Whether one of the turn's inputs is a status reminder.
fn woken_by_status_reminder(loaded: &[charliebot_store::SharedChatEvent], input_ids: &[String]) -> bool {
  let wanted: HashSet<&str> = input_ids.iter().map(String::as_str).collect();
  loaded.iter().any(|event| {
    event.get("id").and_then(CompactValue::as_str).is_some_and(|id| wanted.contains(id))
      && is_status_reminder(event.as_ref())
  })
}

/// Whether the session's newest run left a close request pending. A request that a
/// later run owns was voided by the wake that started that run, so it does not count.
async fn close_request_pending(features: &Arc<FeatureContext>, session_id: &str) -> anyhow::Result<bool> {
  let store = Arc::clone(&features.store);
  let session = session_id.to_string();
  tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
    let facts = store.facts_of(&session)?;
    let newest = store.newest_run_record_sync(&session)?;
    let newest_run_id = newest.as_ref().map(|run| run.id.as_str());
    let closed: HashSet<Option<&str>> =
      facts.close_events.iter().map(|event| event.get("request_id").and_then(CompactValue::as_str)).collect();
    Ok(facts.close_requests.iter().any(|request| {
      let request_id = request.get("request_id").and_then(CompactValue::as_str);
      request.get("owner_run_id").and_then(CompactValue::as_str) == newest_run_id && !closed.contains(&request_id)
    }))
  })
  .await
  .context("the task-tree read stopped")?
}

/// Whether one turn needs one status reminder.
async fn status_reminder_needed(
  features: &Arc<FeatureContext>,
  session_id: &str,
  done: &Value,
  input_ids: &[String],
  loaded: &[charliebot_store::SharedChatEvent],
  pending_close: bool,
) -> anyhow::Result<bool> {
  let store = Arc::clone(&features.store);
  let session = session_id.to_string();
  let state = tokio::task::spawn_blocking(move || -> anyhow::Result<_> { Ok(store.task_state(&session)?.to_string()) })
    .await
    .context("the task-tree read stopped")??;
  if state != "open" {
    return Ok(false);
  }
  if pending_close {
    return Ok(false);
  }
  if status_recorded(turn_events(loaded, done)) {
    return Ok(false);
  }
  if woken_by_status_reminder(loaded, input_ids) {
    return Ok(false);
  }
  Ok(!reminder_already_posted(loaded, done.get("id").and_then(Value::as_str)))
}

/// Persist one status reminder for `turn_id` and wake a new turn with it.
///
/// `nudge_of` is the user message a combined reminder also answers without a web reply.
async fn post_status_reminder(
  features: &Arc<FeatureContext>,
  session_id: &str,
  text: &str,
  turn_id: Option<&str>,
  nudge_of: Option<&str>,
) -> anyhow::Result<()> {
  let provenance = MessageProvenance {
    from_session: session_id.to_string(),
    from_session_name: STATUS_REMINDER_SENDER_NAME.to_string(),
  };
  let mut event = features.store.build_agent_message_event(session_id, text, provenance);
  event[WEB_STATUS_REMINDER] = turn_id.map_or(json!(true), Value::from).into();
  if let Some(user_event_id) = nudge_of {
    event[WEB_REPLY] = json!({(NUDGE_OF): user_event_id}).into();
  }
  let persisted = features.store.persist_and_broadcast(session_id, event.to_value()).await?;
  let reminder_id = persisted["id"].as_str().context("the persisted reminder carries its id")?.to_string();
  spawn_wake(features, session_id, text, &reminder_id, STATUS_REMINDER_SENDER_NAME, "web-status-reminder");
  tracing::info!(session = %session_id, turn_id, combined = nudge_of.is_some(), "{}", WEB_STATUS_REMINDER);
  Ok(())
}

/// Persist the nudge for `user_event_id`, naming the unanswered message `user_text`, then wake a new turn with it as its
/// own task. A failed wake is logged in that task.
async fn nudge(
  features: &Arc<FeatureContext>,
  session_id: &str,
  user_event_id: &str,
  user_text: &str,
) -> anyhow::Result<()> {
  let text = reply::nudge_text(user_text);
  let provenance =
    MessageProvenance { from_session: session_id.to_string(), from_session_name: reply::NUDGE_SENDER_NAME.to_string() };
  let mut event = features.store.build_agent_message_event(session_id, &text, provenance);
  event[WEB_REPLY] = json!({(NUDGE_OF): user_event_id}).into();
  let persisted = features.store.persist_and_broadcast(session_id, event.to_value()).await?;
  let nudge_id = persisted["id"].as_str().context("the persisted nudge carries its id")?.to_string();

  spawn_wake(features, session_id, &text, &nudge_id, reply::NUDGE_SENDER_NAME, "web-reply-nudge");
  tracing::info!(session = %session_id, user_event_id, nudge_id = %nudge_id, "web_reply_nudge");
  Ok(())
}

/// Wake one turn with `text` as its own logged background task (`charliebot_tasks::master_trigger::trigger_master`). A failed wake is
/// logged in that task.
fn spawn_wake(
  features: &Arc<FeatureContext>,
  session_id: &str,
  text: &str,
  input_id: &str,
  from_session_name: &str,
  task_name: &str,
) {
  let task = format!("{task_name}-{session_id}");
  let wake_features = Arc::clone(features);
  let wake_session = session_id.to_string();
  let wake_input = input_id.to_string();
  let wake_text = text.to_string();
  let wake_from_session_name = from_session_name.to_string();
  tokio::spawn(async move {
    let request = TriggerMasterRequest {
      session_id: &wake_session,
      summary: &wake_text,
      input_id: Some(&wake_input),
      event_type: AGENT_MESSAGE,
      actor: "agent",
      from_session: Some(&wake_session),
      from_session_name: Some(&wake_from_session_name),
      pull_back: true,
    };
    if let Err(error) = trigger_master(&wake_features, request).await {
      tracing::error!(target: "charliebot_web_reply", task = %task, error = %error, "web reply wake failed");
    }
  });
}

/// Persist the no-reply notice for `user_event_id`, then mark the session unread.
async fn notice(features: &Arc<FeatureContext>, session_id: &str, user_event_id: &str) -> anyhow::Result<()> {
  features
    .store
    .persist_and_broadcast(
      session_id,
      json!({
        "type": WEB_REPLY_NOTICE,
        "content": reply::NOTICE_TEXT,
        WEB_REPLY_NOTICE: {INPUT_EVENT_ID: user_event_id},
      }),
    )
    .await?;
  features.store.mark_unread(session_id).await?;
  tracing::info!(session = %session_id, user_event_id, "{}", WEB_REPLY_NOTICE);
  Ok(())
}

#[cfg(test)]
mod tests {
  use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};

  use super::*;
  use crate::test_support::{Home, id_of, master_done, user_event, user_event_with_id};

  /// A start time before every event a test appends.
  const TURN_START: &str = "2026-01-01T00:00:00Z";

  /// The log's parsed events, for the order rule's receipt and reply times.
  fn loaded(home: &Home, session_id: &str) -> Vec<charliebot_store::SharedChatEvent> {
    let store = &home.features.store;
    store
      .load_chat_events_shared_sync(session_id)
      .expect("the chat log reads")
      .as_ref()
      .iter()
      .map(std::sync::Arc::clone)
      .collect()
  }

  #[tokio::test]
  async fn a_user_turn_without_a_reply_gets_one_nudge() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    home.launch_run(&sid, "run-1", &[id_of(&user)], TURN_START);
    let done = master_done("run-1", &[id_of(&user)], 0);

    audit_reply(&home.features, &sid, &done).await.expect("the audit runs");
    audit_reply(&home.features, &sid, &done).await.expect("the audit runs");

    let nudges = home.nudges(&sid);
    assert_eq!(nudges.len(), 1);
    assert_eq!(nudges[0]["content"], json!(reply::nudge_text("the question")));
    assert_eq!(nudges[0]["from_session_name"], json!(reply::NUDGE_SENDER_NAME));
    assert_eq!(nudges[0][WEB_REPLY], json!({"nudge_of": id_of(&user)}));
    let text = nudges[0]["content"].as_str().expect("the nudge carries text");
    assert!(text.contains("The unanswered message begins: \"the question\"."), "{text}");
    assert!(!text.contains("--answers"), "{text}");
    assert!(!text.contains("[User message "), "{text}");
  }

  #[tokio::test]
  async fn a_nudge_turn_without_a_reply_posts_one_notice_and_marks_the_session_unread() {
    let home = Home::status_only();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    home.launch_run(&sid, "run-1", &[id_of(&user)], TURN_START);
    audit_reply(&home.features, &sid, &master_done("run-1", &[id_of(&user)], 0)).await.expect("the audit runs");
    let nudge_id = id_of(&home.nudges(&sid)[0]).to_string();
    home.launch_run(&sid, "run-2", &[nudge_id.as_str()], TURN_START);
    let nudged_turn = master_done("run-2", &[nudge_id.as_str()], 0);

    audit_reply(&home.features, &sid, &nudged_turn).await.expect("the audit runs");
    audit_reply(&home.features, &sid, &nudged_turn).await.expect("the audit runs");

    let notices: Vec<Value> = home.events(&sid).into_iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["content"], json!(reply::NOTICE_TEXT));
    assert_eq!(notices[0][WEB_REPLY_NOTICE], json!({"input_event_id": id_of(&user)}));
    assert!(home.meta(&sid).await.has_unread);
  }

  #[test]
  fn a_reply_in_the_nudge_turn_ends_the_audit_with_no_notice() {
    let user = charliebot_store::ChatEvent::from_value(&json!({"type": "user", "id": "user-1"}));
    let nudge = charliebot_store::ChatEvent::from_value(&json!({
      "type": AGENT_MESSAGE,
      "id": "nudge-1",
      "timestamp": "2026-11-02T08:22:23.103374+00:00",
      WEB_REPLY: {(NUDGE_OF): "user-1"},
    }));
    let reply = charliebot_store::ChatEvent::from_value(&json!({
      "type": WEB_REPLY,
      "timestamp": "2026-11-02T08:23:00Z",
      WEB_REPLY: {"chars": 3},
    }));
    let loaded = vec![std::sync::Arc::new(user), std::sync::Arc::new(nudge), std::sync::Arc::new(reply)];
    let receipt = Receipt {
      input_event_id: "nudge-1".to_string(),
      run_id: "run-1".to_string(),
      at: UtcDateTime::parse("2026-11-02T08:22:23.103374Z").expect("the fixture time parses"),
    };

    assert!(reply_steps(&loaded, &[receipt]).expect("the steps read").is_empty());
  }

  /// A resumed turn's receipts time each message the old server delivered. A reply between the two receipts answers the
  /// earlier message only, so the later one gets the turn's one nudge.
  #[tokio::test]
  async fn a_resumed_turn_nudges_only_the_message_received_after_its_reply() {
    let home = Home::reply_only();
    let sid = "session";
    let first = user_event_with_id("first-message", "the first question");
    let second = user_event_with_id("second-message", "the second question");
    let mut first = first;
    first["timestamp"] = json!("2026-11-02T08:20:54.578131+00:00");
    let mut second = second;
    second["timestamp"] = json!("2026-11-02T08:22:22.926386+00:00");
    let reply = json!({
      "type": WEB_REPLY,
      "content": "the first answer",
      "timestamp": "2026-11-02T08:21:10Z",
      WEB_REPLY: {"chars": 15},
    });
    let delivered = json!({
      "type": "input_delivered",
      "input_event_id": "second-message",
      "run_id": "resumed-run",
      "timestamp": "2026-11-02T08:22:23.103374+00:00",
    });
    seed_events(&home, sid, &[first, reply, second, delivered]);
    home.launch_run(sid, "resumed-run", &["first-message", "second-message"], "2026-11-02T08:20:54.838607Z");
    let events = loaded(&home, sid);
    let receipts = reply_order::turn_receipts(&home.features.store, sid, &master_done("resumed-run", &[], 0), &events)
      .expect("the receipts read");
    assert!(reply::replied(&receipts[0], &events).expect("the first receipt reads"));
    assert!(!reply::replied(&receipts[1], &events).expect("the second receipt reads"));

    audit_reply(&home.features, sid, &master_done("resumed-run", &[], 0)).await.expect("the audit runs");

    let nudged: Vec<Value> = home.nudges(sid).iter().map(|nudge| nudge[WEB_REPLY]["nudge_of"].clone()).collect();
    assert_eq!(nudged, vec![json!("second-message")]);
  }

  #[tokio::test]
  async fn a_turn_that_received_an_input_the_log_lacks_fails_its_audit() {
    let home = Home::reply_only();
    let sid = "session";
    home.launch_run(sid, "run-1", &["missing-message"], TURN_START);

    let error = audit_reply(&home.features, sid, &master_done("run-1", &["missing-message"], 0))
      .await
      .expect_err("the audit refuses a receipt that names no event of the log");

    assert!(format!("{error:#}").contains("missing-message"), "{error:#}");
    assert!(home.nudges(sid).is_empty());
  }

  #[tokio::test]
  async fn a_turn_with_two_nudges_posts_a_notice_for_each_unanswered_message() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = home.append(&sid, user_event_with_id("11111111-a", "first question")).await;
    let second = home.append(&sid, user_event_with_id("22222222-b", "second question")).await;
    let ids = [id_of(&first), id_of(&second)];
    home.launch_run(&sid, "run-1", &ids, TURN_START);
    audit_reply(&home.features, &sid, &master_done("run-1", &ids, 0)).await.expect("the audit runs");
    let nudge_ids: Vec<String> = home.nudges(&sid).iter().map(|nudge| id_of(nudge).to_string()).collect();
    assert_eq!(nudge_ids.len(), 2);

    home.launch_run(&sid, "run-2", &[nudge_ids[0].as_str(), nudge_ids[1].as_str()], TURN_START);
    audit_reply(&home.features, &sid, &master_done("run-2", &[nudge_ids[0].as_str(), nudge_ids[1].as_str()], 0))
      .await
      .expect("the audit runs");

    let notices: Vec<Value> = home.events(&sid).into_iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).collect();
    let noticed: Vec<Value> = notices.iter().map(|notice| notice[WEB_REPLY_NOTICE][INPUT_EVENT_ID].clone()).collect();
    assert_eq!(noticed, vec![json!("11111111-a"), json!("22222222-b")]);
  }

  #[tokio::test]
  async fn a_close_request_that_a_later_run_voided_gates_nothing() {
    // The newest run owns the live request. The request of an older run was voided by the wake that started it, so it
    // must not hold the session's audit: the status reminder and the nudge still apply.
    let home = Home::new();
    let stale = home.manager("stale", None).await;
    let control = home.manager("control", None).await;
    home.launch_run(&stale, "run-1", &[], TURN_START);
    home
      .append(
        &stale,
        json!({
          "type": charliebot_core::event_types::TASK_CLOSE_REQUESTED,
          "request_id": "close-1",
          "owner_run_id": "run-0",
          "summary": "page delivered",
          "result_refs": [],
          "run_ids": [],
        }),
      )
      .await;
    let mut turns = Vec::new();
    for session in [&stale, &control] {
      let user = home.append(session, user_event("the question")).await;
      home.launch_run(session, "run-1", &[id_of(&user)], TURN_START);
      turns.push((session.clone(), master_done("run-1", &[id_of(&user)], 0)));
    }

    for (session, done) in &turns {
      audit_turn(&home.features, session, done, false).await.expect("the audit runs");
    }

    assert_eq!(home.nudges(&stale).len(), 1, "a voided close gates nothing");
    assert_eq!(home.nudges(&control).len(), 1, "without a close request the turn is nudged");
  }

  /// A seeded fixture run record becomes the turn's run record on disk.
  fn launch_fixture_run(home: &Home, run: &Value) {
    let ids: Vec<&str> = run["input_event_ids"]
      .as_array()
      .expect("the fixture run lists its input ids")
      .iter()
      .map(|id| id.as_str().expect("a fixture input id is a string"))
      .collect();
    home.launch_run(
      run["session_id"].as_str().expect("the fixture run names its session"),
      run["id"].as_str().expect("the fixture run carries its id"),
      &ids,
      run["started_at"].as_str().expect("the fixture run carries its start time"),
    );
  }

  /// The fixture master_done that names its run.
  fn fixture_done(events: &[Value]) -> Value {
    events
      .iter()
      .find(|event| event["type"] == MASTER_DONE && event.get("run_id").is_some())
      .cloned()
      .expect("the fixture holds the turn's master_done")
  }

  fn replay_fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/replay_resumed_replies.json"))
      .expect("the replay fixture reads")
  }

  /// R1: a release restart resumed the turn after both messages arrived. The resumed turn's master_done names its run
  /// and carries no input ids; the run's receipts time the start input at the run start and the delivered input at its
  /// delivery row, and the one reply after both answers them both.
  #[tokio::test]
  async fn replay_r1_marks_both_messages_of_a_resumed_turn_answered() {
    let fixture = replay_fixture();
    let home = Home::reply_only();
    let r1 = &fixture["r1"];
    let sid = r1["session_id"].as_str().expect("the fixture names its session");
    let events = r1["events"].as_array().expect("the fixture lists its events");
    seed_events(&home, sid, events);
    launch_fixture_run(&home, &r1["run"]);

    let loaded = loaded(&home, sid);
    let done = fixture_done(events);
    let receipts =
      reply_order::turn_receipts(&home.features.store, sid, &done, &loaded).expect("the resumed turn's receipts read");
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].at, UtcDateTime::parse("2026-11-02T08:20:54.838607Z").expect("the fixture time parses"));
    assert_eq!(receipts[1].at, UtcDateTime::parse("2026-11-02T08:22:23.103374Z").expect("the fixture time parses"));
    for receipt in &receipts {
      assert!(reply::replied(receipt, &loaded).expect("the reply reads"));
    }

    audit_reply(&home.features, sid, &done).await.expect("the audit runs");

    assert!(home.nudges(sid).is_empty());
  }

  /// R2: two runs over one session. The stopped run's message and the current turn's message both precede the reply,
  /// so both count as answered and the current turn's audit posts no nudge.
  #[tokio::test]
  async fn replay_r2_answers_the_message_of_a_stopped_run_and_the_current_turns_message() {
    let fixture = replay_fixture();
    let home = Home::reply_only();
    let r2 = &fixture["r2"];
    let sid = r2["session_id"].as_str().expect("the fixture names its session");
    let events = r2["events"].as_array().expect("the fixture lists its events");
    seed_events(&home, sid, events);
    let runs = r2["runs"].as_array().expect("the fixture lists its runs");
    for run in runs {
      launch_fixture_run(&home, run);
    }

    let loaded = loaded(&home, sid);
    let mut answered = Vec::new();
    for run in runs {
      let record: charliebot_store::RunRecord = serde_json::from_value(run.clone()).expect("the fixture run decodes");
      let receipts = reply_order::receipts(&record, &loaded).expect("the run's receipts read");
      answered.extend(receipts.iter().map(|receipt| reply::replied(receipt, &loaded).expect("the reply reads")));
    }
    assert_eq!(answered, [true, true]);
    let received =
      reply_order::received_by_finished_turns(&home.features.store, sid, &loaded).expect("the received ids read");
    assert!(received.contains("70bc9630-25b2-54a7-bd34-f097462893ff"));
    assert!(received.contains("a8223322-6c39-5604-b98e-066e041a20b9"));

    audit_reply(&home.features, sid, &fixture_done(events)).await.expect("the audit runs");

    assert!(home.nudges(sid).is_empty());
  }

  /// Seeds a session's log with events that keep their own ids and timestamps.
  fn seed_events(home: &Home, session_id: &str, events: &[Value]) {
    let metadata = SessionMetadata::new(session_id, "Synthetic web session", TaskProfile::Manager);
    let metadata_path = home.features.store.metadata_path(session_id);
    std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    std::fs::write(&metadata_path, metadata.to_json_string(&home.features.registry.metadata_slots(), true)).unwrap();
    let events_path = home.features.store.chat_events_path(session_id);
    std::fs::create_dir_all(events_path.parent().unwrap()).unwrap();
    let mut lines = String::new();
    for event in events {
      lines.push_str(&event.to_string());
      lines.push('\n');
    }
    std::fs::write(events_path, lines).unwrap();
  }
}
