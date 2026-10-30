//! The shared thread core (`src/features/chat_threads/thread_entry.py`): the platform description, the adapter surface,
//! the reply contract, and the entries the Slack and Discord listeners call. A listener builds one `ThreadPlatform`
//! from its constants, implements `ThreadAdapter` over its client, and calls these entries with the adapter.
//!
//! Public API, each item with its Python anchor (names in `thread_entry.py` unless a path is given):
//! - `ThreadPlatform` (`ThreadPlatform`): the per-platform facts; `notice_key`, `backfill_key` and `ack_event_type`
//!   derive the marker keys from `name`.
//! - `IdOrdering` (`ThreadPlatform.id_key`): text order for Slack ts strings, integer order for Discord snowflakes.
//! - `ThreadMessage` (`ThreadMessage`) and `ThreadAdapter` (`ThreadAdapter`): the message the round side reads, and the
//!   platform face of the entries.
//! - `ThreadReplyError` (`ThreadReplyError`) with `ThreadDetail`: a refusal with its HTTP status and detail; an
//!   unplanned failure answers 500 "Internal Server Error", as Python's unhandled exception does.
//! - `load_prompt_doc`, `thread_reply_contract`, `thread_reply_segment`: the reply contract (`LINKED_PAGES_LINE` joins
//!   it for every platform).
//! - Pure helpers: `follow_wake_label`, `follow_floor`, `chunk_text`, `event_by_id`, `summon_of`, `newest_thread_input`,
//!   `replied`, `nudged`, `noticed`, `lost_summons`, `unread_after`.
//! - Round side: `post_with_retry`, `ack_clear`, `unread_messages`, `assert_thread_fresh` (the reply gate),
//!   `post_reply`, `ack_messages`, `audit_round`, `deliver_done`, `backfill_lost_summons`.
//! - Summon and follow side: `Summon` (the arguments of `accept_summon`), `accept_summon`, `consume_mention`,
//!   `ensure_group`, `armed_follow_triggers`, `cancel_armed_follow_triggers`, `arm_follow_trigger`,
//!   `revive_and_arm_follow`, `follow_message`, `backfill_followed_threads`.
//! - Link checks: `assert_no_file_server_links`, `application_route_links`, `operator_only_note`.
//!
//! Runtime handles come from `FeatureContext`: the store (sessions, chat events, slots, task creation), the
//! `Arc<TriggerManager>` and the `RunningTurnInputs` in startup state, and the master queue's queued ids. The round
//! wake is `charliebot_tasks::master_trigger::trigger_master`. Background work (the round wake, the ack, the nudge)
//! runs as `spawn_logged` tasks, which log a failure as `background_task_failed`.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, Weak};
use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use charliebot_agents::backend_pool;
use charliebot_compat::file_urls;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::paths;
use charliebot_core::str_methods;
use charliebot_registry::turn::{PromptSource, RuleSegment};
use charliebot_registry::{FeatureContext, RunningTurnInputs, StateError};
use charliebot_store::ChatEvent;
use charliebot_store::message_utils::{SESSION_NOT_FOUND_DETAIL, master_done_input_event_ids};
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::trigger_files::{PendingTrigger, TriggerStatus};
use charliebot_store::{CreateTask, SharedChatEvent, Store, StoreError, StoreResult, TaskCaller};
use charliebot_tasks::master_queue;
use charliebot_tasks::master_trigger::{self, TriggerMasterRequest};
use charliebot_triggers::triggers::{TriggerCreateError, TriggerManager};
use chrono_tz::Tz;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

/// The scope of a rule segment's sources (`task_prompts.SCOPE_BASE`).
const SCOPE_BASE: &str = "base";
/// The delivery of a full text segment (`task_prompts.DELIVERY_FULL`).
const DELIVERY_FULL: &str = "full";
/// The page-delivery sentence every platform's contract line carries: the reply path posts the text as written and
/// refuses a file-server link (`LINKED_PAGES_LINE`).
pub const LINKED_PAGES_LINE: &str = concat!(
  "Linked pages: publish each page with `charliebot publish <path>` and write the URL it prints into the reply; ",
  "a CharlieBot file-server link refuses the reply."
);
/// The red-line doc every platform's contract carries.
const REDLINE_DOC: &str = "thread_reply_redline.md";
/// The reply-format doc every platform's contract carries.
const REPLY_FORMAT_DOC: &str = "thread_reply_format.md";
/// Waits between the retries of one platform call (`_RETRY_DELAYS`); exhausting them logs an error, not a raise.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];
/// How much of an unread message's text the 412 refusal carries (`_TEXT_PREVIEW_CHARS`).
const TEXT_PREVIEW_CHARS: usize = 200;
/// The reply budget the format contract states (`_REPLY_BUDGET_CHARS`): the depth goes to a page.
const REPLY_BUDGET_CHARS: usize = 500;
/// A thread-follow batch sleeps out this quiet delay from its newest message (`_FOLLOW_QUIET_SECONDS`).
const FOLLOW_QUIET_SECONDS: i64 = 10;
/// A follow chain never runs past this cap from its first message (`_FOLLOW_CHAIN_CAP_SECONDS`).
const FOLLOW_CHAIN_CAP_SECONDS: i64 = 300;
/// The house timezone of session display names (`config.HOUSE_TIMEZONE`).
const HOUSE_TIMEZONE: Tz = chrono_tz::America::Los_Angeles;
/// The notice a lost summon's thread receives (`_LOST_SUMMON_NOTICE`).
const LOST_SUMMON_NOTICE: &str = "上一次召唤在服务重启时丢失了，没有被处理。需要的话请重新 @ 我一次。";
/// The notice a thread receives when neither its round nor its nudge round replied (`_NO_REPLY_NOTICE`).
const NO_REPLY_NOTICE: &str = "No reply was posted for this mention; the details are in the session log. \
  Mention me again for a thread answer.";
/// The follow-trigger floor the label carries (`_FOLLOW_FLOOR_RE`).
static FOLLOW_FLOOR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"floor=([0-9.]+)").expect("valid pattern"));
static PARAGRAPH_SEPARATOR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{2,}").expect("valid pattern"));
static SESSION_LOCKS: LazyLock<StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
  LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Return the process-local lock shared by reply, summon, and audit work for one session.
pub fn session_lock_for(session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
  let mut locks = SESSION_LOCKS.lock().expect("thread session lock map is not poisoned");
  locks.retain(|_, lock| lock.strong_count() > 0);
  if let Some(lock) = locks.get(session_id).and_then(Weak::upgrade) {
    return lock;
  }
  let lock = Arc::new(tokio::sync::Mutex::new(()));
  locks.insert(session_id.to_string(), Arc::downgrade(&lock));
  lock
}

/// One platform's facts the shared helpers and the entries agree on (`ThreadPlatform`). `name` is the summon
/// event-block key and the prefix of every persisted marker. `id_key` orders the platform's message ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadPlatform {
  /// The summon event-block key and the marker prefix (`slack`, `discord`).
  pub name: &'static str,
  /// The name the platform's text shows (`Slack`, `Discord`).
  pub display_name: &'static str,
  /// The platform's reply event type.
  pub reply_event_type: &'static str,
  /// The command the reply-format contract names for posting a reply (`_REPLY_COMMAND`).
  pub reply_command: &'static str,
  /// The platform's per-message character limit (`max_post_chars`).
  pub max_post_chars: usize,
  /// The platform's scope doc under `prompts/`: what personal information may enter the platform's threads.
  pub scope_doc: &'static str,
  /// The label prefix of the session's armed thread-follow record.
  pub follow_trigger_prefix: &'static str,
  /// The ordering of the platform's message ids.
  pub id_key: IdOrdering,
  /// The thread origin field in the platform's metadata slot.
  pub origin_field: &'static str,
  /// The newest consumed message id field in the platform's metadata slot.
  pub watermark_field: &'static str,
  /// The key naming a message id in readbacks and refusals (`ts` for Slack, `id` for Discord).
  pub id_label: &'static str,
  /// The summon-block key of the mention message; a block without it carries no ack to clear.
  pub mention_key: &'static str,
  /// The summon-block keys a nudge copies from the summon it re-asks.
  pub block_keys: &'static [&'static str],
  /// The whole summon input: one fixed line naming the platform and the event.
  pub summon_line: &'static str,
}

impl ThreadPlatform {
  /// Event payload key of the thread's no-reply notice marker (`notice_key`).
  pub fn notice_key(&self) -> String {
    format!("{}_notice", self.name)
  }

  /// Event payload key of the lost-summon backfill marker (`backfill_key`).
  pub fn backfill_key(&self) -> String {
    format!("{}_backfill", self.name)
  }

  /// Wire type of the platform's persisted ack audit record (`ack_event_type`).
  pub fn ack_event_type(&self) -> String {
    format!("{}_ack", self.name)
  }
}

/// How a platform orders its message ids (`ThreadPlatform.id_key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdOrdering {
  /// Slack's dotted ts strings, ordered as text (`str`).
  Text,
  /// Discord's snowflakes, ordered as unsigned integers (`discord_client.snowflake_key`).
  Snowflake,
}

/// One message id's ordering key. Keys of one `IdOrdering` share a variant, so they compare by their value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdKey<'a> {
  Text(&'a str),
  Snowflake(u64),
}

impl IdOrdering {
  /// The ordering key of `id`. A snowflake that is not an unsigned integer panics, as Python's `int()` raises.
  pub fn key(self, id: &str) -> IdKey<'_> {
    match self {
      IdOrdering::Text => IdKey::Text(id),
      IdOrdering::Snowflake => IdKey::Snowflake(
        id.parse().unwrap_or_else(|error| panic!("a Discord message id is an unsigned integer, got {id:?}: {error}")),
      ),
    }
  }
}

/// One thread message as the round side reads it (`ThreadMessage`). `user` is None when the platform names none, and
/// `text` is empty when it names none. The remaining fields support keyed-post lookup and the reply freshness gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadMessage {
  pub id: String,
  pub user: Option<String>,
  pub text: String,
  pub created_at: Option<String>,
  pub metadata: Option<Value>,
  pub bot_id: Option<String>,
  pub is_bot: bool,
  pub eligible: bool,
}

/// Read one event as the `Value` form these helpers check, from either an owned value or the
/// shared event cache.
pub trait ThreadEventData {
  fn event_value(&self) -> Value;
}

impl ThreadEventData for Value {
  fn event_value(&self) -> Value {
    self.clone()
  }
}

impl ThreadEventData for ChatEvent {
  fn event_value(&self) -> Value {
    self.to_value()
  }
}

impl ThreadEventData for SharedChatEvent {
  fn event_value(&self) -> Value {
    self.as_ref().to_value()
  }
}

/// The platform face the entries work through (`ThreadAdapter`): one implementation per platform over its client.
/// The adapter carries its platform description, so each helper reads the platform from the adapter it was handed.
pub trait ThreadAdapter: Send + Sync {
  /// The platform this adapter speaks for (the ABC's `platform` attribute).
  fn platform(&self) -> &ThreadPlatform;

  /// Post one keyed text message into the thread `address` names.
  fn post<'a>(&'a self, address: &'a Value, text: &'a str, post_key: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;

  /// Find this bot's message for `post_key`, using its content and lower timestamp bound when the platform needs them.
  fn find_own_post_by_key<'a>(
    &'a self,
    address: &'a Value,
    post_key: &'a str,
    text: &'a str,
    created_after: &'a str,
    messages: Option<&'a [ThreadMessage]>,
  ) -> BoxFuture<'a, anyhow::Result<bool>>;

  /// Light the summon ack the summon block carries.
  fn add_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>>;

  /// Clear the summon ack the summon block carries.
  fn remove_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>>;

  /// The thread `origin` names: every message, in platform order.
  fn read_thread<'a>(
    &'a self,
    origin: &'a Value,
    config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, anyhow::Result<Vec<ThreadMessage>>>;

  /// The thread's eligible messages, in platform order.
  fn read_eligible<'a>(
    &'a self,
    origin: &'a Value,
    config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, anyhow::Result<Vec<ThreadMessage>>> {
    Box::pin(async move {
      Ok(self.read_thread(origin, config).await?.into_iter().filter(|message| message.eligible).collect())
    })
  }

  /// The address object `post` accepts for the thread `origin` names.
  fn address_of(&self, origin: &Value) -> Value;

  /// The log fields naming the thread `address` points at, as one JSON object.
  fn log_fields(&self, address: &Value) -> Value;
}

/// The detail of a refusal: a plain text, or the structured 412 payload (`ThreadReplyError.detail`).
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadDetail {
  Text(String),
  Structured(Value),
}

/// A refusal of a thread entry: the HTTP status it maps to and its detail (`ThreadReplyError`).
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadReplyError {
  pub status: StatusCode,
  pub detail: ThreadDetail,
}

impl Display for ThreadReplyError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.detail {
      ThreadDetail::Text(text) => f.write_str(text),
      ThreadDetail::Structured(value) => write!(f, "{value}"),
    }
  }
}

impl ThreadReplyError {
  /// A refusal with a plain text detail.
  pub fn refused(status: StatusCode, detail: impl Into<String>) -> Self {
    Self { status, detail: ThreadDetail::Text(detail.into()) }
  }

  /// An unplanned failure of a store or runtime call. It is logged here and answered as Python's unhandled exception
  /// is: 500 with the text "Internal Server Error".
  pub fn failure(error: impl Display) -> Self {
    tracing::error!(error = %error, "thread_entry_failed");
    Self::refused(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
  }
}

impl std::error::Error for ThreadReplyError {}

impl From<StoreError> for ThreadReplyError {
  fn from(error: StoreError) -> Self {
    Self::failure(error)
  }
}

impl From<StateError> for ThreadReplyError {
  fn from(error: StateError) -> Self {
    Self::failure(error)
  }
}

impl From<anyhow::Error> for ThreadReplyError {
  fn from(error: anyhow::Error) -> Self {
    Self::failure(error)
  }
}

/// Read one prompts doc fresh from disk, with universal newlines and Python's `str.strip()`
/// (`load_prompt_doc`). A missing or unreadable doc fails with its name, its path and the likely cause.
pub fn load_prompt_doc(repo: &RepoRoot, name: &str, likely_cause: &str) -> anyhow::Result<String> {
  let path = repo.path().join("prompts").join(name);
  let bytes =
    std::fs::read(&path).with_context(|| format!("{name} prompt not found at {} — {likely_cause}", path.display()))?;
  let text = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
  Ok(str_methods::strip(&text.replace("\r\n", "\n").replace('\r', "\n")).to_string())
}

/// The thread reply contract: `platform_part`, a blank line, then the platform's scope doc, the red line and the
/// reply format, one per line (`thread_reply_contract`). Each doc is read on every call.
pub fn thread_reply_contract(
  repo: &RepoRoot,
  platform: &ThreadPlatform,
  platform_part: &str,
) -> anyhow::Result<String> {
  let scope_doc = load_prompt_doc(
    repo,
    platform.scope_doc,
    &format!("the repo checkout most likely predates the {} addition commit", platform.scope_doc),
  )?;
  let red_line = load_prompt_doc(
    repo,
    REDLINE_DOC,
    "the repo checkout most likely predates the thread-reply-redline rename commit",
  )?;
  let reply_format = load_prompt_doc(
    repo,
    REPLY_FORMAT_DOC,
    "the repo checkout most likely predates the thread-reply-format rename commit",
  )?;
  Ok(format!("{platform_part}\n\n{scope_doc}\n{red_line}\n{reply_format}"))
}

/// The contract text as one full rule segment. Its sources name the three docs the text joins, so the prompt-source
/// audit reaches each file (`thread_reply_segment`).
pub fn thread_reply_segment(platform: &ThreadPlatform, text: String) -> RuleSegment {
  let sources = [platform.scope_doc, REDLINE_DOC, REPLY_FORMAT_DOC]
    .into_iter()
    .map(|doc| PromptSource {
      scope: SCOPE_BASE.to_string(),
      source_ref: format!("prompts/{doc}"),
      source_session_id: None,
    })
    .collect();
  RuleSegment { text, sources, delivery: DELIVERY_FULL.to_string() }
}

/// The armed follow trigger's label: the platform prefix and the chain floor id (`follow_wake_label`).
pub fn follow_wake_label(platform: &ThreadPlatform, floor: &str) -> String {
  format!("{} floor={floor}", platform.follow_trigger_prefix)
}

/// The `floor=<id>` value on a follow-trigger label, or None when the label carries none (`follow_floor`).
pub fn follow_floor(label: &str) -> Option<String> {
  FOLLOW_FLOOR_RE.captures(label).map(|captures| captures[1].to_string())
}

/// Split `text` into chunks of at most `limit` characters for sequential posting (`chunk_text`).
///
/// Greedy packing over paragraph units (a paragraph plus its trailing blank-line separator); a unit longer than
/// `limit` falls to line units, and a line longer than `limit` is hard-cut. Lengths count characters, as Python's
/// `len` counts code points. A whitespace-only chunk is dropped at return unless every chunk is whitespace.
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
  if text.chars().count() <= limit {
    return vec![text.to_string()];
  }
  let mut pieces: Vec<String> = Vec::new();
  for unit in paragraph_units(text) {
    if unit.chars().count() <= limit {
      pieces.push(unit.to_string());
      continue;
    }
    for line in unit.split_inclusive('\n') {
      if line.chars().count() <= limit {
        pieces.push(line.to_string());
      } else {
        let characters: Vec<char> = line.chars().collect();
        pieces.extend(characters.chunks(limit).map(|cut| cut.iter().collect::<String>()));
      }
    }
  }
  let mut chunks: Vec<String> = Vec::new();
  let mut current = String::new();
  let mut current_len = 0;
  for piece in pieces {
    let piece_len = piece.chars().count();
    if piece_len > 0 && current_len + piece_len > limit {
      chunks.push(std::mem::take(&mut current));
      current_len = 0;
    }
    current.push_str(&piece);
    current_len += piece_len;
  }
  if !current.is_empty() {
    chunks.push(current);
  }
  let nonblank: Vec<String> = chunks.iter().filter(|chunk| !str_methods::strip(chunk).is_empty()).cloned().collect();
  if nonblank.is_empty() { chunks } else { nonblank }
}

/// The paragraph units of `text` (`re.split(r"(\n{2,})")` joined pairwise): each unit is the text before one run of two
/// or more newlines, followed by that run. The last unit has no run after it.
fn paragraph_units(text: &str) -> Vec<&str> {
  let mut units = Vec::new();
  let mut unit_start = 0;
  for separator in PARAGRAPH_SEPARATOR_RE.find_iter(text) {
    units.push(&text[unit_start..separator.end()]);
    unit_start = separator.end();
  }
  units.push(&text[unit_start..]);
  units
}

/// The event with this id, or None (`event_by_id`).
pub fn event_by_id<E: ThreadEventData>(events: &[E], event_id: &str) -> Option<Value> {
  events
    .iter()
    .map(ThreadEventData::event_value)
    .find(|event| event.get("id").and_then(Value::as_str) == Some(event_id))
}

/// The summon a round with this summon block answers: the block's `nudge_of` for a nudge, else the event itself
/// (`summon_of`).
pub fn summon_of(block: &Value, event_id: &str) -> String {
  block
    .get(event_types::NUDGE_OF)
    .and_then(Value::as_str)
    .filter(|nudge| !nudge.is_empty())
    .unwrap_or(event_id)
    .to_string()
}

/// `(event id, summon block)` of the newest summon-bearing input among `event_ids`, or None (`newest_thread_input`).
/// An input binds only when it carries a block under the platform's key; the newest in log order wins.
pub fn newest_thread_input<E: ThreadEventData>(
  platform: &ThreadPlatform,
  events: &[E],
  event_ids: &[String],
) -> Option<(String, Value)> {
  let mut bound = None;
  for shared_event in events {
    let event = shared_event.event_value();
    let id = event.get("id").and_then(Value::as_str);
    let block = event.get(platform.name).filter(|block| block.is_object()).cloned();
    if let (Some(id), Some(block)) = (id, block)
      && event_ids.iter().any(|wanted| wanted.as_str() == id)
    {
      bound = Some((id.to_string(), block));
    }
  }
  bound
}

/// Whether the log holds a reply event answering `summon_id` (`replied`).
pub fn replied<E: ThreadEventData>(platform: &ThreadPlatform, events: &[E], summon_id: &str) -> bool {
  events.iter().any(|event| {
    let event = event.event_value();
    event_type(&event) == Some(platform.reply_event_type)
      && event.get(platform.reply_event_type).and_then(|payload| payload.get("answers")).and_then(Value::as_str)
        == Some(summon_id)
  })
}

/// Whether the log holds the nudge for `summon_id`: a summon-block message carrying `nudge_of` (`nudged`).
pub fn nudged<E: ThreadEventData>(platform: &ThreadPlatform, events: &[E], summon_id: &str) -> bool {
  events.iter().any(|event| {
    let event = event.event_value();
    event_type(&event) == Some(event_types::AGENT_MESSAGE)
      && event.get(platform.name).and_then(|block| block.get(event_types::NUDGE_OF)).and_then(Value::as_str)
        == Some(summon_id)
  })
}

/// Whether the log holds the thread's no-reply notice marker for `summon_id` (`noticed`).
pub fn noticed<E: ThreadEventData>(platform: &ThreadPlatform, events: &[E], summon_id: &str) -> bool {
  let notice_key = platform.notice_key();
  events.iter().any(|event| {
    let event = event.event_value();
    event.get(&notice_key).and_then(|notice| notice.get(event_types::INPUT_EVENT_ID)).and_then(Value::as_str)
      == Some(summon_id)
  })
}

/// The summon injections in one session's log that nothing will ever answer (`lost_summons`). A summon or a nudge is
/// lost when no `master_done` names it, this process does not own it (queued or running), and no earlier backfill
/// marked it. The marker is the `<name>_backfill` payload, never a synthetic `master_done`.
pub fn lost_summons<E: ThreadEventData>(
  platform: &ThreadPlatform,
  events: &[E],
  owned: &HashSet<String>,
  running: &HashSet<String>,
) -> Vec<Value> {
  let backfill_key = platform.backfill_key();
  let mut answered: HashSet<String> = HashSet::new();
  for event in
    events.iter().map(ThreadEventData::event_value).filter(|event| event_type(event) == Some(event_types::MASTER_DONE))
  {
    answered.extend(master_done_input_event_ids(&event));
  }
  let marked: HashSet<String> = events
    .iter()
    .filter_map(|event| {
      event.event_value().get(&backfill_key)?.get(event_types::INPUT_EVENT_ID)?.as_str().map(str::to_string)
    })
    .collect();
  events
    .iter()
    .filter(|event| {
      event.event_value().get("type").and_then(Value::as_str) == Some(event_types::AGENT_MESSAGE)
        && event.event_value().get(platform.name).is_some()
    })
    .filter(|event| {
      let value = event.event_value();
      let id = event_id(&value);
      !answered.contains(id) && !marked.contains(id) && !owned.contains(id) && !running.contains(id)
    })
    .map(|event| event.event_value().clone())
    .collect()
}

/// The eligible messages whose id sorts strictly above `watermark` under `id_key`; a None watermark passes every
/// eligible message (`unread_after`).
pub fn unread_after<'a, T>(
  messages: &'a [T],
  eligible: impl Fn(&T) -> bool,
  message_id: impl Fn(&T) -> &str,
  watermark: Option<&str>,
  id_key: IdOrdering,
) -> Vec<&'a T> {
  let floor = watermark.map(|watermark| id_key.key(watermark));
  messages
    .iter()
    .filter(|message| eligible(message) && floor.is_none_or(|floor| id_key.key(message_id(message)) > floor))
    .collect()
}

/// Post one keyed thread reply, retrying on failure; true when the platform accepted it (`post_with_retry`). Exhausting
/// the retries logs `<name>_post_gave_up` and returns false: the caller decides what a failed post means.
/// Find one keyed post before sending; Slack checks again after a failed write to recover accepted posts.
pub async fn post_with_retry(
  adapter: &dyn ThreadAdapter,
  address: &Value,
  text: &str,
  session_id: &str,
  post_key: &str,
  created_after: &str,
  messages: Option<&[ThreadMessage]>,
) -> anyhow::Result<bool> {
  let platform = adapter.platform();
  if adapter.find_own_post_by_key(address, post_key, text, created_after, messages).await? {
    return Ok(true);
  }
  let attempts = RETRY_DELAYS.len() + 1;
  for attempt in 1..=attempts {
    match adapter.post(address, text, post_key).await {
      Ok(()) => return Ok(true),
      Err(error) => {
        if platform.name == "slack"
          && adapter.find_own_post_by_key(address, post_key, text, created_after, None).await?
        {
          return Ok(true);
        }
        if attempt == attempts {
          let event = format!("{}_post_gave_up", platform.name);
          let thread = adapter.log_fields(address);
          tracing::error!(%event, session = session_id, thread = %thread, attempts, error = %error);
          return Ok(false);
        }
        let event = format!("{}_post_retry", platform.name);
        let thread = adapter.log_fields(address);
        tracing::warn!(%event, session = session_id, thread = %thread, attempt, error = %error);
        tokio::time::sleep(RETRY_DELAYS[attempt - 1]).await;
      }
    }
  }
  unreachable!("the final keyed-post attempt returns")
}

/// Clear the summon ack once its question is closed (`ack_clear`). The remove runs as its own logged task, and a block
/// without the platform's mention key has no ack to clear.
pub fn ack_clear(adapter: &Arc<dyn ThreadAdapter>, block: &Value, session_id: &str) {
  let platform = adapter.platform();
  if block.get(platform.mention_key).is_none_or(Value::is_null) {
    return;
  }
  let adapter = Arc::clone(adapter);
  let block = block.clone();
  spawn_logged(format!("{}-ack-clear-{session_id}", platform.name), async move { adapter.remove_ack(&block).await });
}

/// The thread's eligible messages above `watermark`; None passes all (`unread_messages`).
pub async fn unread_messages(
  adapter: &dyn ThreadAdapter,
  origin: &Value,
  config: &CharlieBotConfig,
  watermark: Option<&str>,
) -> anyhow::Result<Vec<ThreadMessage>> {
  let messages = adapter.read_eligible(origin, config).await?;
  let Some(watermark) = watermark else {
    return Ok(messages);
  };
  let floor = adapter.platform().id_key.key(watermark);
  Ok(messages.into_iter().filter(|message| adapter.platform().id_key.key(&message.id) > floor).collect())
}

/// A session with its thread origin, as the reply path reads it (`require_thread_session`).
pub struct ThreadSession {
  pub meta: SessionMetadata,
  pub origin: Value,
}

/// The session's thread origin, or None when the slot holds none or null.
fn origin_of<'a>(meta: &'a SessionMetadata, platform: &ThreadPlatform) -> Option<&'a Value> {
  meta.extra(platform.origin_field).filter(|origin| !origin.is_null())
}

/// The session's newest consumed message id, or None before any consumption.
fn watermark_of(meta: &SessionMetadata, platform: &ThreadPlatform) -> Option<String> {
  meta.extra(platform.watermark_field).and_then(Value::as_str).map(str::to_string)
}

/// Load the session and its thread origin for a reply-path entry (`require_thread_session`).
pub async fn require_thread_session(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
) -> Result<ThreadSession, ThreadReplyError> {
  let meta = features
    .store
    .get_session(session_id)
    .await?
    .ok_or_else(|| ThreadReplyError::refused(StatusCode::NOT_FOUND, SESSION_NOT_FOUND_DETAIL))?;
  let Some(origin) = origin_of(&meta, platform).cloned() else {
    return Err(ThreadReplyError::refused(
      StatusCode::CONFLICT,
      format!("Session has no {} thread", platform.display_name),
    ));
  };
  Ok(ThreadSession { meta, origin })
}

/// Refuse the reply when eligible thread messages sit above the session's watermark (`assert_thread_fresh`). Refusals:
/// 404 unknown session, 409 no platform thread, then 412 with the structured `stale_thread` payload.
pub async fn assert_thread_fresh(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &FeatureContext,
  session_id: &str,
) -> Result<(), ThreadReplyError> {
  let platform = adapter.platform();
  let session = require_thread_session(platform, features, session_id).await?;
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let watermark = watermark_of(&session.meta, platform);
  let messages = adapter.read_thread(&session.origin, &config).await.map_err(ThreadReplyError::failure)?;
  assert_thread_fresh_messages(platform, watermark.as_deref(), &messages)
}

fn assert_thread_fresh_messages(
  platform: &ThreadPlatform,
  watermark: Option<&str>,
  messages: &[ThreadMessage],
) -> Result<(), ThreadReplyError> {
  let unread =
    unread_after(messages, |message| message.eligible, |message| message.id.as_str(), watermark, platform.id_key);
  if unread.is_empty() {
    return Ok(());
  }
  let new_messages: Vec<Value> = unread
    .iter()
    .map(|message| {
      json!({
        (platform.id_label): message.id,
        "user": message.user,
        "text_preview": message.text.chars().take(TEXT_PREVIEW_CHARS).collect::<String>(),
      })
    })
    .collect();
  let mut detail = Map::new();
  detail.insert("error".to_string(), json!("stale_thread"));
  detail.insert("new_messages".to_string(), Value::Array(new_messages));
  detail.insert(format!("watermark_{}", platform.id_label), json!(watermark));
  Err(ThreadReplyError {
    status: StatusCode::PRECONDITION_FAILED,
    detail: ThreadDetail::Structured(Value::Object(detail)),
  })
}

/// Post `text` to the session's thread and return the readback the CLI prints (`post_reply`). The text posts as
/// written. Refusals: 404 unknown session, 409 no platform thread, 422 blank text, 422 a file-server link, 502 when a
/// chunk exhausted its retries (nothing is persisted then).
pub async fn post_reply(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  text: &str,
) -> Result<Value, ThreadReplyError> {
  post_reply_with_caller_context(adapter, features, session_id, text, None, None).await
}

/// Post a keyed reply with the caller's run start time for Discord's content lookup.
pub async fn post_reply_with_caller_context(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  text: &str,
  request_id: Option<&str>,
  run_started_at: Option<&str>,
) -> Result<Value, ThreadReplyError> {
  let _session_guard = session_lock_for(session_id).lock_owned().await;
  let platform = adapter.platform();
  let session = require_thread_session(platform, features, session_id).await?;
  if str_methods::strip(text).is_empty() {
    return Err(ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "Reply text is empty"));
  }
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let scan_context = file_urls::ScanContext {
    server_port: config.server.port,
    charliebot_home: config.charliebot_home.clone(),
    session_dir: features.store.session_dir(session_id),
    user_home: PathBuf::from(paths::expanduser("~")),
  };
  assert_no_file_server_links(text, Some(&scan_context))?;
  let operator_only_links = application_route_links(text, config.server.port);

  let chat_events = load_chat_events(features, session_id).await?;
  // The in-process work item owns the input batch answered by this reply.
  let running = features.startup.require::<RunningTurnInputs>()?.user_event_ids(session_id);
  let bound = newest_thread_input(platform, &chat_events, &running);
  let answers = bound.as_ref().map(|(input_id, block)| summon_of(block, input_id));
  let ack_block = bound.as_ref().map(|(_, block)| (*block).clone());
  let address = adapter.address_of(&session.origin);
  let bodies = chunk_text(text, platform.max_post_chars);
  let event_id =
    request_id.map(|request_id| charliebot_store::control_events::stable_request_event_id(session_id, request_id));
  if let Some(request_id) = request_id {
    let events = features.store.load_all_facts_view_sync(session_id)?;
    if let Some(existing) = charliebot_store::control_events::replay_request_event(
      events.iter(),
      event_id.as_deref().expect("a request id derives an event id"),
      request_id,
      platform.reply_event_type,
      text,
      None,
    )? {
      return Ok(reply_receipt(&existing, &operator_only_links, platform));
    }
  }

  let thread_messages = adapter.read_thread(&session.origin, &config).await.map_err(ThreadReplyError::failure)?;
  assert_thread_fresh_messages(platform, watermark_of(&session.meta, platform).as_deref(), &thread_messages)?;
  let post_request_id = request_id
    .map(str::to_string)
    .unwrap_or_else(|| format!("operator-reply:{}", charliebot_store::control_events::new_request_id()));
  let created_after = run_started_at.map(str::to_string).unwrap_or_else(|| UtcDateTime::now().to_python_isoformat());
  for (index, body) in bodies.iter().enumerate() {
    if !post_with_retry(
      adapter.as_ref(),
      &address,
      body,
      session_id,
      &format!("{post_request_id}#{}", index + 1),
      &created_after,
      Some(&thread_messages),
    )
    .await?
    {
      let attempts = RETRY_DELAYS.len() + 1;
      return Err(ThreadReplyError::refused(
        StatusCode::BAD_GATEWAY,
        format!(
          "{} did not accept chunk {} of {} after {attempts} attempts; nothing was persisted",
          platform.display_name,
          index + 1,
          bodies.len()
        ),
      ));
    }
  }

  let chars = text.chars().count();
  drop(bound);
  // Python appends to its cached list in place (`chat_events.py:221-224`); release the Arc before the same write.
  drop(chat_events);
  let payload = json!({"answers": answers, "chars": chars, "chunks": bodies.len()});
  let _event = if let (Some(request_id), Some(event_id)) = (request_id, event_id.as_deref()) {
    let event = json!({
      "id": event_id,
      "request_id": request_id,
      "type": platform.reply_event_type,
      "content": text,
      (platform.reply_event_type): payload,
    });
    let epoch = features.store.prime_aggregator(session_id).await?;
    let event = {
      let _guard = features.store.control_lock().await;
      let events = features.store.load_all_facts_view_sync(session_id)?;
      if let Some(existing) = charliebot_store::control_events::replay_request_event(
        events.iter(),
        event_id,
        request_id,
        platform.reply_event_type,
        text,
        None,
      )? {
        return Ok(reply_receipt(&existing, &operator_only_links, platform));
      }
      features.store.append_chat_event(session_id, event).await?
    };
    features.store.announce_appended_event(session_id, &event, epoch).await?;
    event
  } else {
    features
      .store
      .persist_and_broadcast(
        session_id,
        json!({
          "type": platform.reply_event_type,
          "content": text,
          (platform.reply_event_type): payload,
        }),
      )
      .await?
  };
  if let Some(block) = &ack_block {
    ack_clear(adapter, block, session_id);
  }
  let over_budget = chars > REPLY_BUDGET_CHARS;
  let event = format!("{}_reply_posted", platform.name);
  let thread = adapter.log_fields(&address);
  tracing::info!(
    %event,
    session = session_id,
    thread = %thread,
    chars,
    chunks = bodies.len(),
    over_budget,
    budget = REPLY_BUDGET_CHARS,
    answers = ?answers,
  );
  let note = operator_only_note(&operator_only_links);
  Ok(json!({
    "posted": true,
    "text": text,
    "operator_only_note": note,
    "chars": chars,
    "chunks": bodies.len(),
    "over_budget": over_budget,
    "answers": answers,
  }))
}

/// Post one reply while retaining the caller's stable request id for delivery.
pub async fn post_reply_with_request_id(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  text: &str,
  request_id: Option<&str>,
) -> Result<Value, ThreadReplyError> {
  post_reply_with_caller_context(adapter, features, session_id, text, request_id, None).await
}

fn reply_receipt(event: &Value, operator_only_links: &[String], platform: &ThreadPlatform) -> Value {
  let payload = event.get(platform.reply_event_type).expect("a stored thread reply contains its platform payload");
  let chars = payload.get("chars").and_then(Value::as_u64).expect("a stored reply contains its character count");
  json!({
    "posted": true,
    "text": event["content"],
    "operator_only_note": operator_only_note(operator_only_links),
    "chars": chars,
    "chunks": payload["chunks"],
    "over_budget": chars as usize > REPLY_BUDGET_CHARS,
    "answers": payload["answers"],
  })
}

async fn persist_request_event(
  features: &FeatureContext,
  session_id: &str,
  request_id: &str,
  event_type: &str,
  content: &str,
  fields: Map<String, Value>,
) -> Result<ChatEvent, ThreadReplyError> {
  let event_id = charliebot_store::control_events::stable_request_event_id(session_id, request_id);
  let epoch = features.store.prime_aggregator(session_id).await?;
  let event = {
    let _guard = features.store.control_lock().await;
    let events = features.store.load_all_facts_view_sync(session_id)?;
    let request_fields = Value::Object(fields.clone());
    if let Some(existing) = charliebot_store::control_events::replay_request_event(
      events.iter(),
      &event_id,
      request_id,
      event_type,
      content,
      Some(&request_fields),
    )? {
      return Ok(existing.into());
    }
    let mut event = json!({"id": event_id, "request_id": request_id, "type": event_type, "content": content});
    for (key, value) in fields {
      event[key] = value;
    }
    features.store.append_chat_event(session_id, event).await?
  };
  features.store.announce_appended_event(session_id, &event, epoch).await?;
  Ok(event)
}

/// Advance the session's read watermark over `message_ids` and return the readback (`ack_messages`). Refusals: 404
/// unknown session, 409 no platform thread, 422 an empty set, 422 an unknown or ineligible id, 422 an eligible unread
/// id at or below the ceiling that the set skips. A refusal persists nothing.
pub async fn ack_messages(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  message_ids: &[String],
) -> Result<Value, ThreadReplyError> {
  let platform = adapter.platform();
  let session = require_thread_session(platform, features, session_id).await?;
  let mut ids: Vec<String> = message_ids.iter().cloned().collect::<HashSet<_>>().into_iter().collect();
  ids.sort_by(|a, b| platform.id_key.key(a).cmp(&platform.id_key.key(b)));
  if ids.is_empty() {
    return Err(ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, "message_ids is empty"));
  }
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let mut eligible: Vec<String> =
    adapter.read_eligible(&session.origin, &config).await?.into_iter().map(|message| message.id).collect();
  eligible.sort_by(|a, b| platform.id_key.key(a).cmp(&platform.id_key.key(b)));
  if let Some(unknown) = ids.iter().find(|id| !eligible.iter().any(|eligible_id| eligible_id == *id)) {
    return Err(ThreadReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("Unknown or ineligible message id: {unknown}"),
    ));
  }
  let mut watermark = watermark_of(&session.meta, platform);
  let ceiling = ids.last().expect("the set is not empty").clone();
  let floor = watermark.as_deref().map(|watermark| platform.id_key.key(watermark));
  let ceiling_key = platform.id_key.key(&ceiling);
  let skipped = eligible.iter().find(|id| {
    let key = platform.id_key.key(id);
    floor.is_none_or(|floor| key > floor) && key <= ceiling_key && !ids.iter().any(|acked| acked == *id)
  });
  if let Some(skipped) = skipped {
    return Err(ThreadReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("Skipped eligible message id at or below {ceiling}: {skipped}"),
    ));
  }
  if floor.is_none_or(|floor| ceiling_key > floor) {
    features
      .store
      .update_slot_fields_at(
        session_id,
        platform.name,
        Map::from_iter([(platform.watermark_field.to_string(), json!(ceiling))]),
        UtcDateTime::now(),
      )
      .await?
      .ok_or_else(|| ThreadReplyError::failure(format!("session {session_id} vanished during the ack")))?;
    watermark = Some(ceiling.clone());
  }
  features
    .store
    .persist_and_broadcast(
      session_id,
      json!({
        "type": platform.ack_event_type(),
        "content": format!("{} thread ack: {} message(s) read through {ceiling}", platform.display_name, ids.len()),
        (platform.ack_event_type()): {
          "message_ids": ids,
          format!("watermark_{}", platform.id_label): watermark,
        },
      }),
    )
    .await?;
  let event = format!("{}_thread_acked", platform.name);
  tracing::info!(%event, session = session_id, acked = ids.len(), watermark = ?watermark);
  Ok(json!({"acked": ids.len(), format!("watermark_{}", platform.id_label): watermark}))
}

/// The nudge text the summon round's silence triggers (`_NUDGE_TEMPLATE`).
fn nudge_text(platform: &ThreadPlatform) -> String {
  format!(
    "{} thread: the round answering this mention ended without posting a reply. Decide now:\n\
     when the thread should hear something, post a reply; when there is nothing to say, end this\n\
     round and the thread gets a one-line notice pointing to this session.",
    platform.display_name
  )
}

/// The round-end audit of one finished round whose input carried a summon block; true when it acted
/// (`audit_round`). A summon round without a reply gets a nudge, once; a nudge round without one gets the thread
/// notice, once, persisted only after the post succeeds.
pub async fn audit_round(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  input_event_id: &str,
) -> Result<Option<Arc<Vec<SharedChatEvent>>>, ThreadReplyError> {
  let _session_guard = session_lock_for(session_id).lock_owned().await;
  let platform = adapter.platform();
  let chat_events = load_chat_events(features, session_id).await?;
  let Some(input_event) = event_by_id(&chat_events, input_event_id) else {
    return Ok(Some(chat_events));
  };
  let Some(target) = input_event.get(platform.name).filter(|block| block.is_object()).cloned() else {
    return Ok(Some(chat_events));
  };
  let summon_id = summon_of(&target, input_event_id);
  let content = event_by_id(&chat_events, &summon_id)
    .and_then(|summon| summon.get("content").and_then(Value::as_str).map(str::to_string))
    .unwrap_or_default();
  // A summon from the earlier marker contract names neither the one-line input nor the reply command.
  if content != platform.summon_line && !content.contains(platform.reply_command) {
    return Ok(Some(chat_events));
  }
  if replied(platform, &chat_events, &summon_id) {
    return Ok(Some(chat_events));
  }

  if target.get(event_types::NUDGE_OF).is_none() {
    if nudged(platform, &chat_events, &summon_id) {
      return Ok(Some(chat_events));
    }
    let text = nudge_text(platform);
    let mut block = Map::new();
    for key in platform.block_keys {
      if let Some(value) = target.get(*key) {
        block.insert(key.to_string(), value.clone());
      }
    }
    block.insert(event_types::NUDGE_OF.to_string(), json!(summon_id));
    drop(chat_events);
    let dispatcher =
      features.startup.require::<charliebot_tasks::dispatch::Dispatcher>().map_err(ThreadReplyError::failure)?;
    let stored = dispatcher
      .admit_input(
        session_id,
        event_types::AGENT_MESSAGE,
        &text,
        "agent",
        json!({
          "request_id": format!("{}-nudge:{summon_id}", platform.name),
          "from_session": session_id,
          "from_session_name": platform.display_name,
          "fields": {(platform.name): Value::Object(block)},
        }),
      )
      .await?;
    let nudge_id = stored.get("id").and_then(ChatEvent::as_str).expect("a persisted event carries its id");
    spawn_master_wake(
      features,
      format!("{}-nudge-{session_id}", platform.name),
      session_id,
      &text,
      nudge_id,
      platform.display_name,
    );
    let event = format!("{}_reply_nudge", platform.name);
    let thread = adapter.log_fields(&target);
    tracing::info!(%event, session = session_id, thread = %thread, summon_id = %summon_id, nudge_id);
    return Ok(None);
  }

  if noticed(platform, &chat_events, &summon_id) {
    return Ok(Some(chat_events));
  }
  let summon_event = event_by_id(&chat_events, &summon_id).expect("a nudge references its original summon event");
  let created_after = summon_event.get("timestamp").and_then(Value::as_str).expect("a summon event has a timestamp");
  let request_id = format!("{}-notice:{summon_id}", platform.name);
  if !post_with_retry(
    adapter.as_ref(),
    &target,
    NO_REPLY_NOTICE,
    session_id,
    &format!("{request_id}#1"),
    created_after,
    None,
  )
  .await?
  {
    return Ok(Some(chat_events));
  }
  let content = format!(
    "This {} mention got no reply from its round or the nudge round; the thread was told so.",
    platform.display_name
  );
  let mut fields = Map::new();
  fields.insert(platform.notice_key(), json!({event_types::INPUT_EVENT_ID: summon_id}));
  drop(chat_events);
  persist_request_event(features, session_id, &request_id, event_types::ASSISTANT_ERROR, &content, fields).await?;
  ack_clear(adapter, &target, session_id);
  let event = format!("{}_reply_notice", platform.name);
  let thread = adapter.log_fields(&target);
  tracing::info!(%event, session = session_id, thread = %thread, summon_id = %summon_id);
  Ok(None)
}

/// The round-end audit for one finished round (`deliver_done`). It returns false without acting unless the round
/// belongs to a thread-bound session and answered a summon or a nudge.
pub async fn deliver_done(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  done: &Value,
) -> Result<bool, ThreadReplyError> {
  let platform = adapter.platform();
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(false);
  };
  if origin_of(&meta, platform).is_none() {
    return Ok(false);
  }
  let input_event_ids = master_done_input_event_ids(done);
  if input_event_ids.is_empty() {
    return Ok(false);
  }
  let chat_events = load_chat_events(features, session_id).await?;
  let Some((input_event_id, _)) = newest_thread_input(platform, &chat_events, &input_event_ids) else {
    return Ok(false);
  };
  let input_event_id = input_event_id.to_string();
  Ok(audit_round(adapter, features, session_id, &input_event_id).await?.is_none())
}

/// Boot pass over every thread-bound session; returns how many notices and nudges it produced
/// (`backfill_lost_summons`). Summons lost while queued get the lost-summon notice, then every finished round gets the
/// round-end audit. A second pass finds nothing, because every predicate reads the log.
pub async fn backfill_lost_summons(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
) -> Result<usize, ThreadReplyError> {
  let platform = adapter.platform();
  // Archived sessions are included: a thread can be summoned again.
  let sessions = blocking(features, |store| store.load_session_metas(None)).await?;
  let mut reported = 0;
  for meta in sessions {
    if origin_of(&meta, platform).is_none() {
      continue;
    }
    let mut chat_events = {
      let _session_guard = session_lock_for(&meta.id).lock_owned().await;
      let chat_events = load_chat_events(features, &meta.id).await?;
      let owned = master_queue::queued_user_event_ids(features, &meta.id)?;
      let running: HashSet<String> =
        features.startup.require::<RunningTurnInputs>()?.user_event_ids(&meta.id).into_iter().collect();
      let lost = lost_summons(platform, &chat_events, &owned, &running);
      for event in &lost {
        let summon_id = event_id(event).to_string();
        let block = event.get(platform.name).expect("lost summons carry their block");
        let request_id = format!("{}-lost:{summon_id}", platform.name);
        let created_after =
          event.get("timestamp").and_then(Value::as_str).expect("a lost summon event has a timestamp");
        if !post_with_retry(
          adapter.as_ref(),
          block,
          LOST_SUMMON_NOTICE,
          &meta.id,
          &format!("{request_id}#1"),
          created_after,
          None,
        )
        .await?
        {
          continue;
        }
        let mut fields = Map::new();
        fields.insert(platform.backfill_key(), json!({event_types::INPUT_EVENT_ID: summon_id}));
        let content = format!(
          "这条 {} 召唤在服务重启时还排在队列里，没有任何轮次回答它；已在对应线程里说明。",
          platform.display_name
        );
        persist_request_event(features, &meta.id, &request_id, event_types::ASSISTANT_ERROR, &content, fields).await?;
        ack_clear(adapter, block, &meta.id);
        reported += 1;
        let log_event = format!("{}_backfill_lost_summon", platform.name);
        let thread = adapter.log_fields(block);
        tracing::info!(event = %log_event, session = %meta.id, thread = %thread, input_event_id = %summon_id);
      }
      load_chat_events(features, &meta.id).await?
    };

    let dones: Vec<usize> = chat_events
      .iter()
      .enumerate()
      .filter(|(_, event)| event_type(&event.to_value()) == Some(event_types::MASTER_DONE))
      .filter(|(_, event)| !master_done_input_event_ids(event.as_ref()).is_empty())
      .map(|(index, _)| index)
      .collect();
    for done_index in dones {
      let done_ids = master_done_input_event_ids(chat_events[done_index].as_ref());
      let Some((input_event_id, _)) = newest_thread_input(platform, &chat_events, &done_ids) else {
        continue;
      };
      let input_event_id = input_event_id.to_string();
      match audit_round(adapter, features, &meta.id, &input_event_id).await? {
        Some(snapshot) => chat_events = snapshot,
        None => {
          reported += 1;
          // The action appended an event the next done's predicates must see.
          chat_events = load_chat_events(features, &meta.id).await?;
        }
      }
    }
  }
  Ok(reported)
}

/// The session's pending thread-follow trigger records, at most one by construction (`armed_follow_triggers`).
pub async fn armed_follow_triggers(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
) -> Result<Vec<PendingTrigger>, ThreadReplyError> {
  let triggers = features.startup.require::<Arc<TriggerManager>>()?;
  Ok(
    triggers
      .list_triggers(session_id)
      .await?
      .into_iter()
      .filter(|trigger| {
        trigger.status == TriggerStatus::Pending && trigger.message.starts_with(platform.follow_trigger_prefix)
      })
      .collect(),
  )
}

/// Cancel every armed thread-follow trigger of the session; returns how many (`cancel_armed_follow_triggers`).
pub async fn cancel_armed_follow_triggers(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
) -> Result<usize, ThreadReplyError> {
  let armed = armed_follow_triggers(platform, features, session_id).await?;
  let triggers = features.startup.require::<Arc<TriggerManager>>()?;
  for trigger in &armed {
    triggers.cancel_trigger(session_id, &trigger.id).await?;
  }
  Ok(armed.len())
}

/// Cancel-then-create the session's one persisted follow trigger; returns the fresh record (`arm_follow_trigger`).
///
/// The replaced record's `created_at` and floor id are read before the cancel: the new record keeps that `created_at`
/// as the chain's start, so a steady trickle still flushes at `chain_start + FOLLOW_CHAIN_CAP_SECONDS`. The label keeps
/// the chain's oldest unacked id as its floor. An archived session arms nothing and logs
/// `<name>_follow_trigger_not_armed_archived`. The re-arm is never rejected by the pending-trigger limit.
pub async fn arm_follow_trigger(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
  floor: &str,
  log_fields: &Value,
) -> Result<Option<PendingTrigger>, ThreadReplyError> {
  let triggers = features.startup.require::<Arc<TriggerManager>>()?;
  let mut floor = floor.to_string();
  let mut chain_start: Option<UtcDateTime> = None;
  for old in armed_follow_triggers(platform, features, session_id).await? {
    if chain_start.is_none() {
      chain_start = Some(old.created_at);
      if let Some(parsed) = follow_floor(&old.message) {
        floor = parsed;
      }
    }
    triggers.cancel_trigger(session_id, &old.id).await?;
  }
  let now = UtcDateTime::now();
  let start = chain_start.unwrap_or(now);
  let fire_at = (now.into_inner() + time::Duration::seconds(FOLLOW_QUIET_SECONDS))
    .min(start.into_inner() + time::Duration::seconds(FOLLOW_CHAIN_CAP_SECONDS));
  let delay = (fire_at - now.into_inner()).whole_seconds().max(0);
  match triggers
    .create_trigger(session_id, delay, &follow_wake_label(platform, &floor), None, None, Some(start), false)
    .await
  {
    Ok(trigger) => {
      let event = format!("{}_follow_trigger_armed", platform.name);
      tracing::info!(
        %event,
        session = session_id,
        thread = %log_fields,
        floor_id = %floor,
        fire_at = %trigger.fire_at.to_python_isoformat(),
        chain_start = %start.to_python_isoformat(),
      );
      Ok(Some(trigger))
    }
    Err(TriggerCreateError::Archived(error)) => {
      let event = format!("{}_follow_trigger_not_armed_archived", platform.name);
      tracing::info!(%event, session = session_id, thread = %log_fields, error = %error);
      Ok(None)
    }
    Err(error) => Err(ThreadReplyError::failure(error)),
  }
}

/// The mention round consumes its own id: advance the watermark to it and cancel the armed follows (`consume_mention`).
pub async fn consume_mention(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
  mention_id: &str,
) -> Result<(), ThreadReplyError> {
  let meta = features
    .store
    .get_session(session_id)
    .await?
    .ok_or_else(|| ThreadReplyError::failure(format!("the summon path resolves session {session_id} first")))?;
  let watermark = watermark_of(&meta, platform);
  // A missing watermark or an older one takes this mention. The mention's own id is never jumped over.
  if watermark.as_deref().is_none_or(|watermark| platform.id_key.key(watermark) < platform.id_key.key(mention_id)) {
    features
      .store
      .update_slot_fields_at(
        session_id,
        platform.name,
        Map::from_iter([(platform.watermark_field.to_string(), json!(mention_id))]),
        UtcDateTime::now(),
      )
      .await?
      .ok_or_else(|| ThreadReplyError::failure(format!("session {session_id} vanished during the mention")))?;
  }
  let cancelled = cancel_armed_follow_triggers(platform, features, session_id).await?;
  if cancelled > 0 {
    let event = format!("{}_follow_trigger_cancelled_for_mention", platform.name);
    tracing::info!(%event, session = session_id, cancelled);
  }
  Ok(())
}

/// Group a summon session under `label` unless it already has a group (`ensure_group`). It is best effort: a failure
/// logs `<name>_group_assignment_failed` and nothing else changes.
pub async fn ensure_group(platform: &ThreadPlatform, features: &FeatureContext, session_id: &str, label: &str) {
  if let Err(error) = assign_group(platform, features, session_id, label).await {
    let event = format!("{}_group_assignment_failed", platform.name);
    tracing::warn!(%event, session = session_id, label, error = %error);
  }
}

async fn assign_group(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  session_id: &str,
  label: &str,
) -> StoreResult<()> {
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(());
  };
  if origin_of(&meta, platform).is_none() || meta.group.as_deref().is_some_and(|group| !group.is_empty()) {
    return Ok(());
  }
  features.store.set_group(session_id, Some(label)).await?;
  Ok(())
}

/// The arguments of `accept_summon`: the resolved session, the label naming the session and its group, the thread
/// origin, the summon block (the platform's persisted marker payload), the summon line, and the mentioning user.
pub struct Summon<'a> {
  pub session_id: &'a str,
  pub label: &'a str,
  pub origin: &'a Value,
  pub block: &'a Value,
  pub content: &'a str,
  pub user: Option<&'a str>,
}

async fn route_thread_session(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &FeatureContext,
  origin: &Value,
) -> Result<(Option<String>, Option<backend_pool::BackendRoute>), ThreadReplyError> {
  let cfg = features.config.get().map_err(ThreadReplyError::failure)?;
  if cfg.backends.routing.is_none() {
    return Ok((None, None));
  }

  let messages = adapter.read_eligible(origin, &cfg).await.map_err(ThreadReplyError::failure)?;
  let text = messages
    .iter()
    .map(|message| match message.user.as_deref() {
      Some(user) => format!("{user}: {}", message.text),
      None => message.text.clone(),
    })
    .collect::<Vec<_>>()
    .join("\n");
  let candidates = cfg.backends.options.iter().map(|option| option.id.clone()).collect::<Vec<_>>();
  let pool = backend_pool::backend_pool(features).map_err(ThreadReplyError::failure)?;
  let route = match pool.route(&cfg, "thread", &text, &candidates).await {
    Ok(route) => route,
    Err(error) => {
      let message = format!("backend router failed: {error}");
      let address = adapter.address_of(origin);
      adapter
        .post(
          &address,
          &message,
          &format!("{}-routerror:{}#1", adapter.platform().name, charliebot_store::control_events::new_request_id()),
        )
        .await?;
      return Err(ThreadReplyError::failure(message));
    }
  };
  let selected = pool.pick_ordered(&cfg, "thread", &route.backends, &[]).map_err(ThreadReplyError::failure)?;
  let selected = match selected {
    Some(option) => Some(option),
    None => pool.select(&cfg, "thread", &[]).map_err(ThreadReplyError::failure)?,
  };
  let Some(option) = selected else {
    let message = format!("no backend is available for the thread session: {}", pool.recovery_report(&cfg));
    let address = adapter.address_of(origin);
    adapter
      .post(
        &address,
        &message,
        &format!("{}-routerror:{}#1", adapter.platform().name, charliebot_store::control_events::new_request_id()),
      )
      .await?;
    return Err(ThreadReplyError::failure(message));
  };
  Ok((Some(option.id), Some(route)))
}

/// Accept one summon: resolve the session, persist the summon, fire the round, and light the ack (`accept_summon`).
/// The session is created as a manager root named `<label> <local time>` and born with the origin, or reused; a
/// session archived by the operator is unarchived and its task-tree change broadcast. Returns the session id.
pub async fn accept_summon(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  summon: Summon<'_>,
) -> Result<String, ThreadReplyError> {
  let platform = adapter.platform();
  let mut fields = match adapter.log_fields(summon.block) {
    Value::Object(fields) => fields,
    other => return Err(ThreadReplyError::failure(format!("log fields are not an object: {other}"))),
  };
  fields.insert(format!("{}_user", platform.name), json!(summon.user));
  let thread = Value::Object(fields);
  let session_id = summon.session_id;
  let mention_id = summon
    .block
    .get(platform.mention_key)
    .and_then(Value::as_str)
    .ok_or_else(|| ThreadReplyError::failure(format!("the summon block carries no {}", platform.mention_key)))?;
  let request_id = format!("{}-mention:{mention_id}", platform.name);
  let _session_guard = session_lock_for(session_id).lock_owned().await;
  let existing_session = features.store.get_session(session_id).await?;
  if existing_session.is_some() {
    let events = features.store.load_all_facts_view_sync(session_id)?;
    let mut fields = Map::new();
    fields.insert(platform.name.to_string(), summon.block.clone());
    let extra_fields = Value::Object(fields);
    if charliebot_store::control_events::replay_request_event(
      events.iter(),
      &charliebot_store::control_events::stable_request_event_id(session_id, &request_id),
      &request_id,
      event_types::AGENT_MESSAGE,
      summon.content,
      Some(&extra_fields),
    )?
    .is_some()
    {
      return Ok(session_id.to_string());
    }
  }

  match existing_session {
    None => {
      let name = format!("{} {}", summon.label, local_time());
      let (backend, route) = route_thread_session(adapter, features, summon.origin).await?;
      features
        .store
        .create_task(
          CreateTask {
            request_id: format!("{}-summon-{session_id}", platform.name),
            task_parent_id: None,
            profile: TaskProfile::Manager,
            task: None,
            name: Some(name),
            backend,
            group: None,
            session_id: Some(session_id.to_string()),
          },
          TaskCaller::System,
          Map::from_iter([(platform.origin_field.to_string(), summon.origin.clone())]),
        )
        .await?;
      if let Some(route) = route {
        features
          .store
          .update_slot_fields(
            session_id,
            backend_pool::METADATA_OWNER,
            Map::from_iter([("route".to_string(), json!(route))]),
          )
          .await?;
      }
      let event = format!("{}_mention_session_created", platform.name);
      tracing::info!(%event, thread = %thread, session = session_id);
    }
    Some(meta) if meta.status == SessionStatus::Archived => {
      features.store.unarchive_session(session_id).await?;
      let event = format!("{}_mention_session_unarchived", platform.name);
      tracing::info!(%event, thread = %thread, session = session_id);
      // The unarchive write alone notifies nobody: an open sidebar refetches only on a task-tree notification.
      features.store.broadcast_task_tree_changed(session_id, Some("session_unarchived")).await?;
    }
    Some(_) => {
      let event = format!("{}_mention_session_existing", platform.name);
      tracing::info!(%event, thread = %thread, session = session_id);
    }
  }

  consume_mention(platform, features, session_id, mention_id).await?;
  ensure_group(platform, features, session_id, summon.label).await;
  let dispatcher =
    features.startup.require::<charliebot_tasks::dispatch::Dispatcher>().map_err(ThreadReplyError::failure)?;
  let stored = dispatcher
    .admit_input(
      session_id,
      event_types::AGENT_MESSAGE,
      summon.content,
      "agent",
      json!({
        "request_id": request_id,
        "from_session": session_id,
        "from_session_name": platform.display_name,
        "fields": {(platform.name): summon.block},
      }),
    )
    .await?;
  let round_event_id =
    stored.get("id").and_then(ChatEvent::as_str).expect("a persisted event carries its id").to_string();
  let event = format!("{}_mention_round_started", platform.name);
  tracing::info!(%event, thread = %thread, session = session_id, user_event_id = %round_event_id);

  spawn_master_wake(
    features,
    format!("{}-round-{session_id}", platform.name),
    session_id,
    summon.content,
    &round_event_id,
    platform.display_name,
  );
  let adapter_for_ack = Arc::clone(adapter);
  let block = summon.block.clone();
  spawn_logged(format!("{}-ack-{session_id}", platform.name), async move { adapter_for_ack.add_ack(&block).await });
  Ok(session_id.to_string())
}

/// Revive an archived thread session, then arm its follow trigger (`revive_and_arm_follow`). An archived session is
/// unarchived first, because `create_trigger` rejects an archived session; the unarchive is logged and broadcast.
pub async fn revive_and_arm_follow(
  platform: &ThreadPlatform,
  features: &FeatureContext,
  meta: &SessionMetadata,
  floor: &str,
  log_fields: &Value,
) -> Result<Option<PendingTrigger>, ThreadReplyError> {
  let session_id = meta.id.as_str();
  if meta.status == SessionStatus::Archived {
    features.store.unarchive_session(session_id).await?;
    let event = format!("{}_follow_session_unarchived", platform.name);
    tracing::info!(%event, session = session_id, thread = %log_fields);
    features.store.broadcast_task_tree_changed(session_id, Some("session_unarchived")).await?;
  }
  arm_follow_trigger(platform, features, session_id, floor, log_fields).await
}

/// Arm the session's follow trigger for one eligible thread message; the session id when armed (`follow_message`).
/// The session must exist, its origin must pass `origin_matches`, and the message id must sort above the watermark.
pub async fn follow_message(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
  session_id: &str,
  message_id: &str,
  origin_matches: impl Fn(&Value) -> bool,
) -> Result<Option<String>, ThreadReplyError> {
  let platform = adapter.platform();
  let Some(meta) = features.store.get_session(session_id).await? else {
    return Ok(None);
  };
  let Some(origin) = origin_of(&meta, platform) else {
    return Ok(None);
  };
  if !origin_matches(origin) {
    return Ok(None);
  }
  let watermark = watermark_of(&meta, platform);
  if watermark.as_deref().is_some_and(|watermark| platform.id_key.key(message_id) <= platform.id_key.key(watermark)) {
    return Ok(None);
  }
  let log_fields = adapter.log_fields(&adapter.address_of(origin));
  let trigger = revive_and_arm_follow(platform, features, &meta, message_id, &log_fields).await?;
  Ok(trigger.map(|_| session_id.to_string()))
}

/// Arm the follow trigger of every followed session holding unread messages; returns the count
/// (`backfill_followed_threads`). Each armed session arms exactly once. A failing thread is logged and skipped.
pub async fn backfill_followed_threads(
  adapter: &Arc<dyn ThreadAdapter>,
  features: &Arc<FeatureContext>,
) -> Result<usize, ThreadReplyError> {
  let platform = adapter.platform();
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let active = blocking(features, |store| store.list_sessions_readonly(SessionStatus::Active)).await?;
  let archived = blocking(features, |store| store.list_sessions_readonly(SessionStatus::Archived)).await?;
  let mut armed = 0;
  for meta in active.iter().chain(archived.iter()) {
    let Some(origin) = origin_of(meta, platform) else {
      continue;
    };
    let log_fields = adapter.log_fields(&adapter.address_of(origin));
    let outcome: Result<Option<PendingTrigger>, ThreadReplyError> = async {
      let unread = unread_messages(adapter.as_ref(), origin, &config, watermark_of(meta, platform).as_deref()).await?;
      let Some(first) = unread.first() else {
        return Ok(None);
      };
      revive_and_arm_follow(platform, features, meta, &first.id, &log_fields).await
    }
    .await;
    match outcome {
      Ok(Some(_)) => armed += 1,
      Ok(None) => {}
      Err(error) => {
        let event = format!("{}_follow_backfill_thread_failed", platform.name);
        tracing::warn!(%event, session = %meta.id, thread = %log_fields, error = %error);
      }
    }
  }
  if armed > 0 {
    let event = format!("{}_follow_backfill_armed", platform.name);
    tracing::info!(%event, sessions = armed);
  }
  Ok(armed)
}

/// Refuse the reply when `text` links a CharlieBot file-server URL, or a retired or local path that thread readers
/// cannot open (`assert_no_file_server_links`). The first such finding is refused with 422, and the detail names the
/// `charliebot publish` command whose printed URL replaces it.
pub fn assert_no_file_server_links(
  text: &str,
  scan_context: Option<&file_urls::ScanContext>,
) -> Result<(), ThreadReplyError> {
  for finding in file_urls::scan_file_links(text, scan_context).map_err(ThreadReplyError::failure)? {
    if finding.form != file_urls::FileLinkForm::Canonical && !finding.dead() {
      continue;
    }
    let publish_path = finding.path.as_ref().map_or_else(|| finding.link.clone(), |path| path.display().to_string());
    let reason = if finding.form == file_urls::FileLinkForm::Canonical {
      "reply links a CharlieBot file-server URL, which opens only behind the access key"
    } else {
      "reply links a retired or local file path that thread readers cannot open"
    };
    let link = &finding.link;
    return Err(ThreadReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!(
        "{reason}: {link}. Publish the page with `charliebot publish {publish_path}` and write the URL the command \
         prints into the reply"
      ),
    ));
  }
  Ok(())
}

/// The application-route URLs on this server's `port` that the reply names, in first-appearance order, once each
/// (`application_route_links`).
pub fn application_route_links(text: &str, port: u16) -> Vec<String> {
  let mut routes: Vec<String> = Vec::new();
  for captures in file_urls::url_regex().captures_iter(text) {
    let Some(named_port) = captures.name("port") else {
      continue;
    };
    if file_urls::decimal_port(named_port.as_str()) == Some(u64::from(port))
      && !routes.iter().any(|r| r == &captures[0])
    {
      routes.push(captures[0].to_string());
    }
  }
  routes
}

/// The readback's one line naming the application-route links that reach the operator alone (`operator_only_note`).
pub fn operator_only_note(links: &[String]) -> Option<String> {
  if links.is_empty() {
    return None;
  }
  Some(format!("Application-route links stay as written; the operator alone can open them: {}", links.join(", ")))
}

/// The sessions' chat events in log order, read on a worker thread as `asyncio.to_thread` does.
async fn load_chat_events(
  features: &FeatureContext,
  session_id: &str,
) -> Result<Arc<Vec<SharedChatEvent>>, ThreadReplyError> {
  let session_id = session_id.to_string();
  blocking(features, move |store| store.load_chat_events_shared_sync(&session_id)).await
}

/// Run one blocking store read on a worker thread.
async fn blocking<T, F>(features: &FeatureContext, read: F) -> Result<T, ThreadReplyError>
where
  T: Send + 'static,
  F: FnOnce(&Store) -> StoreResult<T> + Send + 'static,
{
  let store = Arc::clone(&features.store);
  let joined = tokio::task::spawn_blocking(move || read(&store)).await;
  Ok(joined.map_err(ThreadReplyError::failure)??)
}

/// Run `future` as its own task; a failure logs `background_task_failed` (`tasks.create_logged_task`).
pub(crate) fn spawn_logged<E, F>(name: String, future: F) -> JoinHandle<()>
where
  E: Display,
  F: Future<Output = Result<(), E>> + Send + 'static,
{
  tokio::spawn(async move {
    if let Err(error) = future.await {
      tracing::error!(task_name = %name, error = %error, "background_task_failed");
    }
  })
}

/// Wake the session's manager on one input, as a logged task (the round wake and the nudge wake).
fn spawn_master_wake(
  features: &Arc<FeatureContext>,
  task_name: String,
  session_id: &str,
  content: &str,
  input_id: &str,
  from_session_name: &str,
) {
  let features = Arc::clone(features);
  let session_id = session_id.to_string();
  let content = content.to_string();
  let input_id = input_id.to_string();
  let from_session_name = from_session_name.to_string();
  spawn_logged(task_name, async move {
    master_trigger::trigger_master(
      &features,
      TriggerMasterRequest {
        session_id: &session_id,
        summary: &content,
        input_id: Some(&input_id),
        event_type: event_types::AGENT_MESSAGE,
        actor: "agent",
        from_session: Some(&session_id),
        from_session_name: Some(&from_session_name),
        pull_back: true,
      },
    )
    .await
  });
}

/// The session's chat-event `type`.
fn event_type(event: &Value) -> Option<&str> {
  event.get("type").and_then(Value::as_str)
}

/// The chat event's `id`. The store stamps every event with one, so a missing id is a broken log.
fn event_id(event: &Value) -> &str {
  event.get("id").and_then(Value::as_str).expect("a chat event carries a string id")
}

/// The house-timezone wall clock of a session name: `YYYY-MM-DD HH:MM` (`_local_time`).
fn local_time() -> String {
  chrono::Utc::now().with_timezone(&HOUSE_TIMEZONE).format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests;
