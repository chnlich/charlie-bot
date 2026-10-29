use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use lru::LruCache;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use charliebot_compat::routing::{get_only, post_only, refuse_other_methods};
use charliebot_compat::{responses, validation};
use charliebot_registry::FeatureContext;
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::sidebar_state;
use http::header::HeaderValue;
use serde_json::{Map, Value, json};

use crate::api;
use crate::listing::{self, SessionsListMemos};
use crate::route_support::{Query, blocking_store, store_error};
use crate::{search, sidebar, tree, write_routes};

static ACTIVE_MEMOS: LazyLock<SessionsListMemos> = LazyLock::new(SessionsListMemos::default);
const STATUS_BODY_MEMO_LIMIT: usize = 4;

type StatusBodyCache = Mutex<LruCache<StatusKey, Vec<u8>>>;
type GzipBodyCache = Mutex<LruCache<Vec<u8>, Vec<u8>>>;

fn gzip_body_memo(limit: usize) -> GzipBodyCache {
  Mutex::new(LruCache::new(NonZeroUsize::new(limit).expect("the gzip memo limit is nonzero")))
}

static STATUS_BODY_MEMO: OnceLock<StatusBodyCache> = OnceLock::new();
static STATUS_GZIP_MEMO: LazyLock<GzipBodyCache> = LazyLock::new(|| gzip_body_memo(16));
static SEARCH_GZIP_MEMO: LazyLock<GzipBodyCache> = LazyLock::new(|| gzip_body_memo(2));

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct StatusKey {
  ids: Vec<String>,
  generation: u64,
}

/// The route patterns `routes` mounts, the merged writes, fork and detail routers included. The trailing-slash redirect
/// matches a path against them.
pub(crate) const ROUTE_PATTERNS: &[&str] = &[
  api::SESSIONS_PATH,
  "/api/sessions/{session_id}/backend",
  "/api/sessions/archived",
  "/api/sessions/starred",
  "/api/sessions/groups",
  "/api/sessions/status",
  api::SESSION_TREE_PATH,
  "/api/sessions/search",
  "/api/sessions/groups/rename",
  "/api/sessions/groups/delete",
  "/api/sessions/{session_id}/read",
  "/api/sessions/{session_id}/star",
  "/api/sessions/{session_id}/unstar",
  api::SESSION_GROUP_PATH,
  "/api/sessions/{session_id}/rounds/{round_id}/rate",
  "/api/sessions/{session_id}/permanent",
  "/api/sessions/{session_id}/fork",
  "/api/sessions/{session_id}/elone",
  "/api/sessions/{session_id}/pending-triggers",
  api::SESSION_BOOTSTRAP_PATH,
  "/api/sessions/{session_id}/usage",
  api::SESSION_EVENTS_PATH,
  "/api/sessions/{session_id}/transcript",
  api::SESSION_EVENTS_JSONL_PATH,
  api::SESSION_RUNS_PATH,
  "/api/sessions/{session_id}/task-inputs/pending",
  api::SESSION_PATH,
];

/// Register every supported listing route below `/api/sessions`.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(api::SESSIONS_PATH, refuse_other_methods(get(list_active).post(write_routes::create_session), "GET"))
    .route("/api/sessions/{session_id}/backend", post_only(write_routes::switch_backend))
    .route("/api/sessions/archived", get_only(list_archived))
    .route("/api/sessions/starred", get_only(list_starred))
    .route("/api/sessions/groups", get_only(list_groups))
    .route("/api/sessions/status", get_only(session_status))
    .route(api::SESSION_TREE_PATH, get_only(session_tree))
    .route("/api/sessions/search", get_only(session_search))
    .merge(crate::writes::routes())
    .merge(crate::fork_routes::routes())
    .merge(crate::detail::routes())
    .route(
      api::SESSION_PATH,
      refuse_other_methods(get(crate::detail::session_detail).patch(write_routes::patch_session), "GET"),
    )
    .with_state(Arc::clone(features))
}

async fn list_active(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let response = blocking_store(move || {
    let listing::ListingCorpus { mut rows, derived, sequence_fields } =
      listing::active_listing_corpus(&features.store, &features.registry)?;
    let roots = features.store.subtree_roots_shared()?;
    let sequence = roots.sequence;
    let views = roots.views;
    rows.retain(|row| !sequence.contains_key(&row.id) && !views.values().any(|members| members.contains_key(&row.id)));
    Ok(listing::sessions_list_response(&request, &features.store, &rows, &derived, &sequence_fields, &ACTIVE_MEMOS))
  })
  .await;
  response.unwrap_or_else(store_error)
}

async fn list_archived(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let query = Query::new(&request);
  let group = query.get("group").map(str::to_string);
  let limit = match query.parse_int("limit", Some(100), None, None) {
    Ok(limit) => limit.clamp(1, 500) as usize,
    Err(response) => return *response,
  };
  let before = query.get("before").map(str::to_string);
  let before_id = query.get("before_id").map(str::to_string);
  match archived_page(features, group, limit, before, before_id).await {
    Ok(response) => response,
    Err(error) => store_error(error),
  }
}

async fn archived_page(
  features: Arc<FeatureContext>,
  group: Option<String>,
  limit: usize,
  before: Option<String>,
  before_id: Option<String>,
) -> Result<Response, charliebot_store::StoreError> {
  let store = Arc::clone(&features.store);
  let page =
    blocking_store(move || store.list_archived_page(group.as_deref(), limit, before.as_deref(), before_id.as_deref()))
      .await?;
  let rows = page.sessions;
  let context = features.store.archived_context_rows(&rows).await?;
  let tail = ArchivedPageTail {
    has_more: page.has_more,
    next_before: page.next_before,
    next_before_id: page.next_before_id,
    groups: page.groups,
  };
  blocking_store(move || archived_page_body(&features, rows, context, tail)).await
}

/// The cursor and group fields of an archived page, which its body copies unchanged.
struct ArchivedPageTail {
  has_more: bool,
  next_before: Option<String>,
  next_before_id: Option<String>,
  groups: Vec<(Option<String>, usize)>,
}

fn archived_page_body(
  features: &FeatureContext,
  mut rows: Vec<SessionMetadata>,
  context: Vec<SessionMetadata>,
  tail: ArchivedPageTail,
) -> Result<Response, charliebot_store::StoreError> {
  let page_ids: std::collections::HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
  let mut merged: Vec<(bool, SessionMetadata)> =
    rows.drain(..).map(|row| (false, row)).chain(context.into_iter().map(|row| (true, row))).collect();
  merged.sort_by(|left, right| right.1.updated_at.cmp(&left.1.updated_at).then_with(|| right.1.id.cmp(&left.1.id)));
  let ids: Vec<String> = merged.iter().map(|(_, row)| row.id.clone()).collect();
  let listing_fields = features.registry.sequence_listing_fields(&ids, UtcDateTime::now());
  let page_rows: Vec<Arc<SessionMetadata>> = merged
    .iter()
    .filter(|(context_only, row)| !context_only && page_ids.contains(&row.id))
    .map(|(_, row)| Arc::new(row.clone()))
    .collect();
  let status =
    sidebar::resolve_sidebar_state(&features.store, &features.registry, &page_rows, true, true, false, false)?;
  let mut session_values = Vec::with_capacity(merged.len());
  for (context_only, row) in merged {
    let mut value = listing::metadata_value(&features.store, &row)?;
    let object = value.as_object_mut().expect("session metadata serializes to an object");
    object.insert(
      "thinking_since".to_string(),
      features.store.busy_since(&row.id).map_or(Value::Null, |value| json!(value.to_python_string())),
    );
    object.insert(
      "run_backend".to_string(),
      listing::run_backend(&features.store, &row)?.map_or(Value::Null, Value::String),
    );
    if !context_only {
      apply_derived_fields(&mut value, &row, status.get(&row.id), &features.store, true, true, false);
    }
    listing::apply_listing_fields(&mut value, listing_fields.get(&row.id).cloned().unwrap_or_default());
    if context_only {
      value
        .as_object_mut()
        .expect("metadata serializes to an object")
        .insert("context_only".to_string(), Value::Bool(true));
    }
    session_values.push(value);
  }
  let groups =
    tail.groups.into_iter().map(|(group, total)| json!({"group": group, "total": total})).collect::<Vec<Value>>();
  let body = json!({
    "sessions": session_values,
    "has_more": tail.has_more,
    "next_before": tail.next_before,
    "next_before_id": tail.next_before_id,
    "groups": groups,
  });
  Ok(responses::fast_json(StatusCode::OK, &body))
}

async fn list_starred(State(features): State<Arc<FeatureContext>>) -> Response {
  blocking_store(move || Ok(starred_response(&features))).await.unwrap_or_else(store_error)
}

fn starred_response(features: &FeatureContext) -> Response {
  let listed = match features.store.list_sessions(None, Some(true), None) {
    Ok(listed) => listed,
    Err(error) => return store_error(error),
  };
  let derived =
    match sidebar::resolve_sidebar_state(&features.store, &features.registry, &listed.rows, true, true, false, false) {
      Ok(derived) => derived,
      Err(error) => return store_error(error),
    };
  let rows = match listing::listed_values(
    &features.store,
    &listed.rows,
    &derived,
    &listed.sequence_fields,
    listing::SidebarProjection {
      include_running: true,
      include_trigger: true,
      include_plan: false,
      display_backend: false,
      include_run_backend: true,
    },
  ) {
    Ok(rows) => rows,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &Value::Array(rows))
}

async fn list_groups(State(features): State<Arc<FeatureContext>>) -> Response {
  match features.store.list_group_names() {
    Ok(groups) => responses::fast_json(StatusCode::OK, &json!(groups)),
    Err(error) => store_error(error),
  }
}

async fn session_status(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  blocking_store(move || Ok(session_status_response(&features, &request))).await.unwrap_or_else(store_error)
}

fn session_status_response(features: &FeatureContext, request: &Request<Body>) -> Response {
  let query = Query::new(request);
  let Some(raw_ids) = query.get("ids") else {
    return validation::response(&[validation::missing(validation::Source::Query, "ids")]);
  };
  let ids = parse_session_ids(raw_ids);
  if ids.is_empty() {
    return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": "ids must name at least one session"}));
  }
  let force = match query.parse_bool("force", false) {
    Ok(force) => force,
    Err(response) => return *response,
  };
  let generation = sidebar_state::derived_generation();
  let key = StatusKey { ids: ids.clone(), generation };
  if !force && let Some(body) = get_status_body(&key) {
    let full_poll = sidebar_state::register_poll(false);
    let _sessions = if full_poll {
      match fresh_sessions(features, &ids) {
        Ok(sessions) => sessions,
        Err(error) => return store_error(error),
      }
    } else {
      Vec::new()
    };
    return gzip_response(request, body, &STATUS_GZIP_MEMO);
  }
  let sessions = match fresh_sessions(features, &ids) {
    Ok(sessions) => sessions,
    Err(error) => return store_error(error),
  };
  if sessions.is_empty() {
    return gzip_response(request, b"{}".to_vec(), &STATUS_GZIP_MEMO);
  }
  let derived =
    match sidebar::resolve_sidebar_state(&features.store, &features.registry, &sessions, true, true, true, force) {
      Ok(derived) => derived,
      Err(error) => return store_error(error),
    };
  let mut result = Map::new();
  for meta in &sessions {
    let entry = derived.get(&meta.id);
    let mut payload = Map::new();
    payload.insert("has_unread".to_string(), Value::Bool(meta.has_unread));
    payload.insert(
      "has_running_tasks".to_string(),
      entry.and_then(|row| row.get("has_running_tasks")).cloned().unwrap_or(Value::Bool(false)),
    );
    payload.insert(
      "thinking_since".to_string(),
      features.store.busy_since(&meta.id).map_or(Value::Null, |value| json!(value.to_python_isoformat())),
    );
    payload.insert(
      "has_pending_trigger".to_string(),
      entry.and_then(|row| row.get("has_pending_trigger")).cloned().unwrap_or(Value::Bool(false)),
    );
    payload.insert(
      "pending_trigger_count".to_string(),
      entry.and_then(|row| row.get("pending_trigger_count")).cloned().unwrap_or(Value::from(0)),
    );
    let next = entry.and_then(|row| row.get("next_trigger_at")).cloned().unwrap_or(Value::Null);
    payload.insert(
      "next_trigger_at".to_string(),
      next.as_str().map_or(next.clone(), |value| {
        UtcDateTime::parse(value).map_or(next.clone(), |at| json!(at.to_python_isoformat()))
      }),
    );
    payload.insert(
      "has_pending_plan_approval".to_string(),
      entry.and_then(|row| row.get("has_pending_plan_approval")).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = entry.and_then(|row| row.get("work_state")) {
      payload.insert("work_state".to_string(), work_state.clone());
    }
    result.insert(meta.id.clone(), Value::Object(payload));
  }
  let body = charliebot_compat::json::fast_json_bytes(&Value::Object(result));
  store_status_body(key, body.clone());
  gzip_response(request, body, &STATUS_GZIP_MEMO)
}

async fn session_tree(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let query = Query::new(&request);
  let parent_id = query.get("parent_id").filter(|value| !value.is_empty());
  let include_archived = match query.parse_bool("include_archived", false) {
    Ok(value) => value,
    Err(response) => return *response,
  };
  let limit = match query.parse_int("limit", Some(100), Some(1), Some(500)) {
    Ok(value) => value as usize,
    Err(response) => return *response,
  };
  let parent_id = parent_id.map(str::to_string);
  let cursor = query.get("cursor").map(str::to_string);
  let page = blocking_store(move || {
    tree::tree_page(&features.store, parent_id.as_deref(), include_archived, limit, cursor.as_deref())
  })
  .await;
  match page {
    Ok(value) => responses::fast_json(StatusCode::OK, &value),
    Err(error) => task_http_error(error),
  }
}

async fn session_search(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let query = Query::new(&request);
  let q = query.get("q").unwrap_or_default().trim().to_string();
  let store = Arc::clone(&features.store);
  let registry = Arc::clone(&features.registry);
  let (rows, derived) = match blocking_store(move || {
    let rows = search::search_sessions(&store, &q)?;
    let derived = sidebar::resolve_sidebar_state(&store, &registry, &rows, true, true, false, false)?;
    Ok((rows, derived))
  })
  .await
  {
    Ok(result) => result,
    Err(error) => return store_error(error),
  };
  let mut values = Vec::with_capacity(rows.len());
  for row in &rows {
    let mut value = match listing::metadata_value(&features.store, row) {
      Ok(value) => value,
      Err(error) => return store_error(error),
    };
    apply_derived_fields(&mut value, row, derived.get(&row.id), &features.store, true, true, false);
    values.push(value);
  }
  let body = charliebot_compat::json::fast_json_bytes(&Value::Array(values));
  gzip_response(&request, body, &SEARCH_GZIP_MEMO)
}

fn apply_derived_fields(
  value: &mut Value,
  meta: &SessionMetadata,
  derived: Option<&Map<String, Value>>,
  store: &charliebot_store::Store,
  include_running: bool,
  include_trigger: bool,
  include_plan: bool,
) {
  let object = value.as_object_mut().expect("session metadata serializes to an object");
  if include_running {
    object.insert(
      "has_running_tasks".to_string(),
      derived.and_then(|row| row.get("has_running_tasks")).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = derived.and_then(|row| row.get("work_state")) {
      object.insert("work_state".to_string(), work_state.clone());
    }
  }
  if include_trigger {
    object.insert(
      "has_pending_trigger".to_string(),
      derived.and_then(|row| row.get("has_pending_trigger")).cloned().unwrap_or(Value::Bool(false)),
    );
    object.insert(
      "pending_trigger_count".to_string(),
      derived.and_then(|row| row.get("pending_trigger_count")).cloned().unwrap_or(Value::from(0)),
    );
    object.insert(
      "next_trigger_at".to_string(),
      derived.and_then(|row| row.get("next_trigger_at")).cloned().unwrap_or(Value::Null),
    );
  }
  if include_plan {
    object.insert(
      "has_pending_plan_approval".to_string(),
      derived.and_then(|row| row.get("has_pending_plan_approval")).cloned().unwrap_or(Value::Bool(false)),
    );
  }
  object.insert(
    "thinking_since".to_string(),
    store.busy_since(&meta.id).map_or(Value::Null, |value| json!(value.to_python_string())),
  );
}

fn fresh_sessions(
  features: &FeatureContext,
  ids: &[String],
) -> Result<Vec<Arc<SessionMetadata>>, charliebot_store::StoreError> {
  let mut rows = Vec::new();
  for id in ids {
    if let Some(meta) = features.store.get_session_cached_or_read_sync(id)? {
      rows.push(Arc::new(meta));
    }
  }
  Ok(rows)
}

fn parse_session_ids(ids: &str) -> Vec<String> {
  let mut seen = std::collections::HashSet::new();
  ids
    .split(',')
    .map(str::trim)
    .filter(|id| !id.is_empty())
    .filter(|id| seen.insert((*id).to_string()))
    .map(str::to_string)
    .collect()
}

fn get_status_body(key: &StatusKey) -> Option<Vec<u8>> {
  let memo = STATUS_BODY_MEMO.get_or_init(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(STATUS_BODY_MEMO_LIMIT).expect("the status memo limit is nonzero")))
  });
  let mut memo = memo.lock().expect("status body memo mutex is not poisoned");
  memo.get(key).cloned()
}

fn store_status_body(key: StatusKey, body: Vec<u8>) {
  let memo = STATUS_BODY_MEMO.get_or_init(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(STATUS_BODY_MEMO_LIMIT).expect("the status memo limit is nonzero")))
  });
  let mut memo = memo.lock().expect("status body memo mutex is not poisoned");
  memo.put(key, body);
}

fn gzip_response(request: &Request<Body>, body: Vec<u8>, memo: &'static GzipBodyCache) -> Response {
  let wants_gzip = request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"));
  if !wants_gzip {
    return json_bytes_response(body, None);
  }
  let compressed = {
    let mut cache = memo.lock().expect("gzip body memo mutex is not poisoned");
    match cache.get(&body).cloned() {
      Some(compressed) => compressed,
      None => {
        let compressed = charliebot_compat::compression::gzip_level_one(&body);
        cache.put(body, compressed.clone());
        compressed
      }
    }
  };
  json_bytes_response(compressed, Some("gzip"))
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
  builder.body(Body::from(body)).expect("a JSON response builds")
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::to_bytes;
  use axum::http::StatusCode;
  use base64::Engine;
  use base64::engine::general_purpose::URL_SAFE_NO_PAD;
  use charliebot_registry::route_claims::RouteClaims;
  use charliebot_registry::{Registry, store_hooks};
  use charliebot_store::Store;
  use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile};

  use super::*;

  #[test]
  fn route_claims_follow_the_mounted_patterns_and_refuse_an_unrouted_sub_path() {
    // Starlette redirects a slash-toggled path only when a route matches it, so an unrouted sub-path answers 404.
    let claims = RouteClaims::new(ROUTE_PATTERNS).expect("the session route patterns are distinct");
    for path in ["/api/sessions/", "/api/sessions/groups/rename", "/api/sessions/s1", "/api/sessions/s1/read"] {
      assert!(claims.claims(path), "{path} should be claimed");
    }
    for path in ["/api/sessions/s1/resource-class", "/api/sessions/s1/unknown"] {
      assert!(!claims.claims(path), "{path} should not be claimed");
    }
  }

  #[test]
  fn session_ids_trim_drop_empty_and_keep_first_occurrence() {
    assert_eq!(parse_session_ids(" alpha, ,beta,alpha, beta "), ["alpha", "beta"]);
  }

  #[tokio::test]
  async fn active_list_rebuilds_one_metadata_corpus_per_call() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "listing-first", None, TaskProfile::Manager).await;
    let (_, before_generation) =
      features.store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active)).unwrap();
    crate::test_support::create_node(&features.store, "listing-added", None, TaskProfile::Manager).await;

    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = list_active(State(Arc::clone(&features)), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, after_generation) =
      features.store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active)).unwrap();

    assert_eq!(after_generation, before_generation + 1);
  }

  #[tokio::test]
  async fn stale_tree_cursor_uses_the_shared_task_conflict_shape() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(Registry::new());
    let config = registry.load_config(home.path()).unwrap();
    let store = Store::new(config, registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap();
    let meta = SessionMetadata::new("synthetic-tree-session", "Synthetic tree", TaskProfile::Manager);
    let metadata_path = store.metadata_path(&meta.id);
    std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    std::fs::write(&metadata_path, meta.to_json_string(&[], true)).unwrap();
    let cursor =
      URL_SAFE_NO_PAD.encode(br#"{"r":"stale-revision","a":"2026-10-18T10:00:00Z","i":"synthetic-tree-session"}"#);
    let error = tree::tree_page(&store, None, false, 1, Some(&cursor)).unwrap_err();
    let detail = "task tree changed during pagination; refresh and re-paginate";
    assert_eq!(error.to_string(), detail);
    let response = task_http_error(error);
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
      serde_json::from_slice::<Value>(&body).unwrap(),
      json!({"detail": {"message": detail, "blockers": [detail]}})
    );
  }
}
