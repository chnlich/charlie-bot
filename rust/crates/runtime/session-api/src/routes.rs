use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use lru::LruCache;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use charliebot_compat::routing::{get_only, post_only, refuse_other_methods};
use charliebot_compat::{responses, validation};
use charliebot_registry::FeatureContext;
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::metadata::{
  HAS_PENDING_TRIGGER, HAS_RUNNING_TASKS, HAS_UNREAD, NEXT_TRIGGER_AT, PENDING_TRIGGER_COUNT, SessionMetadata,
  UtcDateTime, WORK_STATE,
};
use charliebot_store::sidebar_state;
use http::header::HeaderValue;
use serde_json::{Map, Value, json};

use crate::api;
use crate::listing::{self, CompactListingMemo, SessionsListMemos};
use crate::route_support::{Query, blocking_store, store_error};
use crate::{search, sidebar, status_delta, tree, write_routes};

static ACTIVE_MEMOS: LazyLock<SessionsListMemos> = LazyLock::new(SessionsListMemos::default);
static COMPACT_LISTING_MEMO: LazyLock<CompactListingMemo> = LazyLock::new(CompactListingMemo::default);
const STATUS_BODY_MEMO_LIMIT: usize = 4;

type StatusBodyCache = Mutex<LruCache<StatusKey, Arc<StatusBody>>>;
type GzipBodyCache = Mutex<LruCache<Vec<u8>, Vec<u8>>>;

fn gzip_body_memo(limit: usize) -> GzipBodyCache {
  Mutex::new(LruCache::new(NonZeroUsize::new(limit).expect("the gzip memo limit is nonzero")))
}

static STATUS_BODY_MEMO: OnceLock<StatusBodyCache> = OnceLock::new();
static SEARCH_GZIP_MEMO: LazyLock<GzipBodyCache> = LazyLock::new(|| gzip_body_memo(2));
static TREE_GZIP_MEMO: LazyLock<GzipBodyCache> = LazyLock::new(|| gzip_body_memo(4));

/// The body length below which the transport middleware leaves a response uncompressed (`GZipMiddleware`).
const TRANSPORT_GZIP_MINIMUM_SIZE: usize = 1000;

/// The identity of one status body.
///
/// A `filter=all` body answers the Workspace's rows, and the listing's derived overlay names that corpus: it holds one
/// entry per row, and a rebuild replaces it whenever a row, an overlay or the order moves. The key holds that overlay
/// instead of the 7,160 id strings, so a repeat poll neither clones nor sorts nor compares them. A request that names
/// its filter and ids keys on those. `generation` names the sidebar state the body reads beyond the rows, and `since`
/// the answer's own delta base: a page that holds the state of its last answer sends that answer's token back, so the
/// same state answers with the same token.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum StatusKey {
  Listing {
    derived: ListingOverlay,
    extra: Vec<String>,
    generation: u64,
    since: Option<String>,
    answer: StatusAnswer,
  },
  Ids {
    filter: Option<String>,
    members: Vec<String>,
    ids: Vec<String>,
    generation: u64,
    since: Option<String>,
    answer: StatusAnswer,
  },
}

/// Which rows a status body carries.
///
/// `Set` answers every row of the filter's set. `Named` answers the ids the request names, while the filter still
/// names the set that membership follows: a page that paints its first screen asks for the rows it paints rather than
/// every row of the home.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum StatusAnswer {
  Set,
  Named,
}

/// The listing's derived overlay as a key part. It compares and hashes by address, and it holds the overlay so the
/// address stays this overlay's alone while the entry lives.
#[derive(Clone, Debug)]
struct ListingOverlay(Arc<sidebar_state::DerivedSidebarMap>);

impl PartialEq for ListingOverlay {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.0, &other.0)
  }
}

impl Eq for ListingOverlay {}

impl Hash for ListingOverlay {
  fn hash<H: Hasher>(&self, state: &mut H) {
    Arc::as_ptr(&self.0).hash(state);
  }
}

/// One status body: the plain bytes and, on the first request that accepts gzip, their compressed form.
///
/// A repeat request shares both allocations instead of copying the body. The response hands the ETag layer the same
/// `Bytes` every time, so its tag memo answers the repeat without hashing the body again. The memo keeps the body,
/// so its compressed form is paid once per version and takes the smaller level-9 form a kept body uses.
struct StatusBody {
  plain: Bytes,
  gzip: OnceLock<Bytes>,
}

impl StatusBody {
  fn new(plain: Bytes) -> Self {
    Self { plain, gzip: OnceLock::new() }
  }

  /// The bytes a request that names `wants_gzip` sends.
  fn bytes(&self, wants_gzip: bool) -> Bytes {
    if wants_gzip {
      self.gzip.get_or_init(|| Bytes::from(charliebot_compat::compression::gzip_memoized_body(&self.plain))).clone()
    } else {
      self.plain.clone()
    }
  }
}

fn get_status_body(key: &StatusKey) -> Option<Arc<StatusBody>> {
  let memo = STATUS_BODY_MEMO.get_or_init(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(STATUS_BODY_MEMO_LIMIT).expect("the status memo limit is nonzero")))
  });
  let mut memo = memo.lock().expect("status body memo mutex is not poisoned");
  memo.get(key).cloned()
}

fn store_status_body(key: StatusKey, body: Arc<StatusBody>) {
  let memo = STATUS_BODY_MEMO.get_or_init(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(STATUS_BODY_MEMO_LIMIT).expect("the status memo limit is nonzero")))
  });
  let mut memo = memo.lock().expect("status body memo mutex is not poisoned");
  memo.put(key, body);
}

/// The response for one status body, compressed when the request accepts gzip.
fn status_response(request: &Request<Body>, body: &StatusBody) -> Response {
  let wants_gzip = accepts_gzip(request);
  json_bytes_response(body.bytes(wants_gzip), wants_gzip)
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
  // `ids` asks for a row refresh: the named rows that the Workspace lists, and the ids it no longer lists.
  let query = Query::new(&request);
  if let Some(raw_ids) = query.get("ids") {
    let ids = parse_session_ids(raw_ids);
    if ids.is_empty() {
      return responses::json(
        StatusCode::UNPROCESSABLE_ENTITY,
        &json!({"detail": "ids must name at least one session"}),
      );
    }
    let response = blocking_store(move || {
      let page = listing::active_page_rows_for(&features.store, &features.registry, &ids)?;
      let text = listing::row_refresh_text(&features.store, &page, &ids)?;
      Ok(responses::fast_json_rendered(StatusCode::OK, text.into_bytes()))
    })
    .await;
    return response.unwrap_or_else(store_error);
  }
  // `compact=1` asks for the page's column form of the same rows, which the sidebar decodes with the page's own
  // expander. Any other value keeps the row array the route has always sent.
  let compact = query.get("compact") == Some("1");
  let response = blocking_store(move || {
    // The sidebar corpus is already filtered to the rows the Workspace lists, and it is kept until the corpus, the
    // subtree roots or the overlays move.
    let page = listing::active_page_rows(&features.store, &features.registry)?;
    Ok(if compact {
      COMPACT_LISTING_MEMO.response(&request, &features.store, &page)
    } else {
      listing::sessions_list_page_response(&request, &features.store, &page, &ACTIVE_MEMOS)
    })
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
  let ids = charliebot_registry::sequence::listing_session_ids(features.registry.sequence_controllers(), &ids);
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
      features.store.busy_since(&row.id).map_or(Value::Null, |value| json!(value.to_str())),
    );
    object.insert(
      "run_backend".to_string(),
      listing::run_backend(&features.store, &row)?.map_or(Value::Null, Value::String),
    );
    if !context_only {
      apply_derived_fields(&mut value, &row, status.get(&row.id).map(Arc::as_ref), &features.store, true, true, false);
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

async fn list_starred(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  blocking_store(move || Ok(starred_response(&features, &request))).await.unwrap_or_else(store_error)
}

/// The starred view: every starred session, or with `ids`, only the named rows among them, as a row refresh.
fn starred_response(features: &FeatureContext, request: &Request<Body>) -> Response {
  let query = Query::new(request);
  let refresh = query.get("ids").map(parse_session_ids);
  if refresh.as_ref().is_some_and(Vec::is_empty) {
    return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": "ids must name at least one session"}));
  }
  let listed = match features.store.list_sessions(None, Some(true), None) {
    Ok(listed) => listed,
    Err(error) => return store_error(error),
  };
  let derived =
    match sidebar::resolve_sidebar_state(&features.store, &features.registry, &listed.rows, true, true, false, false) {
      Ok(derived) => derived,
      Err(error) => return store_error(error),
    };
  if let Some(ids) = refresh {
    let text = match listing::row_refresh_text_for(
      &features.store,
      &listed.rows,
      &derived,
      &listed.sequence_fields,
      &ids,
      listing::STARRED_PROJECTION,
    ) {
      Ok(text) => text,
      Err(error) => return store_error(error),
    };
    return responses::fast_json_rendered(StatusCode::OK, text.into_bytes());
  }
  let rows = match listing::listed_values(
    &features.store,
    &listed.rows,
    &derived,
    &listed.sequence_fields,
    listing::STARRED_PROJECTION,
  ) {
    Ok(rows) => rows,
    Err(error) => return store_error(error),
  };
  responses::fast_json(StatusCode::OK, &Value::Array(rows))
}

async fn list_groups(State(features): State<Arc<FeatureContext>>) -> Response {
  match blocking_store(move || list_group_names(&features)).await {
    Ok(groups) => responses::fast_json(StatusCode::OK, &json!(groups)),
    Err(error) => store_error(error),
  }
}

/// The sorted, distinct, nonempty group names of every stored session.
///
/// The names come from the memoized metadata corpus, whose rows the listing memo shares as `Arc`s, so a call clones
/// only the names it returns, not the metadata of the sessions that carry no group.
fn list_group_names(features: &FeatureContext) -> charliebot_store::StoreResult<Vec<String>> {
  let (rows, _) = features.store.load_session_metas_snapshot_versioned(None)?;
  let mut groups: Vec<String> =
    rows.iter().filter_map(|meta| meta.group.as_ref().filter(|group| !group.is_empty()).cloned()).collect();
  groups.sort();
  groups.dedup();
  Ok(groups)
}

async fn session_status(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  blocking_store(move || Ok(session_status_response(&features, &request))).await.unwrap_or_else(store_error)
}

/// The sidebar's status poll: the rows of one filter, plus any further sessions the page names, answered as the rows
/// whose state moved since the page's last answer.
///
/// A `since` token names the state the page holds; without one, or with a token this process did not publish, the
/// answer carries every row. `answer=ids` names the rows the request's `ids` carry and leaves the filter to name the
/// set membership follows, so a page that paints its first screen downloads the rows it paints. The answer is
/// `{"token": <state>, "rows": {<id>: <state>}, "gone": [<id>, ...]}`: the token to send back, the rows that moved,
/// and the ids that left the filter's set.
fn session_status_response(features: &FeatureContext, request: &Request<Body>) -> Response {
  let query = Query::new(request);
  let filter = query.get("filter");
  let raw_ids = query.get("ids");
  if filter.is_none() && raw_ids.is_none() {
    return validation::response(&[validation::missing(validation::Source::Query, "ids")]);
  }
  // The filter names the row set the page shows, and the ids add the sessions the list does not paint, such as the
  // open chat. A view the registry does not know is refused rather than answered with an empty set. The filter's own
  // set is kept apart: membership follows it alone, since two pages on one filter name different extra sessions.
  //
  // `all` is the Workspace's page, whose memoized derived overlay names the row corpus; every other filter names its
  // own rows. The key holds that overlay instead of 7,160 ids, so a repeat poll leaves the ids unbuilt.
  let extra = raw_ids.map(parse_session_ids).unwrap_or_default();
  let force = match query.parse_bool("force", false) {
    Ok(force) => force,
    Err(response) => return *response,
  };
  let answer = match query.get("answer") {
    None => StatusAnswer::Set,
    Some("ids") => StatusAnswer::Named,
    Some(other) => {
      return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": format!("unknown answer: {other}")}));
    }
  };
  let since = query.get("since").map(str::to_string);
  // Read the generation before the rows, so a state change during this request leaves the body stamped with a
  // generation no later request serves.
  let generation = sidebar_state::derived_generation();
  let mut page = None;
  let key = match filter {
    Some("all") => match listing::page_corpus_rows(&features.store, &features.registry) {
      Ok(corpus) => {
        let overlay = ListingOverlay(corpus.derived);
        page = Some((corpus.rows, overlay.clone()));
        StatusKey::Listing {
          derived: overlay,
          extra: sorted_unique(extra.clone()),
          generation,
          since: since.clone(),
          answer,
        }
      }
      Err(error) => return store_error(error),
    },
    name => {
      let members = match name {
        None => Vec::new(),
        Some(name) => match listing::status_filter_ids(&features.store, &features.registry, name) {
          Ok(Some(rows)) => rows,
          Ok(None) => {
            return responses::json(
              StatusCode::UNPROCESSABLE_ENTITY,
              &json!({"detail": format!("unknown filter: {name}")}),
            );
          }
          Err(error) => return store_error(error),
        },
      };
      let ids = sorted_unique(members.iter().cloned().chain(extra.iter().cloned()).collect());
      if name.is_none() && ids.is_empty() {
        return responses::json(
          StatusCode::UNPROCESSABLE_ENTITY,
          &json!({"detail": "ids must name at least one session"}),
        );
      }
      StatusKey::Ids {
        filter: name.map(str::to_string),
        members: sorted_unique(members),
        ids,
        generation,
        since: since.clone(),
        answer,
      }
    }
  };
  // A body whose corpus, sidebar state and delta base still hold is the body a rebuild would render. A miss rebuilds
  // every row payload: about 50 ms for 7,160 rows, on a poll an open page sends every few seconds. A repeat request
  // reads the memo instead. The poll still counts toward the periodic self-heal sweep, which moves the generation and
  // so replaces the entry.
  if !force && let Some(body) = get_status_body(&key) {
    let owned;
    let sessions: &[Arc<SessionMetadata>] = if answer == StatusAnswer::Named {
      match fresh_sessions(features, &extra) {
        Ok(sessions) => {
          owned = sessions;
          &owned
        }
        Err(error) => return store_error(error),
      }
    } else {
      match &page {
        Some((rows, _)) => rows.as_slice(),
        None => {
          let ids = match &key {
            StatusKey::Ids { ids, .. } => ids,
            StatusKey::Listing { .. } => unreachable!("a listing poll keeps its page rows"),
          };
          match fresh_sessions(features, ids) {
            Ok(sessions) => {
              owned = sessions;
              &owned
            }
            Err(error) => return store_error(error),
          }
        }
      }
    };
    if let Err(error) = sidebar::count_memoized_read(&features.store, &features.registry, sessions) {
      return store_error(error);
    }
    return status_response(request, &body);
  }
  // A named answer carries the request's rows; the filter's set still rides for membership alone, so a row that
  // leaves the filter is reported gone. Any other answer carries the whole set.
  let (requested, members) = if answer == StatusAnswer::Named {
    let members = match &key {
      StatusKey::Listing { .. } => page
        .as_ref()
        .map(|(rows, _)| sorted_unique(rows.iter().map(|row| row.id.clone()).collect()))
        .expect("a listing poll keeps its page rows"),
      StatusKey::Ids { members, .. } => members.clone(),
    };
    (sorted_unique(extra.clone()), members)
  } else {
    match &page {
      Some((rows, _)) => {
        let members = sorted_unique(rows.iter().map(|row| row.id.clone()).collect());
        let mut requested = members.clone();
        requested.extend(extra.iter().cloned());
        (sorted_unique(requested), members)
      }
      None => match &key {
        StatusKey::Ids { ids, members, .. } => (ids.clone(), members.clone()),
        StatusKey::Listing { .. } => unreachable!("a listing poll keeps its page rows"),
      },
    }
  };
  let sessions = match fresh_sessions(features, &requested) {
    Ok(sessions) => sessions,
    Err(error) => return store_error(error),
  };
  let ids: Vec<String> = sessions.iter().map(|meta| meta.id.clone()).collect();
  // A named answer did not read the filter's other rows, so it cannot drop a member whose read failed.
  let members: Vec<String> = if answer == StatusAnswer::Named {
    members
  } else {
    let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
    members.into_iter().filter(|id| present.contains(id.as_str())).collect()
  };
  let derived =
    match sidebar::resolve_sidebar_state(&features.store, &features.registry, &sessions, true, true, true, force) {
      Ok(derived) => derived,
      Err(error) => return store_error(error),
    };
  let payloads = sessions.iter().map(|meta| status_row_payload(features, meta, derived.get(&meta.id))).collect();
  let delta = status_delta::record(filter, &members, &ids, payloads, since.as_deref());
  let body = Arc::new(StatusBody::new(Bytes::from(charliebot_compat::json::fast_json_bytes(&json!({
    "token": delta.token,
    "rows": Value::Object(delta.rows),
    "gone": delta.gone,
  })))));
  if !force {
    store_status_body(key, Arc::clone(&body));
  }
  status_response(request, &body)
}

/// The ids in sorted order, each once.
fn sorted_unique(mut ids: Vec<String>) -> Vec<String> {
  ids.sort();
  ids.dedup();
  ids
}

/// One row of the status answer: the facts the sidebar paints beside a session's name.
fn status_row_payload(
  features: &FeatureContext,
  meta: &SessionMetadata,
  entry: Option<&Arc<Map<String, Value>>>,
) -> Value {
  let mut payload = Map::new();
  payload.insert(HAS_UNREAD.to_string(), Value::Bool(meta.has_unread));
  payload.insert(
    HAS_RUNNING_TASKS.to_string(),
    entry.and_then(|row| row.get(HAS_RUNNING_TASKS)).cloned().unwrap_or(Value::Bool(false)),
  );
  payload.insert(
    "thinking_since".to_string(),
    features.store.busy_since(&meta.id).map_or(Value::Null, |value| json!(value.to_isoformat_text())),
  );
  payload.insert(
    HAS_PENDING_TRIGGER.to_string(),
    entry.and_then(|row| row.get(HAS_PENDING_TRIGGER)).cloned().unwrap_or(Value::Bool(false)),
  );
  payload.insert(
    PENDING_TRIGGER_COUNT.to_string(),
    entry.and_then(|row| row.get(PENDING_TRIGGER_COUNT)).cloned().unwrap_or(Value::from(0)),
  );
  let next = entry.and_then(|row| row.get(NEXT_TRIGGER_AT)).cloned().unwrap_or(Value::Null);
  payload.insert(
    NEXT_TRIGGER_AT.to_string(),
    next
      .as_str()
      .map_or(next.clone(), |value| UtcDateTime::parse(value).map_or(next.clone(), |at| json!(at.to_isoformat_text()))),
  );
  payload.insert(
    sidebar_state::HAS_PENDING_PLAN_APPROVAL.to_string(),
    entry.and_then(|row| row.get(sidebar_state::HAS_PENDING_PLAN_APPROVAL)).cloned().unwrap_or(Value::Bool(false)),
  );
  if let Some(work_state) = entry.and_then(|row| row.get(WORK_STATE)) {
    payload.insert(WORK_STATE.to_string(), work_state.clone());
  }
  Value::Object(payload)
}

async fn session_tree(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let query = Query::new(&request);
  let parent_id = query.get(api::SESSION_TREE_PARENT_ID_QUERY).filter(|value| !value.is_empty());
  let include_archived = match query.parse_bool(api::SESSION_TREE_INCLUDE_ARCHIVED_QUERY, false) {
    Ok(value) => value,
    Err(response) => return *response,
  };
  let limit = match query.parse_int(api::SESSION_TREE_LIMIT_QUERY, Some(100), Some(1), Some(500)) {
    Ok(value) => value as usize,
    Err(response) => return *response,
  };
  let parent_id = parent_id.map(str::to_string);
  let cursor = query.get(api::SESSION_TREE_CURSOR_QUERY).map(str::to_string);
  let page = blocking_store(move || {
    tree::tree_page(&features.store, parent_id.as_deref(), include_archived, limit, cursor.as_deref())
  })
  .await;
  match page {
    Ok(body) => tree_page_response(&request, body),
    Err(error) => task_http_error(error),
  }
}

/// Send a task-tree page, reusing the compressed body a repeat request already deflated.
///
/// A page holds its bytes across requests while the tree and its rows hold, and the transport middleware deflates
/// every body afresh: on the 7,160-session mock home that deflate costs about two thirds of this route's latency.
/// The memo answers a repeat with the bytes it already compressed.
///
/// The middleware leaves a response that names its own `Content-Encoding` untouched, so this sender writes the
/// headers the middleware would write: `Vary: Accept-Encoding` with the compressed body, and the plain response for
/// a body below the middleware's minimum size.
fn tree_page_response(request: &Request<Body>, body: Vec<u8>) -> Response {
  let wants_gzip = request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"));
  if !wants_gzip || body.len() < TRANSPORT_GZIP_MINIMUM_SIZE {
    return responses::fast_json_rendered(StatusCode::OK, body);
  }
  let compressed = {
    let mut cache = TREE_GZIP_MEMO.lock().expect("tree gzip memo mutex is not poisoned");
    match cache.get(&body).cloned() {
      Some(compressed) => compressed,
      None => {
        let compressed = charliebot_compat::compression::gzip_level_one(&body);
        cache.put(body, compressed.clone());
        compressed
      }
    }
  };
  Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::CONTENT_LENGTH, compressed.len())
    .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
    .header(header::VARY, HeaderValue::from_static("Accept-Encoding"))
    .body(Body::from(compressed))
    .expect("a compressed JSON response builds")
}

async fn session_search(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let query = Query::new(&request);
  let q = query.get("q").unwrap_or_default().trim().to_string();
  let store = Arc::clone(&features.store);
  let registry = Arc::clone(&features.registry);
  // Read the generation before the rows, so a state change during this request leaves the body this request keeps
  // stamped with a generation no later request serves.
  let generation = sidebar_state::derived_generation();
  // A repeat query whose listing memo and file check still hold answers on this thread: the corpus and the body are
  // in memory, and the checks that answer it read no file. A fresh query, or one whose check has lapsed, takes the
  // blocking path below, which reads the files the answer depends on.
  if let Some(corpus) = features.store.cached_session_metas_snapshot()
    && let Some((rows, body)) = search::repeat_answer(&q, &corpus, generation)
  {
    // The body resolved its sidebar fields when it was rendered, but the read still counts: the periodic self-heal
    // sweep a memoized answer starts is what catches an edit made outside the server.
    if let Err(error) = sidebar::count_memoized_read(&features.store, &features.registry, &rows) {
      return store_error(error);
    }
    return gzip_response(&request, (*body).clone(), &SEARCH_GZIP_MEMO);
  }
  let (answer, derived) = match blocking_store(move || {
    let answer = search::search_sessions(&store, &q)?;
    let derived = sidebar::resolve_sidebar_state(&store, &registry, &answer.rows, true, true, false, false)?;
    Ok((answer, derived))
  })
  .await
  {
    Ok(result) => result,
    Err(error) => return store_error(error),
  };
  // A repeat query holds the rows and their file signatures; while the derived state still sits at the generation the
  // body was rendered under, that body is the body a rebuild would render.
  if let Some(source) = &answer.source
    && let Some(body) = source.body(generation)
  {
    return gzip_response(&request, (*body).clone(), &SEARCH_GZIP_MEMO);
  }
  let mut values = Vec::with_capacity(answer.rows.len());
  for row in &answer.rows {
    let mut value = match listing::metadata_value(&features.store, row) {
      Ok(value) => value,
      Err(error) => return store_error(error),
    };
    apply_derived_fields(&mut value, row, derived.get(&row.id).map(Arc::as_ref), &features.store, true, true, false);
    values.push(value);
  }
  let body = charliebot_compat::json::fast_json_bytes(&Value::Array(values));
  if let Some(source) = &answer.source {
    source.remember_body(generation, Arc::new(body.clone()));
  }
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
      HAS_RUNNING_TASKS.to_string(),
      derived.and_then(|row| row.get(HAS_RUNNING_TASKS)).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = derived.and_then(|row| row.get(WORK_STATE)) {
      object.insert(WORK_STATE.to_string(), work_state.clone());
    }
  }
  if include_trigger {
    object.insert(
      HAS_PENDING_TRIGGER.to_string(),
      derived.and_then(|row| row.get(HAS_PENDING_TRIGGER)).cloned().unwrap_or(Value::Bool(false)),
    );
    object.insert(
      PENDING_TRIGGER_COUNT.to_string(),
      derived.and_then(|row| row.get(PENDING_TRIGGER_COUNT)).cloned().unwrap_or(Value::from(0)),
    );
    object.insert(
      NEXT_TRIGGER_AT.to_string(),
      derived.and_then(|row| row.get(NEXT_TRIGGER_AT)).cloned().unwrap_or(Value::Null),
    );
  }
  if include_plan {
    object.insert(
      sidebar_state::HAS_PENDING_PLAN_APPROVAL.to_string(),
      derived.and_then(|row| row.get(sidebar_state::HAS_PENDING_PLAN_APPROVAL)).cloned().unwrap_or(Value::Bool(false)),
    );
  }
  object.insert(
    "thinking_since".to_string(),
    store.busy_since(&meta.id).map_or(Value::Null, |value| json!(value.to_str())),
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
  api::parse_session_ids(ids)
}

/// Whether the request accepts a gzip response body.
fn accepts_gzip(request: &Request<Body>) -> bool {
  request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"))
}

fn gzip_response(request: &Request<Body>, body: Vec<u8>, memo: &'static GzipBodyCache) -> Response {
  if !accepts_gzip(request) {
    return json_bytes_response(Bytes::from(body), false);
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
  json_bytes_response(Bytes::from(compressed), true)
}

fn json_bytes_response(body: Bytes, gzip: bool) -> Response {
  let mut builder = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::CONTENT_LENGTH, body.len());
  if gzip {
    builder = builder
      .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
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

  /// The Workspace route keeps its row array by default and answers `compact=1` with the page's column object.
  #[tokio::test]
  async fn the_active_list_answers_compact_requests_with_the_page_columns() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "compact-route", None, TaskProfile::Manager).await;

    let compact = Request::builder().uri("/api/sessions/?compact=1").body(Body::empty()).unwrap();
    let response = list_active(State(Arc::clone(&features)), compact).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert!(payload.get("$encoded").is_some(), "the compact listing declares its column encodings");

    let rows = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = list_active(State(features), rows).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert!(payload.is_array(), "a request without compact keeps the row array");
  }

  /// A row refresh answers the named rows in the compact form, and names each id the Workspace no longer lists.
  #[tokio::test]
  async fn a_row_refresh_answers_the_named_rows_and_the_ids_the_list_no_longer_holds() {
    let (_home, features) = crate::test_support::feature_context();
    let kept = crate::test_support::create_node(&features.store, "refresh-kept", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "refresh-other", None, TaskProfile::Manager).await;
    let request = Request::builder()
      .uri(format!("/api/sessions/?compact=1&ids={},missing-session", kept.id))
      .body(Body::empty())
      .unwrap();

    let (status, payload) = crate::test_support::answer(list_active(State(features), request).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["gone"], json!(["missing-session"]));
    assert_eq!(payload["sessions"]["name"], json!(["refresh-kept"]), "only the named row answers");
  }

  /// `filter=all` names the Workspace's rows and answers the same rows those ids name. An unknown filter is refused.
  #[tokio::test]
  async fn a_status_filter_names_the_workspace_rows_and_answers_as_their_ids_do() {
    let (_home, features) = crate::test_support::feature_context();
    let first = crate::test_support::create_node(&features.store, "status-first", None, TaskProfile::Manager).await;
    let second = crate::test_support::create_node(&features.store, "status-second", None, TaskProfile::Manager).await;
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let (code, by_filter) = status("/api/sessions/status?filter=all".to_string()).await;
    let (_, by_ids) = status(format!("/api/sessions/status?ids={},{}", first.id, second.id)).await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(by_filter["rows"], by_ids["rows"]);
    assert!(by_filter["rows"].get(&first.id).is_some() && by_filter["rows"].get(&second.id).is_some());
    let (code, _) = status("/api/sessions/status?filter=unknown-filter".to_string()).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
  }

  /// `answer=ids` answers the rows the request names while the filter still names the set membership follows, so a
  /// page that paints its first screen does not download every row of the Workspace.
  #[tokio::test]
  async fn a_named_status_answer_carries_the_named_rows_alone() {
    let (_home, features) = crate::test_support::feature_context();
    let painted = crate::test_support::create_node(&features.store, "named-painted", None, TaskProfile::Manager).await;
    let other = crate::test_support::create_node(&features.store, "named-other", None, TaskProfile::Manager).await;
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let (code, named) = status(format!("/api/sessions/status?filter=all&answer=ids&ids={}", painted.id)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
      named["rows"].as_object().expect("rows are an object").keys().collect::<Vec<_>>(),
      vec![&painted.id],
      "the answer carries the named row, not every Workspace row"
    );
    assert!(named["rows"].get(&other.id).is_none(), "the unnamed Workspace row stays out");

    // The filter still names the set: a session that leaves it is reported gone to a page that carries a token, even
    // though the page never named that session.
    let token = named["token"].as_str().expect("a full answer names the state it answers").to_string();
    let (_, filtered) =
      status(format!("/api/sessions/status?filter=all&answer=ids&since={token}&ids={}", painted.id)).await;
    assert_eq!(filtered["gone"], json!([]), "a row still in the filter is not gone");
    assert!(filtered["rows"].as_object().expect("rows are an object").len() <= 1);

    let (code, _) = status("/api/sessions/status?filter=all&answer=unknown&ids=x".to_string()).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
  }

  /// A poll that carries the token of the answer before it names only the row whose state moved, so a task-tree change
  /// costs one row rather than the whole body.
  #[tokio::test]
  async fn a_status_poll_with_a_token_answers_only_the_rows_that_moved() {
    let (_home, features) = crate::test_support::feature_context();
    let moved = crate::test_support::create_node(&features.store, "delta-moved", None, TaskProfile::Manager).await;
    let quiet = crate::test_support::create_node(&features.store, "delta-quiet", None, TaskProfile::Manager).await;
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let (_, full) = status(format!("/api/sessions/status?ids={},{}", moved.id, quiet.id)).await;
    let token = full["token"].as_str().expect("a full answer names the state it answers").to_string();
    assert_eq!(full["rows"].as_object().expect("rows are an object").len(), 2);

    features.store.mark_busy(&moved.id, UtcDateTime::parse("2026-11-06T06:00:00Z").unwrap()).unwrap();
    let (_, delta) = status(format!("/api/sessions/status?ids={},{}&since={token}", moved.id, quiet.id)).await;
    let rows = delta["rows"].as_object().expect("rows are an object");
    assert_eq!(rows.keys().collect::<Vec<_>>(), vec![&moved.id], "only the busy row answers");
    assert_eq!(rows[&moved.id]["thinking_since"], json!("2026-11-06T06:00:00+00:00"));

    let token = delta["token"].as_str().expect("a delta names its state").to_string();
    let (_, repeat) = status(format!("/api/sessions/status?ids={},{}&since={token}", moved.id, quiet.id)).await;
    assert!(repeat["rows"].as_object().expect("rows are an object").is_empty(), "nothing moved since the delta");
  }

  /// A token names a state this process published. Without one, or with one a restart left behind, the answer is the
  /// whole row set.
  #[tokio::test]
  async fn a_status_poll_answers_every_row_without_a_token_from_this_process() {
    let (_home, features) = crate::test_support::feature_context();
    let node = crate::test_support::create_node(&features.store, "delta-token", None, TaskProfile::Manager).await;
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let (_, stale) = status(format!("/api/sessions/status?ids={}&since=1-7", node.id)).await;
    assert_eq!(stale["rows"].as_object().expect("rows are an object").len(), 1);
    let (_, absent) = status(format!("/api/sessions/status?ids={}", node.id)).await;
    assert_eq!(absent["rows"].as_object().expect("rows are an object").len(), 1);
  }

  /// The starred view's row refresh answers the named rows in the compact form and names the ids it no longer holds.
  #[tokio::test]
  async fn a_starred_row_refresh_answers_the_named_rows_and_the_ids_the_view_no_longer_holds() {
    let (_home, features) = crate::test_support::feature_context();
    let kept = crate::test_support::create_node(&features.store, "starred-kept", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "starred-other", None, TaskProfile::Manager).await;
    features.store.star_session(&kept.id).await.unwrap();
    let request = Request::builder()
      .uri(format!("/api/sessions/starred?ids={},missing-session", kept.id))
      .body(Body::empty())
      .unwrap();

    let (status, payload) = crate::test_support::answer(list_starred(State(features), request).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["gone"], json!(["missing-session"]));
    assert_eq!(payload["sessions"]["name"], json!(["starred-kept"]), "only the named row answers");
  }

  /// The status answer's bytes do not follow the order the caller named the ids in: a row that moved in the listing
  /// must not change the body, or the poll answers 200 with no row's state changed.
  #[tokio::test]
  async fn the_status_body_ignores_the_order_the_caller_named_the_ids_in() {
    let (_home, features) = crate::test_support::feature_context();
    let first = crate::test_support::create_node(&features.store, "order-first", None, TaskProfile::Manager).await;
    let second = crate::test_support::create_node(&features.store, "order-second", None, TaskProfile::Manager).await;
    let body = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = session_status(State(features), request).await;
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
      }
    };

    let forward = body(format!("/api/sessions/status?ids={},{}", first.id, second.id)).await;
    let reverse = body(format!("/api/sessions/status?ids={},{}", second.id, first.id)).await;

    assert_eq!(forward, reverse);
  }

  /// A repeat `filter=all` poll that carries the token of its last answer answers the listing as it stands now: the
  /// row whose fact moved and the row that appeared, and not the rows that stand.
  #[tokio::test]
  async fn a_repeat_status_filter_answers_the_listing_as_it_stands_now() {
    let (_home, features) = crate::test_support::feature_context();
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let listed = crate::test_support::create_node(&features.store, "status-listed", None, TaskProfile::Manager).await;
    let (_, first) = status("/api/sessions/status?filter=all".to_string()).await;
    let token = first["token"].as_str().expect("a full answer names the state it answers").to_string();
    assert_eq!(first["rows"][&listed.id][HAS_UNREAD], json!(false));

    features.store.mark_unread(&listed.id).await.expect("the row is marked unread");
    let later = crate::test_support::create_node(&features.store, "status-later", None, TaskProfile::Manager).await;
    let (_, repeat) = status(format!("/api/sessions/status?filter=all&since={token}")).await;

    assert_eq!(repeat["rows"][&listed.id][HAS_UNREAD], json!(true), "the repeat poll answers the row's new state");
    assert!(repeat["rows"].get(&later.id).is_some(), "the repeat poll answers the row that appeared");
    let answered = repeat["rows"].as_object().expect("rows are an object");
    assert_eq!(answered.len(), 2, "the repeat poll answers only the rows that moved");
  }

  /// A repeat poll without a token answers the state as it stands now rather than the body an earlier poll kept: the
  /// memo answers only while the row corpus and the sidebar state it was built from still hold.
  #[tokio::test]
  async fn a_repeat_status_poll_answers_the_state_as_it_stands_now() {
    let (_home, features) = crate::test_support::feature_context();
    let status = |uri: String| {
      let features = Arc::clone(&features);
      async move {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        crate::test_support::answer(session_status(State(features), request).await).await
      }
    };

    let listed = crate::test_support::create_node(&features.store, "status-repeat", None, TaskProfile::Manager).await;
    let (_, first) = status("/api/sessions/status?filter=all".to_string()).await;
    assert_eq!(first["rows"][&listed.id][HAS_UNREAD], json!(false));

    features.store.mark_unread(&listed.id).await.expect("the row is marked unread");
    let (_, repeated) = status("/api/sessions/status?filter=all".to_string()).await;

    assert_eq!(repeated["rows"][&listed.id][HAS_UNREAD], json!(true), "the repeat poll answers the row's new state");
  }

  #[tokio::test]
  async fn groups_route_lists_each_nonempty_name_once_in_order() {
    let (_home, features) = crate::test_support::feature_context();
    for (name, group) in [
      ("group alpha one", Some("Alpha")),
      ("group beta", Some("Beta")),
      ("group alpha two", Some("Alpha")),
      ("ungrouped", None),
    ] {
      let payload = match group {
        Some(group) => json!({"name": name, "group": group}),
        None => json!({"name": name}),
      };
      let (headers, body) = crate::test_support::json_request(payload);
      let created = crate::write_routes::create_session(State(Arc::clone(&features)), headers, body).await;
      assert_eq!(created.status(), StatusCode::OK, "{name} should be created");
    }

    let response = list_groups(State(Arc::clone(&features))).await;

    let (status, groups) = crate::test_support::answer(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(groups, json!(["Alpha", "Beta"]));
  }

  #[tokio::test]
  async fn a_repeat_search_answers_with_its_built_body_until_the_derived_state_moves() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "search-body", None, TaskProfile::Manager).await;
    let events = features.store.chat_events_path(&meta.id);
    std::fs::create_dir_all(events.parent().unwrap()).unwrap();
    std::fs::write(&events, "{\"content\":\"walrus in the events\"}\n").unwrap();
    let search = || Request::builder().uri("/api/sessions/search?q=walrus").body(Body::empty()).unwrap();

    let quiet =
      to_bytes(session_search(State(Arc::clone(&features)), search()).await.into_body(), usize::MAX).await.unwrap();
    let repeat =
      to_bytes(session_search(State(Arc::clone(&features)), search()).await.into_body(), usize::MAX).await.unwrap();
    assert_eq!(quiet, repeat, "a repeat query answers with the body it built");
    let quiet: Value = serde_json::from_slice(&quiet).unwrap();
    assert_eq!(quiet[0]["thinking_since"], Value::Null);

    features.store.mark_busy(&meta.id, UtcDateTime::now()).unwrap();
    let busy =
      to_bytes(session_search(State(Arc::clone(&features)), search()).await.into_body(), usize::MAX).await.unwrap();
    let busy: Value = serde_json::from_slice(&busy).unwrap();
    assert_ne!(busy[0]["thinking_since"], Value::Null, "a moved derived-state generation renders the row again");
  }

  #[tokio::test]
  async fn a_tree_page_answers_gzip_with_the_bytes_the_transport_would_send() {
    let (_home, features) = crate::test_support::feature_context();
    let long_tree_name = format!("tree-gzip-{}", "g".repeat(1200));
    crate::test_support::create_node(&features.store, &long_tree_name, None, TaskProfile::Manager).await;
    let page = |accept_encoding: &str| {
      Request::builder()
        .uri(api::SESSION_TREE_PATH)
        .header(header::ACCEPT_ENCODING, accept_encoding)
        .body(Body::empty())
        .unwrap()
    };
    let answer = |accept_encoding: &'static str| {
      let features = Arc::clone(&features);
      async move { session_tree(State(features), page(accept_encoding)).await }
    };

    let identity = answer("identity").await;
    assert_eq!(identity.headers().get(header::CONTENT_ENCODING), None);
    let identity = to_bytes(identity.into_body(), usize::MAX).await.unwrap();
    assert!(identity.len() >= TRANSPORT_GZIP_MINIMUM_SIZE, "the page is large enough for the transport to deflate");

    let gzip = answer("gzip").await;
    assert_eq!(gzip.headers().get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(gzip.headers().get(header::VARY).unwrap(), "Accept-Encoding");
    let content_length: usize = gzip.headers().get(header::CONTENT_LENGTH).unwrap().to_str().unwrap().parse().unwrap();
    let compressed = to_bytes(gzip.into_body(), usize::MAX).await.unwrap();
    assert_eq!(compressed.len(), content_length);
    assert_eq!(compressed.as_ref(), charliebot_compat::compression::gzip_level_one(&identity));

    let repeat = answer("gzip").await;
    let repeat = to_bytes(repeat.into_body(), usize::MAX).await.unwrap();
    assert_eq!(repeat, compressed, "a repeat page answers with the compressed body the memo already holds");
  }

  #[tokio::test]
  async fn a_tree_page_below_the_transport_minimum_answers_uncompressed() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "tree-small", None, TaskProfile::Manager).await;
    let request = Request::builder()
      .uri(api::SESSION_TREE_PATH)
      .header(header::ACCEPT_ENCODING, "gzip")
      .body(Body::empty())
      .unwrap();

    let response = session_tree(State(features), request).await;

    assert_eq!(response.headers().get(header::CONTENT_ENCODING), None, "the transport leaves a short body alone");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(body.len() < TRANSPORT_GZIP_MINIMUM_SIZE);
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
