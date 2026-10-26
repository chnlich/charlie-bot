//! Per-session read routes used by the chat view.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, static_files};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pyfmt;
use charliebot_registry::task_errors::task_http_error;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_sched::triggers::{PendingTrigger, TriggerManager, TriggerStatus};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{HostBoot, RunFacts, RunOrder, RunRecord};
use charliebot_store::{Store, StoreError, StoreResult, TaskFacts};
use http::header::HeaderValue;
use serde_json::{Map, Value, json};

use crate::routes::Query;
use crate::{blocking_store, listing, worker_transcript};

const BOOTSTRAP_MESSAGE_LIMIT: usize = 40;
const FILE_GZIP_MEMO_LIMIT: usize = 2;
const BOOTSTRAP_GZIP_MEMO_LIMIT: usize = 16;

type BodyGzipMemo = Mutex<VecDeque<(Vec<u8>, Vec<u8>)>>;
type FileGzipMemo = Mutex<VecDeque<(PathBuf, i64, u64, Vec<u8>)>>;

static BOOTSTRAP_GZIP_MEMO: LazyLock<BodyGzipMemo> = LazyLock::new(|| Mutex::new(VecDeque::new()));
static EVENTS_FILE_GZIP_MEMO: LazyLock<FileGzipMemo> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Register the session detail, transcript, usage, events, and task-read routes.
pub(crate) fn routes() -> Router<Arc<FeatureContext>> {
  Router::new()
    .route("/api/sessions/{session_id}/pending-triggers", get_only(pending_triggers))
    .route("/api/sessions/{session_id}/bootstrap", get_only(session_bootstrap))
    .route("/api/sessions/{session_id}/usage", get_only(session_usage))
    .route("/api/sessions/{session_id}/events", get_only(session_events))
    .route("/api/sessions/{session_id}/transcript", get_only(session_transcript))
    .route("/api/sessions/{session_id}/events.jsonl", get_only(events_jsonl))
    .route("/api/sessions/{session_id}/runs", get_only(session_runs))
    .route("/api/sessions/{session_id}/task-inputs/pending", get_only(pending_task_inputs))
}

/// Build the bootstrap payload for a session, shared with the future index-page owner.
pub fn build_bootstrap_payload(
  store: &Store,
  registry: &Registry,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> StoreResult<Value> {
  let response_meta = response_metadata(store, meta)?;
  let mut worker_summary = None;
  let (messages, pending_draft, event_count, oldest_message_ordinal, has_more) = if meta.profile == TaskProfile::Worker
  {
    let transcript = worker_transcript::load_worker_transcript(store, &meta.id)?;
    let (messages, oldest, has_more) = transcript.projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
    worker_summary = Some(transcript.summary.clone());
    (messages, transcript.projection.pending_draft().cloned(), transcript.projection.event_count(), oldest, has_more)
  } else if let Some(projection) = store.get_message_projection(&meta.id)? {
    let (messages, oldest, has_more) = projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
    (messages, projection.pending_draft().cloned(), projection.event_count(), oldest, has_more)
  } else {
    let archive_offset = usize::try_from(meta.archive_offset.max(0)).expect("non-negative archive offset fits usize");
    let (events, total_count, has_more) = store.load_chat_events_tail(&meta.id, BOOTSTRAP_MESSAGE_LIMIT)?;
    let oldest = archive_offset + total_count.saturating_sub(events.len());
    let (messages, pending_draft) = store.events_to_view_at(&events, oldest)?;
    (messages, pending_draft, archive_offset + total_count, oldest, has_more || archive_offset > 0)
  };

  let mut payload = Map::new();
  payload.insert("session".to_string(), listing::metadata_value(store, &response_meta)?);
  payload.insert("messages".to_string(), Value::Array(messages));
  payload.insert("pending_draft".to_string(), pending_draft.unwrap_or(Value::Null));
  payload.insert("event_count".to_string(), Value::from(event_count));
  payload.insert("oldest_message_ordinal".to_string(), Value::from(oldest_message_ordinal));
  payload.insert("has_more".to_string(), Value::Bool(has_more));
  if let Some(summary) = worker_summary {
    payload.insert("worker_summary".to_string(), summary);
  }
  payload.extend(active_backend_payload(store, &response_meta, cfg, registry)?);
  Ok(Value::Object(payload))
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
  // A stable sort keeps `list_triggers`' creation order among equal deadlines, as Python's `sorted` does.
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
  let result = tokio::task::spawn_blocking(move || build_bootstrap_payload(&store, &registry, &cfg, &meta)).await;
  match result {
    Ok(Ok(payload)) => gzip_json_body(&request, charliebot_compat::json::fast_json_bytes(&payload)),
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
          usage_owner.resolve_events_usage(&events, run_kind, cfg.get_backend_option(&meta.backend)),
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
          let body = charliebot_compat::json::fast_json_bytes(
            &json!({"messages": messages, "has_more": has_more, "next_before": next_before}),
          );
          projection.store_page_body(to_index(before), limit, body.clone());
          body
        }
      };
      if request_wants_gzip(&request) {
        let compressed = match projection.cached_page_body_gzip(to_index(before), limit) {
          Some(compressed) => compressed,
          None => {
            let compressed = gzip_level_one(&body);
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
  let thinking_since = features.store.busy_since(&session_id).map_or(Value::Null, |at| json!(at.to_python_isoformat()));
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

// Borrow the cached projection through serialization as Python does at src/runtime/api/sessions.py:1158-1170.
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
        &json!({"detail": format!("unknown runs order: {} (use 'asc' or 'desc')", pyfmt::str_repr(other))}),
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
    return Err(StoreError::invalid(format!(
      "task {session_id} has no creation or import boundary; it is not a task-tree node"
    )));
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
        let input_id = event.get("id").map_or_else(|| "None".to_string(), python_string);
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
      Value::String(timestamp.to_python_string())
    }
    Some(value) => return Err(StoreError::new(format!("invalid pending input timestamp: {value}"))),
  };
  let text = event
    .get("content")
    .filter(|value| python_truthy(Some(value)))
    .or_else(|| event.get("summary").filter(|value| python_truthy(Some(value))))
    .map_or_else(String::new, python_string);
  Ok(json!({
    "id": event.get("id").map_or_else(|| "None".to_string(), python_string),
    "type": event.get("type").map_or_else(|| "None".to_string(), python_string),
    "timestamp": timestamp,
    "actor": event.get("actor").cloned().unwrap_or(Value::Null),
    "source_session_id": event.get("source_session_id").cloned().unwrap_or(Value::Null),
    "from_session_name": event.get("from_session_name").cloned().unwrap_or(Value::Null),
    "text": text,
  }))
}

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
  let switchable = if let Some(active_option) = active_option {
    if dedicated {
      let active_lifecycle =
        registry.lifecycle_for(&active_option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
      let active_domain = active_lifecycle.continuation_domain(active_option, cfg);
      let mut switchable = Vec::new();
      for option in &cfg.backends.options {
        let lifecycle =
          registry.lifecycle_for(&option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
        if lifecycle.continuation_domain(option, cfg) == active_domain {
          switchable.push(Value::String(option.id.clone()));
        }
      }
      switchable
    } else {
      cfg.backends.options.iter().map(|option| Value::String(option.id.clone())).collect()
    }
  } else {
    Vec::new()
  };
  let mut payload = Map::new();
  payload.insert("active_backend".to_string(), Value::String(active_backend.clone()));
  payload.insert(
    "active_backend_type".to_string(),
    Value::String(active_option.map_or_else(String::new, |option| option.backend_type.clone())),
  );
  payload.insert("switchable_backends".to_string(), Value::Array(switchable));
  Ok(payload)
}

/// Python's `displayed_backend_id`: only a worker's in-memory run backend overrides its metadata backend.
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
  crate::routes::store_error(error)
}

fn is_bad_run_cursor(error: &StoreError) -> bool {
  let detail = error.to_string();
  detail.starts_with("malformed run page cursor") || detail.starts_with("run page cursor was minted")
}

fn events_file_not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Events file not found"}))
}

fn gzip_json_body(request: &Request<Body>, body: Vec<u8>) -> Response {
  if !request_wants_gzip(request) {
    return json_bytes_response(body, None);
  }
  let memo = &BOOTSTRAP_GZIP_MEMO;
  let compressed = {
    let mut memo = memo.lock().expect("bootstrap gzip memo mutex is not poisoned");
    if let Some(index) = memo.iter().position(|(plain, _)| *plain == body) {
      let entry = memo.remove(index).expect("gzip memo index names an entry");
      let compressed = entry.1.clone();
      memo.push_back(entry);
      compressed
    } else {
      let compressed = gzip_level_one(&body);
      memo.push_back((body, compressed.clone()));
      while memo.len() > BOOTSTRAP_GZIP_MEMO_LIMIT {
        memo.pop_front();
      }
      compressed
    }
  };
  json_bytes_response(compressed, Some("gzip"))
}

fn gzip_file(path: &Path, metadata: fs::Metadata) -> StoreResult<Vec<u8>> {
  let mtime_ns = metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000;
  let size = metadata.size();
  {
    let mut memo = EVENTS_FILE_GZIP_MEMO.lock().expect("events-file gzip memo mutex is not poisoned");
    if let Some(index) = memo.iter().position(|(cached_path, cached_mtime, cached_size, _)| {
      cached_path == path && *cached_mtime == mtime_ns && *cached_size == size
    }) {
      let entry = memo.remove(index).expect("gzip memo index names an entry");
      let body = entry.3.clone();
      memo.push_back(entry);
      return Ok(body);
    }
  }
  let bytes = fs::read(path).map_err(|error| path_error(path, error))?;
  let compressed = gzip_level_one(&bytes);
  let mut memo = EVENTS_FILE_GZIP_MEMO.lock().expect("events-file gzip memo mutex is not poisoned");
  memo.retain(|(cached_path, _, _, _)| cached_path != path);
  memo.push_back((path.to_path_buf(), mtime_ns, size, compressed.clone()));
  while memo.len() > FILE_GZIP_MEMO_LIMIT {
    memo.pop_front();
  }
  Ok(compressed)
}

fn gzip_level_one(body: &[u8]) -> Vec<u8> {
  charliebot_compat::compression::gzip_level_one(body)
}

fn json_bytes_response(body: Vec<u8>, encoding: Option<&'static str>) -> Response {
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

fn events_gzip_response(body: Vec<u8>) -> Response {
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

fn python_truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(value)) => *value,
    Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(value)) => !value.is_empty(),
    Some(Value::Object(value)) => !value.is_empty(),
  }
}

fn python_string(value: &Value) -> String {
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

  use super::*;
  use crate::test_support::feature_context;

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

  /// The bootstrap's body-keyed gzip memo: the first gzip answer deflates the plain body once,
  /// and a repeat serves the stored bytes without charging the deflator again.
  #[tokio::test]
  async fn a_repeat_bootstrap_deflates_no_further_body_bytes() {
    let (_home, features) = feature_context();
    let meta = crate::test_support::create_node(&features.store, "gzip-bootstrap", None, TaskProfile::Manager).await;
    features
      .store
      .append_chat_event(&meta.id, json!({"type": "user", "content": "a question for the gzip memo"}))
      .await
      .unwrap();
    let plain = bootstrap_body(&features, &meta.id, false).await;

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
  fn displayed_backend_uses_python_worker_map_and_metadata_fallback() {
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
}
