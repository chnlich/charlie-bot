//! The round-end audit of a web session (`src/features/web_reply/audit.py`): the status reminder, the reply nudge and
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
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use serde_json::{Value, json};

use crate::reply;

/// The status instruction that ends a status reminder (`audit.STATUS_SENTENCES`).
const STATUS_SENTENCES: &str = concat!(
  "Run `charliebot web status set` when Goal, Now or Waiting on you differs from the state that turn read back. ",
  "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back."
);

/// The reminder a turn gets when it recorded no status set or confirm (`audit.status_reminder_text`).
fn status_reminder_text() -> String {
  format!("Session status reminder: your last turn ended without a status set or confirm. {STATUS_SENTENCES}")
}

/// The one reminder that a turn gets when it missed both its web reply and its status, for the unanswered user message
/// `user_event_id` (`audit.combined_reminder_text`).
fn combined_reminder_text(user_event_id: &str) -> String {
  format!(
    "Web reply and session status reminder: your last turn woke on a user message and ended without a web reply and \
     without a status set or confirm. {} {STATUS_SENTENCES}",
    reply::answer_request(user_event_id)
  )
}
/// The sender name a status reminder carries in the chat log (`audit.STATUS_REMINDER_SENDER_NAME`).
pub const STATUS_REMINDER_SENDER_NAME: &str = "Session status";

/// Whether `event` is a reminder that a turn ended without a status set or confirm (`audit.is_status_reminder`).
pub fn is_status_reminder(event: &impl EventFields) -> bool {
  event.field(WEB_STATUS_REMINDER).is_some()
}

/// Audit one finished turn of a web session for its status and its web reply (`audit.audit_turn`).
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
  let pending_close = close_request_pending(features, session_id).await?;
  let status_needed =
    !archived && status_reminder_needed(features, session_id, done, &input_ids, &loaded, pending_close).await?;
  let steps = reply_steps(&loaded, &input_ids);
  let turn_id = done.get("id").and_then(Value::as_str);
  // A turn that needs a status reminder posts it first. Its first nudge rides the reminder, so one wake covers both.
  let mut nudge_on_reminder = None;
  if status_needed {
    nudge_on_reminder =
      steps.iter().find(|step| step.action == ReplyAction::Nudge).map(|step| step.user_event_id.clone());
    match &nudge_on_reminder {
      Some(user_event_id) => {
        let text = combined_reminder_text(user_event_id);
        post_status_reminder(features, session_id, &text, turn_id, Some(user_event_id)).await?;
      }
      None => post_status_reminder(features, session_id, &status_reminder_text(), turn_id, None).await?,
    }
  }
  for step in steps {
    match step.action {
      ReplyAction::Nudge if nudge_on_reminder.as_ref() == Some(&step.user_event_id) => {}
      ReplyAction::Nudge if !pending_close => nudge(features, session_id, &step.user_event_id).await?,
      ReplyAction::Nudge => {}
      ReplyAction::Notice => notice(features, session_id, &step.user_event_id).await?,
    }
  }
  Ok(())
}

/// Audit one finished turn's web reply alone: a nudge for each unanswered user message, then a notice for each nudge
/// that went unanswered (`audit.audit_reply`). The production hook audits the turn through [`audit_turn`]; the tests
/// drive this reply-only step directly, as Python's tests drive `audit.audit_reply`.
#[cfg(test)]
pub async fn audit_reply(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<()> {
  let input_ids = features.store.master_done_input_event_ids(done);
  let loaded = reply::load_events(features, session_id).await?;
  for step in reply_steps(&loaded, &input_ids) {
    match step.action {
      ReplyAction::Nudge => nudge(features, session_id, &step.user_event_id).await?,
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
struct ReplyStep {
  action: ReplyAction,
  user_event_id: String,
}

/// The reply steps of one turn, in log order (`audit._reply_steps`). Each input of the turn, a user message or a nudge,
/// is judged on the user message it answers: a user message that no web reply answers gets a nudge, unless a nudge was
/// already posted for it; a nudge whose user message no web reply answers gets a notice, unless one was posted. A user
/// message named by several inputs gets one step.
fn reply_steps(loaded: &[charliebot_store::SharedChatEvent], input_ids: &[String]) -> Vec<ReplyStep> {
  let wanted: HashSet<&str> = input_ids.iter().map(String::as_str).collect();
  let mut steps: Vec<ReplyStep> = Vec::new();
  for event in loaded {
    let event = event.as_ref();
    let Some(id) = event.get("id").and_then(CompactValue::as_str) else {
      continue;
    };
    if !wanted.contains(id) {
      continue;
    }
    let Some(user_event_id) = reply::answered_user_id(event) else {
      continue;
    };
    if reply::replied(loaded, &user_event_id) || steps.iter().any(|step| step.user_event_id == user_event_id) {
      continue;
    }
    let action = if reply::nudge_of(event).is_some() {
      (!reply::noticed(loaded, &user_event_id)).then_some(ReplyAction::Notice)
    } else {
      (!reply::nudged(loaded, &user_event_id)).then_some(ReplyAction::Nudge)
    };
    if let Some(action) = action {
      steps.push(ReplyStep { action, user_event_id });
    }
  }
  steps
}

/// The events one turn recorded, from the previous round end to `done` (`audit._turn_events`).
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

/// Whether one turn recorded a status set or a status confirm (`audit._status_recorded`).
fn status_recorded(turn_events: &[charliebot_store::SharedChatEvent]) -> bool {
  turn_events.iter().any(|event| {
    matches!(event.get("type").and_then(CompactValue::as_str), Some(WEB_STATUS) | Some(WEB_STATUS_CONFIRMED))
  })
}

/// Whether a status reminder for the turn `turn_id` is already in the log (`audit._reminder_already_posted`).
fn reminder_already_posted(loaded: &[charliebot_store::SharedChatEvent], turn_id: Option<&str>) -> bool {
  let Some(turn_id) = turn_id else { return false };
  loaded.iter().any(|event| event.get(WEB_STATUS_REMINDER).and_then(CompactValue::as_str) == Some(turn_id))
}

/// Whether one of the turn's inputs is a status reminder (`audit._woken_by_status_reminder`).
fn woken_by_status_reminder(loaded: &[charliebot_store::SharedChatEvent], input_ids: &[String]) -> bool {
  let wanted: HashSet<&str> = input_ids.iter().map(String::as_str).collect();
  loaded.iter().any(|event| {
    event.get("id").and_then(CompactValue::as_str).is_some_and(|id| wanted.contains(id))
      && is_status_reminder(event.as_ref())
  })
}

/// Whether the session's newest run left a close request pending (`audit._has_pending_close_request`). A request that a
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

/// Whether one turn needs one status reminder (`audit._status_reminder_needed`).
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

/// Persist one status reminder for `turn_id` and wake a new turn with it (`audit._post_status_reminder`).
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

/// Persist the nudge for `user_event_id`, then wake a new turn with it as its own task (`audit._nudge`). A failed wake
/// is logged in that task.
async fn nudge(features: &Arc<FeatureContext>, session_id: &str, user_event_id: &str) -> anyhow::Result<()> {
  let text = reply::nudge_text(user_event_id);
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

/// Wake one turn with `text` as its own logged background task (`master_trigger.spawn_master_turn`). A failed wake is
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

/// Persist the no-reply notice for `user_event_id`, then mark the session unread (`audit._notice`).
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
  use super::*;
  use crate::test_support::{Home, id_of, master_done, user_event, user_event_with_id};

  #[tokio::test]
  async fn a_user_turn_without_a_reply_gets_one_nudge() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    let done = master_done(&[id_of(&user)], 0);

    audit_reply(&home.features, &sid, &done).await.expect("the audit runs");
    audit_reply(&home.features, &sid, &done).await.expect("the audit runs");

    let nudges = home.nudges(&sid);
    assert_eq!(nudges.len(), 1);
    assert_eq!(nudges[0]["content"], json!(reply::nudge_text(id_of(&user))));
    assert_eq!(nudges[0]["from_session_name"], json!(reply::NUDGE_SENDER_NAME));
    assert_eq!(nudges[0][WEB_REPLY], json!({"nudge_of": id_of(&user)}));
  }

  #[tokio::test]
  async fn a_nudge_turn_without_a_reply_posts_one_notice_and_marks_the_session_unread() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    audit_reply(&home.features, &sid, &master_done(&[id_of(&user)], 0)).await.expect("the audit runs");
    let nudge_id = id_of(&home.nudges(&sid)[0]).to_string();
    let nudged_turn = master_done(&[nudge_id.as_str()], 0);

    audit_reply(&home.features, &sid, &nudged_turn).await.expect("the audit runs");
    audit_reply(&home.features, &sid, &nudged_turn).await.expect("the audit runs");

    let notices: Vec<Value> = home.events(&sid).into_iter().filter(|event| event["type"] == WEB_REPLY_NOTICE).collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["content"], json!(reply::NOTICE_TEXT));
    assert_eq!(notices[0][WEB_REPLY_NOTICE], json!({"input_event_id": id_of(&user)}));
    assert!(home.meta(&sid).await.has_unread);
  }

  #[tokio::test]
  async fn a_reply_in_the_nudge_turn_ends_the_audit_with_no_notice() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    audit_reply(&home.features, &sid, &master_done(&[id_of(&user)], 0)).await.expect("the audit runs");
    let nudge_id = id_of(&home.nudges(&sid)[0]).to_string();

    home.set_running(&[nudge_id.as_str()]);
    reply::post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");
    audit_reply(&home.features, &sid, &master_done(&[nudge_id.as_str()], 0)).await.expect("the audit runs");

    let events = home.events(&sid);
    assert!(events.iter().all(|event| event["type"] != WEB_REPLY_NOTICE));
    let answers: Vec<Value> = events
      .iter()
      .filter(|event| event["type"] == WEB_REPLY)
      .map(|event| event[WEB_REPLY]["answer_ids"].clone())
      .collect();
    assert_eq!(answers, vec![json!([id_of(&user)])]);
  }

  #[tokio::test]
  async fn each_unanswered_user_message_of_a_turn_gets_its_own_nudge() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = home.append(&sid, user_event_with_id("11111111-a", "first question")).await;
    let answered = home.append(&sid, user_event_with_id("22222222-b", "second question")).await;
    let last = home.append(&sid, user_event_with_id("33333333-c", "third question")).await;
    let turn = [id_of(&first), id_of(&answered), id_of(&last)];
    home.set_running(&turn);
    let tags = ["22222222".to_string()];
    reply::post_web_reply_with_request_id(&home.features, &sid, "the answer", &tags, None, None)
      .await
      .expect("the reply posts");
    let done = master_done(&turn, 0);

    audit_turn(&home.features, &sid, &done, false).await.expect("the audit runs");
    audit_turn(&home.features, &sid, &done, false).await.expect("the audit runs");

    let nudged: Vec<Value> = home.nudges(&sid).iter().map(|nudge| nudge[WEB_REPLY]["nudge_of"].clone()).collect();
    assert_eq!(nudged, vec![json!("11111111-a"), json!("33333333-c")]);
  }

  #[tokio::test]
  async fn a_turn_with_two_nudges_posts_a_notice_for_each_unanswered_message() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = home.append(&sid, user_event_with_id("11111111-a", "first question")).await;
    let second = home.append(&sid, user_event_with_id("22222222-b", "second question")).await;
    audit_reply(&home.features, &sid, &master_done(&[id_of(&first), id_of(&second)], 0)).await.expect("the audit runs");
    let nudge_ids: Vec<String> = home.nudges(&sid).iter().map(|nudge| id_of(nudge).to_string()).collect();
    assert_eq!(nudge_ids.len(), 2);

    audit_reply(&home.features, &sid, &master_done(&[nudge_ids[0].as_str(), nudge_ids[1].as_str()], 0))
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
    home.launch_run(&stale, "run-1");
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
      turns.push((session.clone(), master_done(&[id_of(&user)], 0)));
    }

    for (session, done) in &turns {
      audit_turn(&home.features, session, done, false).await.expect("the audit runs");
    }

    assert_eq!(home.nudges(&stale).len(), 1, "a voided close gates nothing");
    assert_eq!(home.nudges(&control).len(), 1, "without a close request the turn is nudged");
  }
}
