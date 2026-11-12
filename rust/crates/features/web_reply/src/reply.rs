//! The web reply record and the web-session test: the refusals, the language check, the For-you layer and the
//! readback. The round-end audit that nudges and notices over these records is in `audit.rs`.
//!
//! The web session test is chat_threads' `thread_sessions::is_thread_session`, restated over the same fields: this
//! package does not depend on chat_threads.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use anyhow::Context;
use axum::http::StatusCode;
use charliebot_compat::file_urls;
use charliebot_core::event_types::EventFields;
use charliebot_core::event_types::{
  AGENT_MESSAGE, INPUT_EVENT_ID, NUDGE_OF, USER, WEB_REPLY, WEB_REPLY_CHECK, WEB_REPLY_NOTICE,
};
use charliebot_core::paths;
use charliebot_core::regex_classes::compile_automata_regex;
use charliebot_core::str_methods;
use charliebot_registry::{FeatureContext, StateError};
#[cfg(test)]
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::reply_order::{self, Receipt};
use charliebot_store::{SESSION_NOT_FOUND_DETAIL, SharedChatEvent, Store, StoreError};
use charliebot_user_text_checks::{self as user_text_checks, UserTextChecks, UserTextChecksConfig};
use regex::{Captures, Regex};
use serde_json::{Value, json};

/// The web reply rules file under `prompts/`.
pub const RULES_FILE: &str = "web_reply.md";

/// The audit's nudge for the unanswered user message `message`.
pub fn nudge_text(message: &str) -> String {
  let opening = "Web reply nudge: your last turn woke on a user message and ended without a web reply. \
                 The For-you view hides your turn output. ";
  format!("{opening}{}", answer_request(message))
}

/// The sentences that ask for the web reply to the unanswered user message `message`, named by its opening, and how a
/// long answer reaches the For-you view.
pub fn answer_request(message: &str) -> String {
  format!(
    "The unanswered message begins: \"{}\". Post your answer as the web reply now. \
     For an answer over 500 characters, write the whole answer on a page first. \
     A reply over 500 characters is over budget, so the page keeps the full answer. \
     Then post the conclusion and the page link as the web reply. \
     The For-you view then shows the conclusion, and the link leads to the full answer.",
    message_opening(message)
  )
}

/// The opening of a user message that a reminder names: its first 60 characters.
pub fn message_opening(text: &str) -> String {
  text.chars().take(60).collect()
}

/// The notice the user reads when the nudge turn also ended without a reply.
pub const NOTICE_TEXT: &str = "No reply was posted for this message. The Everything view shows this turn's output.";

/// The sender name the nudge carries in the chat log.
pub const NUDGE_SENDER_NAME: &str = "Web reply";

/// The reply budget the readback's `over_budget` flag measures; an over-budget reply still posts.
pub const REPLY_BUDGET_CHARS: usize = 500;

/// The language check holds a reply when the answered user message has at least this many Han characters.
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

static CODE_FENCE_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"(?s)```.*?```"));
static INLINE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"`[^`]*`"));
static URL_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"\(?https?://\S+\)?"));
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|[^\p{L}\p{N}_])([~/][\p{L}\p{N}_./-]+)").unwrap());
static MARKDOWN_LINK_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"\[([^\]]*)\]\([^)]*\)"));
static HAN_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"[㐀-䶿一-鿿]"));
static LATIN_RE: LazyLock<Regex> = LazyLock::new(|| compile_automata_regex(r"[A-Za-z]"));

/// The roles the For-you layer shows: the user's messages, the replies and the notices.
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

/// True for a manager session that no Slack or Discord thread owns.
pub fn is_web_session(meta: &SessionMetadata) -> bool {
  charliebot_tasks::midturn_delivery::is_web_session(meta)
}

/// The manager session named by `session_id`, web or thread: 404 for an unknown session, 409 for a worker.
pub async fn require_manager_session(
  features: &FeatureContext,
  session_id: &str,
) -> Result<SessionMetadata, WebReplyError> {
  let meta = features
    .store
    .get_session(session_id)
    .await?
    .ok_or_else(|| WebReplyError::refused(StatusCode::NOT_FOUND, SESSION_NOT_FOUND_DETAIL))?;
  if meta.profile != TaskProfile::Manager {
    return Err(WebReplyError::refused(StatusCode::CONFLICT, "Session is not a manager: it is a worker"));
  }
  Ok(meta)
}

/// The web session named by `session_id`: 404 for an unknown session, 409 for a thread
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

/// The session's chat events in log order, read off the runtime's worker threads.
pub async fn load_events(features: &FeatureContext, session_id: &str) -> anyhow::Result<Arc<Vec<SharedChatEvent>>> {
  let store = Arc::clone(&features.store);
  let session_id = session_id.to_string();
  let loaded = tokio::task::spawn_blocking(move || store.load_chat_events_shared_sync(&session_id))
    .await
    .context("the chat-event read stopped")?;
  Ok(loaded?)
}

/// The nudge's `nudge_of`: the user message an audit nudge was posted for, or `None` for any other event.
pub fn nudge_of(event: &impl EventFields) -> Option<String> {
  if event.field("type").and_then(EventFields::as_str_field) != Some(AGENT_MESSAGE) {
    return None;
  }
  event
    .field(WEB_REPLY)
    .and_then(|record| record.field(NUDGE_OF))
    .and_then(EventFields::as_str_field)
    .map(str::to_string)
}

/// The user message that an input answers: the message itself, or the message its nudge was posted for; `None` for any
/// other event.
pub fn answered_user_id(event: &impl EventFields) -> Option<String> {
  if event.field("type").and_then(EventFields::as_str_field) == Some(USER) {
    return event.field("id").and_then(EventFields::as_str_field).map(str::to_string);
  }
  nudge_of(event)
}

/// The event with `id` in the log.
fn event_by_id<'a>(events: &'a [SharedChatEvent], id: &str) -> Option<&'a CompactValue> {
  events.iter().map(SharedChatEvent::as_ref).find(|event| event.get("id").and_then(CompactValue::as_str) == Some(id))
}

/// The text of every user message the turn whose `receipts` these are received, in receipt order, joined by line breaks.
/// A receipt of a nudge contributes the user message the nudge was posted for. `None` when the turn received no user
/// message; a receipt that names no event of the log is an error.
fn received_user_texts(events: &[SharedChatEvent], receipts: &[Receipt]) -> Result<Option<String>, WebReplyError> {
  let mut seen = HashSet::new();
  let mut texts = Vec::new();
  for receipt in receipts {
    let input = event_by_id(events, &receipt.input_event_id).ok_or_else(|| {
      WebReplyError::Failed(anyhow::anyhow!(
        "run {} claims the input {} that the session log does not hold",
        receipt.run_id,
        receipt.input_event_id
      ))
    })?;
    let Some(user_event_id) = answered_user_id(input) else {
      continue;
    };
    if !seen.insert(user_event_id.clone()) {
      continue;
    }
    let user_event = event_by_id(events, &user_event_id).ok_or_else(|| {
      WebReplyError::Failed(anyhow::anyhow!("the turn's input names the missing user message {user_event_id}"))
    })?;
    if let Some(text) = user_event.get("content").and_then(CompactValue::as_str) {
      texts.push(text.to_string());
    }
  }
  Ok((!texts.is_empty()).then(|| texts.join("\n")))
}

/// Whether the log holds a web reply later than `receipt`: the order rule's answer test for a received user message.
pub fn replied(receipt: &Receipt, events: &[SharedChatEvent]) -> Result<bool, StoreError> {
  reply_order::answered(receipt, events, WEB_REPLY)
}

/// Whether the log holds the nudge posted for `user_event_id`.
pub fn nudged(events: &[SharedChatEvent], user_event_id: &str) -> bool {
  events.iter().any(|event| nudge_of(event.as_ref()).as_deref() == Some(user_event_id))
}

/// Whether the log holds the no-reply notice for `user_event_id`.
pub fn noticed(events: &[SharedChatEvent], user_event_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(CompactValue::as_str) == Some(WEB_REPLY_NOTICE)
      && event.get(WEB_REPLY_NOTICE).and_then(|record| record.get(INPUT_EVENT_ID)).and_then(CompactValue::as_str)
        == Some(user_event_id)
  })
}

/// The Han and Latin-letter counts of `text`, with fenced code, inline code, URLs and paths left out.
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

/// Whether the language check holds `reply` after the answered user messages' text `user_text`, where None means no
/// answered user message (`language_check_holds`). A reply with no Han and no Latin letters is not held.
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

/// The paths that resolve the file links of a web message in `session_id`: this server's port, the CharlieBot home, the
/// session directory and the user's home.
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

/// Record `text` as the session's web reply and return the readback the CLI prints.
///
/// Accepted text is recorded exactly as written. The language check compares it with the text of every user message the
/// running turn received, replies posted earlier in the turn included. The check holds the text unless `keep_language`,
/// the agent's reason for posting it as written, is given; the reason is then recorded on the reply. User-text refusals
/// give a correction and its purpose so the writer can fix the text. A refusal records nothing and marks nothing.
pub const REPLY_SHA256: &str = "reply_sha256";

/// The SHA-256 that names a web reply text as it stands.
pub fn reply_sha256(text: &str) -> String {
  charliebot_store::control_events::sha256_bytes_hex(text.as_bytes())
}

/// The findings of a recorded `web_reply_check` event for this reply attempt, if any check has been recorded.
pub async fn checked_reply_findings(
  features: &FeatureContext,
  session_id: &str,
  digest: &str,
  run_id: Option<&str>,
) -> Result<Option<Vec<String>>, WebReplyError> {
  let events = load_events(features, session_id).await?;
  let reply_index = run_id.map(|run_id| run_reply_index(events.iter().map(SharedChatEvent::as_ref), run_id));
  for event in events.iter().rev() {
    let matches = match (run_id, reply_index) {
      (Some(run_id), Some(reply_index)) => {
        event.get("run_id").and_then(CompactValue::as_str) == Some(run_id)
          && event.get("reply_index").and_then(CompactValue::as_u64) == Some(reply_index as u64)
      }
      (None, None) => event.get(REPLY_SHA256).and_then(CompactValue::as_str) == Some(digest),
      _ => unreachable!("a reply index is present exactly when a run id is present"),
    };
    if event.get("type").and_then(CompactValue::as_str) == Some(WEB_REPLY_CHECK) && matches {
      let findings = event
        .get("findings")
        .and_then(CompactValue::as_array)
        .map(|arr| arr.iter().filter_map(CompactValue::as_str).map(str::to_string).collect())
        .unwrap_or_default();
      return Ok(Some(findings));
    }
  }
  Ok(None)
}

/// Return whether `text` has no matching cold-read check in the session yet, running code assertions if unread.
pub async fn unread_reply(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  run_id: Option<&str>,
  request_id: Option<&str>,
) -> Result<Value, WebReplyError> {
  require_web_session(features, session_id).await?;
  if str_methods::strip(text).is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Reply text is empty"));
  }
  if let Some(request_id) = request_id {
    let event_id = charliebot_store::control_events::stable_request_event_id(session_id, request_id);
    let events = load_events(features, session_id).await?;
    let existing = charliebot_store::control_events::replay_request_event(
      events.iter().map(SharedChatEvent::as_ref),
      &event_id,
      request_id,
      WEB_REPLY,
      text,
      None,
    )?;
    if existing.is_some() {
      return Ok(json!({ "unread": false, "remaining_findings": [] }));
    }
  }
  let digest = reply_sha256(text);
  if let Some(findings) = checked_reply_findings(features, session_id, &digest, run_id).await? {
    return Ok(json!({ "unread": false, "remaining_findings": findings }));
  }
  let file_scan_context = file_link_scan_context(features, session_id)?;
  refuse_dead_links([text], &file_scan_context)?;
  let config = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  let checks = UserTextChecks::new(
    config.section::<UserTextChecksConfig>(user_text_checks::CONFIG_SECTION),
    &config.charliebot_home,
  )?;
  let violations = checks.check_reply(text, &file_scan_context)?;
  let code_findings: Vec<String> = violations
    .into_iter()
    .map(|v| match v.check {
      user_text_checks::COMMIT_NAMED => format!("bare commit hash {} stands without a readable name", v.snippet),
      user_text_checks::VOCABULARY_RULE_TERM => format!("rule-defined agent-work term '{}'", v.snippet),
      _ => format!("{}: {}", v.check, v.snippet),
    })
    .collect();
  Ok(json!({ "unread": true, "code_findings": code_findings }))
}

/// Record one cold read of `text` with its findings.
pub async fn record_reply_check(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  backend: &str,
  findings: &[String],
  answer: Option<&str>,
  run_id: Option<&str>,
) -> Result<Value, WebReplyError> {
  require_web_session(features, session_id).await?;
  let digest = reply_sha256(text);
  let reply_index = if let Some(run_id) = run_id {
    let events = load_events(features, session_id).await?;
    Some(run_reply_index(events.iter().map(SharedChatEvent::as_ref), run_id))
  } else {
    None
  };
  let mut event = json!({
    "type": WEB_REPLY_CHECK,
    "content": "web reply check",
    REPLY_SHA256: &digest,
    "backend": backend,
    "findings": findings,
  });
  if let (Some(run_id), Some(reply_index)) = (run_id, reply_index) {
    event["run_id"] = json!(run_id);
    event["reply_index"] = json!(reply_index);
  }
  if let Some(answer) = answer {
    event["answer"] = json!(answer);
  }
  features.store.persist_and_broadcast(session_id, event).await?;
  tracing::info!(session = %session_id, backend, count = findings.len(), "web_reply_check_recorded");
  Ok(json!({ REPLY_SHA256: digest }))
}

/// The number of replies already posted by `run_id`, which identifies the next reply within the run.
fn run_reply_index<'a, E: EventFields + 'a>(events: impl Iterator<Item = &'a E>, run_id: &str) -> usize {
  events
    .filter(|event| {
      event.field("type").and_then(EventFields::as_str_field) == Some(WEB_REPLY)
        && event.field("run_id").and_then(EventFields::as_str_field) == Some(run_id)
    })
    .count()
}

#[cfg(test)]
pub async fn post_web_reply(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  keep_language: Option<&str>,
) -> Result<Value, WebReplyError> {
  post_web_reply_with_identity(features, session_id, text, keep_language, None, None).await
}

/// Record a web reply for one caller request, bounding the cold-read send-back to one attempt within that run reply.
pub async fn post_web_reply_with_identity(
  features: &FeatureContext,
  session_id: &str,
  text: &str,
  keep_language: Option<&str>,
  request_id: Option<&str>,
  run_id: Option<&str>,
) -> Result<Value, WebReplyError> {
  require_web_session(features, session_id).await?;
  if str_methods::strip(text).is_empty() {
    return Err(WebReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Reply text is empty"));
  }
  let file_scan_context = file_link_scan_context(features, session_id)?;
  refuse_dead_links([text], &file_scan_context)?;
  let config = features.config.get().map_err(|error| WebReplyError::Failed(error.into()))?;
  let checks = UserTextChecks::new(
    config.section::<UserTextChecksConfig>(user_text_checks::CONFIG_SECTION),
    &config.charliebot_home,
  )?;
  let violations = checks.check_reply(text, &file_scan_context)?;
  let digest = reply_sha256(text);
  let prior_check = checked_reply_findings(features, session_id, &digest, run_id).await?;
  let remaining_findings = if let Some(checked_findings) = prior_check {
    let mut findings = checked_findings;
    for v in &violations {
      let code_finding = match v.check {
        user_text_checks::COMMIT_NAMED => format!("bare commit hash {} stands without a readable name", v.snippet),
        user_text_checks::VOCABULARY_RULE_TERM => format!("rule-defined agent-work term '{}'", v.snippet),
        _ => format!("{}: {}", v.check, v.snippet),
      };
      if !findings.contains(&code_finding) {
        findings.push(code_finding);
      }
    }
    (!findings.is_empty()).then_some(findings)
  } else {
    if !violations.is_empty() {
      return Err(WebReplyError::refused(
        StatusCode::UNPROCESSABLE_ENTITY,
        user_text_checks::reply_refusal_detail(&violations),
      ));
    }
    None
  };
  if keep_language.is_some_and(|reason| str_methods::strip(reason).is_empty()) {
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
    let receipts = reply_order::running_receipts(&features.store, session_id, &events)?;
    let received = received_user_texts(&events, &receipts)?;
    if keep_language.is_none() && language_check_holds(received.as_deref(), text)? {
      tracing::info!(session = %session_id, chars, "web_reply_held");
      return Err(WebReplyError::refused(StatusCode::CONFLICT, LANGUAGE_HOLD_DETAIL));
    }
    let mut web_reply = json!({"chars": chars});
    if let Some(reason) = keep_language {
      web_reply["keep_language"] = json!(reason);
    }
    if let Some(findings) = &remaining_findings {
      web_reply["findings"] = json!(findings);
    }
    let mut event = json!({"type": WEB_REPLY, "content": text, WEB_REPLY: web_reply});
    if let Some(request_id) = request_id {
      event["id"] = json!(charliebot_store::control_events::stable_request_event_id(session_id, request_id));
      event["request_id"] = json!(request_id);
    }
    if let Some(run_id) = run_id {
      event["run_id"] = json!(run_id);
    }
    // Release the shared snapshot before append; the store appends into the cached list.
    // Keeping this Arc alive would make Store::append_chat_event deep-clone the entire event vector through Arc::make_mut.
    drop(events);
    features.store.append_chat_event(session_id, event).await?
  };
  features.store.announce_appended_event(session_id, &event, epoch).await?;
  features.store.mark_unread(session_id).await?;
  let over_budget = chars > REPLY_BUDGET_CHARS;
  tracing::info!(session = %session_id, chars, over_budget, "web_reply_posted");
  Ok(json!({"posted": true, "chars": chars, "over_budget": over_budget}))
}

fn web_reply_receipt(event: &Value) -> Value {
  let reply = event.get(WEB_REPLY).expect("a stored web reply contains its payload");
  let chars = reply.get("chars").and_then(Value::as_u64).expect("a stored web reply contains its character count");
  json!({
    "posted": true,
    "chars": chars,
    "over_budget": chars as usize > REPLY_BUDGET_CHARS,
  })
}

/// The For-you layer: the user's messages, the replies and the notices, in log order, as `{role, content}` rows.
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
  use crate::test_support::{Home, id_of, user_event, user_event_with_id};

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

  fn seed_web_session(home: &Home, session_id: &str, events: &[Value]) {
    let metadata = SessionMetadata::new(session_id, "Synthetic web session", TaskProfile::Manager);
    let metadata_path = home.features.store.metadata_path(session_id);
    fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    fs::write(&metadata_path, metadata.to_json_string(&home.features.registry.metadata_slots(), true)).unwrap();

    let events_path = home.features.store.chat_events_path(session_id);
    fs::create_dir_all(events_path.parent().unwrap()).unwrap();
    let mut lines = String::new();
    for event in events {
      lines.push_str(&event.to_string());
      lines.push('\n');
    }
    fs::write(events_path, lines).unwrap();
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

    assert_eq!(readback, json!({"posted": true, "chars": 10, "over_budget": false}));
    assert_eq!(replies(&home, &sid), vec![(json!("the answer"), json!({"chars": 10}))]);
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

  // The replay readback pins the stored visible count, not the label count.
  #[tokio::test]
  async fn markdown_link_reply_replay_returns_the_stored_visible_count() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let diff_url = format!("https://example.com/{}", "d".repeat(150 - "https://example.com/".len()));
    let pr_url = format!("https://example.com/{}", "p".repeat(183 - "https://example.com/".len()));
    let text = format!("{}[diff]({diff_url})[PR 页]({pr_url})", "x".repeat(200));
    let expected = json!({"posted": true, "chars": 208, "over_budget": false});

    let first = post_web_reply_with_identity(&home.features, &sid, &text, None, Some("same-reply"), None)
      .await
      .expect("the first reply posts");
    let replay = post_web_reply_with_identity(&home.features, &sid, &text, None, Some("same-reply"), None)
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
  async fn a_checked_reply_gets_one_sendback_even_after_its_text_changes() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let run_id = "reply-run";
    let first_text = "已完成部署。";
    let revised_text = "部署已经完成。";
    let findings = vec!["deployment name is unclear".to_string()];

    record_reply_check(&home.features, &sid, first_text, "probe", &findings, Some("(1) unclear"), Some(run_id))
      .await
      .expect("the failed cold read is recorded");
    let request_id = "checked-reply";
    let unread = unread_reply(&home.features, &sid, revised_text, Some(run_id), Some(request_id))
      .await
      .expect("the revised submission reads the existing attempt");

    assert_eq!(unread["unread"], json!(false));
    assert_eq!(unread["remaining_findings"], json!(findings));

    post_web_reply_with_identity(&home.features, &sid, revised_text, None, Some(request_id), Some(run_id))
      .await
      .expect("the revised second submission posts");
    let posted = home.events(&sid).into_iter().find(|event| event["type"] == WEB_REPLY).unwrap();
    assert_eq!(posted["content"], revised_text);
    assert_eq!(posted["run_id"], run_id);
    assert_eq!(posted[WEB_REPLY]["findings"], json!(findings));

    let retry = unread_reply(&home.features, &sid, revised_text, Some(run_id), Some(request_id))
      .await
      .expect("an already posted reply does not cold-read again");
    assert_eq!(retry["unread"], json!(false));

    let next_reply = unread_reply(&home.features, &sid, "修复已经完成。", Some(run_id), None)
      .await
      .expect("the next reply starts a new check");
    assert_eq!(next_reply["unread"], json!(true));
    let next_run = unread_reply(&home.features, &sid, revised_text, Some("next-run"), None)
      .await
      .expect("a later run starts a new check");
    assert_eq!(next_run["unread"], json!(true));
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

  async fn assert_user_text_refusal(home: &Home, sid: &str, text: &str, check: &str, snippet: &str, fix: &str) {
    let events_before = home.events(sid);

    let (status, response) = post_reply(home, sid, text, None).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let detail = response["detail"].as_str().expect("the refusal has text detail");
    let lines = detail.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{detail}");
    assert!(lines[0].starts_with(&format!("{check}:")), "{detail}");
    assert!(lines[0].contains(snippet), "{detail}");
    assert!(lines[0].contains(fix), "{detail}");
    assert_eq!(home.events(sid), events_before);
    assert!(replies(home, sid).is_empty());
    assert!(!home.meta(sid).await.has_unread);
  }

  #[tokio::test]
  async fn an_internal_id_without_a_nearby_linear_id_refuses_a_web_reply() {
    let home = Home::with_config("user_text_checks:\n  internal_id_patterns: ['zz\\d{3}']\n");
    let sid = home.manager("session", None).await;

    assert_user_text_refusal(
      &home,
      &sid,
      "The run is zz123.",
      "tracker-id-nearby",
      "zz123",
      "write the Linear id right next to the internal id",
    )
    .await;
  }

  #[tokio::test]
  async fn an_unlinked_session_id_refuses_a_web_reply() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let text = format!("The session is {sid}.");

    assert_user_text_refusal(&home, &sid, &text, "session-linked", &sid, "write the id as").await;
  }

  #[tokio::test]
  async fn a_raw_tsv_link_refuses_a_web_reply() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let text = "[table](https://example.invalid/export.TSV?download=1)";

    assert_user_text_refusal(
      &home,
      &sid,
      text,
      "link-renderable",
      "export.TSV",
      "link an HTML page, or the directory that holds the file",
    )
    .await;
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
    let home = Home::new();
    let sid = home.manager("session", None).await;
    for (index, (chars, over_budget)) in [(500, false), (501, true)].into_iter().enumerate() {
      let readback = post_web_reply(&home.features, &sid, &"x".repeat(chars), None).await.expect("the reply posts");

      assert_eq!(readback, json!({"posted": true, "chars": chars, "over_budget": over_budget}));
      assert_eq!(replies(&home, &sid).len(), index + 1);
    }
  }

  // A markdown link counts as its label, and a bare URL counts in full in the reply.
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

    assert_eq!(readback, json!({"posted": true, "chars": chars, "over_budget": false}));
    assert_eq!(replies(&home, &sid), vec![(json!(text), json!({"chars": chars}))]);
  }

  /// A turn whose run record holds the receipts but whose `master_done` input ids were dropped by a restart is judged
  /// from those receipts: the language check still compares the Chinese message the resumed turn received.
  #[tokio::test]
  async fn a_web_reply_on_a_resumed_turn_compares_the_messages_its_run_received() {
    let home = Home::reply_only();
    let sid = "session";
    let chinese = user_event_with_id("chinese-message", CHINESE_MESSAGE);
    seed_web_session(&home, sid, &[chinese]);
    home.launch_run(sid, "resumed-run", &["chinese-message"], "2026-01-01T00:00:00Z");
    let events_before = home.events(sid).len();

    let (status, response) = post_reply(&home, sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(response["detail"], json!(LANGUAGE_HOLD_DETAIL));
    assert_eq!(home.events(sid).len(), events_before);
  }

  /// A session whose newest manager run ended has no running turn, so the language check has no received message to
  /// compare.
  #[tokio::test]
  async fn a_web_reply_after_the_running_turn_ended_compares_no_messages() {
    let home = Home::reply_only();
    let sid = "session";
    seed_web_session(&home, sid, &[user_event_with_id("chinese-message", CHINESE_MESSAGE)]);
    home.launch_run(sid, "run-1", &["chinese-message"], "2026-01-01T00:00:00Z");
    home.end_run(sid, "run-1", "2026-01-02T00:00:00Z");

    let (status, readback) = post_reply(&home, sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  #[tokio::test]
  async fn the_for_you_layer_keeps_user_messages_replies_and_notices_in_log_order() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    home.append(&sid, user_event("first")).await;
    home.append(&sid, json!({"type": "scheduled_trigger", "content": "wake"})).await;
    home.append(&sid, json!({"type": WEB_REPLY, "content": "answer", WEB_REPLY: {"chars": 6}})).await;
    home.append(&sid, json!({"type": AGENT_MESSAGE, "content": nudge_text("u2"), WEB_REPLY: {"nudge_of": "u2"}})).await;
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
    charliebot_registry::thread_origin::register_origin_field("slack_origin");
    charliebot_registry::thread_origin::register_origin_field("discord_origin");
    charliebot_registry::thread_origin::register_origin_field("feishu_origin");
    let origin = json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"});
    let mut slack = SessionMetadata::new("s", "s", TaskProfile::Manager);
    slack.extra.insert("slack_origin".to_string(), origin);
    let mut discord = SessionMetadata::new("s", "s", TaskProfile::Manager);
    discord.extra.insert("discord_origin".to_string(), json!({"guild_id": "G1"}));
    let mut feishu = SessionMetadata::new("s", "s", TaskProfile::Manager);
    feishu.extra.insert("feishu_origin".to_string(), json!({"tenant_key": "T1", "chat_id": "oc_1", "root_id": "om_1"}));

    assert!(is_web_session(&SessionMetadata::new("s", "s", TaskProfile::Manager)));
    assert!(!is_web_session(&slack));
    assert!(!is_web_session(&discord));
    assert!(!is_web_session(&feishu));
    assert!(!is_web_session(&SessionMetadata::new("s", "s", TaskProfile::Worker)));
  }

  #[test]
  fn strip_trims_the_information_separators() {
    assert_eq!(str_methods::strip(" \u{1c}x\u{1f}\n"), "x");
    assert_eq!(str_methods::strip(" \t\n"), "");
  }

  #[test]
  fn the_nudge_of_reads_only_an_agent_message() {
    let nudge = json!({"type": AGENT_MESSAGE, "web_reply": {"nudge_of": "u1"}});
    assert_eq!(nudge_of(&nudge).as_deref(), Some("u1"));
    assert_eq!(nudge_of(&json!({"type": USER, "web_reply": {"nudge_of": "u1"}})), None);
    assert_eq!(nudge_of(&json!({"type": AGENT_MESSAGE})), None);
  }

  /// A user message as the chat API writes it: the actor is the user.
  fn user_message(content: &str) -> Value {
    let mut event = user_event(content);
    event["actor"] = json!("user");
    event
  }

  /// A start time before every event a test appends.
  const RUN_START: &str = "2026-01-01T00:00:00Z";

  /// Appends `message` as a user message that the session's running turn receives, and returns its id.
  async fn running_user_message(home: &Home, session_id: &str, message: Value) -> String {
    let id = id_of(&home.append(session_id, message).await).to_string();
    home.launch_run(session_id, "run-1", &[id.as_str()], RUN_START);
    id
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

  const CODE_HEAVY_REPLY: &str = concat!(
    "改好了，入口在下面：\n\n```\n",
    "ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ",
    "ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle\n",
    "ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ",
    "ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle ChatPanelStatusToggle\n",
    "```\n测试全部通过。"
  );

  #[tokio::test]
  async fn an_english_reply_after_a_chinese_message_is_held_and_records_nothing() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    running_user_message(&home, &sid, user_message(CHINESE_MESSAGE)).await;
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
    running_user_message(&home, &sid, user_message(CHINESE_MESSAGE)).await;
    let reason = "The reply quotes an English term.";

    let (status, readback) = post_reply(&home, &sid, ENGLISH_REPLY, Some(reason)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
    assert_eq!(
      replies(&home, &sid),
      vec![(json!(ENGLISH_REPLY), json!({"chars": ENGLISH_REPLY.chars().count(), "keep_language": reason}),)]
    );
  }

  #[tokio::test]
  async fn an_empty_keep_language_reason_is_refused_and_records_nothing() {
    for reason in ["", "  "] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      running_user_message(&home, &sid, user_message(CHINESE_MESSAGE)).await;
      let events_before = home.events(&sid).len();

      let (status, detail) = post_reply(&home, &sid, ENGLISH_REPLY, Some(reason)).await;

      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "reason {reason:?}");
      assert_eq!(detail["detail"], json!("Keep-language reason is empty"), "reason {reason:?}");
      assert_eq!(home.events(&sid).len(), events_before, "reason {reason:?}");
    }
  }

  #[test]
  fn english_terms_inside_chinese_user_messages_still_trigger_the_language_hold() {
    for message in [MANY_TERMS_MESSAGE, ARTIFACT_COMMENT_MESSAGE] {
      assert!(language_check_holds(Some(message), ENGLISH_REPLY).unwrap(), "message {message:?}");
    }
  }

  #[tokio::test]
  async fn an_english_reply_after_an_english_message_posts_even_after_a_chinese_one() {
    let home = Home::reply_only();
    let sid = "session";
    let mut chinese = user_message(CHINESE_MESSAGE);
    chinese["id"] = json!("chinese");
    let mut english = user_message("Please rerun the suite and report the failures.");
    english["id"] = json!("english");
    seed_web_session(&home, sid, &[chinese, english]);
    home.launch_run(sid, "run-1", &["english"], RUN_START);

    let (status, readback) = post_reply(&home, sid, ENGLISH_REPLY, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(readback["posted"], json!(true));
  }

  #[test]
  fn fenced_code_with_many_names_does_not_trigger_the_language_hold() {
    assert!(!language_check_holds(Some(CHINESE_MESSAGE), CODE_HEAVY_REPLY).unwrap());
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
    running_user_message(&home, &sid, user_message(CHINESE_MESSAGE)).await;
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
  async fn a_path_after_nonword_alphanumerics_is_removed() {
    let home = Home::reply_only();
    let sid = "session";
    let user = user_message(CHINESE_MESSAGE);
    let mut user = user;
    user["id"] = json!("path-user");
    seed_web_session(&home, sid, &[user]);
    home.launch_run(sid, "run-1", &["path-user"], RUN_START);
    let reply = "好。ⓐ/ChatPanelStatusToggleWithLongName";

    let (status, _) = post_reply(&home, sid, reply, None).await;

    assert_eq!(status, StatusCode::OK, "reply {reply:?}");
  }

  /// The route runs in-process and its file access stays in this test's TempDir-backed Home.
  /// With no other process or network in the call chain, plan 1 v5 classifies this as a unit test.
  #[tokio::test]
  async fn a_nonword_alphanumeric_stops_a_path_run() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    running_user_message(&home, &sid, user_message(CHINESE_MESSAGE)).await;
    let reply = "好。/ⓐChatPanelStatusToggleWithLongName";

    let (status, _) = post_reply(&home, &sid, reply, None).await;

    assert_eq!(status, StatusCode::CONFLICT, "reply {reply:?}");
  }

  #[test]
  fn a_nonword_alphanumeric_stops_a_path_run_in_the_language_check() {
    for marker in ["ⓐ", "\u{0345}"] {
      let reply = format!("好。/{marker}ChatPanelStatusToggleWithLongName");

      assert!(language_check_holds(Some(CHINESE_MESSAGE), &reply).unwrap(), "reply {reply:?}");
    }
  }
}
