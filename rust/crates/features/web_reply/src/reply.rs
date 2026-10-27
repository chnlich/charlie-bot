//! The web reply record and the web-session test (`src/features/web_reply/reply.py`): the refusals, the `answers`
//! binding, the For-you layer and the readback. The round-end audit that nudges and notices over these records is in
//! `audit.rs`.
//!
//! The web session test is chat_threads' `thread_sessions.is_thread_session`, restated over the same fields: this
//! package does not depend on chat_threads.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use anyhow::Context;
use axum::http::StatusCode;
use charliebot_compat::file_urls;
use charliebot_core::event_types::EventFields;
use charliebot_core::event_types::{AGENT_MESSAGE, INPUT_EVENT_ID, USER, WEB_REPLY, WEB_REPLY_NOTICE};
use charliebot_core::paths;
use charliebot_core::pyregex::compile_python_automata_regex;
use charliebot_core::pystr;
use charliebot_registry::{FeatureContext, RunningTurnInputs, StateError};
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::{SESSION_NOT_FOUND_DETAIL, SharedChatEvent, Store, StoreError};
use regex::{Captures, Regex};
use serde_json::{Value, json};

/// The web reply rules file under `prompts/` (`reply.RULES_FILE`).
pub const RULES_FILE: &str = "web_reply.md";

/// The audit's nudge: the agent's input when a user message's turn ended without a reply (`reply.NUDGE_TEXT`).
pub const NUDGE_TEXT: &str = concat!(
  "Web reply nudge: your last turn woke on a user message and ended without a web reply. The For-you view ",
  "hides your turn output. Post your answer as the web reply now. For an answer over 500 characters, write ",
  "the whole answer on a page first. Then post the conclusion and the page link as the web reply."
);

/// The notice the user reads when the nudge turn also ended without a reply (`reply.NOTICE_TEXT`).
pub const NOTICE_TEXT: &str = "No reply was posted for this message. The Everything view shows this turn's output.";

/// The sender name the nudge carries in the chat log (`reply.NUDGE_SENDER_NAME`).
pub const NUDGE_SENDER_NAME: &str = "Web reply";

/// The reply budget the readback's `over_budget` flag measures; an over-budget reply still posts
/// (`reply.REPLY_BUDGET_CHARS`).
pub const REPLY_BUDGET_CHARS: usize = 500;

/// The language check holds a reply when the newest user message has at least this many Han characters.
const LANGUAGE_CHECK_MIN_USER_HAN: usize = 5;
/// The language check holds a reply whose Han share, Han / (Han + Latin letters), is below this.
const LANGUAGE_CHECK_MAX_REPLY_HAN_SHARE: f64 = 0.05;
/// The 409 detail of a held reply. The agent reads it: it rewrites the reply in Chinese, or adds `--keep-language`.
const LANGUAGE_HOLD_DETAIL: &str = concat!(
  "Language check: the user's last message is in Chinese.\n",
  "This reply has almost no Chinese.\n",
  "The command posted nothing.\n",
  "Rewrite the reply in Chinese.\n",
  "Then post the reply again.\n",
  "To post the reply as written, add --keep-language \"<reason>\" to the command."
);

static CODE_FENCE_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"(?s)```.*?```"));
static INLINE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"`[^`]*`"));
static URL_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"\(?https?://\S+\)?"));
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|[^\p{L}\p{N}_])([~/][\p{L}\p{N}_./-]+)").unwrap());
static MARKDOWN_LINK_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"\[([^\]]*)\]\([^)]*\)"));
static HAN_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"[㐀-䶿一-鿿]"));
static LATIN_RE: LazyLock<Regex> = LazyLock::new(|| compile_python_automata_regex(r"[A-Za-z]"));

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
  charliebot_tasks::midturn_delivery::is_web_session(meta)
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
pub async fn load_events(features: &FeatureContext, session_id: &str) -> anyhow::Result<Arc<Vec<SharedChatEvent>>> {
  let store = Arc::clone(&features.store);
  let session_id = session_id.to_string();
  let loaded = tokio::task::spawn_blocking(move || store.load_chat_events_shared_sync(&session_id))
    .await
    .context("the chat-event read stopped")?;
  Ok(loaded?)
}

/// The nudge's `nudge_of`: the user message an audit nudge was posted for, or `None` for any other event
/// (`reply.nudge_of`).
pub fn nudge_of(event: &impl EventFields) -> Option<String> {
  if event.field("type").and_then(EventFields::as_str_field) != Some(AGENT_MESSAGE) {
    return None;
  }
  event
    .field(WEB_REPLY)
    .and_then(|record| record.field("nudge_of"))
    .and_then(EventFields::as_str_field)
    .map(str::to_string)
}

/// The newest user message or nudge among `input_event_ids`, with the user message it answers; `None` when neither
/// (`reply.answered_input`). `events` is in log order, so the last match is the newest. A user message answers itself;
/// a nudge answers the user message it was posted for.
pub fn answered_input<'a>(
  events: &'a [SharedChatEvent],
  input_event_ids: &[String],
) -> Option<(&'a ChatEvent, String)> {
  answered_input_from(events.iter().rev().map(SharedChatEvent::as_ref), input_event_ids)
}

fn answered_input_from<'a>(
  events: impl Iterator<Item = &'a ChatEvent>,
  input_event_ids: &[String],
) -> Option<(&'a ChatEvent, String)> {
  // Python builds `wanted = set(input_event_ids)` before this event pass (`src/features/web_reply/reply.py:97-108`).
  if input_event_ids.is_empty() {
    return None;
  }
  let input_event_ids: HashSet<&str> = input_event_ids.iter().map(String::as_str).collect();
  // The forward Python pass overwrites `bound`, so a reverse pass can stop at the same last match.
  for event in events {
    let Some(id) = event.get("id").and_then(CompactValue::as_str) else {
      continue;
    };
    if !input_event_ids.contains(id) {
      continue;
    }
    if event.get("type").and_then(CompactValue::as_str) == Some(USER) {
      return Some((event, id.to_string()));
    }
    if let Some(nudged) = nudge_of(event) {
      return Some((event, nudged.to_string()));
    }
  }
  None
}

/// Whether the log holds a web reply that answers `user_event_id` (`reply.replied`).
pub fn replied(events: &[SharedChatEvent], user_event_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(CompactValue::as_str) == Some(WEB_REPLY)
      && event.get(WEB_REPLY).and_then(|record| record.get("answers")).and_then(CompactValue::as_str)
        == Some(user_event_id)
  })
}

/// Whether the log holds the nudge posted for `user_event_id` (`reply.nudged`).
pub fn nudged(events: &[SharedChatEvent], user_event_id: &str) -> bool {
  events.iter().any(|event| nudge_of(event.as_ref()).as_deref() == Some(user_event_id))
}

/// Whether the log holds the no-reply notice for `user_event_id` (`reply.noticed`).
pub fn noticed(events: &[SharedChatEvent], user_event_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(CompactValue::as_str) == Some(WEB_REPLY_NOTICE)
      && event.get(WEB_REPLY_NOTICE).and_then(|record| record.get(INPUT_EVENT_ID)).and_then(CompactValue::as_str)
        == Some(user_event_id)
  })
}

/// The Han and Latin-letter counts of `text`, with fenced code, inline code, URLs and paths left out (`_han_and_latin`).
fn han_and_latin(text: &str) -> anyhow::Result<(usize, usize)> {
  let mut body = text.to_owned();
  for pattern in [&*CODE_FENCE_RE, &*INLINE_CODE_RE, &*URL_RE] {
    body = pattern.replace_all(&body, " ").into_owned();
  }
  body = PATH_RE
    .replace_all(&body, |captures: &Captures<'_>| {
      format!("{} ", captures.get(1).expect("the path regex captures its preceding character").as_str())
    })
    .into_owned();
  Ok((count_matches(&HAN_RE, &body), count_matches(&LATIN_RE, &body)))
}

fn count_matches(pattern: &Regex, text: &str) -> usize {
  pattern.find_iter(text).count()
}

/// Whether the language check holds `reply` after the user message `user_text`, where None means no user message
/// (`language_check_holds`). A reply with no Han and no Latin letters is not held.
fn language_check_holds(user_text: Option<&str>, reply: &str) -> anyhow::Result<bool> {
  let Some(user_text) = user_text else {
    return Ok(false);
  };
  if han_and_latin(user_text)?.0 < LANGUAGE_CHECK_MIN_USER_HAN {
    return Ok(false);
  }
  let (reply_han, reply_latin) = han_and_latin(reply)?;
  let letters = reply_han + reply_latin;
  Ok(letters > 0 && (reply_han as f64) / (letters as f64) < LANGUAGE_CHECK_MAX_REPLY_HAN_SHARE)
}

fn newest_user_text_shared(events: &[SharedChatEvent]) -> Option<&str> {
  events.iter().rev().find_map(|event| {
    let event = event.as_ref();
    let from_user = event.get("type").and_then(CompactValue::as_str) == Some(USER)
      && event.get("actor").and_then(CompactValue::as_str) == Some("user");
    if from_user { event.get("content").and_then(CompactValue::as_str) } else { None }
  })
}

/// The paths that resolve the file links of a web message in `session_id`: this server's port, the CharlieBot home, the
/// session directory and the user's home (`web_reply.api._file_link_scan_context`).
pub fn file_link_scan_context(
  features: &FeatureContext,
  session_id: &str,
) -> Result<file_urls::ScanContext, WebReplyError> {
  let config = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  Ok(file_urls::ScanContext {
    server_port: config.server.port,
    charliebot_home: config.charliebot_home.clone(),
    session_dir: features.store.session_dir(session_id),
    user_home: PathBuf::from(paths::expanduser("~")),
  })
}

/// Refuses with 422 when a text in `texts` links a dead file. The detail is `dead_links_detail` over the dead links in
/// scan order, so nothing is recorded or stored when any text names one.
pub fn refuse_dead_links<'a>(
  texts: impl IntoIterator<Item = &'a str>,
  scan_context: &file_urls::ScanContext,
) -> Result<(), WebReplyError> {
  let mut dead = Vec::new();
  for text in texts {
    let findings =
      file_urls::scan_file_links(text, Some(scan_context)).map_err(|error| WebReplyError::Failed(error.into()))?;
    dead.extend(findings.into_iter().filter(file_urls::FileLink::dead));
  }
  if dead.is_empty() {
    return Ok(());
  }
  Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, file_urls::dead_links_detail(&dead)))
}

/// The characters the reader counts in `text`: each inline markdown link counts as its label (`visible_reply_chars`).
fn visible_reply_chars(text: &str) -> anyhow::Result<usize> {
  let visible = MARKDOWN_LINK_RE.replace_all(text, "$1");
  Ok(visible.chars().count())
}

/// Record `text` as the session's web reply and return the readback the CLI prints (`reply.post_web_reply`).
///
/// The text is recorded exactly as written. It answers the newest user message or nudge of the running turn, and
/// nothing when the running turn has neither. The language check holds the text unless `keep_language`, the agent's
/// reason for posting it as written, is given; the reason is then recorded on the reply. A refusal records nothing and
/// marks nothing.
#[cfg(test)]
pub async fn post_web_reply(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  keep_language: Option<&str>,
) -> Result<Value, WebReplyError> {
  post_web_reply_with_request_id(features, session_id, text, keep_language, None).await
}

/// Record a web reply for one caller request, replaying the original event when the run retries it.
pub async fn post_web_reply_with_request_id(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  keep_language: Option<&str>,
  request_id: Option<&str>,
) -> Result<Value, WebReplyError> {
  require_web_session(features, session_id).await?;
  if pystr::strip(text).is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Reply text is empty"));
  }
  refuse_dead_links([text], &file_link_scan_context(features, session_id)?)?;
  if keep_language.is_some_and(|reason| pystr::strip(reason).is_empty()) {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Keep-language reason is empty"));
  }
  let chars = visible_reply_chars(text)?;
  let epoch = features.store.prime_aggregator(session_id).await?;
  let event = {
    let _guard = features.store.control_lock().await;
    let events = features.store.load_all_facts_shared_sync(session_id)?;
    if let Some(request_id) = request_id {
      let event_id = charliebot_store::control_events::stable_request_event_id(session_id, request_id);
      if let Some(existing) = charliebot_store::control_events::replay_request_event(
        events.iter().map(SharedChatEvent::as_ref),
        &event_id,
        request_id,
        WEB_REPLY,
        text,
        None,
      )? {
        return Ok(web_reply_receipt(&existing));
      }
    }
    let running = features.startup.require::<RunningTurnInputs>()?.user_event_ids(session_id);
    if keep_language.is_none() && language_check_holds(newest_user_text_shared(&events), text)? {
      tracing::info!(session = %session_id, chars, "web_reply_held");
      return Err(WebReplyError::refused(StatusCode::CONFLICT, LANGUAGE_HOLD_DETAIL));
    }
    let answers = answered_input(&events, &running).map(|(_, user_event_id)| user_event_id);
    let mut web_reply = json!({"answers": answers, "chars": chars});
    if let Some(reason) = keep_language {
      web_reply["keep_language"] = json!(reason);
    }
    let mut event = json!({"type": WEB_REPLY, "content": text, WEB_REPLY: web_reply});
    if let Some(request_id) = request_id {
      event["id"] = json!(charliebot_store::control_events::stable_request_event_id(session_id, request_id));
      event["request_id"] = json!(request_id);
    }
    // Release the shared snapshot before append; Python appends into the cached list (`chat_events.py:221-224`).
    // Keeping this Arc alive would make Store::append_chat_event deep-clone the entire event vector through Arc::make_mut.
    drop(events);
    features.store.append_chat_event(session_id, event).await?
  };
  features.store.announce_appended_event(session_id, &event, epoch).await?;
  features.store.mark_unread(session_id).await?;
  let over_budget = chars > REPLY_BUDGET_CHARS;
  let answers = event[WEB_REPLY]["answers"].to_value();
  tracing::info!(session = %session_id, chars, over_budget, answers = ?answers, "web_reply_posted");
  Ok(json!({"posted": true, "chars": chars, "over_budget": over_budget, "answers": answers}))
}

fn web_reply_receipt(event: &Value) -> Value {
  let reply = event.get(WEB_REPLY).expect("a stored web reply contains its payload");
  let chars = reply.get("chars").and_then(Value::as_u64).expect("a stored web reply contains its character count");
  json!({
    "posted": true,
    "chars": chars,
    "over_budget": chars as usize > REPLY_BUDGET_CHARS,
    "answers": reply["answers"],
  })
}

/// The For-you layer: the user's messages, the replies and the notices, in log order, as `{role, content}` rows
/// (`reply.for_you_entries`).
#[cfg(test)]
pub fn for_you_entries(store: &Store, events: &[Value]) -> Result<Vec<Value>, WebReplyError> {
  let compact = events.iter().map(ChatEvent::from_value).collect::<Vec<_>>();
  let messages = store.events_to_messages(&compact)?;
  Ok(
    messages
      .into_iter()
      .filter(|message| message.get("role").and_then(Value::as_str).is_some_and(|role| FOR_YOU_ROLES.contains(&role)))
      .map(|message| json!({"role": message["role"].clone(), "content": message["content"].clone()}))
      .collect(),
  )
}

/// Build the For-you layer from the shared parsed-event cache.
pub fn for_you_entries_shared(store: &Store, events: &[SharedChatEvent]) -> Result<Vec<Value>, WebReplyError> {
  let messages = store.events_to_messages_shared(events)?;
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
  use std::fs;
  use std::time::Duration;

  use axum::http::Method;
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::{SIDEBAR_CHANNEL, Subscription};

  use super::*;
  use crate::test_support::{Home, id_of, user_event};

  #[tokio::test]
  async fn event_readers_share_the_store_snapshot() {
    let home = Home::new();
    let session_id = home.manager("shared-events", None).await;
    home.append(&session_id, user_event("first event")).await;

    let first = load_events(&home.features, &session_id).await.expect("the event snapshot reads");
    let second = load_events(&home.features, &session_id).await.expect("the event snapshot reads again");

    assert!(Arc::ptr_eq(&first, &second), "both calls hold the same event allocation");
  }

  /// The web reply events of the session's log, as `(content, web_reply)` pairs.
  fn replies(home: &Home, session_id: &str) -> Vec<(Value, Value)> {
    home
      .events(session_id)
      .iter()
      .filter(|event| event["type"] == WEB_REPLY)
      .map(|event| (event["content"].clone(), event[WEB_REPLY].clone()))
      .collect()
  }

  async fn sidebar_frames(subscription: &mut Subscription) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(1), subscription.recv()).await {
      frames.push(serde_json::from_str(&frame).expect("a sidebar frame is JSON"));
    }
    frames
  }

  #[tokio::test]
  async fn a_web_reply_records_the_text_and_reads_back_the_outcome() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let readback = post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

    assert_eq!(readback, json!({"posted": true, "chars": 10, "over_budget": false, "answers": null}));
    assert_eq!(replies(&home, &sid), vec![(json!("the answer"), json!({"answers": null, "chars": 10}))]);
    assert!(home.meta(&sid).await.has_unread);
  }

  #[tokio::test]
  async fn a_web_reply_broadcasts_only_the_unread_change_to_the_sidebar() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);

    post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

    assert_eq!(
      sidebar_frames(&mut sidebar).await,
      vec![json!({
        "type": "unread_changed",
        "session_id": sid,
        "has_unread": true,
      })]
    );
    assert_eq!(replies(&home, &sid).len(), 1);
  }

  #[tokio::test]
  async fn a_second_web_reply_while_unread_sends_no_sidebar_frame() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    post_web_reply(&home.features, &sid, "first answer", None).await.expect("the first reply posts");
    let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);

    post_web_reply(&home.features, &sid, "second answer", None).await.expect("the second reply posts");

    assert!(sidebar_frames(&mut sidebar).await.is_empty());
    assert_eq!(replies(&home, &sid).len(), 2);
  }

  // Python: tests/test_web_reply.py::test_markdown_link_labels_set_the_reply_count_and_replay_readback
  #[tokio::test]
  async fn markdown_link_reply_replay_returns_the_stored_visible_count() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let diff_url = format!("https://example.com/{}", "d".repeat(150 - "https://example.com/".len()));
    let pr_url = format!("https://example.com/{}", "p".repeat(183 - "https://example.com/".len()));
    let text = format!("{}[diff]({diff_url})[PR 页]({pr_url})", "x".repeat(200));
    let expected = json!({"posted": true, "chars": 208, "over_budget": false, "answers": null});

    let first = post_web_reply_with_request_id(&home.features, &sid, &text, None, Some("same-reply"))
      .await
      .expect("the first reply posts");
    let replay = post_web_reply_with_request_id(&home.features, &sid, &text, None, Some("same-reply"))
      .await
      .expect("the retry replays its reply");

    assert_eq!(first, replay);
    assert_eq!(first, expected);
    let replies = replies(&home, &sid);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].1["chars"], 208);
    let event = home.events(&sid).into_iter().find(|event| event["type"] == WEB_REPLY).unwrap();
    assert_eq!(event["request_id"], "same-reply");
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

      let error = post_web_reply(&home.features, target, text, None).await.expect_err("the reply is refused");

      match error {
        WebReplyError::Refused { status: code, .. } => assert_eq!(code.as_u16(), status, "case {case}"),
        other => panic!("case {case}: expected a refusal, got {other:?}"),
      }
      assert_eq!(home.events(&sid).len(), events_before, "case {case}");
      assert!(!home.meta(&sid).await.has_unread, "case {case}");
    }
  }

  #[tokio::test]
  async fn a_dead_file_link_refuses_a_web_reply_with_a_correct_link_and_records_nothing() {
    for form in ["retired", "local", "canonical"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let port = home.features.config.get().expect("the config loads").server.port;
      let session_dir = home.features.store.session_dir(&sid);
      let (link, detail) = match form {
        "retired" => {
          let target = home.root().join("sessions/synthetic-session/plan.html");
          fs::create_dir_all(target.parent().unwrap()).unwrap();
          fs::write(&target, "page").unwrap();
          let link = format!("https://charliebot.example:{port}/files/sessions/synthetic-session/plan.html");
          let correct = format!("https://charliebot.example:{port}/absolute_filepath{}", target.display());
          let detail = format!(
            "Dead file links; nothing was recorded.\n- {link}: /files/ is a retired prefix. Correct link: {correct}"
          );
          (link, detail)
        }
        "local" => {
          let correct = format!("/absolute_filepath{}", session_dir.join("artifacts/plan.html").display());
          let detail = format!(
            "Dead file links; nothing was recorded.\n- artifacts/plan.html: a local path opens nothing on the web page. \
             Correct link: {correct}"
          );
          ("[plan](artifacts/plan.html)".to_string(), detail)
        }
        _ => {
          let missing = home.root().join("missing.html");
          let link = format!("http://localhost:{port}/absolute_filepath{}", missing.display());
          let detail =
            format!("Dead file links; nothing was recorded.\n- {link}: no file exists at {}.", missing.display());
          (link, detail)
        }
      };
      let events_before = home.events(&sid).len();

      let (status, response) = post_reply(&home, &sid, &format!("See {link}"), None).await;

      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "form {form}");
      assert_eq!(response["detail"], json!(detail), "form {form}");
      assert_eq!(home.events(&sid).len(), events_before, "form {form}");
      assert!(replies(&home, &sid).is_empty(), "form {form}");
      assert!(!home.meta(&sid).await.has_unread, "form {form}");
    }
  }

  #[tokio::test]
  async fn a_live_file_link_posts_as_a_web_reply() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let port = home.features.config.get().expect("the config loads").server.port;
    let page = home.root().join("live.html");
    fs::write(&page, "page").unwrap();
    let text = format!("See http://localhost:{port}/absolute_filepath{}", page.display());

    let (status, _) = post_reply(&home, &sid, &text, None).await;

    assert_eq!(status, StatusCode::OK);
    let contents: Vec<Value> = replies(&home, &sid).into_iter().map(|(content, _)| content).collect();
    assert_eq!(contents, vec![json!(text)]);
  }

  #[tokio::test]
  async fn the_budget_flags_an_over_budget_reply_and_the_reply_still_posts() {
    for (chars, over_budget) in [(500, false), (501, true)] {
      let home = Home::new();
      let sid = home.manager("session", None).await;

      let readback = post_web_reply(&home.features, &sid, &"x".repeat(chars), None).await.expect("the reply posts");

      assert_eq!(readback, json!({"posted": true, "chars": chars, "over_budget": over_budget, "answers": null}));
      assert_eq!(replies(&home, &sid).len(), 1);
    }
  }

  // Python: tests/test_web_reply.py::test_markdown_link_labels_set_the_reply_count_and_replay_readback
  // Python: tests/test_web_reply.py::test_a_bare_url_counts_in_full_in_a_web_reply
  #[tokio::test]
  async fn a_markdown_link_counts_as_its_label_and_a_bare_url_counts_in_full() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let diff = format!("https://example.com/{}", "d".repeat(150 - "https://example.com/".len()));
    let pr = format!("https://example.com/{}", "p".repeat(183 - "https://example.com/".len()));
    let bare = format!(" See https://example.com/{}", "u".repeat(120));
    let text = format!("{}[diff]({diff})[PR 页]({pr}){bare}", "x".repeat(200));
    // The 200 body characters, the labels `diff` and `PR 页` (4 characters each), then the bare URL in full.
    let chars = 208 + bare.chars().count();

    let readback = post_web_reply(&home.features, &sid, &text, None).await.expect("the reply posts");

    assert_eq!(readback, json!({"posted": true, "chars": chars, "over_budget": false, "answers": null}));
    assert_eq!(replies(&home, &sid), vec![(json!(text), json!({"answers": null, "chars": chars}))]);
  }

  #[tokio::test]
  async fn a_reply_answers_the_user_message_that_woke_its_turn() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    home.set_running(&[id_of(&user)]);

    let readback = post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

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

    let readback = post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

    assert_eq!(readback["answers"], json!(id_of(&user)));
  }

  #[tokio::test]
  async fn a_reply_in_a_background_turn_answers_nothing() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let wake = home.append(&sid, json!({"type": "scheduled_trigger", "content": "wake"})).await;
    home.set_running(&[id_of(&wake)]);

    let readback = post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

    assert_eq!(readback["answers"], Value::Null);
  }

  #[tokio::test]
  async fn a_reply_answers_the_newest_input_in_log_order_not_in_input_order() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let first = home.append(&sid, user_event("first")).await;
    let second = home.append(&sid, user_event("second")).await;
    home.set_running(&[id_of(&second), id_of(&first)]);

    let readback = post_web_reply(&home.features, &sid, "the answer", None).await.expect("the reply posts");

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
    assert_eq!(nudge_of(&nudge).as_deref(), Some("u1"));
    assert_eq!(nudge_of(&json!({"type": USER, "web_reply": {"nudge_of": "u1"}})), None);
    assert_eq!(nudge_of(&json!({"type": AGENT_MESSAGE})), None);
  }

  #[test]
  fn empty_input_binding_visits_no_event_rows() {
    let event = ChatEvent::from_value(&user_event("the question"));
    let mut visited = 0;
    let events = std::iter::once(&event).inspect(|_| visited += 1);

    assert!(answered_input_from(events, &[]).is_none());
    assert_eq!(visited, 0);
  }

  /// A user message as the chat API writes it: the actor is the user.
  fn user_message(content: &str) -> Value {
    let mut event = user_event(content);
    event["actor"] = json!("user");
    event
  }

  /// Posts `text` through the web reply route, with `keep_language` in the body only when the case names a reason.
  async fn post_reply(home: &Home, session_id: &str, text: &str, keep_language: Option<&str>) -> (StatusCode, Value) {
    let mut body = json!({"session_id": session_id, "text": text});
    if let Some(reason) = keep_language {
      body["keep_language"] = json!(reason);
    }
    home.send(Method::POST, "/api/internal/web/reply", Some(body)).await
  }

  const CHINESE_MESSAGE: &str = "这个改动先别合入，我想再看一下状态面板的折叠逻辑。";
  const MANY_TERMS_MESSAGE: &str =
    concat!("这个 PR 的 CI 挂了，pytest 的 log 里有 Kubernetes pod 的 timeout，", "你看一下 rebase 之后还在不在。");
  const ARTIFACT_COMMENT_MESSAGE: &str = concat!(
    "[Artifact comments · plan_32.html] (1)\n\n",
    "1. ▸ \"Keep the phone status toggle sticky while scrolling\"\n",
    "   ↳ 这一条为什么要改？先说明理由再动手。"
  );
  const ENGLISH_REPLY: &str = "I will hold the merge until the review is done.";

  /// A Chinese reply whose fenced block holds twenty code names, which the language check does not count.
  fn code_heavy_reply() -> String {
    let names = vec!["ChatPanelStatusToggle"; 20].join(" ");
    format!("改好了，入口在下面：\n\n```\n{names}\n```\n测试全部通过。")
  }

  #[tokio::test]
  async fn an_english_reply_after_a_chinese_message_is_held_and_records_nothing() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_message(CHINESE_MESSAGE)).await;
    let events_before = home.events(&sid).len();

    let (status, response) = post_reply(&home, &sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(response["detail"], json!(LANGUAGE_HOLD_DETAIL));
    assert_eq!(home.events(&sid).len(), events_before);
    assert!(!home.meta(&sid).await.has_unread);
  }

  #[tokio::test]
  async fn a_reply_with_a_keep_language_reason_posts_as_written_and_records_the_reason() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_message(CHINESE_MESSAGE)).await;
    let reason = "The reply quotes an English term.";

    let (status, readback) = post_reply(&home, &sid, ENGLISH_REPLY, Some(reason)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
    assert_eq!(
      replies(&home, &sid),
      vec![(
        json!(ENGLISH_REPLY),
        json!({"answers": null, "chars": ENGLISH_REPLY.chars().count(), "keep_language": reason}),
      )]
    );
  }

  #[tokio::test]
  async fn an_empty_keep_language_reason_is_refused_and_records_nothing() {
    for reason in ["", "  "] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      home.append(&sid, user_message(CHINESE_MESSAGE)).await;
      let events_before = home.events(&sid).len();

      let (status, detail) = post_reply(&home, &sid, ENGLISH_REPLY, Some(reason)).await;

      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "reason {reason:?}");
      assert_eq!(detail["detail"], json!("Keep-language reason is empty"), "reason {reason:?}");
      assert_eq!(home.events(&sid).len(), events_before, "reason {reason:?}");
    }
  }

  #[tokio::test]
  async fn an_english_reply_after_a_chinese_message_with_english_inside_it_is_held() {
    for message in [MANY_TERMS_MESSAGE, ARTIFACT_COMMENT_MESSAGE] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      home.append(&sid, user_message(message)).await;

      let (status, response) = post_reply(&home, &sid, ENGLISH_REPLY, None).await;

      assert_eq!(status, StatusCode::CONFLICT, "message {message:?}");
      assert_eq!(response["detail"], json!(LANGUAGE_HOLD_DETAIL), "message {message:?}");
    }
  }

  #[tokio::test]
  async fn an_english_reply_after_an_english_message_posts_even_after_a_chinese_one() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_message(CHINESE_MESSAGE)).await;
    home.append(&sid, user_message("Please rerun the suite and report the failures.")).await;

    let (status, readback) = post_reply(&home, &sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  #[tokio::test]
  async fn a_chinese_reply_with_many_code_names_posts() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_message(CHINESE_MESSAGE)).await;

    let (status, readback) = post_reply(&home, &sid, &code_heavy_reply(), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  #[tokio::test]
  async fn a_reply_in_a_session_without_a_user_message_posts() {
    let home = Home::new();
    let sid = home.manager("session", None).await;

    let (status, readback) = post_reply(&home, &sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  /// Latin letters inside inline code, a URL and a path are not counted, so the Han share is 1 and the reply posts.
  /// Each span holds over twenty Latin letters per Han character, so one span left in the count holds the reply.
  #[tokio::test]
  async fn a_reply_whose_latin_letters_sit_in_code_urls_and_paths_posts() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_message(CHINESE_MESSAGE)).await;
    let reply = concat!(
      "好。`ChatPanelStatusToggleWithLongName` ",
      "https://example.com/docs/PanelGuideAndMore ",
      "/home/example/worktrees/panel-guide-review"
    );

    let (status, readback) = post_reply(&home, &sid, reply, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  #[tokio::test]
  async fn a_path_after_python_nonword_alphanumerics_is_removed() {
    for marker in ["ⓐ", "\u{0345}"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      home.append(&sid, user_message(CHINESE_MESSAGE)).await;
      let reply = format!("好。{marker}/ChatPanelStatusToggleWithLongName");

      let (status, _) = post_reply(&home, &sid, &reply, None).await;

      assert_eq!(status, StatusCode::OK, "reply {reply:?}");
    }
  }

  #[tokio::test]
  async fn a_python_nonword_alphanumeric_stops_a_path_run() {
    for marker in ["ⓐ", "\u{0345}"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      home.append(&sid, user_message(CHINESE_MESSAGE)).await;
      let reply = format!("好。/{marker}ChatPanelStatusToggleWithLongName");

      let (status, _) = post_reply(&home, &sid, &reply, None).await;

      assert_eq!(status, StatusCode::CONFLICT, "reply {reply:?}");
    }
  }
}
