//! Per-session read routes used by the chat view.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_agents::model_pool::MISSING_BACKEND_OPTION;
use charliebot_agents::pool_display::session_detail_shown;
use charliebot_cli_common::fields::LIMIT;
use charliebot_compat::compression::{accepts_gzip, gzip_level_one};
use charliebot_compat::mimetypes_table::APPLICATION_JSON;
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, static_files};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::headers::ACCEPT_ENCODING;
use charliebot_core::text_repr;
use charliebot_core::timefmt;
use charliebot_registry::FeatureContext;
use charliebot_registry::task_errors::task_http_error;
use charliebot_registry::thread_origin::is_thread_session;
use charliebot_store::control_events::SOURCE_SESSION_ID_FIELD;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{HostBoot, RunFacts, RunOrder, RunRecord};
use charliebot_store::trigger_files::{PendingTrigger, TriggerStatus};
use charliebot_store::{Store, StoreError, StoreResult, TaskFacts, path_error};
use charliebot_triggers::triggers::TriggerManager;
use http::header::HeaderValue;
use lru::LruCache;
use serde_json::{Map, Value, json};

use crate::api;
use crate::route_support::{Query, blocking_store};
use crate::{listing, worker_transcript};

const BOOTSTRAP_MESSAGE_LIMIT: usize = 40;
/// The bootstrap field of the For-you rows before the first screen, oldest first. Every session carries it; a worker
/// or thread session carries an empty array.
const OLDER_FOR_YOU_FIELD: &str = "older_for_you";
/// The wire budget of the bootstrap's first screen. The payload trims its message tail until its
/// gzip fits the budget, so a switch's own transfer sets its time on a slow link. The messages the
/// budget drops reach the page through the scroll-up paging the tail's cursor already feeds.
const FIRST_SCREEN_WIRE_BUDGET_BYTES: usize = 10 * 1024;
/// The response header carrying the wire bytes the payload would take without the trim: the size a
/// realistic switch target's content has to reach. The header also states what the trim saved on
/// this answer.
pub const BOOTSTRAP_UNTRIMMED_BYTES_HEADER: &str = "x-bootstrap-untrimmed-bytes";
const FILE_GZIP_MEMO_LIMIT: usize = 2;
/// The most lines one events window may ask for.
const EVENTS_WINDOW_MAX_LINES: usize = 2_000;
/// The bytes one backwards read of an events window examines at a time.
const EVENTS_WINDOW_CHUNK: u64 = 64 * 1024;

/// The compressed body of an events file, keyed by its path and `(mtime, size)`.
///
/// The value is a [`Bytes`] handle, so a hit shares the one stored buffer through a refcount bump.
/// The largest production log holds about 18 MB of compressed bytes.
type FileGzipMemo = Mutex<LruCache<(PathBuf, i64, u64), Bytes>>;

static EVENTS_FILE_GZIP_MEMO: LazyLock<FileGzipMemo> = LazyLock::new(|| {
  Mutex::new(LruCache::new(NonZeroUsize::new(FILE_GZIP_MEMO_LIMIT).expect("the gzip memo limit is nonzero")))
});

/// The session detail, transcript, usage, events, and task-read routes.
pub(crate) fn routes() -> Router<Arc<FeatureContext>> {
  Router::new()
    .route(api::SESSION_PENDING_TRIGGERS_PATH, get_only(pending_triggers))
    .route(api::SESSION_BOOTSTRAP_PATH, get_only(session_bootstrap))
    .route("/api/sessions/{session_id}/usage", get_only(session_usage))
    .route(api::SESSION_EVENTS_PATH, get_only(session_events))
    .route("/api/sessions/{session_id}/transcript", get_only(session_transcript))
    .route(api::SESSION_EVENTS_JSONL_PATH, get_only(events_jsonl))
    .route(api::SESSION_RUNS_PATH, get_only(session_runs))
    .route("/api/sessions/{session_id}/task-inputs/pending", get_only(pending_task_inputs))
}

/// The bootstrap payload of one build, with the wire bytes the payload takes without the first
/// screen's trim.
pub struct BootstrapPayload {
  pub value: Value,
  /// The gzip bytes the untrimmed payload takes: what the trim saved, and the size a realistic
  /// switch target's content has to reach (`BOOTSTRAP_UNTRIMMED_BYTES_HEADER`).
  pub untrimmed_wire_bytes: usize,
}

/// The bootstrap payload for a session, shared with the future index-page owner.
///
/// The payload renders the session's message tail trimmed to [`FIRST_SCREEN_WIRE_BUDGET_BYTES`] on
/// the wire, so the switch's own transfer sets its time on a slow link. The trim drops the tail's
/// oldest whole turns and moves the paging cursor onto the first kept one. The dropped history
/// still reaches the page through scroll-up paging. The tail's newest visible row always renders on
/// the first screen. When the turn holding it (the running turn, or the last finished one while nothing
/// runs) is over the budget alone, the cut lands inside that turn, keeping its newest events that fit,
/// and the payload may then exceed the budget by that row.
pub fn build_bootstrap_payload(
  store: &Store,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> StoreResult<BootstrapPayload> {
  let response_meta = response_metadata(store, meta)?;
  let mut worker_summary = None;
  // The tail the payload renders, oldest first, with the ordinal of its first slot in the space
  // the session's older pages read. That ordinal is a message ordinal in a worker's transcript
  // projection, and an event index for every other session, whose events fold to messages one to
  // many. The events branch keeps the fold of the whole tail, which is the payload's first trim step.
  enum Tail {
    Slots { messages: Vec<Value>, start: usize },
    Events { events: Vec<charliebot_store::ChatEvent>, start: usize, messages: Vec<Value> },
  }
  let (tail, pending_draft, event_count, newest) = if meta.profile == TaskProfile::Worker {
    let transcript = worker_transcript::load_worker_transcript(store, &meta.id)?;
    let (messages, oldest, _has_more) = transcript.projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
    worker_summary = Some(transcript.summary.clone());
    let pending_draft = transcript.projection.pending_draft().cloned();
    let event_count = transcript.projection.event_count();
    let separators: Vec<usize> = messages
      .iter()
      .enumerate()
      .filter(|(_, message)| message["role"] == SEPARATOR_ROLE)
      .map(|(index, _)| index)
      .collect();
    let rows =
      messages.iter().enumerate().filter(|(_, message)| message["role"] != SEPARATOR_ROLE).map(|(index, _)| index);
    let newest =
      NewestTurn::of(messages.len(), &separators, rows, |separator| worker_turn_start_after(&messages, separator));
    (Tail::Slots { messages, start: oldest }, pending_draft, event_count, newest)
  } else {
    let archive_offset = usize::try_from(meta.archive_offset.max(0)).expect("non-negative archive offset fits usize");
    // A turn's events fold to more than one message, so a read of the message budget's count of events can fold to
    // fewer messages than the page shows. The read that ends the loop keeps its fold: the payload's first trim step
    // takes that same tail, and a longer tail only adds whole turns the trim drops. The next read therefore holds
    // the events this session's own ratio needs, never the next doubling of the last.
    // The read covers the log's last `read` lines, and a line that does not parse is no event. The cursor counts
    // the lines the tail covers, not the events they parse to, so a malformed line never advances it.
    let mut read = BOOTSTRAP_MESSAGE_LIMIT;
    let (events, total_count, oldest, messages, pending_draft) = loop {
      let (events, total_count, _has_more) = store.load_chat_events_tail(&meta.id, read)?;
      let complete = total_count <= read;
      let oldest = archive_offset + total_count.saturating_sub(read.min(total_count));
      let (messages, pending_draft) = store.events_to_view_at(&events, oldest)?;
      if messages.len() >= BOOTSTRAP_MESSAGE_LIMIT || complete {
        break (events, total_count, oldest, messages, pending_draft);
      }
      read = next_bootstrap_read(read, messages.len(), total_count);
    };
    let separators: Vec<usize> = events
      .iter()
      .enumerate()
      .filter(|(_, event)| {
        matches!(event["type"].as_str(), Some(event_types::MASTER_DONE) | Some(event_types::RUN_END))
      })
      .map(|(index, _)| index)
      .collect();
    let rows = messages
      .iter()
      .filter(|message| message["role"] != SEPARATOR_ROLE)
      .filter_map(|message| message[event_types::EVENT_INDEX].as_u64())
      .map(|index| index as usize - oldest);
    let newest =
      NewestTurn::of(events.len(), &separators, rows, |separator| events_turn_start_after(&events, separator));
    (Tail::Events { events, start: oldest, messages }, pending_draft, archive_offset + total_count, newest)
  };

  // The payload's fields that no trim step changes, built once. Each step clones them and inserts
  // the tail it keeps, so a trim of the largest tail costs its turns' serializations alone.
  let mut base = Map::new();
  base.insert("session".to_string(), listing::metadata_value(store, &response_meta)?);
  base.insert("pending_draft".to_string(), pending_draft.unwrap_or(Value::Null));
  base.insert("event_count".to_string(), Value::from(event_count));
  if let Some(summary) = worker_summary {
    base.insert(event_types::WORKER_SUMMARY.to_string(), summary);
  }
  base.extend(active_backend_payload(store, &response_meta, cfg)?);

  // The payload of one trim step: the tail kept from slot `drop` on, its paging cursor moved onto
  // the first kept slot, and `has_more` true whenever older history remains.
  let mut build = |drop: usize| -> StoreResult<Value> {
    let (messages, start) = match &tail {
      Tail::Slots { messages, start, .. } => (messages[drop..].to_vec(), start + drop),
      Tail::Events { events, start, messages } => {
        if drop == 0 {
          (messages.clone(), *start)
        } else {
          let (messages, _) = store.events_to_view_at(&events[drop..], start + drop)?;
          (messages, start + drop)
        }
      }
    };
    let mut payload = base.clone();
    payload.insert("messages".to_string(), Value::Array(messages));
    payload.insert(api::OLDEST_MESSAGE_ORDINAL_FIELD.to_string(), Value::from(start));
    payload.insert(api::HAS_MORE_FIELD.to_string(), Value::Bool(start > 0));
    Ok(Value::Object(payload))
  };
  // The next turn start a trim step may drop to, in the tail's own indexes (the `drop` a `build` step takes). The
  // trim accepts a step only up to the start of the turn holding the newest visible row, so every older turn is dropped
  // whole. A worker's step reads the separator messages of its tail, and every other session reads the events behind
  // the fold's turn separators (master_done, run_end).
  let next_drop = |drop: usize| -> Option<usize> {
    match &tail {
      Tail::Slots { messages, .. } => messages
        .iter()
        .enumerate()
        .skip(drop)
        .find(|(_, message)| message["role"] == SEPARATOR_ROLE)
        .map(|(index, _)| worker_turn_start_after(messages, index)),
      Tail::Events { events, .. } => events
        .iter()
        .enumerate()
        .skip(drop)
        .find(|(_, event)| {
          matches!(event["type"].as_str(), Some(event_types::MASTER_DONE) | Some(event_types::RUN_END))
        })
        .map(|(index, _)| events_turn_start_after(events, index)),
    }
  };
  let (mut value, untrimmed_wire_bytes) = fit_first_screen_payload(&mut build, &next_drop, newest)?;
  // The older rows stay outside the first screen's wire budget: they are the For-you catch-up the page shows before
  // its older pages land, so a trim never drops a turn to make room for them.
  let oldest = value[api::OLDEST_MESSAGE_ORDINAL_FIELD].as_u64().expect("the payload names its oldest ordinal");
  let older_for_you = if is_web_session(meta) {
    store.for_you_rows_before(&meta.id, usize::try_from(oldest).expect("an ordinal fits usize"))?
  } else {
    Vec::new()
  };
  value
    .as_object_mut()
    .expect("the bootstrap payload is an object")
    .insert(OLDER_FOR_YOU_FIELD.to_string(), Value::Array(older_for_you));
  Ok(BootstrapPayload { value, untrimmed_wire_bytes })
}

/// A web session: a manager session that no chat thread owns. Its For-you view opens on the page's own rule.
fn is_web_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager && !is_thread_session(meta)
}

/// The next tail read after one that folded `read` events to `messages` messages. It holds the events the message
/// budget needs at that ratio, bounded to eight times the last read and to the log's own line count.
///
/// The read grows by at least half, so a fold that missed the budget by a few messages still ends the loop at the
/// next read.
fn next_bootstrap_read(read: usize, messages: usize, total: usize) -> usize {
  let wanted = read.saturating_mul(BOOTSTRAP_MESSAGE_LIMIT) / messages.max(1);
  let grown = read.saturating_add((read / 2).max(1));
  grown.max(wanted.min(read.saturating_mul(8))).min(total)
}

/// The newest visible row of a tail and the turn that holds it, in the tail's own indexes: the trim's two stages both
/// stay inside that turn.
#[derive(Clone, Copy)]
struct NewestTurn {
  /// The first unit of the turn holding the newest visible row. A turn-boundary step never moves past it, so every
  /// older turn is dropped whole before that turn is cut.
  start: usize,
  /// The unit of the tail's newest visible row. A cut inside the turn never passes it.
  last_row: usize,
}

impl NewestTurn {
  /// The turn holding the newest visible row of a tail of `len` units. `separators` lists the units holding a turn
  /// separator in order, `rows` the units of the tail's visible rows, and `turn_start_after` the first unit of the turn
  /// after a separator.
  ///
  /// The row is the tail's last one, so a running turn holds it once that turn has a visible row, and the last finished
  /// turn holds it while the running turn has none. A run's `run_delivery` or `run_finished` follows its separator and
  /// belongs to the turn before it, so the next turn's start is past it.
  fn of(
    len: usize,
    separators: &[usize],
    rows: impl Iterator<Item = usize>,
    turn_start_after: impl Fn(usize) -> usize,
  ) -> NewestTurn {
    let last_row = rows.max().unwrap_or(len.saturating_sub(1));
    let start = separators
      .iter()
      .map(|separator| turn_start_after(*separator))
      .filter(|start| *start <= last_row)
      .max()
      .unwrap_or(0);
    NewestTurn { start, last_row }
  }
}

/// The role the fold gives a turn's separator row, which `master_done` and `run_end` render as.
const SEPARATOR_ROLE: &str = "separator";

/// The first event of the turn after the separator at `index`. The run's `run_finished` closes the run behind its
/// separator and folds to no message, so the turn starts past it.
fn events_turn_start_after(events: &[charliebot_store::ChatEvent], index: usize) -> usize {
  let mut next = index + 1;
  while events.get(next).is_some_and(|event| event["type"] == event_types::RUN_FINISHED) {
    next += 1;
  }
  next
}

/// The first message of the run after the separator at `index`. A worker's run closes with its `run_delivery` row right
/// after the separator, so the next run starts past that row.
fn worker_turn_start_after(messages: &[Value], index: usize) -> usize {
  index + 1 + messages[index + 1..].iter().take_while(|message| message["role"] == event_types::RUN_DELIVERY).count()
}

/// The payload with the tail kept from unit `drop` on, for the trim's smallest cut whose messages fit
/// `BOOTSTRAP_MESSAGE_LIMIT` and whose gzip fits [`FIRST_SCREEN_WIRE_BUDGET_BYTES`]. Returns the payload and the wire
/// bytes the untrimmed payload takes.
///
/// The trim runs in two stages. It first drops whole older turns, one turn-boundary step at a time, and never past the
/// start of the turn holding the newest visible row. When that turn alone is still over the budget, the trim searches
/// the cuts inside it for the smallest that fits, which keeps its newest events that fit, and it never cuts past that
/// row. A payload still over the budget at that row keeps the row, so the served size exceeds the budget by one row. Each
/// step serializes and gzips one payload, well under a millisecond of a level-one gzip at these sizes.
fn fit_first_screen_payload(
  build: &mut dyn FnMut(usize) -> StoreResult<Value>,
  next_drop: &dyn Fn(usize) -> Option<usize>,
  newest: NewestTurn,
) -> StoreResult<(Value, usize)> {
  let wire_of = |payload: &Value| gzip_level_one(&charliebot_compat::json::fast_json_bytes(payload)).len();
  let messages_of = |payload: &Value| payload["messages"].as_array().map_or(0, Vec::len);
  let over_budget = |payload: &Value| {
    wire_of(payload) > FIRST_SCREEN_WIRE_BUDGET_BYTES || messages_of(payload) > BOOTSTRAP_MESSAGE_LIMIT
  };
  let mut payload = build(0)?;
  let untrimmed_wire = wire_of(&payload);
  let mut drop = 0;
  while over_budget(&payload) {
    let Some(next) = next_drop(drop).filter(|next| *next <= newest.start) else { break };
    drop = next;
    payload = build(drop)?;
  }
  if !over_budget(&payload) || newest.last_row <= drop {
    return Ok((payload, untrimmed_wire));
  }
  // `low` is over the budget, and `high` is the largest cut the newest turn keeps its last row at. The smallest cut
  // in (low, high] that fits is the answer, when one fits at all.
  let mut low = drop;
  let mut high = newest.last_row;
  let mut fitting = build(high)?;
  if over_budget(&fitting) {
    return Ok((fitting, untrimmed_wire));
  }
  while high - low > 1 {
    let middle = low + (high - low) / 2;
    let candidate = build(middle)?;
    if over_budget(&candidate) {
      low = middle;
    } else {
      high = middle;
      fitting = candidate;
    }
  }
  Ok((fitting, untrimmed_wire))
}

/// A session page's bootstrap payload as the page carries it.
///
/// The `session` object is the payload's largest value, and the page reads it without changing it. A memo hit
/// therefore hands back the object the memo built instead of copying it.
#[derive(Clone)]
pub struct PageBootstrap {
  /// The payload's Jinja `tojson` text, the value the page's `SESSION_BOOTSTRAP` constant carries.
  pub text: Arc<String>,
  /// The payload's `session` object.
  pub session: Arc<Value>,
  pub pending_draft: Value,
  pub event_count: Value,
}

/// The page bootstraps the last few sessions produced, newest last, each kept while every payload input still holds
/// its value.
///
/// The payload is the page's largest value, and its text is its whole escaped form, so a page request that rebuilds it
/// serializes the session's message tail from scratch. A reader who moves between two sessions asks for a payload the
/// page built a moment ago, and one slot would rebuild it on every switch.
#[derive(Default)]
pub struct PageBootstrapMemo {
  entries: Mutex<Vec<(PageBootstrapKey, PageBootstrap)>>,
}

/// The bootstraps one page keeps: the session it served most recently, and the ones before it a reader comes back to.
const PAGE_BOOTSTRAP_MEMO_ENTRIES: usize = 4;

/// The identity of a session's live event log: its modification time in nanoseconds and its size.
type EventLogIdentity = (i128, u64);

/// The inputs [`build_bootstrap_payload`] reads for a session whose events it reads from the live log.
///
/// The tail the payload renders is the log's last events, so the log's identity names that tail: an appended event
/// moves it. The metadata compares by content, because the store hands a caller its own copy of the row. Everything
/// else compares by value, except the config, which compares by pointer because a reload builds a new one.
struct PageBootstrapKey {
  session_id: String,
  meta: SessionMetadata,
  run_backend: Option<String>,
  thinking_since: Option<UtcDateTime>,
  backend: Map<String, Value>,
  config: Arc<CharlieBotConfig>,
  events: EventLogIdentity,
}

/// One request's payload inputs, borrowed from the row the request read, so a memo hit compares them without
/// copying the row or building the key it already holds.
struct PageBootstrapInputs<'a> {
  session_id: &'a str,
  meta: &'a SessionMetadata,
  run_backend: Option<String>,
  thinking_since: Option<UtcDateTime>,
  backend: Map<String, Value>,
  config: &'a Arc<CharlieBotConfig>,
  events: EventLogIdentity,
}

impl PageBootstrapKey {
  /// The key of one request's inputs, for a memo that holds no entry matching them.
  fn from_inputs(inputs: &PageBootstrapInputs<'_>) -> PageBootstrapKey {
    PageBootstrapKey {
      session_id: inputs.session_id.to_string(),
      meta: inputs.meta.clone(),
      run_backend: inputs.run_backend.clone(),
      thinking_since: inputs.thinking_since,
      backend: inputs.backend.clone(),
      config: Arc::clone(inputs.config),
      events: inputs.events,
    }
  }

  /// Whether this key's inputs still hold these values.
  fn matches(&self, inputs: &PageBootstrapInputs<'_>) -> bool {
    self.session_id == inputs.session_id
      && self.meta == *inputs.meta
      && self.run_backend == inputs.run_backend
      && self.thinking_since == inputs.thinking_since
      && self.backend == inputs.backend
      && Arc::ptr_eq(&self.config, inputs.config)
      && self.events == inputs.events
  }
}

/// One body-memo entry: the payload key, the JSON body, its gzip when built, and the wire bytes the
/// untrimmed payload takes.
type BodyMemoEntry = (PageBootstrapKey, Bytes, Option<Bytes>, usize);

/// The bootstrap route's JSON body one request built last, with its gzip when a request asked for
/// one and the wire bytes the untrimmed payload takes.
///
/// The body carries the session's message tail: the largest session's is about a hundred megabytes, and building and
/// serializing it takes about two hundred milliseconds. A repeat request whose payload inputs still hold their value
/// serves the bytes already built. The key is the page bootstrap's, so a page and this route memoize the same payload
/// under the same inputs. One entry holds one session, as the page's memo does.
#[derive(Default)]
struct BootstrapBodyMemo {
  entry: Mutex<Option<BodyMemoEntry>>,
}

static BOOTSTRAP_BODY_MEMO: LazyLock<BootstrapBodyMemo> = LazyLock::new(BootstrapBodyMemo::default);

/// The bootstrap payload's JSON body for `meta`, its gzip when `accepts_gzip`, and the untrimmed
/// wire size, from `memo` when every payload input still holds its value.
fn bootstrap_body(
  memo: &BootstrapBodyMemo,
  store: &Store,
  cfg: &Arc<CharlieBotConfig>,
  meta: &SessionMetadata,
  accepts_gzip: bool,
) -> StoreResult<(Bytes, Option<Bytes>, usize)> {
  let inputs = page_bootstrap_inputs(store, cfg, meta)?;
  let mut entry = memo.entry.lock().expect("the bootstrap body memo mutex is not poisoned");
  let cached = match &inputs {
    Some(inputs) => match entry.as_ref() {
      Some((cached, body, gzip, untrimmed)) if cached.matches(inputs) => Some((body.clone(), gzip.clone(), *untrimmed)),
      None | Some(_) => None,
    },
    None => None,
  };
  let (body, mut gzip, untrimmed, held) = match cached {
    Some(cached) => (cached.0, cached.1, cached.2, true),
    None => {
      let payload = build_bootstrap_payload(store, cfg, meta)?;
      let body = Bytes::from(charliebot_compat::json::fast_json_bytes(&payload.value));
      let untrimmed = payload.untrimmed_wire_bytes;
      let held = inputs.is_some();
      if let Some(inputs) = &inputs {
        *entry = Some((PageBootstrapKey::from_inputs(inputs), body.clone(), None, untrimmed));
      }
      (body, None, untrimmed, held)
    }
  };
  if !accepts_gzip {
    return Ok((body, None, untrimmed));
  }
  if gzip.is_none() {
    let compressed = Bytes::from(gzip_level_one(&body));
    if held {
      entry.as_mut().expect("the memo holds the body it built").2 = Some(compressed.clone());
    }
    gzip = Some(compressed);
  }
  Ok((body, gzip, untrimmed))
}

/// The bootstrap payload of `meta`, with its text taken from `memo` when every payload input still holds its value.
///
/// A memo hit hands back the payload the memo holds, whose `session` object it shares, so a page request serves the
/// largest value the page carries without copying it.
pub fn page_bootstrap(
  memo: &PageBootstrapMemo,
  store: &Store,
  cfg: &Arc<CharlieBotConfig>,
  meta: &SessionMetadata,
) -> StoreResult<PageBootstrap> {
  let inputs = page_bootstrap_inputs(store, cfg, meta)?;
  if let Some(inputs) = &inputs {
    let mut entries = memo.entries.lock().expect("the page bootstrap memo mutex is not poisoned");
    if let Some(at) = entries.iter().position(|(cached_key, _)| cached_key.matches(inputs)) {
      // The session served again is the newest entry, so an eviction takes the one served longest ago.
      let entry = entries.remove(at);
      let bootstrap = entry.1.clone();
      entries.push(entry);
      return Ok(bootstrap);
    }
  }
  let payload = build_bootstrap_payload(store, cfg, meta)?;
  let bootstrap = PageBootstrap {
    text: Arc::new(charliebot_compat::jinja::tojson_text(&payload.value)),
    session: Arc::new(payload.value["session"].clone()),
    pending_draft: payload.value["pending_draft"].clone(),
    event_count: payload.value["event_count"].clone(),
  };
  if let Some(inputs) = inputs {
    let mut entries = memo.entries.lock().expect("the page bootstrap memo mutex is not poisoned");
    entries.push((PageBootstrapKey::from_inputs(&inputs), bootstrap.clone()));
    if entries.len() > PAGE_BOOTSTRAP_MEMO_ENTRIES {
      entries.remove(0);
    }
  }
  Ok(bootstrap)
}

/// The payload inputs of `meta`, or `None` when the payload reads state the memo key cannot name.
///
/// A worker session's transcript and an archived session's event file are read fresh for every payload, so neither
/// session kind is memoized.
fn page_bootstrap_inputs<'a>(
  store: &Store,
  cfg: &'a Arc<CharlieBotConfig>,
  meta: &'a SessionMetadata,
) -> StoreResult<Option<PageBootstrapInputs<'a>>> {
  if meta.profile == TaskProfile::Worker || meta.archive_offset != 0 {
    return Ok(None);
  }
  let Some(events) = live_event_log_identity(store, &meta.id)? else { return Ok(None) };
  // The payload renders the displayed backend from the row's own backend, its profile and its bound picker entries,
  // so the row itself carries every input this payload reads.
  let backend = active_backend_payload(store, meta, cfg)?;
  Ok(Some(PageBootstrapInputs {
    session_id: &meta.id,
    meta,
    run_backend: listing::run_backend(store, meta)?,
    thinking_since: store.busy_since(&meta.id),
    backend,
    config: cfg,
    events,
  }))
}

/// The identity of a session's live event log: the modification time in nanoseconds and the size.
///
/// A payload that reads the log's tail holds this pair. An appended event moves the pair, so the memo no
/// longer serves the body it built before the append.
fn live_event_log_identity(store: &Store, session_id: &str) -> StoreResult<Option<EventLogIdentity>> {
  let path = store.chat_events_path(session_id);
  match fs::metadata(&path) {
    Ok(metadata) => Ok(Some((timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()), metadata.len()))),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(path_error(&path, error)),
  }
}

pub(crate) async fn session_detail(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let store = Arc::clone(&features.store);
  match blocking_store(move || session_detail_shown(&store, &meta.id)).await {
    Ok(payload) => responses::fast_json(StatusCode::OK, &payload),
    Err(error) => task_http_error(error),
  }
}

/// `GET /api/sessions/{session_id}/pending-triggers`: the session's pending delayed triggers, `fire_at` ascending.
/// Fired and cancelled records stay out: the transcript shows fired triggers, and the cancel route owns removal.
async fn pending_triggers(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  if let Err(error) = features.store.require_session(&session_id).await {
    return store_error(error);
  }
  let manager = match features.startup.require::<Arc<TriggerManager>>() {
    Ok(manager) => manager,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let triggers = match manager.list_triggers(&session_id).await {
    Ok(triggers) => triggers,
    Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  let mut pending: Vec<PendingTrigger> =
    triggers.into_iter().filter(|trigger| trigger.status == TriggerStatus::Pending).collect();
  // A stable sort keeps `list_triggers`' creation order among equal deadlines.
  pending.sort_by_key(|trigger| trigger.fire_at);
  let values = match pending.iter().map(serde_json::to_value).collect::<Result<Vec<Value>, _>>() {
    Ok(values) => values,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  responses::json(StatusCode::OK, &Value::Array(values))
}

async fn session_bootstrap(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let store = Arc::clone(&features.store);
  let wants_gzip = accepts_gzip(request.headers());
  let result =
    tokio::task::spawn_blocking(move || bootstrap_body(&BOOTSTRAP_BODY_MEMO, &store, &cfg, &meta, wants_gzip)).await;
  match result {
    Ok(Ok((_body, Some(gzip), untrimmed))) => bootstrap_response(gzip, Some("gzip"), untrimmed),
    Ok(Ok((body, None, untrimmed))) => bootstrap_response(body, None, untrimmed),
    Ok(Err(error)) => store_error(error),
    Err(error) => store_error(StoreError::new(error.to_string())),
  }
}

async fn session_usage(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let usage_owner = match features.startup.require::<charliebot_usage::SessionUsage>() {
    Ok(usage) => usage,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let run_kind = charliebot_usage::usage_run_kind(&meta);
  let (usage, active_run_id) = if meta.profile == TaskProfile::Worker {
    match worker_transcript::load_worker_transcript(&features.store, &session_id) {
      Ok(transcript) => {
        let events = transcript.events.iter().map(Arc::as_ref).collect::<Vec<_>>();
        (
          usage_owner.resolve_events_usage(&session_id, &events, run_kind, cfg.get_backend_option(&meta.backend)),
          transcript.active_run_id.clone(),
        )
      }
      Err(error) => return store_error(error),
    }
  } else {
    (usage_owner.resolve_session_usage(&session_id, &meta, cfg.get_backend_option(&meta.backend)), None)
  };
  let usage = match usage {
    Ok(usage) => usage.unwrap_or(Value::Null),
    Err(error) => return store_error(error),
  };
  let mut payload = Map::new();
  payload.insert(
    "session".to_string(),
    match listed_session_metadata(&features.store, &meta) {
      Ok(value) => value,
      Err(error) => return store_error(error),
    },
  );
  payload.insert("usage".to_string(), usage);
  if meta.profile == TaskProfile::Worker {
    payload.insert("active_run_id".to_string(), active_run_id.map_or(Value::Null, Value::String));
  }
  let mut response_meta = meta;
  response_meta.run_backend = match listing::run_backend(&features.store, &response_meta) {
    Ok(backend) => backend,
    Err(error) => return store_error(error),
  };
  response_meta.thinking_since = features.store.busy_since(&session_id);
  match active_backend_payload(&features.store, &response_meta, &cfg) {
    Ok(fields) => payload.extend(fields),
    Err(error) => return store_error(error),
  }
  responses::fast_json(StatusCode::OK, &Value::Object(payload))
}

/// The events pages this route keeps, each the body it built while its inputs still held their value.
///
/// A page is fixed by the lines it covers. A page far inside a long log covers lines no append touches, so the
/// same request answers the same bytes until the log is rewritten. Building it parses and projects the page's events
/// on every request. A reader who scrolls back and forth asks for the same pages again, so the memo serves the
/// body it already built. A body past [`EVENTS_PAGE_MEMO_MAX_BODY_BYTES`] is not kept.
#[derive(Default)]
struct EventsPageMemo {
  entries: Mutex<Vec<(EventsPageKey, Bytes)>>,
}

/// The pages one server keeps: a reader's last few pages, which its scrolling moves between.
const EVENTS_PAGE_MEMO_ENTRIES: usize = 4;

/// The longest body the memo keeps. A page whose messages run large can pass it at any limit, and such a page builds
/// on every request rather than holding megabytes of body per entry.
const EVENTS_PAGE_MEMO_MAX_BODY_BYTES: usize = 1024 * 1024;

/// The inputs an events page body reads are the session, the page's range in the absolute event index, and the
/// archive split that index counts. The key also holds the identity of the live log the range ends in.
///
/// An appended event moves the log's identity, so a page the new event can reach builds again. An archive write moves
/// the offset, which names which events the range covers.
struct EventsPageKey {
  session_id: String,
  before: i64,
  limit: usize,
  archive_offset: usize,
  events: EventLogIdentity,
}

impl EventsPageKey {
  /// Whether this key's inputs still hold these values.
  fn matches(&self, inputs: &EventsPageInputs<'_>) -> bool {
    self.session_id == inputs.session_id
      && self.before == inputs.before
      && self.limit == inputs.limit
      && self.archive_offset == inputs.archive_offset
      && self.events == inputs.events
  }
}

/// One request's page inputs, borrowed from the values the request read, so a memo hit compares them without building
/// the key it already holds.
struct EventsPageInputs<'a> {
  session_id: &'a str,
  before: i64,
  limit: usize,
  archive_offset: usize,
  events: EventLogIdentity,
}

static EVENTS_PAGE_MEMO: LazyLock<EventsPageMemo> = LazyLock::new(EventsPageMemo::default);

impl EventsPageMemo {
  /// The body this memo built for the request's inputs, moved to the newest slot.
  fn served(&self, inputs: &EventsPageInputs<'_>) -> Option<Bytes> {
    let mut entries = self.entries.lock().expect("the events page memo mutex is not poisoned");
    let at = entries.iter().position(|(key, _)| key.matches(inputs))?;
    let entry = entries.remove(at);
    let body = entry.1.clone();
    entries.push(entry);
    Some(body)
  }

  /// Keeps `body` for the request's inputs, evicting the page served longest ago.
  fn keep(&self, inputs: &EventsPageInputs<'_>, body: Bytes) {
    if body.len() > EVENTS_PAGE_MEMO_MAX_BODY_BYTES {
      return;
    }
    let key = EventsPageKey {
      session_id: inputs.session_id.to_string(),
      before: inputs.before,
      limit: inputs.limit,
      archive_offset: inputs.archive_offset,
      events: inputs.events,
    };
    let mut entries = self.entries.lock().expect("the events page memo mutex is not poisoned");
    entries.push((key, body));
    while entries.len() > EVENTS_PAGE_MEMO_ENTRIES {
      entries.remove(0);
    }
  }
}

async fn session_events(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let query = Query::new(&request);
  let before = match query.parse_int("before", None, None, None) {
    Ok(before) => before,
    Err(response) => return *response,
  };
  let raw_limit = match query.parse_int(LIMIT, Some(40), None, None) {
    Ok(limit) => limit,
    Err(response) => return *response,
  };
  let limit = raw_limit.clamp(1, 200) as usize;

  if meta.profile == TaskProfile::Worker {
    let transcript = match worker_transcript::load_worker_transcript(&features.store, &session_id) {
      Ok(transcript) => transcript,
      Err(error) => return store_error(error),
    };
    let (messages, next_before, has_more) = transcript.projection.slice_before(to_index(before), limit);
    return responses::fast_json(
      StatusCode::OK,
      &json!({
        "messages": messages,
        (api::HAS_MORE_FIELD): has_more,
        (api::NEXT_BEFORE_FIELD): next_before,
      }),
    );
  }

  let archive_offset = usize::try_from(meta.archive_offset.max(0)).expect("non-negative archive offset fits usize");
  // A session reads its page from the live log, whose identity names the lines behind the answer. A session whose log
  // is gone builds its page every time, because no identity would keep the memo honest.
  let events = match live_event_log_identity(&features.store, &session_id) {
    Ok(Some(events)) => events,
    Ok(None) => return events_page_response(&features, &session_id, archive_offset, before, limit, None),
    Err(error) => return store_error(error),
  };
  let inputs = EventsPageInputs { session_id: &session_id, before, limit, archive_offset, events };
  if let Some(body) = EVENTS_PAGE_MEMO.served(&inputs) {
    return responses::fast_json_shared(StatusCode::OK, body);
  }
  events_page_response(&features, &session_id, archive_offset, before, limit, Some(inputs))
}

/// The page's response, kept in the memo under `inputs` when the page has an identity to keep it under.
fn events_page_response(
  features: &Arc<FeatureContext>,
  session_id: &str,
  archive_offset: usize,
  before: i64,
  limit: usize,
  inputs: Option<EventsPageInputs<'_>>,
) -> Response {
  let payload = match events_range_page(&features.store, session_id, archive_offset, before, limit) {
    Ok(payload) => payload,
    Err(error) => return store_error(error),
  };
  let body = Bytes::from(charliebot_compat::json::fast_json_bytes(&payload));
  if let Some(inputs) = inputs {
    EVENTS_PAGE_MEMO.keep(&inputs, body.clone());
  }
  responses::fast_json_shared(StatusCode::OK, body)
}

/// The messages of the page that ends at or before `before`, an absolute event index whose space counts the archived
/// events before the live ones.
///
/// A `before` past the history's end names the newest page, so a caller that does not know the history's length still
/// reads the newest message.
fn events_range_page(
  store: &Store,
  session_id: &str,
  archive_offset: usize,
  before: i64,
  limit: usize,
) -> StoreResult<Value> {
  let (_, live_events, _) = store.load_chat_events_tail(session_id, 0)?;
  let end = usize::try_from(before.max(0)).unwrap_or(usize::MAX).min(archive_offset + live_events);
  let start = end.saturating_sub(limit);
  let (events, has_more) = store.load_chat_events_range(session_id, start, end)?;
  let messages = store.events_to_messages_at(&events, start)?;
  Ok(json!({
    "messages": messages,
    (api::HAS_MORE_FIELD): has_more,
    (api::NEXT_BEFORE_FIELD): start,
  }))
}

async fn session_transcript(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let query = Query::new(&request);
  let after = match query.parse_int("after", Some(0), Some(0), None) {
    Ok(after) => after,
    Err(response) => return *response,
  };
  let revision = query.get("revision").unwrap_or("");
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  if meta.profile != TaskProfile::Worker {
    return responses::json(
      StatusCode::BAD_REQUEST,
      &json!({"detail": format!("session {session_id} has no worker transcript")}),
    );
  }
  let transcript = match worker_transcript::load_worker_transcript(&features.store, &session_id) {
    Ok(transcript) => transcript,
    Err(error) => return store_error(error),
  };
  let reset = revision.is_empty() || revision != transcript.revision;
  let committed = transcript.projection.committed();
  let messages = if reset { committed } else { committed.get(to_index(after)..).unwrap_or(&[]) };
  let thinking_since = features.store.busy_since(&session_id).map_or(Value::Null, |at| json!(at.to_isoformat_text()));
  let body = transcript_json_body(TranscriptJson {
    messages,
    total: committed.len(),
    pending_draft: transcript.projection.pending_draft(),
    revision: &transcript.revision,
    reset,
    active_run_id: transcript.active_run_id.as_deref(),
    summary: &transcript.summary,
    thinking_since: &thinking_since,
  });
  Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, APPLICATION_JSON)
    .header(header::CONTENT_LENGTH, body.len())
    .body(Body::from(body))
    .expect("worker transcript response builds")
}

// Borrow the cached projection through serialization.
struct TranscriptJson<'a> {
  messages: &'a [std::sync::Arc<Value>],
  total: usize,
  pending_draft: Option<&'a Value>,
  revision: &'a str,
  reset: bool,
  active_run_id: Option<&'a str>,
  summary: &'a Value,
  thinking_since: &'a Value,
}

fn transcript_json_body(transcript: TranscriptJson<'_>) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(b"{\"messages\":[");
  for (index, message) in transcript.messages.iter().enumerate() {
    if index > 0 {
      body.push(b',');
    }
    body.extend_from_slice(&charliebot_compat::json::fast_json_bytes(message.as_ref()));
  }
  body.extend_from_slice(b"],\"total\":");
  append_fast_json(&mut body, &json!(transcript.total));
  body.extend_from_slice(b",\"pending_draft\":");
  if let Some(pending_draft) = transcript.pending_draft {
    append_fast_json(&mut body, pending_draft);
  } else {
    body.extend_from_slice(b"null");
  }
  body.extend_from_slice(b",\"revision\":");
  append_fast_json(&mut body, &json!(transcript.revision));
  body.extend_from_slice(b",\"reset\":");
  append_fast_json(&mut body, &json!(transcript.reset));
  body.extend_from_slice(b",\"active_run_id\":");
  append_fast_json(&mut body, &json!(transcript.active_run_id));
  body.extend_from_slice(b",\"summary\":");
  append_fast_json(&mut body, transcript.summary);
  body.extend_from_slice(b",\"thinking_since\":");
  append_fast_json(&mut body, transcript.thinking_since);
  body.push(b'}');
  body
}

fn append_fast_json(body: &mut Vec<u8>, value: &Value) {
  body.extend_from_slice(&charliebot_compat::json::fast_json_bytes(value));
}

async fn events_jsonl(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let path = features.store.chat_events_path(&session_id);
  let metadata = match fs::metadata(&path) {
    Ok(metadata) if metadata.is_file() => metadata,
    Ok(_) => return events_file_not_found(),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return events_file_not_found(),
    Err(error) => return responses::internal_error("OSError", format!("{}: {error}", path.display())),
  };
  let query = Query::new(&request);
  let tail = match query.get("tail") {
    Some(_) => match query.parse_int("tail", None, Some(1), Some(EVENTS_WINDOW_MAX_LINES as i64)) {
      Ok(tail) => Some(tail as usize),
      Err(response) => return *response,
    },
    None => None,
  };
  let before = match query.get("before") {
    Some(_) if tail.is_none() => {
      return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": "before requires tail"}));
    }
    Some(_) => match query.parse_int("before", None, Some(0), None) {
      Ok(before) => Some(before as u64),
      Err(response) => return *response,
    },
    None => None,
  };
  if let Some(tail) = tail {
    let size = metadata.size();
    let wants_gzip = accepts_gzip(request.headers());
    let result = tokio::task::spawn_blocking(move || {
      let (body, start) = events_window(&path, size, tail, before)?;
      let gzip = wants_gzip.then(|| Bytes::from(gzip_level_one(&body)));
      Ok::<_, StoreError>((body, gzip, start))
    })
    .await;
    return match result {
      Ok(Ok((body, gzip, start))) => events_window_response(body, gzip, start),
      Ok(Err(error)) => store_error(error),
      Err(error) => store_error(StoreError::new(error.to_string())),
    };
  }
  if !accepts_gzip(request.headers()) {
    return static_files::serve_file(&path, "application/x-ndjson", request.method(), request.headers());
  }
  let result = tokio::task::spawn_blocking(move || gzip_file(&path, metadata)).await;
  match result {
    Ok(Ok(body)) => events_gzip_response(body),
    Ok(Err(error)) => store_error(error),
    Err(error) => store_error(StoreError::new(error.to_string())),
  }
}

/// The last `tail` lines of the events file that end at or before byte `before` (the file end when `before` is
/// `None`). The tuple also names the byte offset where the window starts.
///
/// A `before` that lands inside a line snaps down to the previous line boundary, so every returned byte belongs to
/// whole lines. The scan reads backwards from the end of the window and stops once `tail` line breaks lie behind it.
/// A window of a few hundred lines therefore costs a few reads instead of a read of the whole file.
fn events_window(path: &Path, size: u64, tail: usize, before: Option<u64>) -> StoreResult<(Bytes, u64)> {
  let mut file = fs::File::open(path).map_err(|error| path_error(path, error))?;
  let mut end = before.map_or(size, |offset| offset.min(size));
  if end > 0 && end < size && read_at(&mut file, end - 1, 1)?[0] != b'\n' {
    end = line_boundary(&mut file, end)?;
  }
  // The file's final line counts as a line even when it carries no terminator.
  let mut lines = usize::from(end == size && end > 0 && read_at(&mut file, end - 1, 1)?[0] != b'\n');
  let mut cursor = end;
  loop {
    let from = cursor.saturating_sub(EVENTS_WINDOW_CHUNK);
    let chunk = read_at(&mut file, from, cursor - from)?;
    for index in (0..chunk.len()).rev() {
      if chunk[index] == b'\n' {
        lines += 1;
        if lines > tail {
          let start = from + index as u64 + 1;
          return Ok((Bytes::from(read_at(&mut file, start, end - start)?), start));
        }
      }
    }
    cursor = from;
    if cursor == 0 {
      return Ok((Bytes::from(read_at(&mut file, 0, end)?), 0));
    }
  }
}

/// The largest line boundary at or before `offset`: 0, or the byte after the last newline before it.
fn line_boundary(file: &mut fs::File, offset: u64) -> StoreResult<u64> {
  let mut cursor = offset;
  while cursor > 0 {
    let from = cursor.saturating_sub(EVENTS_WINDOW_CHUNK);
    let chunk = read_at(file, from, cursor - from)?;
    if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
      return Ok(from + index as u64 + 1);
    }
    cursor = from;
  }
  Ok(0)
}

/// `length` bytes of the file from `start`.
fn read_at(file: &mut fs::File, start: u64, length: u64) -> StoreResult<Vec<u8>> {
  let length =
    usize::try_from(length).map_err(|_| StoreError::new(format!("events window of {length} bytes is too large")))?;
  let mut buffer = vec![0u8; length];
  file.seek(SeekFrom::Start(start)).map_err(|error| StoreError::new(error.to_string()))?;
  file.read_exact(&mut buffer).map_err(|error| StoreError::new(error.to_string()))?;
  Ok(buffer)
}

async fn session_runs(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let query = Query::new(&request);
  let limit = match query.parse_int(LIMIT, Some(100), Some(1), Some(500)) {
    Ok(limit) => limit as usize,
    Err(response) => return *response,
  };
  let cursor = query.get("cursor");
  let order = query.get("order").unwrap_or("asc");
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let run_order = match order {
    "asc" => RunOrder::Ascending,
    "desc" => RunOrder::Descending,
    other => {
      return responses::json(
        StatusCode::BAD_REQUEST,
        &json!({"detail": format!("unknown runs order: {} (use 'asc' or 'desc')", text_repr::str_repr(other))}),
      );
    }
  };
  let page = match features.store.list_runs_page_sync(&meta.id, cursor, limit, run_order) {
    Ok(page) => page,
    Err(error) if is_bad_run_cursor(&error) => {
      return responses::json(StatusCode::BAD_REQUEST, &json!({"detail": error.to_string()}));
    }
    Err(error) => return store_error(error),
  };
  let node = match features.store.node_state(&meta.id) {
    Ok(node) => node,
    Err(error) => return store_error(error),
  };
  let mut boot = HostBoot::default();
  let mut items = Vec::with_capacity(page.items.len());
  for run in page.items {
    let state = match node.display_state(&run, &mut boot) {
      Ok(state) => state.as_str(),
      Err(error) => return store_error(error),
    };
    let mut row = match serde_json::to_value(&run) {
      Ok(row) => row,
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    };
    let object = row.as_object_mut().expect("run record serializes to an object");
    object.insert("state".to_string(), Value::String(state.to_string()));
    object.insert("stop_requested".to_string(), Value::Bool(node.runs.stop_requested(&run.id, None)));
    items.push(row);
  }
  responses::fast_json(StatusCode::OK, &json!({"items": items, "next_cursor": page.next_cursor}))
}

async fn pending_task_inputs(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let node_facts = match features.store.node_facts(&meta.id) {
    Ok(facts) => facts,
    Err(error) => return task_http_error(error),
  };
  let facts = match node_facts.task() {
    Ok(facts) => facts,
    Err(error) => return task_http_error(error),
  };
  let run_records = match features.store.list_run_records_sync(&meta.id) {
    Ok(runs) => runs,
    Err(error) => return task_http_error(error),
  };
  let run_facts = match features.store.run_facts(&meta.id) {
    Ok(facts) => facts,
    Err(error) => return task_http_error(error),
  };
  let pending = match pending_input_events(facts, &run_records, &run_facts, &meta.id) {
    Ok(pending) => pending,
    Err(error) => return task_http_error(error),
  };
  let items = match pending.iter().map(pending_input_value).collect::<StoreResult<Vec<Value>>>() {
    Ok(items) => items,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &json!({"items": items}))
}

fn pending_input_events(
  facts: &TaskFacts,
  runs: &[RunRecord],
  run_facts: &RunFacts,
  session_id: &str,
) -> StoreResult<Vec<Value>> {
  if facts.boundary_index.is_none() && facts.imported_pending_ids.is_empty() {
    return Err(charliebot_store::task_node_absence::not_a_task_tree_node(session_id));
  }
  let mut claimed = HashSet::new();
  for run in runs {
    let launched = run.pid.is_some() || run_facts.terminal_outcome(&run.id)?.is_some();
    if !launched && run_facts.stop_requested(&run.id, None) {
      continue;
    }
    claimed.extend(run.input_event_ids.iter().cloned());
  }
  let mut seen = HashSet::new();
  Ok(
    facts
      .input_candidates
      .iter()
      .map(|event| event.to_value())
      .filter(|event| {
        let input_id = text_repr::field_text(event.get("id"));
        !facts.confirmed_input_ids.contains(&input_id) && !claimed.contains(&input_id) && seen.insert(input_id)
      })
      .collect(),
  )
}

fn pending_input_value(event: &Value) -> StoreResult<Value> {
  let timestamp = match event.get("timestamp") {
    None | Some(Value::Null) => Value::Null,
    Some(Value::String(timestamp)) => {
      let timestamp = UtcDateTime::parse(timestamp)
        .map_err(|error| StoreError::new(format!("invalid pending input timestamp: {error}")))?;
      Value::String(timestamp.to_str())
    }
    Some(value) => return Err(StoreError::new(format!("invalid pending input timestamp: {value}"))),
  };
  let text = event
    .get("content")
    .filter(|value| text_repr::is_truthy(value))
    .or_else(|| event.get("summary").filter(|value| text_repr::is_truthy(value)))
    .map_or_else(String::new, |value| text_repr::field_text(Some(value)));
  Ok(json!({
    "id": text_repr::field_text(event.get("id")),
    "type": text_repr::field_text(event.get("type")),
    "timestamp": timestamp,
    "actor": event.get("actor").cloned().unwrap_or(Value::Null),
    (SOURCE_SESSION_ID_FIELD): event.get(SOURCE_SESSION_ID_FIELD).cloned().unwrap_or(Value::Null),
    (event_types::FROM_SESSION_NAME): event.get(event_types::FROM_SESSION_NAME).cloned().unwrap_or(Value::Null),
    "text": text,
  }))
}

/// The payload key holding the backend id the session page shows.
pub const ACTIVE_BACKEND_FIELD: &str = "active_backend";
/// The payload key holding that backend's configured type, empty when no configured option matches it.
pub const ACTIVE_BACKEND_TYPE_FIELD: &str = "active_backend_type";

fn active_backend_payload(
  store: &Store,
  meta: &SessionMetadata,
  cfg: &CharlieBotConfig,
) -> StoreResult<Map<String, Value>> {
  let live_run_backend = (meta.profile == TaskProfile::Worker).then(|| store.run_backend(&meta.id)).flatten();
  let active_backend = match displayed_backend_id(meta, live_run_backend.as_deref()) {
    Some(backend) => backend,
    None => cfg.backends.new_session_backend().ok_or_else(|| StoreError::invalid(MISSING_BACKEND_OPTION))?,
  };
  let active_option = cfg.get_backend_option(&active_backend);
  let known_active = cfg.backends.pool(&active_backend).is_some() || active_option.is_some();
  // A session whose backend names no configured id keeps that id first, marked "(not in config)" by the page.
  // The picker then shows the current value and lists the configured choices after it.
  let mut switchable: Vec<String> = if known_active { Vec::new() } else { vec![active_backend.clone()] };
  switchable.extend(cfg.backends.options.iter().map(|option| option.id.clone()));
  switchable.extend(cfg.backends.pools.iter().map(|pool| pool.id.clone()));
  // The pickers list only the config's visible entries. The session's own active entry stays exempt, so the
  // header menu still shows it as the current value.
  let switchable: Vec<String> =
    switchable.into_iter().filter(|id| id == &active_backend || cfg.backends.picker_visible(id)).collect();
  let mut payload = Map::new();
  payload.insert(ACTIVE_BACKEND_FIELD.to_string(), Value::String(active_backend.clone()));
  payload.insert(
    ACTIVE_BACKEND_TYPE_FIELD.to_string(),
    Value::String(active_option.map_or_else(String::new, |option| option.backend_type.clone())),
  );
  payload.insert("switchable_backends".to_string(), Value::Array(switchable.into_iter().map(Value::String).collect()));
  Ok(payload)
}

/// The displayed backend: only a worker's in-memory run backend overrides its metadata backend.
fn displayed_backend_id(meta: &SessionMetadata, run_backend: Option<&str>) -> Option<String> {
  let active_backend = if meta.profile == TaskProfile::Worker {
    run_backend
      .filter(|backend| !backend.is_empty())
      .or_else(|| (!meta.backend.is_empty()).then_some(meta.backend.as_str()))
  } else {
    (!meta.backend.is_empty()).then_some(meta.backend.as_str())
  };
  active_backend.map(str::to_string)
}

fn listed_session_metadata(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  listing::metadata_value(store, &response_metadata(store, meta)?)
}

pub(crate) fn response_metadata(store: &Store, meta: &SessionMetadata) -> StoreResult<SessionMetadata> {
  let mut response_meta = meta.clone();
  response_meta.run_backend = listing::run_backend(store, meta)?;
  response_meta.thinking_since = store.busy_since(&meta.id);
  Ok(response_meta)
}

fn store_error(error: StoreError) -> Response {
  crate::route_support::store_error(error)
}

fn is_bad_run_cursor(error: &StoreError) -> bool {
  let detail = error.to_string();
  detail.starts_with("malformed run page cursor") || detail.starts_with("run page cursor was minted")
}

fn events_file_not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Events file not found"}))
}

fn gzip_file(path: &Path, metadata: fs::Metadata) -> StoreResult<Bytes> {
  let mtime_ns = timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()) as i64;
  let size = metadata.size();
  let key = (path.to_path_buf(), mtime_ns, size);
  if let Some(body) =
    EVENTS_FILE_GZIP_MEMO.lock().expect("events-file gzip memo mutex is not poisoned").get(&key).cloned()
  {
    return Ok(body);
  }
  let bytes = fs::read(path).map_err(|error| path_error(path, error))?;
  let compressed = Bytes::from(gzip_level_one(&bytes));
  let mut memo = EVENTS_FILE_GZIP_MEMO.lock().expect("events-file gzip memo mutex is not poisoned");
  memo.put(key, compressed.clone());
  Ok(compressed)
}

fn json_bytes_response(body: Bytes, encoding: Option<&'static str>) -> Response {
  let mut builder = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, APPLICATION_JSON)
    .header(header::CONTENT_LENGTH, body.len());
  if let Some(encoding) = encoding {
    builder = builder
      .header(header::CONTENT_ENCODING, HeaderValue::from_static(encoding))
      .header(header::VARY, HeaderValue::from_static(ACCEPT_ENCODING));
  }
  builder.body(Body::from(body)).expect("a session JSON response builds")
}

/// The bootstrap route's answer: the payload body, plus the header that names the wire bytes the
/// untrimmed payload takes, the size a realistic switch target's content is measured against.
fn bootstrap_response(body: Bytes, encoding: Option<&'static str>, untrimmed_wire_bytes: usize) -> Response {
  let mut response = json_bytes_response(body, encoding);
  response.headers_mut().insert(
    BOOTSTRAP_UNTRIMMED_BYTES_HEADER,
    HeaderValue::from_str(&untrimmed_wire_bytes.to_string()).expect("a byte count is a valid header value"),
  );
  response
}

/// One window of the events file: its bytes, the byte offset where the window starts, and its gzip form when the
/// request asked for one.
fn events_window_response(body: Bytes, gzip: Option<Bytes>, start: u64) -> Response {
  let (served, encoding) = match gzip {
    Some(compressed) => (compressed, Some("gzip")),
    None => (body, None),
  };
  let mut builder = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/x-ndjson")
    .header(header::CONTENT_LENGTH, served.len())
    .header("x-window-start", start)
    .header(header::VARY, ACCEPT_ENCODING);
  if let Some(encoding) = encoding {
    builder = builder.header(header::CONTENT_ENCODING, encoding);
  }
  builder.body(Body::from(served)).expect("an events window response builds")
}

fn events_gzip_response(body: Bytes) -> Response {
  Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/x-ndjson")
    .header(header::CONTENT_LENGTH, body.len())
    .header(header::CONTENT_ENCODING, "gzip")
    .header(header::VARY, ACCEPT_ENCODING)
    .body(Body::from(body))
    .expect("a compressed events response builds")
}

fn to_index(value: i64) -> usize {
  usize::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use charliebot_registry::{Registry, store_hooks};
  use charliebot_store::for_you::{self, FOR_YOU_MAX_JSON_BYTES, FOR_YOU_MAX_ROWS};
  use charliebot_store::run_records::RunKind;
  use charliebot_store::{CompactValue, CreateTask, SESSION_NOT_FOUND_DETAIL, TaskCaller};

  use super::*;
  use crate::test_support::{answer, create_node, feature_context, feature_context_with_auto};

  const AUTO_CONFIG: &str = "\
backends:
  options:
    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a, billing: subscription}
    - {id: synthetic-b, label: Synthetic B, type: synthetic, model: model-b, billing: payg}
  pools:
    - id: auto
      label: Auto
      rule: priority
      members:
        - synthetic-a
        - synthetic-b
";

  #[tokio::test]
  async fn the_session_detail_returns_the_pool_member_and_lists_the_pools_switchable() {
    let (_home, features) = feature_context_with_auto(AUTO_CONFIG);
    let meta = features
      .store
      .create_task(
        CreateTask {
          request_id: "auto-detail".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Auto detail".to_string()),
          backend: Some("auto".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .expect("the auto session is created");
    features
      .store
      .update_slot_fields(
        &meta.id,
        charliebot_agents::model_pool::OWNER,
        Map::from_iter([("pool_member".to_string(), json!("synthetic-a"))]),
      )
      .await
      .expect("the member is recorded");

    let payload =
      charliebot_agents::pool_display::session_detail_shown(&features.store, &meta.id).expect("the detail reads");
    assert_eq!(payload["backend"], "auto");
    assert_eq!(payload["pool_member"], "synthetic-a");

    // A recorded member the pool no longer lists is not shown; the pool name still is.
    features
      .store
      .update_slot_fields(
        &meta.id,
        charliebot_agents::model_pool::OWNER,
        Map::from_iter([("pool_member".to_string(), json!("retired"))]),
      )
      .await
      .expect("the member is recorded");
    let payload =
      charliebot_agents::pool_display::session_detail_shown(&features.store, &meta.id).expect("the detail reads");
    assert_eq!(payload["backend"], "auto");
    assert_eq!(payload["pool_member"], Value::Null);

    let cfg = features.config.get().expect("the test config loads");
    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "auto");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b", "auto"]));
  }

  #[tokio::test]
  async fn a_session_on_a_plain_option_lists_the_pools_when_pools_are_configured() {
    let (_home, features) = feature_context_with_auto(AUTO_CONFIG);
    let meta = create_node(&features.store, "plain-option-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "synthetic-a");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b", "auto"]));
  }

  /// A config with two pools: the priority pool `auto` and the rotate pool `light`.
  const TWO_POOLS_CONFIG: &str = "\
backends:
  options:
    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a, billing: subscription}
    - {id: synthetic-b, label: Synthetic B, type: synthetic, model: model-b, billing: payg}
  pools:
    - id: auto
      label: Auto
      rule: priority
      members:
        - synthetic-a
        - synthetic-b
    - id: light
      label: Light
      rule: rotate
      members: [synthetic-b]
";

  #[tokio::test]
  async fn a_session_on_a_plain_option_lists_every_pool_as_switchable() {
    let (_home, features) = feature_context_with_auto(TWO_POOLS_CONFIG);
    let meta = create_node(&features.store, "two-pools-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "synthetic-a");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b", "auto", "light"]));
  }

  /// The two-pools config with one option and one pool hidden from the pickers.
  const VISIBILITY_CONFIG: &str = "\
backends:
  options:
    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a, billing: subscription}
    - {id: synthetic-b, label: Synthetic B, type: synthetic, model: model-b, billing: payg, visible: false}
  pools:
    - id: auto
      label: Auto
      rule: priority
      members:
        - synthetic-a
        - synthetic-b
    - id: light
      label: Light
      rule: rotate
      visible: false
      members: [synthetic-b]
";

  /// The pickers list only the visible entries: the hidden option and the hidden pool stay out, and the
  /// visible option and pool stay in config order.
  #[tokio::test]
  async fn a_switch_list_drops_the_hidden_option_and_the_hidden_pool() {
    let (_home, features) = feature_context_with_auto(VISIBILITY_CONFIG);
    let meta = create_node(&features.store, "visibility-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "synthetic-a");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "auto"]));
  }

  /// A session whose active backend is a hidden entry still shows it. The hidden active stays in the
  /// list as the current value, while the other hidden entries stay out.
  #[tokio::test]
  async fn a_session_on_a_hidden_option_still_shows_it_as_its_current_value() {
    let (_home, features) = feature_context_with_auto(VISIBILITY_CONFIG);
    let meta = features
      .store
      .create_task(
        CreateTask {
          request_id: "hidden-active-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Hidden active".to_string()),
          backend: Some("synthetic-b".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .expect("the hidden-active node");
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "synthetic-b");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b", "auto"]));
  }

  /// The same exemption covers a hidden pool: a session running it lists it as current, and the visible
  /// pool stays offered.
  #[tokio::test]
  async fn a_session_on_a_hidden_pool_still_shows_it_as_its_current_value() {
    let (_home, features) = feature_context_with_auto(VISIBILITY_CONFIG);
    let meta = features
      .store
      .create_task(
        CreateTask {
          request_id: "hidden-pool-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Hidden pool".to_string()),
          backend: Some("light".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .expect("the hidden-pool node");
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "light");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "auto", "light"]));
  }

  #[tokio::test]
  async fn without_pools_the_switch_list_holds_the_options_only() {
    let (_home, features) = feature_context();
    let meta = create_node(&features.store, "plain-no-pool-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b"]));
  }

  #[tokio::test]
  async fn a_session_on_an_unconfigured_backend_lists_its_id_first() {
    let (_home, features) = feature_context();
    let meta = features
      .store
      .create_task(
        CreateTask {
          request_id: "unconfigured-backend-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Unconfigured backend".to_string()),
          backend: Some("gone-id".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .expect("the node");
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg).expect("the backend reads");
    assert_eq!(backend["active_backend"], "gone-id");
    assert_eq!(backend["switchable_backends"], json!(["gone-id", "synthetic-a", "synthetic-b"]));
  }

  fn run(id: &str, inputs: &[&str], pid: Option<u32>) -> RunRecord {
    RunRecord {
      id: id.to_string(),
      session_id: "worker".to_string(),
      kind: RunKind::Work,
      input_event_ids: inputs.iter().map(|id| id.to_string()).collect(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: None,
      model: None,
      native_session_id: None,
      pid,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    }
  }

  async fn bootstrap_body(features: &Arc<FeatureContext>, session_id: &str, gzip: bool) -> Vec<u8> {
    let mut request = Request::builder().uri("/bootstrap");
    if gzip {
      request = request.header(header::ACCEPT_ENCODING, "gzip");
    }
    let response = session_bootstrap(
      State(Arc::clone(features)),
      AxumPath(session_id.to_string()),
      request.body(Body::empty()).expect("the bootstrap request builds"),
    )
    .await;
    axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("bootstrap body").to_vec()
  }

  /// The bootstrap's body memo: a repeat serves the body it built until an event moves the projection. The first
  /// gzip answer deflates that body once, while later ones reuse the stored gzip.
  #[tokio::test]
  async fn a_repeat_bootstrap_serves_its_body_until_an_event_moves_the_projection() {
    let (_home, features) = feature_context();
    let cfg = features.config.get().expect("the test config loads");
    let meta = crate::test_support::create_node(&features.store, "gzip-bootstrap", None, TaskProfile::Manager).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": "a question for the gzip memo"}))
      .await
      .unwrap();
    let plain = bootstrap_body(&features, &meta.id, false).await;
    assert_eq!(bootstrap_body(&features, &meta.id, false).await, plain, "an unchanged payload keeps its body");

    let before = charliebot_compat::compression::level_one_bytes_deflated();
    let first = bootstrap_body(&features, &meta.id, true).await;
    assert_eq!(
      charliebot_compat::compression::level_one_bytes_deflated() - before,
      plain.len() as u64,
      "the first gzip answer deflates exactly the plain body"
    );

    let before = charliebot_compat::compression::level_one_bytes_deflated();
    let second = bootstrap_body(&features, &meta.id, true).await;
    assert_eq!(charliebot_compat::compression::level_one_bytes_deflated() - before, 0);
    assert_eq!(first, second, "the memo serves the stored gzip bytes");

    features
      .store
      .append_chat_event(&meta.id, json!({"type": "assistant", "content": "a reply that moves the projection"}))
      .await
      .unwrap();
    let after = bootstrap_body(&features, &meta.id, false).await;
    let fresh =
      charliebot_compat::json::fast_json_bytes(&build_bootstrap_payload(&features.store, &cfg, &meta).unwrap().value);
    assert_ne!(after, plain, "a new event rebuilds the body");
    assert_eq!(after, fresh, "the rebuilt body matches a fresh payload");
  }

  /// The page bootstrap memo serves the text it built until an event moves the projection, then rebuilds it.
  ///
  /// The text is the payload's Jinja `tojson` bytes, so a page that substitutes it carries the bytes the template's
  /// `tojson` filter would have written.
  #[tokio::test]
  async fn a_page_bootstrap_keeps_its_text_until_an_event_moves_the_projection() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "page-bootstrap", None, TaskProfile::Manager).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": "a question for the page"}))
      .await
      .unwrap();
    let memo = PageBootstrapMemo::default();
    let cfg = features.config.get().expect("the test config loads");
    let page_bootstrap_text = |memo: &PageBootstrapMemo| page_bootstrap(memo, &features.store, &cfg, &meta).unwrap();

    let first = page_bootstrap_text(&memo);
    let payload = build_bootstrap_payload(&features.store, &cfg, &meta).unwrap();
    assert_eq!(first.text.as_str(), charliebot_compat::jinja::tojson_text(&payload.value));
    assert_eq!(*first.session, payload.value["session"]);

    let repeat = page_bootstrap_text(&memo);
    assert!(Arc::ptr_eq(&first.text, &repeat.text), "an unchanged payload keeps its text");
    assert_eq!(repeat.pending_draft, first.pending_draft);

    features
      .store
      .append_chat_event(&meta.id, json!({"type": "assistant", "content": "an answer for the page"}))
      .await
      .unwrap();
    let after = page_bootstrap_text(&memo);
    let payload_after = build_bootstrap_payload(&features.store, &cfg, &meta).unwrap();
    assert_eq!(after.text.as_str(), charliebot_compat::jinja::tojson_text(&payload_after.value));
    assert_ne!(after.text.as_str(), first.text.as_str(), "a new event rebuilds the text");
  }

  /// A reader who moves between two sessions and comes back is served the payload the page built before, because one
  /// session's payload no longer displaces the other's.
  #[tokio::test]
  async fn a_page_bootstrap_keeps_the_sessions_it_served_before() {
    let (_home, features) = feature_context();
    let first = crate::test_support::create_node(&features.store, "page-bootstrap-a", None, TaskProfile::Manager).await;
    let second =
      crate::test_support::create_node(&features.store, "page-bootstrap-b", None, TaskProfile::Manager).await;
    let memo = PageBootstrapMemo::default();
    let cfg = features.config.get().expect("the test config loads");
    let served = |session: &SessionMetadata| page_bootstrap(&memo, &features.store, &cfg, session);

    let first_a = served(&first).unwrap();
    let first_b = served(&second).unwrap();
    let again_a = served(&first).unwrap();
    let again_b = served(&second).unwrap();
    assert!(!Arc::ptr_eq(&first_a.text, &first_b.text), "the two sessions build different payloads");
    assert!(Arc::ptr_eq(&first_a.text, &again_a.text), "the session opened before another one is still kept");
    assert!(Arc::ptr_eq(&first_b.text, &again_b.text));
    assert!(
      Arc::ptr_eq(&first_a.session, &again_a.session) && Arc::ptr_eq(&first_b.session, &again_b.session),
      "a served-again payload shares the session object it built instead of copying it"
    );
  }

  /// Deterministic incompressible-ish text: a fixed LCG over a 62-character alphabet. The gzip of one
  /// slot is then a real share of its raw size, so the fixture's wire tracks its raw bytes.
  fn fixture_text(seed: u64, length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..length)
      .map(|_| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ALPHABET[(state >> 33) as usize % ALPHABET.len()] as char
      })
      .collect()
  }

  /// The fit drops whole turns only: the kept slots are the tail's largest turn-aligned suffix
  /// whose wire fits the budget, and the newest turn always survives.
  #[test]
  fn a_first_screen_fit_keeps_the_largest_turn_aligned_tail_the_budget_holds() {
    let slots = 40usize;
    let turn_slots = 5usize;
    let slot_text = |index: usize| fixture_text(index as u64 + 1, 1_600);
    // `build` keeps the tail from slot `drop` on, as the payload builder's trim step does.
    let mut build = |drop: usize| -> StoreResult<Value> {
      let messages: Vec<Value> =
        (drop..slots).map(|index| json!({"role": "user", "content": slot_text(index)})).collect();
      Ok(json!({"messages": messages}))
    };
    let next_drop = |drop: usize| if drop + turn_slots <= slots { Some(drop + turn_slots) } else { None };
    let newest = NewestTurn { start: slots - turn_slots, last_row: slots - 1 };
    let (payload, untrimmed) = fit_first_screen_payload(&mut build, &next_drop, newest).unwrap();
    assert!(untrimmed > FIRST_SCREEN_WIRE_BUDGET_BYTES, "the fixture's full payload must exceed the budget");
    let served_wire = gzip_level_one(&charliebot_compat::json::fast_json_bytes(&payload)).len();
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    let kept = payload["messages"].as_array().unwrap().len();
    assert!(kept > 0 && kept < slots, "kept {kept} of {slots} slots");
    assert_eq!(kept % turn_slots, 0, "the kept slots are whole turns");
    let last = payload["messages"][kept - 1]["content"].as_str().unwrap();
    assert_eq!(last, slot_text(slots - 1), "the kept slots are the tail's last ones");
    let first = payload["messages"][0]["content"].as_str().unwrap();
    assert_eq!(first, slot_text(slots - kept), "the kept slots start at a turn boundary");
    // One turn fewer would not fit: the fit is the largest turn-aligned suffix that does.
    let one_turn_more: Vec<Value> =
      ((slots - kept - turn_slots)..slots).map(|index| json!({"role": "user", "content": slot_text(index)})).collect();
    let one_turn_more_wire =
      gzip_level_one(&charliebot_compat::json::fast_json_bytes(&json!({ "messages": one_turn_more }))).len();
    assert!(one_turn_more_wire > FIRST_SCREEN_WIRE_BUDGET_BYTES, "a turn more would also fit");
    let all: Vec<Value> = (0..slots).map(|index| json!({"role": "user", "content": slot_text(index)})).collect();
    assert_eq!(untrimmed, gzip_level_one(&charliebot_compat::json::fast_json_bytes(&json!({ "messages": all }))).len());
  }

  /// A turn larger than the whole budget still renders whole: it is the first screen.
  #[test]
  fn a_single_turn_over_the_budget_still_renders() {
    let oversized = fixture_text(7, 20_000);
    let mut build = |drop: usize| -> StoreResult<Value> {
      Ok(json!({"messages": (drop..1).map(|_| json!({"role": "user", "content": oversized})).collect::<Vec<_>>()}))
    };
    let newest = NewestTurn { start: 0, last_row: 0 };
    let (payload, untrimmed) = fit_first_screen_payload(&mut build, &|_| None, newest).unwrap();
    assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
    assert!(untrimmed > FIRST_SCREEN_WIRE_BUDGET_BYTES);
  }

  /// Inside the newest turn, the cut keeps the largest suffix of its rows that fits, and the last row always survives.
  #[test]
  fn a_cut_inside_the_newest_turn_keeps_the_largest_row_suffix_that_fits() {
    let rows = 10usize;
    let row_text = |index: usize| fixture_text(index as u64 + 500, 1_600);
    let suffix = |from: usize| -> Value {
      json!({"messages": (from..rows).map(|index| json!({"role": "user", "content": row_text(index)})).collect::<Vec<_>>()})
    };
    let mut build = |drop: usize| -> StoreResult<Value> { Ok(suffix(drop)) };
    let newest = NewestTurn { start: 0, last_row: rows - 1 };
    let (payload, _) = fit_first_screen_payload(&mut build, &|_| None, newest).unwrap();
    let wire_of = |value: &Value| gzip_level_one(&charliebot_compat::json::fast_json_bytes(value)).len();
    let kept = payload["messages"].as_array().unwrap().len();
    assert!(wire_of(&suffix(rows - kept)) <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "the kept rows fit");
    assert!(wire_of(&suffix(rows - kept - 1)) > FIRST_SCREEN_WIRE_BUDGET_BYTES, "one row more would not fit");
    assert_eq!(payload["messages"][kept - 1]["content"], json!(row_text(rows - 1)), "the last row renders");
  }

  /// A worker's run closes with its delivery row after the separator, so the next run starts past that row.
  #[test]
  fn a_worker_run_starts_past_its_delivery_row() {
    let messages = [
      json!({"role": "assistant"}),
      json!({"role": "separator"}),
      json!({"role": "run_delivery"}),
      json!({"role": "user"}),
    ];
    assert_eq!(worker_turn_start_after(&messages, 1), 3);
    assert_eq!(worker_turn_start_after(&messages[..2], 1), 2);
  }

  /// A last row that is over the budget alone is the first screen: the cut keeps that row and nothing older.
  #[test]
  fn a_last_row_over_the_budget_alone_is_kept_past_the_budget() {
    let rows = [fixture_text(1, 900), fixture_text(2, 900), fixture_text(3, 20_000)];
    let mut build = |drop: usize| -> StoreResult<Value> {
      Ok(
        json!({"messages": rows[drop..].iter().map(|text| json!({"role": "user", "content": text})).collect::<Vec<_>>()}),
      )
    };
    let newest = NewestTurn { start: 0, last_row: 2 };
    let (payload, _) = fit_first_screen_payload(&mut build, &|_| None, newest).unwrap();
    assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
    assert_eq!(payload["messages"][0]["content"], json!(rows[2]));
  }

  /// A tail over the budget trims through the route's payload, whole turns only. The newest turn's
  /// user message stays on the first screen, and the paging cursor moves onto the first kept turn,
  /// where scroll-up paging picks the dropped history up.
  #[tokio::test]
  async fn a_bootstrap_over_the_first_screen_budget_trims_whole_turns_and_keeps_the_newest_marker() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "trim-bootstrap", None, TaskProfile::Manager).await;
    for turn in 1..=8usize {
      features
        .store
        .append_chat_event(&meta.id, json!({"type": "user", "content": fixture_text(turn as u64, 1_600)}))
        .await
        .unwrap();
      features
        .store
        .append_chat_event(
          &meta.id,
          json!({"type": "assistant", "message": {"content": [{"type": "text", "text": fixture_text(turn as u64 + 100, 1_600)}]}}),
        )
        .await
        .unwrap();
      features.store.append_chat_event(&meta.id, json!({"type": "master_done"})).await.unwrap();
      features
        .store
        .append_chat_event(&meta.id, json!({"type": "run_finished", "run_id": format!("run-{turn}")}))
        .await
        .unwrap();
    }
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let kept = messages.len();
    let total_events = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    assert!(kept > 0 && kept < 24, "kept {kept} of the tail's 40 events");
    let served_wire = first_screen_wire(&payload.value);
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
    assert_eq!(payload.value["has_more"], json!(true));
    // A turn is four events: user, assistant and separator fold to one message each, and the run's `run_finished`
    // folds to none. The kept suffix therefore starts at a turn's user message, and the newest turn's user message is
    // on the first screen.
    assert_eq!(kept % 3, 0, "the kept events are whole turns");
    assert_eq!(payload.value["oldest_message_ordinal"], json!(total_events - 4 * (kept / 3)));
    assert_eq!(messages[0]["role"], "user", "a kept turn starts at its user message");
    let newest_marker = messages.iter().find(|message| message["content"] == json!(fixture_text(8, 1_600)));
    assert!(newest_marker.is_some(), "the newest turn's user message renders on the first screen");
  }

  /// Appends one turn of the fixture as a real log writes it: a user message of `user_chars`, an assistant reply when
  /// `with_reply`, the turn's separator, and the run's `run_finished` after it.
  async fn append_fixture_turn(store: &Store, session_id: &str, turn: usize, user_chars: usize, with_reply: bool) {
    store
      .append_chat_event(session_id, json!({"type": "user", "content": fixture_text(turn as u64, user_chars)}))
      .await
      .unwrap();
    if with_reply {
      let reply = fixture_text(turn as u64 + 100, 700);
      store
        .append_chat_event(
          session_id,
          json!({"type": "assistant", "message": {"content": [{"type": "text", "text": reply}]}}),
        )
        .await
        .unwrap();
    }
    store.append_chat_event(session_id, json!({"type": "master_done"})).await.unwrap();
    store
      .append_chat_event(session_id, json!({"type": "run_finished", "run_id": format!("run-{turn}")}))
      .await
      .unwrap();
  }

  /// A tail that starts past the history's first event trims in its own indexes. The kept events are the tail's last
  /// whole turns, and the paging cursor is the absolute ordinal of the first kept event. The turns take 3, 3, 2, 2
  /// events in turn, and the tail starts at event 6, which is 6 modulo the 10-event period. A trim that walked the
  /// history's turn starts as the tail's own indexes would then land between turns at every step.
  #[tokio::test]
  async fn a_first_screen_trim_past_the_history_start_keeps_whole_turns_at_absolute_ordinals() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "trim-long-bootstrap", None, TaskProfile::Manager).await;
    let turns = 18usize;
    let mut turn_starts = Vec::new();
    for turn in 1..=turns {
      turn_starts.push(features.store.load_chat_events_sync(&meta.id).unwrap().len());
      let with_reply = matches!(turn % 4, 1 | 2);
      append_fixture_turn(&features.store, &meta.id, turn, 700, with_reply).await;
    }
    let total = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    assert!(start > 0, "the tail starts past the history's first event");
    let first_turn = turn_starts.iter().position(|turn_start| *turn_start == start);
    let first_turn = first_turn.expect("the kept events start at a turn boundary") + 1;
    // Each kept turn's `run_finished` is kept too, and it folds to no message.
    let kept_closes = features.store.load_chat_events_sync(&meta.id).unwrap()[start..]
      .iter()
      .filter(|event| event["type"] == "run_finished")
      .count();
    assert_eq!(start + messages.len() + kept_closes, total, "the kept events run to the history's end");
    assert_eq!(
      messages[0]["content"],
      json!(fixture_text(first_turn as u64, 700)),
      "the first kept event is its turn's user message"
    );
    let served_wire = first_screen_wire(&payload.value);
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
    assert_eq!(payload.value["has_more"], json!(true));
    assert!(
      messages.iter().any(|message| message["content"] == json!(fixture_text(turns as u64, 700))),
      "the newest turn renders on the first screen"
    );
  }

  /// The next read covers the events the message budget needs at the fold's own ratio, and it grows by at least
  /// half and never past the log's line count.
  #[test]
  fn a_bootstrap_read_grows_to_the_budget_and_no_further_than_the_log() {
    // 40 events folding to 20 messages need 80 events for the budget's 40 messages.
    assert_eq!(next_bootstrap_read(40, 20, 1_000), 80);
    // A tail that folds to no message grows by the eightfold bound instead of dividing by zero.
    assert_eq!(next_bootstrap_read(40, 0, 1_000), 320);
    // The log's own line count caps the read.
    assert_eq!(next_bootstrap_read(40, 20, 60), 60);
    // The read always grows, so the loop reaches the log's end.
    assert_eq!(next_bootstrap_read(90, 45, 100), 100);
  }

  /// A log whose events fold to fewer messages than the budget needs a second read. The second read serves the
  /// budget's messages, and the cursor names the first of their events inside the log.
  #[tokio::test]
  async fn a_log_past_the_first_read_budget_reads_again_for_the_message_budget() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "two-read-bootstrap", None, TaskProfile::Manager).await;
    // Each turn is a user event and a note that folds to nothing, so the first read of the budget's count of events
    // folds to half the budget's messages. The loop then reads again.
    let turns = 100usize;
    for turn in 1..=turns {
      features
        .store
        .append_chat_event(&meta.id, json!({"type": "user", "content": fixture_text(turn as u64, 40)}))
        .await
        .unwrap();
      features
        .store
        .append_chat_event(&meta.id, json!({"type": "unrendered_note", "content": "filler"}))
        .await
        .unwrap();
    }
    let total = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    assert_eq!(total, 2 * turns + 1, "the fixture's log is its two events per turn and the node's own event");
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), BOOTSTRAP_MESSAGE_LIMIT, "the second read fills the message budget");
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    assert!(start > 0, "the kept events start past the log's first event");
    assert!(start + messages.len() <= total, "the cursor names events inside the log");
    assert!(
      messages.iter().any(|message| message["content"] == json!(fixture_text(turns as u64, 40))),
      "the newest turn renders on the first screen"
    );
    let served_wire = first_screen_wire(&payload.value);
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
  }

  /// A closed newest turn over the budget keeps the newest events that fit and always its last reply. Its 20,000-
  /// character user message cannot fit, so the cut lands on the reply, and the separator and `run_finished` follow it.
  #[tokio::test]
  async fn a_closed_newest_turn_over_the_budget_keeps_its_last_reply() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "trim-oversized-turn", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 1_600, true).await;
    append_fixture_turn(&features.store, &meta.id, 2, 20_000, true).await;
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "the reply and its separator render");
    assert!(renders(messages, &fixture_text(102, 700)), "the newest turn's reply renders");
    let total = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    assert_eq!(payload.value["oldest_message_ordinal"], json!(total - 3), "the cursor is the reply's event");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
  }

  /// Whether one of `messages` carries `text` anywhere in its JSON, whatever shape its renderer gave the content.
  fn renders(messages: &[Value], text: &str) -> bool {
    messages.iter().any(|message| message.to_string().contains(text))
  }

  /// A running session's newest turn is the running one. When it fits the budget with the last finished reply, the reply
  /// renders above the running turn.
  #[tokio::test]
  async fn a_running_session_keeps_the_last_finished_reply_when_both_fit() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "trim-running", None, TaskProfile::Manager).await;
    let finished_reply = fixture_text(201, 700);
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": fixture_text(1, 1_600)}))
      .await
      .unwrap();
    features
      .store
      .append_chat_event(
        &meta.id,
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": finished_reply}]}}),
      )
      .await
      .unwrap();
    features.store.append_chat_event(&meta.id, json!({"type": "master_done"})).await.unwrap();
    features.store.append_chat_event(&meta.id, json!({"type": "run_finished", "run_id": "run-1"})).await.unwrap();
    // The running turn: its user message and an assistant reply still in progress, with no separator yet.
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": fixture_text(2, 1_600)}))
      .await
      .unwrap();
    features
      .store
      .append_chat_event(
        &meta.id,
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": fixture_text(202, 700)}]}}),
      )
      .await
      .unwrap();
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    assert!(payload.untrimmed_wire_bytes <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "the fixture's tail fits the budget");
    assert!(renders(messages, &finished_reply), "the last finished reply renders");
    assert!(renders(messages, &fixture_text(2, 1_600)), "the running turn's user message renders");
    assert!(payload.value["pending_draft"].to_string().contains(&fixture_text(202, 700)), "the running draft renders");
  }

  /// A running turn over the budget is the newest turn, so the cut lands inside it and keeps its newest replies that
  /// fit. The last finished reply drops: the first screen shows the running turn, and its size stays bounded.
  #[tokio::test]
  async fn a_running_turn_over_the_budget_keeps_its_newest_row() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "trim-running-over", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 700, true).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": fixture_text(2, 1_600)}))
      .await
      .unwrap();
    // Four replies of 4,000 characters: the running turn alone is over the budget, and no separator closes it. Each
    // reply folds to a row once the next one starts, so the last reply is the running draft, which the payload carries
    // beside the rows.
    let replies: Vec<String> = (0..4).map(|index| fixture_text(400 + index, 4_000)).collect();
    for reply in &replies {
      features
        .store
        .append_chat_event(
          &meta.id,
          json!({"type": "assistant", "message": {"content": [{"type": "text", "text": reply}]}}),
        )
        .await
        .unwrap();
    }
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let served_wire = gzip_level_one(&charliebot_compat::json::fast_json_bytes(&payload.value)).len();
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES, "the running turn is over the budget");
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(renders(messages, &replies[2]), "the running turn's newest row renders");
    assert!(payload.value["pending_draft"].to_string().contains(&replies[3]), "the running draft renders");
    assert!(!renders(messages, &fixture_text(101, 700)), "the last finished reply drops for the running turn");
    let total = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    // The events are the node's, the finished turn's four, the running user message, and four replies.
    assert!(start > 5 && start < total, "the cursor lands inside the running turn at {start} of {total}");
  }

  /// A newest turn over the budget keeps the newest of its events that fit, so its cursor lands inside the turn. The
  /// reply and the separator still render, and the trim keeps more than the last row.
  #[tokio::test]
  async fn a_newest_turn_over_the_budget_keeps_the_newest_events_that_fit() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "trim-mid-turn", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 700, true).await;
    // Ten user messages in one turn: each folds to one row, and together they are over the budget.
    let turn_users: Vec<String> = (0..10).map(|index| fixture_text(300 + index, 1_600)).collect();
    for user in &turn_users {
      features.store.append_chat_event(&meta.id, json!({"type": "user", "content": user})).await.unwrap();
    }
    let reply = fixture_text(399, 700);
    features
      .store
      .append_chat_event(
        &meta.id,
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": reply}]}}),
      )
      .await
      .unwrap();
    features.store.append_chat_event(&meta.id, json!({"type": "master_done"})).await.unwrap();
    features.store.append_chat_event(&meta.id, json!({"type": "run_finished", "run_id": "run-2"})).await.unwrap();
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let served_wire = gzip_level_one(&charliebot_compat::json::fast_json_bytes(&payload.value)).len();
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(renders(messages, &reply), "the newest turn's reply renders");
    assert!(renders(messages, &turn_users[9]), "the newest user message renders");
    assert!(!renders(messages, &turn_users[0]), "the oldest user messages of the turn do not fit");
    let kept_users = turn_users.iter().filter(|user| renders(messages, user)).count();
    assert!(kept_users >= 2, "the trim keeps the newest events that fit, not only the last row: kept {kept_users}");
    let total = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    assert!(start > 0 && start < total - 1, "the cursor lands inside the newest turn at {start} of {total}");
  }

  /// `session.web_status` counts against the budget. When it alone is over the budget, the newest reply still renders.
  #[tokio::test]
  async fn a_payload_whose_web_status_alone_exceeds_the_budget_keeps_the_newest_reply() {
    let (_home, features) = feature_context();
    let mut meta =
      crate::test_support::create_node(&features.store, "trim-web-status", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 700, true).await;
    append_fixture_turn(&features.store, &meta.id, 2, 700, true).await;
    meta.set_extra("web_status", json!({"goal": fixture_text(9, 17_000), "now": null, "waiting": []}));
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    assert!(payload.value["session"]["web_status"].is_object(), "the session object carries web_status");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES, "web_status alone is over the budget");
    let messages = payload.value["messages"].as_array().unwrap();
    assert!(renders(messages, &fixture_text(102, 700)), "the newest reply renders past the budget");
    assert!(messages.len() <= 2, "only the reply and its separator render: {} rows", messages.len());
  }

  /// A log whose last line does not parse names the tail's first line as the paging cursor. The cursor counts the
  /// lines the tail covers, so the next page starts where the bootstrap's messages do instead of past them.
  #[tokio::test]
  async fn a_malformed_line_does_not_advance_the_first_screen_cursor() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "malformed-tail", None, TaskProfile::Manager).await;
    for turn in 1..=3usize {
      append_fixture_turn(&features.store, &meta.id, turn, 700, true).await;
    }
    let path = features.store.chat_events_path(&meta.id);
    let mut lines = fs::read_to_string(&path).unwrap();
    lines.push_str("{malformed\n");
    fs::write(&path, lines).unwrap();
    features.store.clear_chat_events_cache(&meta.id);

    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    assert_eq!(payload.value["oldest_message_ordinal"], json!(0), "the tail starts at the log's first line");
    assert_eq!(payload.value["has_more"], json!(false));
    let messages = payload.value["messages"].as_array().unwrap();
    assert!(
      messages.iter().any(|message| message["content"] == json!(fixture_text(3 + 100, 700))),
      "the newest turn renders on the first screen"
    );
  }

  /// A log whose turns carry more events than messages still fills the first screen's message budget. The read walks
  /// past the budget's count of events until the tail folds to the budget, and the trim keeps its last messages.
  #[tokio::test]
  async fn the_first_screen_fills_its_message_budget_from_turns_of_many_events() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "budget-bootstrap", None, TaskProfile::Manager).await;
    // 45 turns of two events that fold to one message. The budget's count of events folds to 20 messages, so the read
    // doubles to 80 and stops at the budget, leaving events older than the kept tail.
    for turn in 1..=45usize {
      let events = [
        json!({"type": "user", "content": fixture_text(turn as u64, 40)}),
        json!({"type": "unrendered_note", "content": "filler"}),
      ];
      for event in events {
        features.store.append_chat_event(&meta.id, event).await.unwrap();
      }
    }
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), BOOTSTRAP_MESSAGE_LIMIT, "the first screen carries the budget's messages");
    let total_events = features.store.load_chat_events_sync(&meta.id).unwrap().len();
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    assert!(start > 0, "the kept messages start past the log's first event");
    assert!(start + messages.len() <= total_events, "the cursor names events inside the log");
    assert_eq!(payload.value["has_more"], json!(true));
  }

  /// The route's answer carries the untrimmed wire size in its header.
  #[tokio::test]
  async fn the_bootstrap_route_names_the_untrimmed_wire_size() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "header-bootstrap", None, TaskProfile::Manager).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": "a question for the header"}))
      .await
      .unwrap();
    let response = session_bootstrap(
      State(Arc::clone(&features)),
      AxumPath(meta.id.clone()),
      Request::builder().uri("/bootstrap").body(Body::empty()).expect("the bootstrap request builds"),
    )
    .await;
    let untrimmed = response
      .headers()
      .get(BOOTSTRAP_UNTRIMMED_BYTES_HEADER)
      .expect("the header names the untrimmed size")
      .to_str()
      .unwrap()
      .parse::<usize>()
      .unwrap();
    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    assert_eq!(untrimmed, payload.untrimmed_wire_bytes);
  }

  async fn append_all(features: &FeatureContext, session_id: &str, events: Vec<Value>) {
    for event in events {
      features.store.append_chat_event(session_id, event).await.unwrap();
    }
  }

  /// The gzip bytes of a bootstrap's first screen: the payload without the For-you rows, which ride outside the budget.
  fn first_screen_wire(value: &Value) -> usize {
    let mut first_screen = value.clone();
    first_screen.as_object_mut().expect("the payload is an object").remove(OLDER_FOR_YOU_FIELD);
    gzip_level_one(&charliebot_compat::json::fast_json_bytes(&first_screen)).len()
  }

  fn row_contents(rows: &[Value]) -> Vec<String> {
    rows.iter().map(|row| row["content"].as_str().expect("a row has text").to_string()).collect()
  }

  fn hidden_filler() -> Value {
    json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hidden"}]}})
  }

  /// A web session's bootstrap carries the newest For-you rows before its first screen, oldest first, ten at most.
  /// The user's messages and the error rows show; the assistant's reply between them does not.
  #[tokio::test]
  async fn a_web_session_bootstrap_carries_the_newest_for_you_rows_before_its_first_screen() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "for-you-rows", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    let mut events = Vec::new();
    // Each shown event with its event index and the content its row carries.
    let mut shown = Vec::new();
    for turn in 0..30usize {
      shown.push((base + events.len(), format!("user {turn}")));
      events.push(json!({"type": "user", "content": format!("user {turn}")}));
      events.push(hidden_filler());
      if turn % 3 == 0 {
        shown.push((base + events.len(), format!("Error: error {turn}")));
        events.push(json!({"type": "error", "content": format!("error {turn}")}));
      }
      events.push(json!({"type": "master_done"}));
    }
    append_all(&features, &meta.id, events).await;

    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let start = payload.value[api::OLDEST_MESSAGE_ORDINAL_FIELD].as_u64().unwrap() as usize;
    let before_start: Vec<String> =
      shown.iter().filter(|(index, _)| *index < start).map(|(_, content)| content.clone()).collect();
    let expected = before_start[before_start.len().saturating_sub(FOR_YOU_MAX_ROWS)..].to_vec();
    assert_eq!(expected.len(), FOR_YOU_MAX_ROWS, "the fixture has more rows before the first screen than the cap");
    let older = payload.value[OLDER_FOR_YOU_FIELD].as_array().expect("the bootstrap carries the older rows");
    assert_eq!(row_contents(older), expected);
    assert!(older.iter().all(|row| for_you::is_reply_row(row) || for_you::is_error_row(row)));
  }

  /// The scan keeps the newest ten rows, and within 16 KiB of JSON the newest row is kept whatever its size.
  #[tokio::test]
  async fn the_for_you_scan_keeps_the_newest_ten_rows_within_16_kib() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "for-you-cap", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    let events = (0..30usize).map(|turn| json!({"type": "user", "content": format!("cap {turn}")})).collect();
    append_all(&features, &meta.id, events).await;
    let rows = features.store.for_you_rows_before(&meta.id, base + 30).unwrap();
    assert_eq!(row_contents(&rows), (20..30).map(|turn| format!("cap {turn}")).collect::<Vec<_>>());

    // Each row takes about 5 KB, so three fit under 16 KiB and a fourth would pass it.
    let bulky = |turn: usize, size: usize| json!({"type": "user", "content": format!("{turn} {}", "x".repeat(size))});
    let meta = crate::test_support::create_node(&features.store, "for-you-bytes", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    append_all(&features, &meta.id, (0..6usize).map(|turn| bulky(turn, 5_000)).collect()).await;
    let rows = features.store.for_you_rows_before(&meta.id, base + 6).unwrap();
    assert_eq!(row_contents(&rows).iter().map(|c| c.split(' ').next().unwrap()).collect::<Vec<_>>(), ["3", "4", "5"]);

    let meta = crate::test_support::create_node(&features.store, "for-you-newest", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    append_all(&features, &meta.id, vec![bulky(0, 5_000), bulky(1, 20_000)]).await;
    let rows = features.store.for_you_rows_before(&meta.id, base + 2).unwrap();
    assert_eq!(rows.len(), 1, "the newest row alone is over the cap, and it is kept");
    assert_eq!(rows[0]["content"].as_str().unwrap().split(' ').next(), Some("1"));
  }

  /// The JSON cap counts the array brackets and commas, so an array one byte over the cap drops its older row.
  #[tokio::test]
  async fn the_for_you_json_cap_counts_the_array_delimiters() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "for-you-json-boundary", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    let mut events = vec![
      json!({"type": "user", "content": "x"}),
      json!({"type": "master_done"}),
      json!({"type": "user", "content": "x"}),
      json!({"type": "master_done"}),
    ];
    let compact_events: Vec<CompactValue> = events.iter().map(CompactValue::from_value).collect();
    let projected = features.store.events_to_messages_at(&compact_events, base).unwrap();
    let reply_rows: Vec<&Value> = projected.iter().filter(|message| for_you::is_reply_row(message)).collect();
    assert_eq!(reply_rows.len(), 2);
    // The old accounting adds one byte per row, but omits the brackets; make it land exactly on the cap.
    let old_accounting = reply_rows.iter().map(|message| message.to_string().len() + 1).sum::<usize>();
    let extra = FOR_YOU_MAX_JSON_BYTES - old_accounting;
    events[2]["content"] = json!("x".repeat(extra + 1));
    append_all(&features, &meta.id, events).await;

    let rows = features.store.for_you_rows_before(&meta.id, base + 4).unwrap();
    assert_eq!(rows.len(), 1, "the oldest row would make the serialized array one byte too large");
    assert_eq!(rows[0]["content"].as_str().unwrap().len(), extra + 1);
    assert!(serde_json::to_vec(&rows).unwrap().len() <= FOR_YOU_MAX_JSON_BYTES);
  }

  /// The scan reads 2,000 event lines back from the line it starts at and no further. The fixture appends 2,600 event
  /// lines one write at a time, which is past the unit-test time bound, so the test runs with the integration suite.
  #[tokio::test]
  #[ignore = "integration"]
  async fn the_for_you_scan_stops_at_its_2000_line_bound() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "for-you-bound", None, TaskProfile::Manager).await;
    let base = features.store.get_chat_event_count_sync(&meta.id).unwrap();
    let mut events = vec![json!({"type": "user", "content": "far"})];
    events.extend((1..700).map(|_| hidden_filler()));
    events.push(json!({"type": "user", "content": "mid"}));
    events.extend((701..2_500).map(|_| hidden_filler()));
    events.push(json!({"type": "user", "content": "last"}));
    events.extend((2_501..2_600).map(|_| hidden_filler()));
    append_all(&features, &meta.id, events).await;
    // Two thousand lines back from line 2,600 starts at line 600: "mid" at 700 is in, "far" at 0 is out.
    let rows = features.store.for_you_rows_before(&meta.id, base + 2_600).unwrap();
    assert_eq!(row_contents(&rows), ["mid", "last"]);
  }

  /// The scan reads the archived lines behind a session's first screen.
  #[tokio::test]
  async fn the_for_you_scan_reads_rows_from_the_archive() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "for-you-archive", None, TaskProfile::Manager).await;
    let mut events = Vec::new();
    for index in 0..200usize {
      let mut event =
        if index % 20 == 0 { json!({"type": "user", "content": format!("archived {index}")}) } else { hidden_filler() };
      event["timestamp"] = json!("2025-02-01T00:00:00+00:00");
      events.push(event);
    }
    for _ in 0..60usize {
      let stamp = json!("2100-01-01T00:00:00+00:00");
      events.push(
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "live"}]}, "timestamp": stamp}),
      );
      events.push(json!({"type": "master_done", "timestamp": stamp}));
    }
    append_all(&features, &meta.id, events).await;
    let archived = features
      .store
      .archive_events_before(&meta.id, UtcDateTime::parse("2099-01-01T00:00:00Z").unwrap())
      .await
      .unwrap();
    assert!(archived.is_some(), "the lines before the cutoff move to the archive");
    // The bootstrap reads the session's own archive offset, which the archive moved.
    let meta = features.store.require_session(&meta.id).await.unwrap();

    let payload = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let start = payload.value[api::OLDEST_MESSAGE_ORDINAL_FIELD].as_u64().unwrap() as usize;
    assert!(start > 200, "the first screen starts past the archived lines, at {start}");
    let older = payload.value[OLDER_FOR_YOU_FIELD].as_array().unwrap();
    assert_eq!(row_contents(older), (0..10usize).map(|slot| format!("archived {}", slot * 20)).collect::<Vec<_>>());
  }

  /// Only a manager session outside a chat thread is a web session. A worker and a thread session carry no older rows.
  #[test]
  fn only_a_manager_session_outside_a_chat_thread_is_a_web_session() {
    charliebot_registry::thread_origin::register_origin_field("for_you_test_origin");
    let mut thread = SessionMetadata::new("thread", "Thread", TaskProfile::Manager);
    thread.set_extra("for_you_test_origin", json!({"thread": "t1"}));
    assert!(!is_web_session(&thread));
    assert!(is_web_session(&SessionMetadata::new("manager", "Manager", TaskProfile::Manager)));
    assert!(!is_web_session(&SessionMetadata::new("worker", "Worker", TaskProfile::Worker)));
  }

  fn cfg_of(features: &Arc<FeatureContext>) -> Arc<CharlieBotConfig> {
    features.config.get().expect("the test config loads")
  }

  #[tokio::test]
  async fn pending_inputs_read_run_stop_facts_from_the_shared_fold() {
    let (_home, features) = feature_context();
    features
      .store
      .append_chat_event("worker", json!({"type": "run_stop_requested", "run_id": "stopped"}))
      .await
      .unwrap();
    let facts = TaskFacts {
      task_state: "open".to_string(),
      run_outcomes: HashMap::new(),
      run_outcome_string_none_ids: HashSet::new(),
      run_finish_positions: HashMap::new(),
      confirmed_input_ids: HashSet::new(),
      input_candidates: vec![Arc::new(json!({"id": "held"}).into()), Arc::new(json!({"id": "free"}).into())],
      imported_pending_ids: HashSet::new(),
      boundary_index: Some(0),
      boundary_open: true,
      close_events: Vec::new(),
      close_requests: Vec::new(),
      close_request_ids: HashSet::new(),
      first_close_by_request: HashMap::new(),
      delivered_reports: HashSet::new(),
      clicked: Vec::new(),
      events_by_id: Default::default(),
      covered: 0,
    };
    let run_facts = features.store.run_facts("worker").unwrap();
    let pending = pending_input_events(&facts, &[run("stopped", &["held"], None)], &run_facts, "worker").unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0]["id"], "held");
    assert_eq!(pending[1]["id"], "free");
  }

  #[test]
  fn pending_inputs_remove_confirmed_and_claimed_ids() {
    let mut facts = TaskFacts {
      task_state: "open".to_string(),
      run_outcomes: HashMap::new(),
      run_outcome_string_none_ids: HashSet::new(),
      run_finish_positions: HashMap::new(),
      confirmed_input_ids: HashSet::new(),
      input_candidates: vec![
        Arc::new(json!({"id": "confirmed"}).into()),
        Arc::new(json!({"id": "claimed"}).into()),
        Arc::new(json!({"id": "pending"}).into()),
      ],
      imported_pending_ids: HashSet::new(),
      boundary_index: Some(0),
      boundary_open: true,
      close_events: Vec::new(),
      close_requests: Vec::new(),
      close_request_ids: HashSet::new(),
      first_close_by_request: HashMap::new(),
      delivered_reports: HashSet::new(),
      clicked: Vec::new(),
      events_by_id: Default::default(),
      covered: 0,
    };
    facts.confirmed_input_ids.insert("confirmed".to_string());
    let pending =
      pending_input_events(&facts, &[run("live", &["claimed"], Some(12))], &RunFacts::default(), "worker").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], "pending");
  }

  #[tokio::test]
  async fn pending_inputs_fold_the_history_once_per_cache_generation() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(
      &features.store,
      "pending-inputs-count",
      None,
      charliebot_store::metadata::TaskProfile::Manager,
    )
    .await;
    features.store.append_chat_event(&meta.id, json!({"type": "user", "content": "a pending question"})).await.unwrap();
    let before = features.store.history_work();

    let (status, body) =
      crate::test_support::answer(pending_task_inputs(State(Arc::clone(&features)), AxumPath(meta.id.clone())).await)
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let after_first = features.store.history_work();
    assert_eq!(after_first.file_reads - before.file_reads, 0, "the cold fold scans the log and loads none of it");
    assert_eq!(after_first.full_folds - before.full_folds, 1);
    let (second_status, _) =
      crate::test_support::answer(pending_task_inputs(State(Arc::clone(&features)), AxumPath(meta.id)).await).await;
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(features.store.history_work(), after_first);
  }

  /// The /runs page of a session with no fact checkpoint reads from a cold event cache the page and run states a warm
  /// read reads, and the cold read loads no history. No scan has run for the session, so no checkpoint exists before
  /// the read.
  #[tokio::test]
  async fn runs_of_a_cold_session_without_a_checkpoint_answer_what_a_warm_read_answers() {
    let (_home, features) = feature_context();
    let meta = create_node(&features.store, "cold-runs", None, TaskProfile::Manager).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"id": "ask", "type": "user", "content": "do the work"}))
      .await
      .unwrap();
    features
      .store
      .append_chat_event(
        &meta.id,
        json!({"id": "finish-1", "type": "run_finished", "run_id": "run-1", "outcome": "success",
               "input_event_ids": ["ask"]}),
      )
      .await
      .unwrap();
    let mut record = run("run-1", &["ask"], None);
    record.session_id = meta.id.clone();
    let run_dir = features.store.run_dir(&meta.id, "run-1");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
      features.store.run_metadata_path(&meta.id, "run-1"),
      charliebot_store::run_records::run_record_text(&record),
    )
    .unwrap();
    features.store.clear_chat_events_cache(&meta.id);

    let page = |features: &Arc<FeatureContext>, id: &str| {
      let request =
        Request::builder().uri(format!("/api/sessions/{id}/runs?order=asc&limit=500")).body(Body::empty()).unwrap();
      session_runs(State(Arc::clone(features)), AxumPath(id.to_string()), request)
    };
    let reads = features.store.history_work().file_reads;
    let (cold_status, cold) = crate::test_support::answer(page(&features, &meta.id).await).await;
    assert_eq!(features.store.history_work().file_reads, reads, "the cold read loads no history");
    features.store.load_chat_events_shared_sync(&meta.id).unwrap();
    let (warm_status, warm) = crate::test_support::answer(page(&features, &meta.id).await).await;

    assert_eq!(cold_status, StatusCode::OK);
    assert_eq!((cold_status, &cold), (warm_status, &warm), "the cold page equals the warm page");
    assert_eq!(cold["items"][0]["id"], "run-1");
    assert_eq!(cold["items"][0]["state"], "success");
  }

  #[test]
  fn displayed_backend_uses_worker_map_and_metadata_fallback() {
    let mut manager = SessionMetadata::new("synthetic-manager", "Synthetic manager", TaskProfile::Manager);
    manager.backend = "synthetic-configured-backend".to_string();
    manager.run_backend = Some("synthetic-retired-backend".to_string());
    assert_eq!(
      displayed_backend_id(&manager, Some("synthetic-retired-backend")).as_deref(),
      Some("synthetic-configured-backend")
    );

    let mut worker = SessionMetadata::new("synthetic-worker", "Synthetic worker", TaskProfile::Worker);
    worker.backend = "synthetic-configured-backend".to_string();
    worker.run_backend = Some("synthetic-retired-backend".to_string());
    assert_eq!(displayed_backend_id(&worker, None).as_deref(), Some("synthetic-configured-backend"));
    assert_eq!(
      displayed_backend_id(&worker, Some("synthetic-retired-backend")).as_deref(),
      Some("synthetic-retired-backend")
    );

    let (_home, features) = feature_context();
    features.store.note_run_backend(&worker.id, "synthetic-retired-backend");
    let cfg = features.config.get().expect("the test config loads");
    let payload = active_backend_payload(&features.store, &worker, &cfg).unwrap();
    assert_eq!(payload["active_backend"], "synthetic-retired-backend");
    assert_eq!(payload["active_backend_type"], "");
    assert_eq!(payload["switchable_backends"], json!(["synthetic-retired-backend", "synthetic-a", "synthetic-b"]));
  }

  const TRIGGER_SESSION_ID: &str = "synthetic-trigger-session";

  fn add_trigger_session(features: &FeatureContext) {
    let metadata = SessionMetadata::new(TRIGGER_SESSION_ID, "Synthetic trigger session", TaskProfile::Manager);
    let path = features.store.metadata_path(TRIGGER_SESSION_ID);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, metadata.to_json_string(&features.registry.metadata_slots(), true)).unwrap();
  }

  fn trigger_record(id: &str, status: TriggerStatus, fire_at: &str, created_at: &str) -> PendingTrigger {
    PendingTrigger {
      id: id.to_string(),
      session_id: TRIGGER_SESSION_ID.to_string(),
      fire_at: UtcDateTime::parse(fire_at).unwrap(),
      message: format!("synthetic wake {id}"),
      created_at: UtcDateTime::parse(created_at).unwrap(),
      status,
      fired_at: None,
      watch_targets: Vec::new(),
      fire_reason: None,
    }
  }

  fn write_trigger(features: &FeatureContext, trigger: &PendingTrigger) {
    let directory = features.store.session_dir(&trigger.session_id).join("triggers");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{}.json", trigger.id)), serde_json::to_vec_pretty(trigger).unwrap())
      .unwrap();
  }

  async fn pending_triggers_response(features: &Arc<FeatureContext>, session_id: &str) -> (StatusCode, Value) {
    let response = pending_triggers(State(Arc::clone(features)), AxumPath(session_id.to_string())).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
  }

  #[tokio::test]
  async fn pending_triggers_are_empty_without_trigger_records() {
    let (_home, features) = feature_context();
    add_trigger_session(&features);
    features.startup.insert(Arc::new(TriggerManager::new(Arc::clone(&features)))).unwrap();
    let (status, body) = pending_triggers_response(&features, TRIGGER_SESSION_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
  }

  #[tokio::test]
  async fn pending_triggers_list_only_pending_records_by_fire_at() {
    let (_home, features) = feature_context();
    add_trigger_session(&features);
    features.startup.insert(Arc::new(TriggerManager::new(Arc::clone(&features)))).unwrap();
    // Creation order runs opposite to fire order, so `soon` leads only because the route sorts by `fire_at`.
    for trigger in [
      trigger_record("late", TriggerStatus::Pending, "2026-10-18T12:00:00Z", "2026-10-18T09:30:00Z"),
      trigger_record("fired", TriggerStatus::Fired, "2026-10-18T10:00:00Z", "2026-10-18T09:20:00Z"),
      trigger_record("soon", TriggerStatus::Pending, "2026-10-18T10:30:00Z", "2026-10-18T09:00:00Z"),
      trigger_record("cancelled", TriggerStatus::Cancelled, "2026-10-18T10:15:00Z", "2026-10-18T09:10:00Z"),
      trigger_record("mid", TriggerStatus::Pending, "2026-10-18T11:00:00Z", "2026-10-18T09:05:00Z"),
    ] {
      write_trigger(&features, &trigger);
    }
    let (status, body) = pending_triggers_response(&features, TRIGGER_SESSION_ID).await;
    assert_eq!(status, StatusCode::OK);
    let records = body.as_array().unwrap();
    let ids: Vec<&str> = records.iter().map(|record| record["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["soon", "mid", "late"]);
    assert!(records.iter().all(|record| record["status"] == "pending"));
  }

  #[tokio::test]
  async fn pending_triggers_refuse_a_missing_session() {
    let (_home, features) = feature_context();
    features.startup.insert(Arc::new(TriggerManager::new(Arc::clone(&features)))).unwrap();
    let (status, body) = pending_triggers_response(&features, "synthetic-missing-session").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": SESSION_NOT_FOUND_DETAIL}));
  }

  #[test]
  fn fallback_events_pages_keep_cold_physical_line_indexes() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(Registry::new());
    let config = registry.load_config(home.path()).unwrap();
    let slots = registry.metadata_slots();
    let store = Store::new(config, slots.clone(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap();
    let session_id = "synthetic-lines";
    let meta = SessionMetadata::new(session_id, "Synthetic lines", TaskProfile::Manager);
    let metadata_path = store.metadata_path(session_id);
    fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    fs::write(&metadata_path, meta.to_json_string(&slots, true)).unwrap();

    let events_path = store.chat_events_path(session_id);
    fs::create_dir_all(events_path.parent().unwrap()).unwrap();
    fs::write(
      &events_path,
      concat!(
        "{\"type\":\"user\",\"id\":\"line-0\",\"content\":\"Question zero\"}\n",
        "\n",
        "{malformed\n",
        "{\"type\":\"assistant\",\"id\":\"line-3\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Answer three\"}]}}\n",
        "{\"type\":\"master_done\",\"id\":\"line-4\"}\n",
        "{\"type\":\"user\",\"id\":\"line-5\",\"content\":\"Question five\"}\n",
        "\n",
        "{malformed\n",
        "{\"type\":\"assistant\",\"id\":\"line-8\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Answer eight\"}]}}\n",
        "{\"type\":\"master_done\",\"id\":\"line-9\"}"
      ),
    )
    .unwrap();

    let cases = [
      (4, 2, 2, vec!["Answer three"]),
      (8, 3, 5, vec!["Question five"]),
      (10, 4, 6, vec!["Answer eight", ""]),
      (3, 5, 0, vec!["Question zero"]),
      (u32::MAX as i64, 4, 6, vec!["Answer eight", ""]),
    ];
    for (before, limit, next_before, expected_content) in cases {
      let page = events_range_page(&store, session_id, 0, before, limit).unwrap();
      let content: Vec<&str> =
        page["messages"].as_array().unwrap().iter().map(|message| message["content"].as_str().unwrap_or("")).collect();
      assert_eq!(content, expected_content, "before={before}, limit={limit}");
      assert_eq!(page["next_before"], next_before, "before={before}, limit={limit}");
      assert_eq!(page["has_more"], next_before > 0, "before={before}, limit={limit}");
    }
  }

  async fn events_page_body(features: &Arc<FeatureContext>, session_id: &str, query: &str) -> Value {
    let response = session_events(
      State(Arc::clone(features)),
      AxumPath(session_id.to_string()),
      Request::builder()
        .uri(format!("/api/sessions/{session_id}/events?{query}"))
        .body(Body::empty())
        .expect("the events request builds"),
    )
    .await;
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::OK);
    body
  }

  /// A first screen whose cursor lands inside its newest turn pages back from that cursor with no gap and no duplicate:
  /// the older page holds the events before the cursor, and the first screen holds the events from it on.
  #[tokio::test]
  async fn an_events_page_before_a_mid_turn_cursor_joins_the_first_screen_with_no_gap() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "mid-cursor-join", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 700, true).await;
    let turn_users: Vec<String> = (0..10).map(|index| fixture_text(300 + index, 1_600)).collect();
    for user in &turn_users {
      features.store.append_chat_event(&meta.id, json!({"type": "user", "content": user})).await.unwrap();
    }
    features.store.append_chat_event(&meta.id, json!({"type": "master_done"})).await.unwrap();
    features.store.append_chat_event(&meta.id, json!({"type": "run_finished", "run_id": "run-2"})).await.unwrap();
    let bootstrap = build_bootstrap_payload(&features.store, &cfg_of(&features), &meta).unwrap();
    let cursor = bootstrap.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    let first_screen = bootstrap.value["messages"].as_array().unwrap();
    assert!(!renders(first_screen, &turn_users[0]), "the cursor lands inside the newest turn");
    assert!(renders(first_screen, &turn_users[9]), "the newest turn's last user message renders");
    let page = events_page_body(&features, &meta.id, &format!("before={cursor}&limit=40")).await;
    let older = page["messages"].as_array().unwrap();
    assert_eq!(page["next_before"], json!(cursor.saturating_sub(40)), "the page starts 40 events before the cursor");
    let mut all_users = vec![fixture_text(1, 700)];
    all_users.extend(turn_users.iter().cloned());
    for user in &all_users {
      assert!(renders(older, user) ^ renders(first_screen, user), "a user message sits on exactly one page");
    }
  }

  /// The events page memo: a repeat request serves the body it built until the live log grows past what the page
  /// covers, then the page rebuilds. A `before` past the log's end names the newest page, so an appended event lands
  /// in it.
  #[tokio::test]
  async fn a_repeat_events_page_serves_its_body_until_the_log_grows() {
    let (_home, features) = feature_context();
    let meta = create_node(&features.store, "events-page-memo", None, TaskProfile::Manager).await;
    for turn in 0..3 {
      features
        .store
        .append_chat_event(
          &meta.id,
          json!({"type": "user", "id": format!("u{turn}"), "content": format!("question {turn}")}),
        )
        .await
        .unwrap();
    }
    let page = events_page_body(&features, &meta.id, "before=1000&limit=40").await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
      events_page_body(&features, &meta.id, "before=1000&limit=40").await,
      page,
      "an unchanged log keeps the body it built"
    );

    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "id": "u3", "content": "question 3"}))
      .await
      .unwrap();
    let grown = events_page_body(&features, &meta.id, "before=1000&limit=40").await;
    assert_eq!(grown["messages"].as_array().unwrap().len(), 4, "a grown log rebuilds the page");
  }

  #[test]
  fn an_events_window_walks_back_through_the_lines_of_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chat_events.jsonl");
    fs::write(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n{\"n\":5}\n").unwrap();
    let size = fs::metadata(&path).unwrap().size();

    let (body, start) = events_window(&path, size, 2, None).unwrap();
    assert_eq!(body.as_ref(), b"{\"n\":4}\n{\"n\":5}\n");
    assert_eq!(start, 24);

    let (body, start) = events_window(&path, size, 2, Some(start)).unwrap();
    assert_eq!(body.as_ref(), b"{\"n\":2}\n{\"n\":3}\n");
    assert_eq!(start, 8);

    let (body, start) = events_window(&path, size, 2, Some(start)).unwrap();
    assert_eq!(body.as_ref(), b"{\"n\":1}\n", "a window short of its count starts at the file's first byte");
    assert_eq!(start, 0);

    let (body, start) = events_window(&path, size, 2, Some(0)).unwrap();
    assert!(body.is_empty(), "a window before the first byte holds no line");
    assert_eq!(start, 0);
  }

  #[test]
  fn an_events_window_counts_a_final_line_without_a_terminator() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chat_events.jsonl");
    fs::write(&path, "a\nb\nc").unwrap();
    let size = fs::metadata(&path).unwrap().size();

    let (body, start) = events_window(&path, size, 1, None).unwrap();
    assert_eq!(body.as_ref(), b"c");
    assert_eq!(start, 4);

    let (body, start) = events_window(&path, size, 2, None).unwrap();
    assert_eq!(body.as_ref(), b"b\nc");
    assert_eq!(start, 2);
  }

  #[test]
  fn an_events_window_snaps_into_line_before_a_mid_line_offset() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chat_events.jsonl");
    fs::write(&path, "a\nb\nc\n").unwrap();
    let size = fs::metadata(&path).unwrap().size();

    let (body, start) = events_window(&path, size, 1, Some(5)).unwrap();
    assert_eq!(body.as_ref(), b"b\n");
    assert_eq!(start, 2);
  }
}
