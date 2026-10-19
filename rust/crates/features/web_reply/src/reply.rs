//! The web reply record and the web-session test (`src/features/web_reply/reply.py`): the refusals, the `answers`
//! binding, the For-you layer and the readback. The round-end audit that nudges and notices over these records is in
//! `audit.rs`.
//!
//! The web session test is chat_threads' `thread_sessions.is_thread_session`, restated over the same fields: this
//! package does not depend on chat_threads.

use std::sync::Arc;

use anyhow::Context;
use axum::http::StatusCode;
use charliebot_core::event_types::{AGENT_MESSAGE, INPUT_EVENT_ID, USER, WEB_REPLY, WEB_REPLY_NOTICE};
use charliebot_core::pystr;
use charliebot_registry::{FeatureContext, RunningTurnInputs, StateError};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{SESSION_NOT_FOUND_DETAIL, Store, StoreError};
use serde_json::{Value, json};

/// The web reply rules file under `prompts/` (`reply.RULES_FILE`).
pub const RULES_FILE: &str = "web_reply.md";

/// The audit's nudge: the agent's input when a user message's turn ended without a reply (`reply.NUDGE_TEXT`).
pub const NUDGE_TEXT: &str = "Post the web reply now.";

/// The notice the user reads when the nudge turn also ended without a reply (`reply.NOTICE_TEXT`).
pub const NOTICE_TEXT: &str = "No reply was posted for this message. The Everything view shows this turn's output.";

/// The sender name the nudge carries in the chat log (`reply.NUDGE_SENDER_NAME`).
pub const NUDGE_SENDER_NAME: &str = "Web reply";

/// The reply budget the readback's `over_budget` flag measures; an over-budget reply still posts
/// (`reply.REPLY_BUDGET_CHARS`).
pub const REPLY_BUDGET_CHARS: usize = 500;

/// The roles the For-you layer shows: the user's messages, the replies and the notices (`reply._FOR_YOU_ROLES`).
const FOR_YOU_ROLES: [&str; 3] = [USER, WEB_REPLY, WEB_REPLY_NOTICE];

/// A refused web reply or status write, or a failure the route answers as an unhandled exception.
#[derive(Debug)]
pub enum WebReplyError {
  /// `WebReplyError`: the HTTP status and the detail the route answers with. Nothing is recorded or stored.
  Refused { status: StatusCode, detail: String },
  /// An unexpected failure of a store read or write.
  Failed(anyhow::Error),
}

impl WebReplyError {
  /// A refusal that answers `status` with `detail`.
  pub fn refused(status: StatusCode, detail: impl Into<String>) -> Self {
    Self::Refused { status, detail: detail.into() }
  }
}

impl From<StoreError> for WebReplyError {
  fn from(error: StoreError) -> Self {
    Self::Failed(error.into())
  }
}

impl From<StateError> for WebReplyError {
  fn from(error: StateError) -> Self {
    Self::Failed(error.into())
  }
}

impl From<anyhow::Error> for WebReplyError {
  fn from(error: anyhow::Error) -> Self {
    Self::Failed(error)
  }
}

/// True for a manager session that no Slack or Discord thread owns (`reply.is_web_session`).
pub fn is_web_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager
    && meta.extra.get("slack_origin").is_none_or(Value::is_null)
    && meta.extra.get("discord_origin").is_none_or(Value::is_null)
}

/// The web session named by `session_id` (`reply.require_web_session`): 404 for an unknown session, 409 for a thread
/// session or a worker.
pub async fn require_web_session(
  features: &FeatureContext,
  session_id: &str,
) -> Result<SessionMetadata, WebReplyError> {
  let meta = features
    .store
    .get_session(session_id)
    .await?
    .ok_or_else(|| WebReplyError::refused(StatusCode::NOT_FOUND, SESSION_NOT_FOUND_DETAIL))?;
  if !is_web_session(&meta) {
    return Err(WebReplyError::refused(
      StatusCode::CONFLICT,
      "Session is not a web session: it is a thread session or a worker",
    ));
  }
  Ok(meta)
}

/// The session's chat events in log order, read off the runtime's worker threads as `asyncio.to_thread` does.
pub async fn load_events(features: &FeatureContext, session_id: &str) -> anyhow::Result<Vec<Value>> {
  let store = Arc::clone(&features.store);
  let session_id = session_id.to_string();
  let loaded = tokio::task::spawn_blocking(move || store.load_chat_events_sync(&session_id))
    .await
    .context("the chat-event read stopped")?;
  Ok(loaded?)
}

/// The nudge's `nudge_of`: the user message an audit nudge was posted for, or `None` for any other event
/// (`reply.nudge_of`).
pub fn nudge_of(event: &Value) -> Option<&str> {
  if event.get("type").and_then(Value::as_str) != Some(AGENT_MESSAGE) {
    return None;
  }
  event.get(WEB_REPLY)?.get("nudge_of")?.as_str()
}

/// The newest user message or nudge among `input_event_ids`, with the user message it answers; `None` when neither
/// (`reply.answered_input`). `events` is in log order, so the last match is the newest. A user message answers itself;
/// a nudge answers the user message it was posted for.
pub fn answered_input<'a>(events: &'a [Value], input_event_ids: &[String]) -> Option<(&'a Value, String)> {
  let mut bound = None;
  for event in events {
    let Some(id) = event.get("id").and_then(Value::as_str) else {
      continue;
    };
    if !input_event_ids.iter().any(|wanted| wanted == id) {
      continue;
    }
    if event.get("type").and_then(Value::as_str) == Some(USER) {
      bound = Some((event, id.to_string()));
    } else if let Some(nudged) = nudge_of(event) {
      bound = Some((event, nudged.to_string()));
    }
  }
  bound
}

/// Whether the log holds a web reply that answers `user_event_id` (`reply.replied`).
pub fn replied(events: &[Value], user_event_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(Value::as_str) == Some(WEB_REPLY)
      && event.get(WEB_REPLY).and_then(|record| record.get("answers")).and_then(Value::as_str) == Some(user_event_id)
  })
}

/// Whether the log holds the nudge posted for `user_event_id` (`reply.nudged`).
pub fn nudged(events: &[Value], user_event_id: &str) -> bool {
  events.iter().any(|event| nudge_of(event) == Some(user_event_id))
}

/// Whether the log holds the no-reply notice for `user_event_id` (`reply.noticed`).
pub fn noticed(events: &[Value], user_event_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(Value::as_str) == Some(WEB_REPLY_NOTICE)
      && event.get(WEB_REPLY_NOTICE).and_then(|record| record.get(INPUT_EVENT_ID)).and_then(Value::as_str)
        == Some(user_event_id)
  })
}

/// Record `text` as the session's web reply and return the readback the CLI prints (`reply.post_web_reply`).
///
/// The text is recorded exactly as written. It answers the newest user message or nudge of the running turn, and
/// nothing when the running turn has neither. A refusal records nothing and marks nothing.
pub async fn post_web_reply(features: &FeatureContext, session_id: &str, text: &str) -> Result<Value, WebReplyError> {
  require_web_session(features, session_id).await?;
  if pystr::strip(text).is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Reply text is empty"));
  }
  let loaded = load_events(features, session_id).await?;
  let running = features.startup.require::<RunningTurnInputs>()?.user_event_ids(session_id);
  let answers = answered_input(&loaded, &running).map(|(_, user_event_id)| user_event_id);
  let chars = text.chars().count();
  features
    .store
    .persist_and_broadcast(
      session_id,
      json!({
        "type": WEB_REPLY,
        "content": text,
        WEB_REPLY: {"answers": answers, "chars": chars},
      }),
    )
    .await?;
  features.store.mark_unread(session_id).await?;
  let over_budget = chars > REPLY_BUDGET_CHARS;
  tracing::info!(session = %session_id, chars, over_budget, answers = ?answers, "web_reply_posted");
  Ok(json!({"posted": true, "chars": chars, "over_budget": over_budget, "answers": answers}))
}

/// The For-you layer: the user's messages, the replies and the notices, in log order, as `{role, content}` rows
/// (`reply.for_you_entries`).
pub fn for_you_entries(store: &Store, events: &[Value]) -> Result<Vec<Value>, WebReplyError> {
  let messages = store.events_to_messages(events)?;
  Ok(
    messages
      .into_iter()
      .filter(|message| message.get("role").and_then(Value::as_str).is_some_and(|role| FOR_YOU_ROLES.contains(&role)))
      .map(|message| json!({"role": message["role"].clone(), "content": message["content"].clone()}))
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::{Home, id_of, user_event};

  /// The web reply events of the session's log, as `(content, web_reply)` pairs.
  fn replies(home: &Home, session_id: &str) -> Vec<(Value, Value)> {
    home
      .events(session_id)
      .iter()
      .filter(|event| event["type"] == WEB_REPLY)
      .map(|event| (event["content"].clone(), event[WEB_REPLY].clone()))
      .collect()
  }

  #[tokio::test]
  async fn a_web_reply_records_the_text_and_reads_back_the_outcome() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let readback = post_web_reply(&home.features, &sid, "the answer").await.expect("the reply posts");

    assert_eq!(readback, json!({"posted": true, "chars": 10, "over_budget": false, "answers": null}));
    assert_eq!(replies(&home, &sid), vec![(json!("the answer"), json!({"answers": null, "chars": 10}))]);
    assert!(home.meta(&sid).await.has_unread);
  }

  #[tokio::test]
  async fn a_refused_web_reply_records_nothing_and_marks_nothing() {
    for (case, status) in [("unknown", 404), ("thread", 409), ("worker", 409), ("blank", 422)] {
      let home = Home::new();
      let origin = (case == "thread")
        .then(|| ("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"})));
      let root = home.manager("session", origin).await;
      let sid = if case == "worker" { home.worker(&root, "worker").await } else { root };
      let target = if case == "unknown" { "no-such-session" } else { sid.as_str() };
      let text = if case == "blank" { " \n\t" } else { "the answer" };
      let events_before = home.events(&sid).len();

      let error = post_web_reply(&home.features, target, text).await.expect_err("the reply is refused");

      match error {
        WebReplyError::Refused { status: code, .. } => assert_eq!(code.as_u16(), status, "case {case}"),
        other => panic!("case {case}: expected a refusal, got {other:?}"),
      }
      assert_eq!(home.events(&sid).len(), events_before, "case {case}");
      assert!(!home.meta(&sid).await.has_unread, "case {case}");
    }
  }

  #[tokio::test]
  async fn the_budget_flags_an_over_budget_reply_and_the_reply_still_posts() {
    for (chars, over_budget) in [(500, false), (501, true)] {
      let home = Home::new();
      let sid = home.manager("session", None).await;

      let readback = post_web_reply(&home.features, &sid, &"x".repeat(chars)).await.expect("the reply posts");

      assert_eq!(readback, json!({"posted": true, "chars": chars, "over_budget": over_budget, "answers": null}));
      assert_eq!(replies(&home, &sid).len(), 1);
    }
  }

  #[tokio::test]
  async fn a_reply_answers_the_user_message_that_woke_its_turn() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    home.set_running(&[id_of(&user)]);

    let readback = post_web_reply(&home.features, &sid, "the answer").await.expect("the reply posts");

    assert_eq!(readback["answers"], json!(id_of(&user)));
  }

  #[tokio::test]
  async fn a_reply_in_a_nudge_turn_answers_the_user_message_the_nudge_asks_about() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    let nudge = home
      .append(&sid, json!({"type": AGENT_MESSAGE, "content": NUDGE_TEXT, WEB_REPLY: {"nudge_of": id_of(&user)}}))
      .await;
    home.set_running(&[id_of(&nudge)]);

    let readback = post_web_reply(&home.features, &sid, "the answer").await.expect("the reply posts");

    assert_eq!(readback["answers"], json!(id_of(&user)));
  }

  #[tokio::test]
  async fn a_reply_in_a_background_turn_answers_nothing() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let wake = home.append(&sid, json!({"type": "scheduled_trigger", "content": "wake"})).await;
    home.set_running(&[id_of(&wake)]);

    let readback = post_web_reply(&home.features, &sid, "the answer").await.expect("the reply posts");

    assert_eq!(readback["answers"], Value::Null);
  }

  #[tokio::test]
  async fn a_reply_answers_the_newest_input_in_log_order_not_in_input_order() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = home.append(&sid, user_event("first")).await;
    let second = home.append(&sid, user_event("second")).await;
    home.set_running(&[id_of(&second), id_of(&first)]);

    let readback = post_web_reply(&home.features, &sid, "the answer").await.expect("the reply posts");

    assert_eq!(readback["answers"], json!(id_of(&second)));
  }

  #[tokio::test]
  async fn the_for_you_layer_keeps_user_messages_replies_and_notices_in_log_order() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_event("first")).await;
    home.append(&sid, json!({"type": "scheduled_trigger", "content": "wake"})).await;
    home.append(&sid, json!({"type": WEB_REPLY, "content": "answer", WEB_REPLY: {"answers": null, "chars": 6}})).await;
    home.append(&sid, json!({"type": AGENT_MESSAGE, "content": NUDGE_TEXT, WEB_REPLY: {"nudge_of": "u2"}})).await;
    home.append(&sid, user_event("second")).await;
    home
      .append(&sid, json!({"type": WEB_REPLY_NOTICE, "content": "notice", WEB_REPLY_NOTICE: {"input_event_id": "u2"}}))
      .await;

    let entries = for_you_entries(&home.features.store, &home.events(&sid)).expect("the layer reads");

    assert_eq!(
      entries,
      vec![
        json!({"role": "user", "content": "first"}),
        json!({"role": WEB_REPLY, "content": "answer"}),
        json!({"role": "user", "content": "second"}),
        json!({"role": WEB_REPLY_NOTICE, "content": "notice"}),
      ]
    );
  }

  #[test]
  fn a_web_session_is_a_manager_that_no_slack_or_discord_thread_owns() {
    let origin = json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"});
    let mut slack = SessionMetadata::new("s", "s", TaskProfile::Manager);
    slack.extra.insert("slack_origin".to_string(), origin);
    let mut discord = SessionMetadata::new("s", "s", TaskProfile::Manager);
    discord.extra.insert("discord_origin".to_string(), json!({"guild_id": "G1"}));

    assert!(is_web_session(&SessionMetadata::new("s", "s", TaskProfile::Manager)));
    assert!(!is_web_session(&slack));
    assert!(!is_web_session(&discord));
    assert!(!is_web_session(&SessionMetadata::new("s", "s", TaskProfile::Worker)));
  }

  #[test]
  fn py_strip_trims_the_information_separators_as_python_does() {
    assert_eq!(pystr::strip(" \u{1c}x\u{1f}\n"), "x");
    assert_eq!(pystr::strip(" \t\n"), "");
  }

  #[test]
  fn the_nudge_of_reads_only_an_agent_message() {
    let nudge = json!({"type": AGENT_MESSAGE, "web_reply": {"nudge_of": "u1"}});
    assert_eq!(nudge_of(&nudge), Some("u1"));
    assert_eq!(nudge_of(&json!({"type": USER, "web_reply": {"nudge_of": "u1"}})), None);
    assert_eq!(nudge_of(&json!({"type": AGENT_MESSAGE})), None);
  }
}
