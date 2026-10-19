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
use axum::routing::get;
use charliebot_compat::{responses, static_files};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pyfmt;
use charliebot_registry::task_errors::task_http_error;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{self, RunOrder, RunRecord};
use charliebot_store::{Store, StoreError, StoreResult, TaskFacts};
use flate2::Compression;
use flate2::write::GzEncoder;
use http::header::HeaderValue;
use serde_json::{Map, Value, json};

use crate::listing;
use crate::routes::Query;
use crate::worker_transcript;

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
    .route("/api/sessions/{session_id}", get(session_detail).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/bootstrap", get(session_bootstrap).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/usage", get(session_usage).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/events", get(session_events).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/transcript", get(session_transcript).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/events.jsonl", get(events_jsonl).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/runs", get(session_runs).fallback(method_not_allowed))
    .route("/api/sessions/{session_id}/task-inputs/pending", get(pending_task_inputs).fallback(method_not_allowed))
}

/// Build the bootstrap payload for a session, shared with the future index-page owner.
pub fn build_bootstrap_payload(
  store: &Store,
  registry: &Registry,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> StoreResult<Value> {
  let response_meta = response_metadata(store, meta)?;
  let (messages, pending_draft, event_count, oldest_message_ordinal, has_more) = if meta.profile == TaskProfile::Worker
  {
    let transcript = worker_transcript::load_worker_transcript(store, &meta.id)?;
    let (messages, oldest, has_more) = transcript.projection.tail(BOOTSTRAP_MESSAGE_LIMIT);
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
  payload.insert("session".to_string(), listing::metadata_value(&response_meta)?);
  payload.insert("messages".to_string(), Value::Array(messages));
  payload.insert("pending_draft".to_string(), pending_draft.unwrap_or(Value::Null));
  payload.insert("event_count".to_string(), Value::from(event_count));
  payload.insert("oldest_message_ordinal".to_string(), Value::from(oldest_message_ordinal));
  payload.insert("has_more".to_string(), Value::Bool(has_more));
  payload.extend(active_backend_payload(&response_meta, cfg, registry)?);
  Ok(Value::Object(payload))
}

async fn session_detail(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  match session_detail_payload(&features.store, &meta).await {
    Ok(payload) => responses::fast_json(StatusCode::OK, &payload),
    Err(error) => task_http_error(error),
  }
}

async fn session_detail_payload(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  let mut detail = serde_json::to_value(meta)?;
  let object = detail.as_object_mut().expect("session metadata serializes to an object");
  object.insert("task_state".to_string(), Value::String(store.task_state(&meta.id)?));
  object.insert("work_state".to_string(), Value::String(store.task_tree_activity(&meta.id)?.work_state));
  object.insert("archived".to_string(), Value::Bool(store.archived_of(&meta.id)?));

  let mut ancestors = Vec::new();
  for ancestor_id in store.ancestor_ids(&meta.id)? {
    let ancestor = store
      .get_session(&ancestor_id)
      .await?
      .ok_or_else(|| StoreError::not_found(format!("task {ancestor_id} not found")))?;
    ancestors.push(json!({"id": ancestor.id, "name": ancestor.name}));
  }
  object.insert("ancestors".to_string(), Value::Array(ancestors));

  let descendants = store.descendant_ids(&meta.id)?.len();
  let prompt_rules = json!({
    "subtree": prompt_rule_summary(store, meta.subtree_prompt_ref.as_deref())?,
    "node": prompt_rule_summary(store, meta.node_prompt_ref.as_deref())?,
    "affected_descendants": descendants,
  });
  object.insert("prompt_rules".to_string(), prompt_rules);
  Ok(detail)
}

fn prompt_rule_summary(store: &Store, prompt_ref: Option<&str>) -> StoreResult<Value> {
  let Some(prompt_ref) = prompt_ref else {
    return Ok(json!({"ref": null, "source": null, "chars": 0, "text": null}));
  };
  let path = store.prompt_bodies_dir().join(format!("{prompt_ref}.md"));
  let body = fs::read_to_string(&path).map_err(|error| path_error(&path, error))?;
  Ok(json!({
    "ref": prompt_ref,
    "source": path.to_string_lossy(),
    "chars": body.chars().count(),
    "text": body,
  }))
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
  match build_bootstrap_payload(&features.store, &features.registry, &cfg, &meta) {
    Ok(payload) => gzip_json_body(&request, charliebot_compat::json::fast_json_bytes(&payload)),
    Err(error) => store_error(error),
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
  let usage_owner = match features.startup.require::<charliebot_usage::SessionUsage>() {
    Ok(usage) => usage,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let (usage, active_run_id) = if meta.profile == TaskProfile::Worker {
    match worker_transcript::load_worker_transcript(&features.store, &session_id) {
      Ok(transcript) => (usage_owner.resolve_events_usage(&transcript.events), transcript.active_run_id.clone()),
      Err(error) => return store_error(error),
    }
  } else {
    (usage_owner.resolve_session_usage(&session_id, &meta), None)
  };
  let usage = match usage {
    Ok(usage) => usage.unwrap_or(Value::Null),
    Err(error) => return store_error(error),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
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
  response_meta.run_backend = match listing::run_backend(&features.store, &session_id) {
    Ok(backend) => backend,
    Err(error) => return store_error(error),
  };
  response_meta.thinking_since = features.store.busy_since(&session_id);
  match active_backend_payload(&response_meta, &cfg, &features.registry) {
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
  let query = match Query::new(&request) {
    Ok(query) => query,
    Err(response) => return *response,
  };
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

  let start = if before <= 0 { 0 } else { (before - limit as i64).max(0) as usize };
  let end = usize::try_from(before).unwrap_or(0);
  let (events, has_more) = if meta.archive_offset == 0 {
    let snapshot = match features.store.load_chat_events_shared_sync(&session_id) {
      Ok(snapshot) => snapshot,
      Err(error) => return store_error(error),
    };
    let page_start = start.min(snapshot.len());
    let page_end = end.min(snapshot.len()).max(page_start);
    (snapshot[page_start..page_end].to_vec(), start > 0)
  } else {
    match features.store.load_chat_events_range(&session_id, start, end) {
      Ok(result) => result,
      Err(error) => return store_error(error),
    }
  };
  let messages = match features.store.events_to_messages_at(&events, start) {
    Ok(messages) => messages,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &json!({"messages": messages, "has_more": has_more, "next_before": start}))
}

async fn session_transcript(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let query = match Query::new(&request) {
    Ok(query) => query,
    Err(response) => return *response,
  };
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
  let messages = if reset { committed.to_vec() } else { committed.get(to_index(after)..).unwrap_or(&[]).to_vec() };
  let thinking_since = features.store.busy_since(&session_id).map_or(Value::Null, |at| json!(at.to_python_isoformat()));
  responses::fast_json(
    StatusCode::OK,
    &json!({
      "messages": messages,
      "total": committed.len(),
      "pending_draft": transcript.projection.pending_draft(),
      "revision": transcript.revision,
      "reset": reset,
      "active_run_id": transcript.active_run_id,
      "thinking_since": thinking_since,
    }),
  )
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
  match gzip_file(&path, metadata) {
    Ok(body) => events_gzip_response(body),
    Err(error) => store_error(error),
  }
}

async fn session_runs(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  request: Request<Body>,
) -> Response {
  let query = match Query::new(&request) {
    Ok(query) => query,
    Err(response) => return *response,
  };
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
  let events = match features.store.load_all_facts_shared_sync(&meta.id) {
    Ok(events) => events,
    Err(error) => return store_error(error),
  };
  let mut items = Vec::with_capacity(page.items.len());
  for run in page.items {
    let state = match features.store.run_display_state(&meta.id, &run.id) {
      Ok(state) => state.as_str(),
      Err(error) => return store_error(error),
    };
    let mut row = match serde_json::to_value(&run) {
      Ok(row) => row,
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    };
    let object = row.as_object_mut().expect("run record serializes to an object");
    object.insert("state".to_string(), Value::String(state.to_string()));
    object.insert(
      "stop_requested".to_string(),
      Value::Bool(run_records::stop_requested_in_events(events.as_slice(), &run.id, None)),
    );
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
  let facts = match features.store.facts_of(&meta.id) {
    Ok(facts) => facts,
    Err(error) => return task_http_error(error),
  };
  let run_records = match features.store.list_run_records_sync(&meta.id) {
    Ok(runs) => runs,
    Err(error) => return task_http_error(error),
  };
  let all_events = match features.store.load_all_facts_shared_sync(&meta.id) {
    Ok(events) => events,
    Err(error) => return task_http_error(error),
  };
  let pending = match pending_input_events(&facts, &run_records, all_events.as_slice(), &meta.id) {
    Ok(pending) => pending,
    Err(error) => return task_http_error(error),
  };
  let items = match pending.iter().map(|event| pending_input_value(event)).collect::<StoreResult<Vec<Value>>>() {
    Ok(items) => items,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &json!({"items": items}))
}

fn pending_input_events<'a>(
  facts: &'a TaskFacts,
  runs: &[RunRecord],
  events: &[Value],
  session_id: &str,
) -> StoreResult<Vec<&'a Value>> {
  if facts.boundary_index.is_none() && facts.imported_pending_ids.is_empty() {
    return Err(StoreError::invalid(format!(
      "task {session_id} has no creation or import boundary; it is not a task-tree node"
    )));
  }
  let mut claimed = HashSet::new();
  for run in runs {
    let launched = run.pid.is_some() || run_records::terminal_outcome(events, &run.id)?.is_some();
    if !launched && run_records::stop_requested_in_events(events, &run.id, None) {
      continue;
    }
    claimed.extend(run.input_event_ids.iter().cloned());
  }
  let mut seen = HashSet::new();
  Ok(
    facts
      .input_candidates
      .iter()
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
  meta: &SessionMetadata,
  cfg: &CharlieBotConfig,
  registry: &Registry,
) -> StoreResult<Map<String, Value>> {
  let active_backend = meta
    .run_backend
    .as_deref()
    .filter(|backend| !backend.is_empty())
    .or_else(|| (!meta.backend.is_empty()).then_some(meta.backend.as_str()))
    .unwrap_or_else(|| cfg.backends.options.first().map_or("claude", |option| option.id.as_str()))
    .to_string();
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

fn session_metadata_value(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  listing::metadata_value(&response_metadata(store, meta)?)
}

fn response_metadata(store: &Store, meta: &SessionMetadata) -> StoreResult<SessionMetadata> {
  let mut response_meta = meta.clone();
  response_meta.run_backend = listing::run_backend(store, &meta.id)?;
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

async fn method_not_allowed() -> Response {
  responses::method_not_allowed("GET")
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
  let mut encoder = GzEncoder::new(Vec::new(), Compression::new(1));
  use std::io::Write;
  encoder.write_all(body).expect("gzip writes into a Vec");
  encoder.finish().expect("gzip finishes into a Vec")
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
  use super::*;
  use charliebot_store::run_records::RunKind;

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

  #[test]
  fn pending_inputs_release_a_stopped_unlaunched_run_batch() {
    let facts = TaskFacts {
      boundary_index: Some(0),
      input_candidates: vec![json!({"id": "held"}), json!({"id": "free"})],
      ..TaskFacts::default()
    };
    let events = vec![json!({"type": "run_stop_requested", "run_id": "stopped"})];
    let pending = pending_input_events(&facts, &[run("stopped", &["held"], None)], &events, "worker").unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0]["id"], "held");
    assert_eq!(pending[1]["id"], "free");
  }

  #[test]
  fn pending_inputs_remove_confirmed_and_claimed_ids() {
    let mut facts = TaskFacts {
      boundary_index: Some(0),
      input_candidates: vec![json!({"id": "confirmed"}), json!({"id": "claimed"}), json!({"id": "pending"})],
      ..TaskFacts::default()
    };
    facts.confirmed_input_ids.insert("confirmed".to_string());
    let pending = pending_input_events(&facts, &[run("live", &["claimed"], Some(12))], &[], "worker").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], "pending");
  }
}
