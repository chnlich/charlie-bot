//! The round-end audit of a web session (`src/features/web_reply/audit.py`): a nudge for an unanswered user message,
//! then a notice if the nudged turn also answers nothing.
//!
//! Each step leaves a log marker (the nudge, the notice), so each happens at most once per user message.

use std::sync::Arc;

use anyhow::Context;
use charliebot_core::event_types::{AGENT_MESSAGE, INPUT_EVENT_ID, WEB_REPLY, WEB_REPLY_NOTICE};
use charliebot_registry::FeatureContext;
use charliebot_store::MessageProvenance;
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use serde_json::{Value, json};

use crate::reply;

/// Audit one finished turn of a web session (`audit.audit_turn`).
///
/// The turn's newest user message or nudge decides the step. A user message that the log shows neither answered nor
/// nudged gets the nudge, which wakes a new turn. A nudge whose user message the log still shows unanswered and
/// unnoticed gets the notice, which marks the session unread. A turn with neither input, a background turn, gets
/// nothing.
pub async fn audit_turn(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<()> {
  let input_ids = features.store.master_done_input_event_ids(done);
  if input_ids.is_empty() {
    return Ok(());
  }
  let loaded = reply::load_events(features, session_id).await?;
  let Some((input_event, user_event_id)) = reply::answered_input(&loaded, &input_ids) else {
    return Ok(());
  };
  if reply::replied(&loaded, &user_event_id) {
    return Ok(());
  }
  let is_nudge = reply::nudge_of(input_event).is_some();
  let action = if is_nudge {
    (!reply::noticed(&loaded, &user_event_id)).then_some(false)
  } else {
    (!reply::nudged(&loaded, &user_event_id)).then_some(true)
  };
  // The Python cached list is appended in place (`chat_events.py:221-224`); release the shared Arc before recording.
  drop(loaded);
  match action {
    Some(true) => nudge(features, session_id, &user_event_id).await?,
    Some(false) => notice(features, session_id, &user_event_id).await?,
    None => {}
  }
  Ok(())
}

/// Persist the nudge for `user_event_id`, then wake a new turn with it as its own task (`audit._nudge`). A failed wake
/// is logged in that task.
async fn nudge(features: &Arc<FeatureContext>, session_id: &str, user_event_id: &str) -> anyhow::Result<()> {
  let provenance =
    MessageProvenance { from_session: session_id.to_string(), from_session_name: reply::NUDGE_SENDER_NAME.to_string() };
  let mut event = features.store.build_agent_message_event(session_id, reply::NUDGE_TEXT, provenance);
  event[WEB_REPLY] = json!({"nudge_of": user_event_id}).into();
  let persisted = features.store.persist_and_broadcast(session_id, event.to_value()).await?;
  let nudge_id = persisted["id"].as_str().context("the persisted nudge carries its id")?.to_string();

  let task = format!("web-reply-nudge-{session_id}");
  let wake_features = Arc::clone(features);
  let wake_session = session_id.to_string();
  let wake_input = nudge_id.clone();
  tokio::spawn(async move {
    let request = TriggerMasterRequest {
      session_id: &wake_session,
      summary: reply::NUDGE_TEXT,
      input_id: Some(&wake_input),
      event_type: AGENT_MESSAGE,
      actor: "agent",
      from_session: Some(&wake_session),
      from_session_name: Some(reply::NUDGE_SENDER_NAME),
      pull_back: true,
    };
    if let Err(error) = trigger_master(&wake_features, request).await {
      tracing::error!(target: "charliebot_web_reply", task = %task, error = %error, "web reply nudge wake failed");
    }
  });
  tracing::info!(session = %session_id, user_event_id, nudge_id = %nudge_id, "web_reply_nudge");
  Ok(())
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
  tracing::info!(session = %session_id, user_event_id, "web_reply_notice");
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

    audit_turn(&home.features, &sid, &done).await.expect("the audit runs");
    audit_turn(&home.features, &sid, &done).await.expect("the audit runs");

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
    audit_turn(&home.features, &sid, &master_done(&[id_of(&user)], 0)).await.expect("the audit runs");
    let nudge_id = id_of(&home.nudges(&sid)[0]).to_string();
    let nudged_turn = master_done(&[nudge_id.as_str()], 0);

    audit_turn(&home.features, &sid, &nudged_turn).await.expect("the audit runs");
    audit_turn(&home.features, &sid, &nudged_turn).await.expect("the audit runs");

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
    audit_turn(&home.features, &sid, &master_done(&[id_of(&user)], 0)).await.expect("the audit runs");
    let nudge_id = id_of(&home.nudges(&sid)[0]).to_string();

    home.set_running(&[nudge_id.as_str()]);
    reply::post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");
    audit_turn(&home.features, &sid, &master_done(&[nudge_id.as_str()], 0)).await.expect("the audit runs");

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
