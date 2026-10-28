//! The round-end audit of a web session (`src/features/web_reply/audit.py`): the status reminder, the reply nudge and
//! the reply notice.
//!
//! The status reminder covers every finished turn of an open task, background turns included. A turn that ran
//! `charliebot web status set` or `charliebot web status confirm` needs no reminder, and neither does a turn that a
//! status reminder woke. The reply nudge covers a turn that answered a user message without a web reply. When one
//! turn misses both, one combined reminder covers both and wakes one turn.
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
use charliebot_store::TaskFacts;
use charliebot_store::compact::CompactValue;
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use serde_json::{Value, json};

use crate::reply;

/// The reminder a turn gets when it recorded no status set or confirm (`audit.STATUS_REMINDER_TEXT`).
pub const STATUS_REMINDER_TEXT: &str = concat!(
  "Session status reminder: your last turn ended without a status set or confirm. ",
  "Run `charliebot web status set` when Goal, Now or Waiting on you differs from the state that turn read back. ",
  "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back."
);
/// The one reminder a turn gets when it missed both its web reply and its status (`audit.COMBINED_REMINDER_TEXT`).
pub const COMBINED_REMINDER_TEXT: &str = concat!(
  "Web reply and session status reminder: your last turn woke on a user message and ended without a web reply and ",
  "without a status set or confirm. Post your answer as the web reply now. For an answer over 500 characters, write ",
  "the whole answer on a page first. Then post the conclusion and the page link as the web reply. ",
  "Run `charliebot web status set` when Goal, Now or Waiting on you differs from the state that turn read back. ",
  "Otherwise run `charliebot web status confirm`. The panel then shows the state the last turn read back."
);
/// The sender name a status reminder carries in the chat log (`audit.STATUS_REMINDER_SENDER_NAME`).
pub const STATUS_REMINDER_SENDER_NAME: &str = "Session status";

/// Whether `event` is a reminder that a turn ended without a status set or confirm (`audit.is_status_reminder`).
pub fn is_status_reminder(event: &impl EventFields) -> bool {
  event.field(WEB_STATUS_REMINDER).is_some()
}

/// Audit one finished turn of a web session for its status and its web reply (`audit.audit_turn`).
///
/// The status reminder needs a session that is not archived, an open task, no pending close request, a turn without a
/// status event and a turn that no status reminder woke. The reply step is [`audit_reply`]'s. A turn that needs both
/// gets one combined reminder and one wake.
pub async fn audit_turn(
  features: &Arc<FeatureContext>,
  session_id: &str,
  done: &Value,
  archived: bool,
) -> anyhow::Result<()> {
  let input_ids = features.store.master_done_input_event_ids(done);
  let loaded = reply::load_events(features, session_id).await?;
  let status_needed = !archived && status_reminder_needed(features, session_id, done, &input_ids, &loaded).await?;
  let (action, user_event_id) = reply_action(&loaded, &input_ids);
  let turn_id = done.get("id").and_then(Value::as_str);
  match (status_needed, action) {
    (true, ReplyAction::Nudge) => {
      let user_event_id = user_event_id.expect("a reply nudge names the user message it answers");
      post_status_reminder(features, session_id, COMBINED_REMINDER_TEXT, turn_id, Some(&user_event_id)).await?;
    }
    (true, action) => {
      post_status_reminder(features, session_id, STATUS_REMINDER_TEXT, turn_id, None).await?;
      if action == ReplyAction::Notice {
        let user_event_id = user_event_id.expect("a reply notice names the user message it answers");
        notice(features, session_id, &user_event_id).await?;
      }
    }
    (false, ReplyAction::Nudge) => {
      let user_event_id = user_event_id.expect("a reply nudge names the user message it answers");
      nudge(features, session_id, &user_event_id).await?;
    }
    (false, ReplyAction::Notice) => {
      let user_event_id = user_event_id.expect("a reply notice names the user message it answers");
      notice(features, session_id, &user_event_id).await?;
    }
    (false, ReplyAction::None) => {}
  }
  Ok(())
}

/// Audit one finished turn's web reply alone: a nudge for an unanswered user message, then a notice
/// (`audit.audit_reply`). The production hook audits the turn through [`audit_turn`]; the tests drive this reply-only
/// step directly, as Python's tests drive `audit.audit_reply`.
///
/// The turn's newest user message or nudge decides the step. A user message that the log shows neither answered nor
/// nudged gets the nudge, which wakes a new turn. A nudge whose user message the log still shows unanswered and
/// unnoticed gets the notice, which marks the session unread. A turn with neither input, a background turn, gets
/// nothing.
#[cfg(test)]
pub async fn audit_reply(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<()> {
  let input_ids = features.store.master_done_input_event_ids(done);
  let loaded = reply::load_events(features, session_id).await?;
  let (action, user_event_id) = reply_action(&loaded, &input_ids);
  match action {
    ReplyAction::Nudge => {
      let user_event_id = user_event_id.expect("a reply nudge names the user message it answers");
      nudge(features, session_id, &user_event_id).await?;
    }
    ReplyAction::Notice => {
      let user_event_id = user_event_id.expect("a reply notice names the user message it answers");
      notice(features, session_id, &user_event_id).await?;
    }
    ReplyAction::None => {}
  }
  Ok(())
}

/// The reply step a turn needs: none, a nudge, or a notice, and the user message it answers (`audit._reply_action`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyAction {
  None,
  Nudge,
  Notice,
}

fn reply_action(loaded: &[charliebot_store::SharedChatEvent], input_ids: &[String]) -> (ReplyAction, Option<String>) {
  let Some((input_event, user_event_id)) = reply::answered_input(loaded, input_ids) else {
    return (ReplyAction::None, None);
  };
  if reply::replied(loaded, &user_event_id) {
    return (ReplyAction::None, None);
  }
  if reply::nudge_of(input_event).is_none() {
    let action = (!reply::nudged(loaded, &user_event_id)).then_some(ReplyAction::Nudge);
    return (action.unwrap_or(ReplyAction::None), Some(user_event_id));
  }
  let action = (!reply::noticed(loaded, &user_event_id)).then_some(ReplyAction::Notice);
  (action.unwrap_or(ReplyAction::None), Some(user_event_id))
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

/// Whether the task holds a close request that no close event has answered (`audit._has_pending_close_request`).
fn has_pending_close_request(facts: &TaskFacts) -> bool {
  let closed: HashSet<Option<&str>> =
    facts.close_events.iter().map(|event| event.get("request_id").and_then(CompactValue::as_str)).collect();
  facts.close_requests.iter().any(|request| {
    let request_id = request.get("request_id").and_then(CompactValue::as_str);
    !closed.contains(&request_id)
  })
}

/// Whether one turn needs one status reminder (`audit._status_reminder_needed`).
async fn status_reminder_needed(
  features: &Arc<FeatureContext>,
  session_id: &str,
  done: &Value,
  input_ids: &[String],
  loaded: &[charliebot_store::SharedChatEvent],
) -> anyhow::Result<bool> {
  let store = Arc::clone(&features.store);
  let session = session_id.to_string();
  let (state, facts) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
    let state = store.task_state(&session)?.to_string();
    let facts = store.facts_of(&session)?;
    Ok((state, facts))
  })
  .await
  .context("the task-tree read stopped")??;
  if state != "open" {
    return Ok(false);
  }
  if has_pending_close_request(&facts) {
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
  let provenance =
    MessageProvenance { from_session: session_id.to_string(), from_session_name: reply::NUDGE_SENDER_NAME.to_string() };
  let mut event = features.store.build_agent_message_event(session_id, reply::NUDGE_TEXT, provenance);
  event[WEB_REPLY] = json!({(NUDGE_OF): user_event_id}).into();
  let persisted = features.store.persist_and_broadcast(session_id, event.to_value()).await?;
  let nudge_id = persisted["id"].as_str().context("the persisted nudge carries its id")?.to_string();

  spawn_wake(features, session_id, reply::NUDGE_TEXT, &nudge_id, reply::NUDGE_SENDER_NAME, "web-reply-nudge");
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
  use crate::test_support::{Home, id_of, master_done, user_event};

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
    assert_eq!(nudges[0]["content"], json!(reply::NUDGE_TEXT));
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
      .map(|event| event[WEB_REPLY]["answers"].clone())
      .collect();
    assert_eq!(answers, vec![json!(id_of(&user))]);
  }
}
