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
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, static_files};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::text_repr;
use charliebot_core::timefmt;
use charliebot_registry::task_errors::task_http_error;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{HostBoot, RunFacts, RunOrder, RunRecord};
use charliebot_store::trigger_files::{PendingTrigger, TriggerStatus};
use charliebot_store::{MessageProjection, Store, StoreError, StoreResult, TaskFacts};
use charliebot_triggers::triggers::TriggerManager;
use http::header::HeaderValue;
use lru::LruCache;
use serde_json::{Map, Value, json};

use crate::api;
use crate::route_support::{Query, blocking_store};
use crate::{listing, worker_transcript};

const BOOTSTRAP_MESSAGE_LIMIT: usize = 40;
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
/// The value is a [`Bytes`] handle, so a hit shares the one stored buffer through a refcount bump;
/// the largest production log holds about 18 MB of compressed bytes.
type FileGzipMemo = Mutex<LruCache<(PathBuf, i64, u64), Bytes>>;

static EVENTS_FILE_GZIP_MEMO: LazyLock<FileGzipMemo> = LazyLock::new(|| {
  Mutex::new(LruCache::new(NonZeroUsize::new(FILE_GZIP_MEMO_LIMIT).expect("the gzip memo limit is nonzero")))
});

/// Register the session detail, transcript, usage, events, and task-read routes.
pub(crate) fn routes() -> Router<Arc<FeatureContext>> {
  Router::new()
    .route("/api/sessions/{session_id}/pending-triggers", get_only(pending_triggers))
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

/// Build the bootstrap payload for a session, shared with the future index-page owner.
///
/// The payload renders the session's message tail trimmed to [`FIRST_SCREEN_WIRE_BUDGET_BYTES`] on
/// the wire, so the switch's own transfer sets its time on a slow link. The trim drops the tail's
/// oldest whole turns and moves the paging cursor onto the first kept one. The dropped history
/// still reaches the page through scroll-up paging, and the newest turn — its user message
/// included — always renders on the first screen.
pub fn build_bootstrap_payload(
  store: &Store,
  registry: &Registry,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> StoreResult<BootstrapPayload> {
  let response_meta = response_metadata(store, meta)?;
  let mut worker_summary = None;
  // The tail the payload renders, oldest first, with the ordinal of its first slot in the space
  // the session's older pages read. A message ordinal in the projection branches, an event index
  // in the archived branch, whose events fold to messages one to many.
  enum Tail {
    Slots { messages: Vec<Value>, start: usize, projection: Arc<MessageProjection> },
    Events { events: Vec<charliebot_store::ChatEvent>, start: usize },
  }
  let (tail, pending_draft, event_count) = if meta.profile == TaskProfile::Worker {
    let transcript = worker_transcript::load_worker_transcript(store, &meta.id)?;
    let (messages, oldest, _has_more) = transcript.projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
    worker_summary = Some(transcript.summary.clone());
    let pending_draft = transcript.projection.pending_draft().cloned();
    let event_count = transcript.projection.event_count();
    (
      Tail::Slots { messages, start: oldest, projection: Arc::clone(&transcript.projection) },
      pending_draft,
      event_count,
    )
  } else if let Some(projection) = store.get_message_projection(&meta.id)? {
    let (messages, oldest, _has_more) = projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
    let pending_draft = projection.pending_draft().cloned();
    let event_count = projection.event_count();
    (Tail::Slots { messages, start: oldest, projection }, pending_draft, event_count)
  } else {
    let archive_offset = usize::try_from(meta.archive_offset.max(0)).expect("non-negative archive offset fits usize");
    let (events, total_count, _has_more) = store.load_chat_events_tail(&meta.id, BOOTSTRAP_MESSAGE_LIMIT)?;
    let oldest = archive_offset + total_count.saturating_sub(events.len());
    let (_, pending_draft) = store.events_to_view_at(&events, oldest)?;
    (Tail::Events { events, start: oldest }, pending_draft, archive_offset + total_count)
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
  base.extend(active_backend_payload(store, &response_meta, cfg, registry)?);

  // The payload of one trim step: the tail kept from slot `drop` on, its paging cursor moved onto
  // the first kept slot, and `has_more` true whenever older history remains.
  let mut build = |drop: usize| -> StoreResult<Value> {
    let (messages, start) = match &tail {
      Tail::Slots { messages, start, .. } => (messages[drop..].to_vec(), start + drop),
      Tail::Events { events, start } => {
        let (messages, _) = store.events_to_view_at(&events[drop..], start + drop)?;
        (messages, start + drop)
      }
    };
    let mut payload = base.clone();
    payload.insert("messages".to_string(), Value::Array(messages));
    payload.insert("oldest_message_ordinal".to_string(), Value::from(start));
    payload.insert("has_more".to_string(), Value::Bool(start > 0));
    Ok(Value::Object(payload))
  };
  // The next turn start a trim step may drop to, in the tail's own indexes (the `drop` a `build` step takes).
  // A step therefore keeps every kept turn whole and never cuts the newest turn. The projection branches read
  // the projection's separator ordinals, which are history ordinals; the archived branch falls back to the
  // events behind the fold's turn separators (master_done, run_end).
  let next_drop = |drop: usize| -> Option<usize> {
    match &tail {
      Tail::Slots { start, projection, .. } => {
        let start = *start;
        projection.next_turn_start_after(start + drop).map(|next| next - start)
      }
      Tail::Events { events, .. } => events
        .iter()
        .enumerate()
        .skip(drop)
        .find(|(_, event)| {
          matches!(event["type"].as_str(), Some(event_types::MASTER_DONE) | Some(event_types::RUN_END))
        })
        .map(|(index, _)| index + 1)
        .filter(|next| *next < events.len()),
    }
  };
  let (value, untrimmed_wire_bytes) = fit_first_screen_payload(&mut build, &next_drop)?;
  Ok(BootstrapPayload { value, untrimmed_wire_bytes })
}

/// The payload with the tail kept from turn boundary `drop` on, for the smallest turn-aligned
/// `drop` whose gzip fits [`FIRST_SCREEN_WIRE_BUDGET_BYTES`]. Returns the payload and the wire
/// bytes the untrimmed payload takes.
///
/// The trim drops one whole turn per step and stops at the turn boundary when the newest turn
/// alone exceeds the budget. That turn is the first screen, and the served size then exceeds the
/// budget, with the untrimmed size naming it in the response header. Each step serializes and gzips one
/// payload, well under a millisecond of a level-one gzip at these sizes, and the step count is the
/// number of dropped turns.
fn fit_first_screen_payload(
  build: &mut dyn FnMut(usize) -> StoreResult<Value>,
  next_drop: &dyn Fn(usize) -> Option<usize>,
) -> StoreResult<(Value, usize)> {
  let wire_of = |payload: &Value| gzip_level_one(&charliebot_compat::json::fast_json_bytes(payload)).len();
  let mut payload = build(0)?;
  let untrimmed_wire = wire_of(&payload);
  let mut wire = untrimmed_wire;
  let mut drop = 0;
  while wire > FIRST_SCREEN_WIRE_BUDGET_BYTES {
    let Some(next) = next_drop(drop) else { break };
    drop = next;
    payload = build(drop)?;
    wire = wire_of(&payload);
  }
  Ok((payload, untrimmed_wire))
}

/// A session page's bootstrap payload as the page carries it.
#[derive(Clone)]
pub struct PageBootstrap {
  /// The payload's Jinja `tojson` text, the value the page's `SESSION_BOOTSTRAP` constant carries.
  pub text: Arc<String>,
  /// The payload's `session` object.
  pub session: Value,
  pub pending_draft: Value,
  pub event_count: Value,
}

/// The page bootstrap one session produced last, kept while every payload input still holds its value.
///
/// The payload is the page's largest value, and its text is its whole escaped form, so a page request that rebuilds it
/// serializes the session's message tail from scratch. The memo holds one session's bootstrap, about one page's worth
/// of memory, and a page for another session replaces it.
#[derive(Default)]
pub struct PageBootstrapMemo {
  entry: Mutex<Option<(PageBootstrapKey, PageBootstrap)>>,
}

/// The inputs [`build_bootstrap_payload`] reads for a session whose events a live projection holds.
///
/// The projection is shared and immutable: an event that lands moves the store to a new projection, so its pointer
/// names its content. The metadata compares by content, because the store hands a caller its own copy of the row.
/// Everything else compares by value, except the config, which compares by pointer because a reload builds a new one.
struct PageBootstrapKey {
  session_id: String,
  meta: SessionMetadata,
  run_backend: Option<String>,
  thinking_since: Option<UtcDateTime>,
  backend: Map<String, Value>,
  config: Arc<CharlieBotConfig>,
  projection: Arc<MessageProjection>,
}

impl PartialEq for PageBootstrapKey {
  fn eq(&self, other: &Self) -> bool {
    self.session_id == other.session_id
      && self.meta == other.meta
      && self.run_backend == other.run_backend
      && self.thinking_since == other.thinking_since
      && self.backend == other.backend
      && Arc::ptr_eq(&self.config, &other.config)
      && Arc::ptr_eq(&self.projection, &other.projection)
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
  registry: &Registry,
  cfg: &Arc<CharlieBotConfig>,
  meta: &SessionMetadata,
  accepts_gzip: bool,
) -> StoreResult<(Bytes, Option<Bytes>, usize)> {
  let key = page_bootstrap_key(store, registry, cfg, meta)?;
  let mut entry = memo.entry.lock().expect("the bootstrap body memo mutex is not poisoned");
  let cached = match &key {
    Some(current) => match entry.as_ref() {
      Some((cached, body, gzip, untrimmed)) if cached == current => Some((body.clone(), gzip.clone(), *untrimmed)),
      _ => None,
    },
    None => None,
  };
  let (body, mut gzip, untrimmed, held) = match cached {
    Some(cached) => (cached.0, cached.1, cached.2, true),
    None => {
      let payload = build_bootstrap_payload(store, registry, cfg, meta)?;
      let body = Bytes::from(charliebot_compat::json::fast_json_bytes(&payload.value));
      let untrimmed = payload.untrimmed_wire_bytes;
      let held = key.is_some();
      if let Some(key) = key {
        *entry = Some((key, body.clone(), None, untrimmed));
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
pub fn page_bootstrap(
  memo: &PageBootstrapMemo,
  store: &Store,
  registry: &Registry,
  cfg: &Arc<CharlieBotConfig>,
  meta: &SessionMetadata,
) -> StoreResult<PageBootstrap> {
  let key = page_bootstrap_key(store, registry, cfg, meta)?;
  if let Some(key) = &key
    && let Some((cached_key, cached)) =
      memo.entry.lock().expect("the page bootstrap memo mutex is not poisoned").as_ref()
    && cached_key == key
  {
    return Ok(cached.clone());
  }
  let payload = build_bootstrap_payload(store, registry, cfg, meta)?;
  let bootstrap = PageBootstrap {
    text: Arc::new(charliebot_compat::jinja::tojson_text(&payload.value)),
    session: payload.value["session"].clone(),
    pending_draft: payload.value["pending_draft"].clone(),
    event_count: payload.value["event_count"].clone(),
  };
  if let Some(key) = key {
    *memo.entry.lock().expect("the page bootstrap memo mutex is not poisoned") = Some((key, bootstrap.clone()));
  }
  Ok(bootstrap)
}

/// The memo key of `meta`'s payload, or `None` when the payload reads state the key cannot name.
///
/// A worker session's transcript and an archived session's event file are read fresh for every payload, so neither
/// session kind is memoized.
fn page_bootstrap_key(
  store: &Store,
  registry: &Registry,
  cfg: &Arc<CharlieBotConfig>,
  meta: &SessionMetadata,
) -> StoreResult<Option<PageBootstrapKey>> {
  if meta.profile == TaskProfile::Worker || meta.archive_offset != 0 {
    return Ok(None);
  }
  let Some(projection) = store.get_message_projection(&meta.id)? else { return Ok(None) };
  let response_meta = response_metadata(store, meta)?;
  let backend = active_backend_payload(store, &response_meta, cfg, registry)?;
  Ok(Some(PageBootstrapKey {
    session_id: meta.id.clone(),
    meta: meta.clone(),
    run_backend: response_meta.run_backend,
    thinking_since: response_meta.thinking_since,
    backend,
    config: Arc::clone(cfg),
    projection,
  }))
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
  match blocking_store(move || store.session_detail(&meta.id)).await {
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
  let registry = Arc::clone(&features.registry);
  let accepts_gzip = request_wants_gzip(&request);
  let result = tokio::task::spawn_blocking(move || {
    bootstrap_body(&BOOTSTRAP_BODY_MEMO, &store, &registry, &cfg, &meta, accepts_gzip)
  })
  .await;
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
    match session_metadata_value(&features.store, &meta) {
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
  match active_backend_payload(&features.store, &response_meta, &cfg, &features.registry) {
    Ok(fields) => payload.extend(fields),
    Err(error) => return store_error(error),
  }
  responses::fast_json(StatusCode::OK, &Value::Object(payload))
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
  let raw_limit = match query.parse_int("limit", Some(40), None, None) {
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
      &json!({"messages": messages, "has_more": has_more, "next_before": next_before}),
    );
  }

  if meta.archive_offset == 0 {
    let projection = match features.store.get_message_projection(&session_id) {
      Ok(projection) => projection,
      Err(error) => return store_error(error),
    };
    if let Some(projection) = projection {
      let body = match projection.cached_page_body(to_index(before), limit) {
        Some(body) => body,
        None => {
          let (messages, next_before, has_more) = projection.slice_before(to_index(before), limit);
          let body = Bytes::from(charliebot_compat::json::fast_json_bytes(
            &json!({"messages": messages, "has_more": has_more, "next_before": next_before}),
          ));
          projection.store_page_body(to_index(before), limit, body.clone());
          body
        }
      };
      if request_wants_gzip(&request) {
        let compressed = match projection.cached_page_body_gzip(to_index(before), limit) {
          Some(compressed) => compressed,
          None => {
            let compressed = Bytes::from(gzip_level_one(&body));
            projection.store_page_body_gzip(to_index(before), limit, compressed.clone());
            compressed
          }
        };
        return json_bytes_response(compressed, Some("gzip"));
      }
      return json_bytes_response(body, None);
    }
  }

  let payload = match events_range_page(&features.store, &session_id, before, limit) {
    Ok(payload) => payload,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &payload)
}

fn events_range_page(store: &Store, session_id: &str, before: i64, limit: usize) -> StoreResult<Value> {
  let start = if before <= 0 { 0 } else { (before - limit as i64).max(0) as usize };
  let end = usize::try_from(before).unwrap_or(0);
  let (events, has_more) = store.load_chat_events_range(session_id, start, end)?;
  let messages = store.events_to_messages_at(&events, start)?;
  Ok(json!({"messages": messages, "has_more": has_more, "next_before": start}))
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
    .header(header::CONTENT_TYPE, "application/json")
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
    let wants_gzip = request_wants_gzip(&request);
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
  if !request_wants_gzip(&request) {
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
  let limit = match query.parse_int("limit", Some(100), Some(1), Some(500)) {
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
        let input_id = event.get("id").map_or_else(|| "None".to_string(), value_str);
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
    .map_or_else(String::new, value_str);
  Ok(json!({
    "id": event.get("id").map_or_else(|| "None".to_string(), value_str),
    "type": event.get("type").map_or_else(|| "None".to_string(), value_str),
    "timestamp": timestamp,
    "actor": event.get("actor").cloned().unwrap_or(Value::Null),
    "source_session_id": event.get("source_session_id").cloned().unwrap_or(Value::Null),
    "from_session_name": event.get("from_session_name").cloned().unwrap_or(Value::Null),
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
  registry: &Registry,
) -> StoreResult<Map<String, Value>> {
  let live_run_backend = (meta.profile == TaskProfile::Worker).then(|| store.run_backend(&meta.id)).flatten();
  let active_backend = displayed_backend_id(
    meta,
    live_run_backend.as_deref(),
    cfg.backends.options.first().map_or("claude", |option| option.id.as_str()),
  );
  let active_option = cfg.get_backend_option(&active_backend);
  let dedicated = registry
    .binding_for(&meta.id)
    .map_err(|error| StoreError::new(error.to_string()))?
    .is_some_and(|binding| binding.dedicated_backend());
  let switchable = if cfg.backends.pool(&active_backend).is_some() {
    // The switch route refuses an option from a pool session under a dedicated lock: `same_continuation_domain` has no
    // domain for a pool. The list follows the same rule; each pool stays listed, since the route takes its own id as a
    // no-op.
    let mut switchable: Vec<String> =
      if dedicated { Vec::new() } else { cfg.backends.options.iter().map(|option| option.id.clone()).collect() };
    switchable.extend(cfg.backends.pools.iter().map(|pool| pool.id.clone()));
    switchable
  } else if let Some(active_option) = active_option {
    if dedicated {
      let active_lifecycle =
        registry.lifecycle_for(&active_option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
      let active_domain = active_lifecycle.continuation_domain(active_option, cfg);
      let mut switchable = Vec::new();
      for option in &cfg.backends.options {
        let lifecycle =
          registry.lifecycle_for(&option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
        if lifecycle.continuation_domain(option, cfg) == active_domain {
          switchable.push(option.id.clone());
        }
      }
      switchable
    } else {
      let mut switchable: Vec<String> = cfg.backends.options.iter().map(|option| option.id.clone()).collect();
      switchable.extend(cfg.backends.pools.iter().map(|pool| pool.id.clone()));
      switchable
    }
  } else {
    Vec::new()
  };
  // The pickers list only the config's visible entries; the session's own active entry stays exempt, so the
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
fn displayed_backend_id(meta: &SessionMetadata, run_backend: Option<&str>, default_backend: &str) -> String {
  let active_backend = if meta.profile == TaskProfile::Worker {
    run_backend
      .filter(|backend| !backend.is_empty())
      .or_else(|| (!meta.backend.is_empty()).then_some(meta.backend.as_str()))
  } else {
    (!meta.backend.is_empty()).then_some(meta.backend.as_str())
  };
  active_backend.unwrap_or(default_backend).to_string()
}

fn session_metadata_value(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
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

fn gzip_level_one(body: &[u8]) -> Vec<u8> {
  charliebot_compat::compression::gzip_level_one(body)
}

fn json_bytes_response(body: Bytes, encoding: Option<&'static str>) -> Response {
  let mut builder = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::CONTENT_LENGTH, body.len());
  if let Some(encoding) = encoding {
    builder = builder
      .header(header::CONTENT_ENCODING, HeaderValue::from_static(encoding))
      .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
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
    .header(header::VARY, "Accept-Encoding");
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
    .header(header::VARY, "Accept-Encoding")
    .body(Body::from(body))
    .expect("a compressed events response builds")
}

fn request_wants_gzip(request: &Request<Body>) -> bool {
  request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"))
}

fn to_index(value: i64) -> usize {
  usize::try_from(value).unwrap_or(0)
}

fn value_str(value: &Value) -> String {
  match value {
    Value::String(value) => value.clone(),
    Value::Null => "None".to_string(),
    Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
    _ => value.to_string(),
  }
}

fn path_error(path: &Path, error: std::io::Error) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use charliebot_registry::{Registry, store_hooks};
  use charliebot_store::run_records::RunKind;
  use charliebot_store::{CreateTask, TaskCaller};

  use super::*;
  use crate::test_support::{create_node, feature_context, feature_context_with_auto};

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

    let payload = features.store.session_detail(&meta.id).expect("the detail reads");
    assert_eq!(payload["backend"], "auto");
    assert_eq!(payload["pool_member"], "synthetic-a");

    let cfg = features.config.get().expect("the test config loads");
    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
    assert_eq!(backend["active_backend"], "auto");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b", "auto"]));
  }

  #[tokio::test]
  async fn a_session_on_a_plain_option_lists_the_pools_when_pools_are_configured() {
    let (_home, features) = feature_context_with_auto(AUTO_CONFIG);
    let meta = create_node(&features.store, "plain-option-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
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

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
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

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
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

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
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

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
    assert_eq!(backend["active_backend"], "light");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "auto", "light"]));
  }

  #[tokio::test]
  async fn a_dedicated_lock_keeps_the_pools_out_of_the_switch_list() {
    let (_home, features) = feature_context_with_auto(AUTO_CONFIG);
    let request = CreateTask {
      request_id: "locked-plain-option-node".to_string(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some("Locked plain option".to_string()),
      backend: Some("synthetic-a".to_string()),
      group: None,
      session_id: Some("sequence-locked-node".to_string()),
    };
    let meta = features.store.create_task(request, TaskCaller::System, Map::new()).await.expect("the locked node");
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a"]));
  }

  #[tokio::test]
  async fn a_dedicated_pool_session_lists_only_its_pools_as_switchable() {
    let (_home, features) = feature_context_with_auto(AUTO_CONFIG);
    let request = CreateTask {
      request_id: "locked-auto-node".to_string(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some("Locked Auto".to_string()),
      backend: Some("auto".to_string()),
      group: None,
      session_id: Some("sequence-locked-node".to_string()),
    };
    let meta = features.store.create_task(request, TaskCaller::System, Map::new()).await.expect("the locked node");
    features
      .store
      .update_slot_fields(
        &meta.id,
        charliebot_agents::model_pool::OWNER,
        Map::from_iter([("pool_member".to_string(), json!("synthetic-a"))]),
      )
      .await
      .expect("the member is recorded");
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
    assert_eq!(backend["active_backend"], "auto");
    assert_eq!(backend["switchable_backends"], json!(["auto"]));
  }

  #[tokio::test]
  async fn without_pools_the_switch_list_holds_the_options_only() {
    let (_home, features) = feature_context();
    let meta = create_node(&features.store, "plain-no-pool-node", None, TaskProfile::Manager).await;
    let cfg = features.config.get().expect("the test config loads");

    let backend = active_backend_payload(&features.store, &meta, &cfg, &features.registry).expect("the backend reads");
    assert_eq!(backend["switchable_backends"], json!(["synthetic-a", "synthetic-b"]));
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
    let fresh = charliebot_compat::json::fast_json_bytes(
      &build_bootstrap_payload(&features.store, &features.registry, &cfg, &meta).unwrap().value,
    );
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
    let page_bootstrap_text =
      |memo: &PageBootstrapMemo| page_bootstrap(memo, &features.store, &features.registry, &cfg, &meta).unwrap();

    let first = page_bootstrap_text(&memo);
    let payload = build_bootstrap_payload(&features.store, &features.registry, &cfg, &meta).unwrap();
    assert_eq!(first.text.as_str(), charliebot_compat::jinja::tojson_text(&payload.value));
    assert_eq!(first.session, payload.value["session"]);

    let repeat = page_bootstrap_text(&memo);
    assert!(Arc::ptr_eq(&first.text, &repeat.text), "an unchanged payload keeps its text");
    assert_eq!(repeat.pending_draft, first.pending_draft);

    features
      .store
      .append_chat_event(&meta.id, json!({"type": "assistant", "content": "an answer for the page"}))
      .await
      .unwrap();
    let after = page_bootstrap_text(&memo);
    let payload_after = build_bootstrap_payload(&features.store, &features.registry, &cfg, &meta).unwrap();
    assert_eq!(after.text.as_str(), charliebot_compat::jinja::tojson_text(&payload_after.value));
    assert_ne!(after.text.as_str(), first.text.as_str(), "a new event rebuilds the text");
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
    let (payload, untrimmed) = fit_first_screen_payload(&mut build, &next_drop).unwrap();
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
    let (payload, untrimmed) = fit_first_screen_payload(&mut build, &|_| None).unwrap();
    assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
    assert!(untrimmed > FIRST_SCREEN_WIRE_BUDGET_BYTES);
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
    }
    let payload = build_bootstrap_payload(&features.store, &features.registry, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let kept = messages.len();
    assert!(kept > 0 && kept < 24, "kept {kept} of the 24-slot tail");
    let served_wire = gzip_level_one(&charliebot_compat::json::fast_json_bytes(&payload.value)).len();
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
    assert_eq!(payload.value["has_more"], json!(true));
    // A turn is three messages (user, assistant, separator), so the kept suffix starts at a turn
    // boundary and the newest turn's user message is on the first screen.
    assert_eq!(kept % 3, 0, "the kept slots are whole turns");
    assert_eq!(payload.value["oldest_message_ordinal"], json!(24 - kept));
    assert_eq!(messages[0]["role"], "user", "a kept turn starts at its user message");
    let newest_marker = messages.iter().find(|message| message["content"] == json!(fixture_text(8, 1_600)));
    assert!(newest_marker.is_some(), "the newest turn's user message renders on the first screen");
  }

  /// Appends one turn of the fixture: a user message of `user_chars`, an assistant reply when `with_reply`, and the
  /// turn's separator.
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
  }

  /// A tail that starts past the history's first slot trims in its own indexes. The kept slots are the tail's last
  /// whole turns, and the paging cursor is the absolute ordinal of the first kept slot. The turns take 3, 3, 2, 2
  /// slots in turn, and the tail starts at slot 6, which is 6 modulo the 10-slot period. A trim that walked the
  /// history's turn starts as the tail's own indexes would then land between turns at every step.
  #[tokio::test]
  async fn a_first_screen_trim_past_the_history_start_keeps_whole_turns_at_absolute_ordinals() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "trim-long-bootstrap", None, TaskProfile::Manager).await;
    let turns = 18usize;
    let mut turn_starts = Vec::new();
    let mut total = 0usize;
    for turn in 1..=turns {
      turn_starts.push(total);
      let with_reply = matches!(turn % 4, 1 | 2);
      append_fixture_turn(&features.store, &meta.id, turn, 700, with_reply).await;
      total += if with_reply { 3 } else { 2 };
    }
    let payload = build_bootstrap_payload(&features.store, &features.registry, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    let start = payload.value["oldest_message_ordinal"].as_u64().unwrap() as usize;
    assert!(start > 0, "the tail starts past the history's first slot");
    let first_turn = turn_starts.iter().position(|turn_start| *turn_start == start);
    let first_turn = first_turn.expect("the kept slots start at a turn boundary") + 1;
    assert_eq!(start + messages.len(), total, "the kept slots run to the history's end");
    assert_eq!(
      messages[0]["content"],
      json!(fixture_text(first_turn as u64, 700)),
      "the first kept slot is its turn's user message"
    );
    let served_wire = gzip_level_one(&charliebot_compat::json::fast_json_bytes(&payload.value)).len();
    assert!(served_wire <= FIRST_SCREEN_WIRE_BUDGET_BYTES, "served {served_wire} over the budget");
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
    assert_eq!(payload.value["has_more"], json!(true));
    assert!(
      messages.iter().any(|message| message["content"] == json!(fixture_text(turns as u64, 700))),
      "the newest turn renders on the first screen"
    );
  }

  /// A closed newest turn over the whole budget renders whole: the trim stops at its start, not past its end.
  #[tokio::test]
  async fn a_closed_newest_turn_over_the_budget_renders_whole() {
    let (_home, features) = feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "trim-oversized-turn", None, TaskProfile::Manager).await;
    append_fixture_turn(&features.store, &meta.id, 1, 1_600, true).await;
    append_fixture_turn(&features.store, &meta.id, 2, 20_000, true).await;
    let payload = build_bootstrap_payload(&features.store, &features.registry, &cfg_of(&features), &meta).unwrap();
    let messages = payload.value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "the newest turn's three slots render");
    assert_eq!(messages[0]["content"], json!(fixture_text(2, 20_000)));
    assert_eq!(payload.value["oldest_message_ordinal"], json!(3));
    assert!(payload.untrimmed_wire_bytes > FIRST_SCREEN_WIRE_BUDGET_BYTES);
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
    let payload = build_bootstrap_payload(&features.store, &features.registry, &cfg_of(&features), &meta).unwrap();
    assert_eq!(untrimmed, payload.untrimmed_wire_bytes);
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
      boundary_index: Some(0),
      input_candidates: vec![Arc::new(json!({"id": "held"}).into()), Arc::new(json!({"id": "free"}).into())],
      ..TaskFacts::default()
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
      boundary_index: Some(0),
      input_candidates: vec![
        Arc::new(json!({"id": "confirmed"}).into()),
        Arc::new(json!({"id": "claimed"}).into()),
        Arc::new(json!({"id": "pending"}).into()),
      ],
      ..TaskFacts::default()
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
    assert_eq!(after_first.file_reads - before.file_reads, 1);
    assert_eq!(after_first.full_folds - before.full_folds, 1);
    let (second_status, _) =
      crate::test_support::answer(pending_task_inputs(State(Arc::clone(&features)), AxumPath(meta.id)).await).await;
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(features.store.history_work(), after_first);
  }

  #[test]
  fn displayed_backend_uses_worker_map_and_metadata_fallback() {
    let mut manager = SessionMetadata::new("synthetic-manager", "Synthetic manager", TaskProfile::Manager);
    manager.backend = "synthetic-configured-backend".to_string();
    manager.run_backend = Some("synthetic-retired-backend".to_string());
    assert_eq!(
      displayed_backend_id(&manager, Some("synthetic-retired-backend"), "claude"),
      "synthetic-configured-backend"
    );

    let mut worker = SessionMetadata::new("synthetic-worker", "Synthetic worker", TaskProfile::Worker);
    worker.backend = "synthetic-configured-backend".to_string();
    worker.run_backend = Some("synthetic-retired-backend".to_string());
    assert_eq!(displayed_backend_id(&worker, None, "claude"), "synthetic-configured-backend");
    assert_eq!(displayed_backend_id(&worker, Some("synthetic-retired-backend"), "claude"), "synthetic-retired-backend");

    let (_home, features) = feature_context();
    features.store.note_run_backend(&worker.id, "synthetic-retired-backend");
    let cfg = features.config.get().expect("the test config loads");
    let payload = active_backend_payload(&features.store, &worker, &cfg, &features.registry).unwrap();
    assert_eq!(payload["active_backend"], "synthetic-retired-backend");
    assert_eq!(payload["active_backend_type"], "");
    assert_eq!(payload["switchable_backends"], json!([]));
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
    assert_eq!(body, json!({"detail": "Session not found"}));
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
    ];
    for (before, limit, next_before, expected_content) in cases {
      let page = events_range_page(&store, session_id, before, limit).unwrap();
      let content: Vec<&str> =
        page["messages"].as_array().unwrap().iter().map(|message| message["content"].as_str().unwrap_or("")).collect();
      assert_eq!(content, expected_content, "before={before}, limit={limit}");
      assert_eq!(page["next_before"], next_before, "before={before}, limit={limit}");
      assert_eq!(page["has_more"], next_before > 0, "before={before}, limit={limit}");
    }
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
