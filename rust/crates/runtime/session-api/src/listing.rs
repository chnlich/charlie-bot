//! Session listing projections shared by sidebar routes.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_registry::Registry;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile};
use charliebot_store::{SharedSubtreeRoots, Store, StoreResult};
use http::HeaderValue;
use lru::LruCache;
use serde_json::{Map, Value, json};

use crate::sidebar;
pub use crate::sidebar::DerivedSidebarMap;

const SCHEDULE_FIELDS: [&str; 6] = [
  "schedule_cron",
  "schedule_timezone",
  "schedule_enabled",
  "schedule_next_run",
  "schedule_project",
  "schedule_allow_failure",
];
const GZIP_MEMO_LIMIT: usize = 4;

#[derive(Clone, Copy)]
pub(crate) struct SidebarProjection {
  pub(crate) include_running: bool,
  pub(crate) include_trigger: bool,
  pub(crate) include_plan: bool,
  pub(crate) display_backend: bool,
  pub(crate) include_run_backend: bool,
}

struct SessionsListMemoState {
  whole_body: Option<(Vec<RowInputs>, Vec<u8>)>,
  gzip: LruCache<Vec<u8>, Vec<u8>>,
  #[cfg(test)]
  render_calls: usize,
}

impl Default for SessionsListMemoState {
  fn default() -> Self {
    Self {
      whole_body: None,
      gzip: LruCache::new(NonZeroUsize::new(GZIP_MEMO_LIMIT).expect("the gzip memo limit is nonzero")),
      #[cfg(test)]
      render_calls: 0,
    }
  }
}

/// What one sidebar row renders from: its metadata, its derived overlay, its busy mark, its run and display backends,
/// and the controller's listing fields. Rows with equal inputs render equal values.
#[derive(PartialEq)]
pub(crate) struct RowInputs {
  meta: Arc<SessionMetadata>,
  derived: Option<Map<String, Value>>,
  thinking_since: Value,
  run_backend: Option<String>,
  display_backend: Option<String>,
  listing: Option<Map<String, Value>>,
}

/// The render and gzip memos owned by one sidebar listing route.
#[derive(Default)]
pub struct SessionsListMemos {
  state: Mutex<SessionsListMemoState>,
}

/// The active corpus a sidebar listing renders from: its rows, their derived overlays, and the controllers' row fields.
pub struct ListingCorpus {
  /// Active, unscheduled sessions, newest first.
  pub rows: Vec<Arc<SessionMetadata>>,
  /// The derived overlay of each row, keyed by session id.
  pub derived: DerivedSidebarMap,
  /// The controllers' listing fields of each row, keyed by session id.
  pub sequence_fields: HashMap<String, Map<String, Value>>,
}

/// Fetch active, non-scheduled sessions, their sidebar overlays and their controller row fields.
pub fn active_listing_corpus(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<ListingCorpus> {
  let listed = store.list_sessions(Some(SessionStatus::Active), None, Some(false))?;
  let derived = sidebar::resolve_sidebar_state(store, registry, &listed.rows, true, true, true, false)?;
  Ok(ListingCorpus { rows: listed.rows, derived, sequence_fields: listed.sequence_fields })
}

/// The chat page's sidebar corpus: active, unscheduled sessions outside every owned subtree, with their overlays.
pub struct PageRows {
  rows: Vec<Arc<SessionMetadata>>,
  derived: DerivedSidebarMap,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

/// Read the chat page's sidebar corpus without rendering a row.
pub fn active_page_rows(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageRows> {
  let ListingCorpus { mut rows, derived, sequence_fields } = active_listing_corpus(store, registry)?;
  let roots = store.subtree_roots_shared()?;
  rows.retain(|row| outside_owned_subtrees(row, &roots));
  Ok(PageRows { rows, derived, sequence_fields })
}

/// The id of the row that a page opened without a session redirects to.
///
/// The redirect reads only the row order, so it skips the derived overlays and the controller row fields that the
/// rendered page needs.
pub fn first_page_row_id(store: &Arc<Store>) -> StoreResult<Option<String>> {
  let rows = store.list_session_rows(Some(SessionStatus::Active), None, Some(false))?;
  let roots = store.subtree_roots_shared()?;
  Ok(rows.iter().find(|row| outside_owned_subtrees(row, &roots)).map(|row| row.id.clone()))
}

/// Whether a row stands outside every sequence subtree and every view subtree.
fn outside_owned_subtrees(row: &SessionMetadata, roots: &SharedSubtreeRoots) -> bool {
  !roots.sequence.contains_key(&row.id) && !roots.views.values().any(|members| members.contains_key(&row.id))
}

/// The chat page's row projection: running and trigger state, the run backend, and no plan badge.
const PAGE_PROJECTION: SidebarProjection = SidebarProjection {
  include_running: true,
  include_trigger: true,
  include_plan: false,
  display_backend: true,
  include_run_backend: true,
};

/// The page's whole sidebar text and the inputs it rendered from.
struct WholePageText {
  inputs: Vec<RowInputs>,
  clear_unread: Option<String>,
  text: Arc<String>,
}

/// The chat page's rendered sidebar texts.
///
/// Every page request renders the same rows from the same inputs, so the memo serves the whole sidebar text until one
/// of the inputs [`render_row_text`] reads moves. A row whose inputs did not move keeps its text across a rebuild, so
/// one changed row costs one row render rather than the whole sidebar. The memo holds the page's whole sidebar text,
/// about as much memory as one response body.
#[derive(Default)]
pub struct PageRowsMemo {
  state: Mutex<PageRowsMemoState>,
  #[cfg(test)]
  renders: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct PageRowsMemoState {
  rows: HashMap<String, String>,
  whole: Option<WholePageText>,
}

impl PageRowsMemo {
  #[cfg(test)]
  fn render_calls(&self) -> usize {
    self.renders.load(std::sync::atomic::Ordering::Relaxed)
  }
}

/// Render the chat page's sidebar rows as the Jinja `tojson` text of their array.
///
/// `clear_unread` names the active session: its row renders with `has_unread` false and is left out of the memo, so a
/// later request that names another active session does not serve the marked row.
pub fn active_page_json_text(
  store: &Store,
  page: &PageRows,
  clear_unread: Option<&str>,
  memo: &PageRowsMemo,
) -> StoreResult<Arc<String>> {
  let inputs = row_inputs(store, &page.rows, &page.derived, &page.sequence_fields, PAGE_PROJECTION)?;
  let mut state = memo.state.lock().expect("the page row memo mutex is not poisoned");
  if let Some(whole) = &state.whole
    && whole.clear_unread.as_deref() == clear_unread
    && whole.inputs == inputs
  {
    return Ok(Arc::clone(&whole.text));
  }
  let previous: HashMap<String, RowInputs> = state
    .whole
    .take()
    .map_or_else(HashMap::new, |whole| whole.inputs.into_iter().map(|row| (row.meta.id.clone(), row)).collect());
  let current: HashSet<&str> = inputs.iter().map(|row| row.meta.id.as_str()).collect();
  state.rows.retain(|session_id, _| current.contains(session_id.as_str()));
  let cached_bytes: usize = state.rows.values().map(String::len).sum();
  let mut text = String::with_capacity(2 + cached_bytes + 256 * inputs.len());
  text.push('[');
  for (index, row) in inputs.iter().enumerate() {
    if index > 0 {
      text.push_str(", ");
    }
    let clear = clear_unread == Some(row.meta.id.as_str());
    let inputs_moved = previous.get(row.meta.id.as_str()) != Some(row);
    if !clear
      && !inputs_moved
      && let Some(row_text) = state.rows.get(row.meta.id.as_str())
    {
      text.push_str(row_text);
      continue;
    }
    #[cfg(test)]
    memo.renders.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let row_text = render_row_text(store, row, clear)?;
    text.push_str(&row_text);
    if clear {
      // The active row renders without its unread mark, so its text cannot answer an ordinary request. A row whose
      // inputs moved drops its older text here, which keeps that text from serving the next request with them.
      if inputs_moved {
        state.rows.remove(row.meta.id.as_str());
      }
    } else {
      state.rows.insert(row.meta.id.clone(), row_text);
    }
  }
  text.push(']');
  let text = Arc::new(text);
  state.whole = Some(WholePageText { inputs, clear_unread: clear_unread.map(str::to_string), text: Arc::clone(&text) });
  Ok(text)
}

/// One row as the page's JSON text, with its `has_unread` cleared for the active session.
///
/// Unlike [`render_row`], the page's rows keep `task.goal` (see `docs/contract/shapes.md`, "Listing rows").
fn render_row_text(store: &Store, row: &RowInputs, clear_unread: bool) -> StoreResult<String> {
  let mut value = store.session_metadata_value(&row.meta);
  apply_sidebar_fields(&mut value, row, PAGE_PROJECTION);
  apply_listing_fields(&mut value, row.listing.clone().expect("every sidebar row has sequence listing fields"));
  if clear_unread {
    value
      .as_object_mut()
      .expect("a session metadata value is an object")
      .insert("has_unread".to_string(), Value::Bool(false));
  }
  Ok(charliebot_compat::jinja::tojson_text(&value))
}

/// Render sidebar rows with their controller fields and route-local body and gzip memos.
pub fn sessions_list_response(
  request: &Request<Body>,
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  memos: &SessionsListMemos,
) -> Response {
  let projection = SidebarProjection {
    include_running: true,
    include_trigger: true,
    include_plan: true,
    display_backend: true,
    include_run_backend: false,
  };
  let inputs = match row_inputs(store, rows, derived, sequence_fields, projection) {
    Ok(inputs) => inputs,
    Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
  };
  let body = {
    let mut state = memos.state.lock().expect("sessions list memo mutex is not poisoned");
    // As `_sessions_list_response`: an unchanged corpus with unchanged overlays serves the rendered body again and
    // renders no row.
    let body = match &state.whole_body {
      Some((cached, body)) if *cached == inputs => body.clone(),
      _ => {
        #[cfg(test)]
        {
          state.render_calls += inputs.len();
        }
        let values = match inputs.iter().map(|row| render_row(store, row, projection)).collect::<StoreResult<Vec<_>>>()
        {
          Ok(values) => values,
          Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
        };
        let body = charliebot_compat::json::fast_json_bytes(&Value::Array(values));
        state.whole_body = Some((inputs, body.clone()));
        body
      }
    };
    let wants_gzip = request
      .headers()
      .get(header::ACCEPT_ENCODING)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| value.contains("gzip"));
    if wants_gzip {
      if let Some(compressed) = state.gzip.get(&body).cloned() {
        return json_bytes_response(compressed, true);
      }
      let compressed = charliebot_compat::compression::gzip_level_one(&body);
      state.gzip.put(body, compressed.clone());
      return json_bytes_response(compressed, true);
    }
    body
  };
  json_bytes_response(body, false)
}

/// Render metadata as the Python response model does, excluding `task.goal`.
pub(crate) fn metadata_value(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  let mut value = store.session_metadata_value(meta);
  if let Some(task) = value.get_mut("task").and_then(Value::as_object_mut) {
    task.remove("goal");
  }
  Ok(value)
}

/// Remove schedule model fields and apply the controller's row fields.
pub fn apply_listing_fields(dump: &mut Value, fields: Map<String, Value>) {
  let object = dump.as_object_mut().expect("session metadata serializes to an object");
  for field in SCHEDULE_FIELDS {
    object.remove(field);
  }
  object.insert("schedule_task".to_string(), Value::Null);
  object.extend(fields);
}

pub(crate) fn run_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  Ok(store.run_backend(&meta.id))
}

fn display_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  // Matches displayed_backend_id at src/runtime/api/sessions.py:71-74. The serialized run_backend field remains the
  // thinking_state-only value that session_listing.py:93 uses.
  if meta.profile == TaskProfile::Worker {
    return Ok(Some(store.run_backend(&meta.id).unwrap_or_else(|| meta.backend.clone())));
  }
  Ok(Some(meta.backend.clone()))
}

pub(crate) fn listed_values(
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  projection: SidebarProjection,
) -> StoreResult<Vec<Value>> {
  row_inputs(store, rows, derived, sequence_fields, projection)?
    .iter()
    .map(|row| render_row(store, row, projection))
    .collect()
}

/// Read every input that the rows of one listing render from.
fn row_inputs(
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  projection: SidebarProjection,
) -> StoreResult<Vec<RowInputs>> {
  rows
    .iter()
    .map(|row| {
      Ok(RowInputs {
        meta: Arc::clone(row),
        derived: derived.get(&row.id).cloned(),
        thinking_since: store.busy_since(&row.id).map_or(Value::Null, |value| json!(value.to_python_string())),
        run_backend: if projection.include_run_backend { run_backend(store, row)? } else { None },
        display_backend: if projection.display_backend { display_backend(store, row)? } else { None },
        listing: sequence_fields.get(&row.id).cloned(),
      })
    })
    .collect()
}

/// Render one listing row from its inputs.
fn render_row(store: &Store, row: &RowInputs, projection: SidebarProjection) -> StoreResult<Value> {
  let mut value = metadata_value(store, &row.meta)?;
  apply_sidebar_fields(&mut value, row, projection);
  apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
  Ok(value)
}

fn apply_sidebar_fields(value: &mut Value, row: &RowInputs, projection: SidebarProjection) {
  let object = value.as_object_mut().expect("session metadata serializes to an object");
  let derived = row.derived.as_ref();
  if projection.include_running {
    object.insert(
      "has_running_tasks".to_string(),
      derived.and_then(|entry| entry.get("has_running_tasks")).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = derived.and_then(|entry| entry.get("work_state")) {
      object.insert("work_state".to_string(), work_state.clone());
    }
  }
  if projection.include_trigger {
    object.insert(
      "has_pending_trigger".to_string(),
      derived.and_then(|entry| entry.get("has_pending_trigger")).cloned().unwrap_or(Value::Bool(false)),
    );
    object.insert(
      "pending_trigger_count".to_string(),
      derived.and_then(|entry| entry.get("pending_trigger_count")).cloned().unwrap_or(Value::from(0)),
    );
    object.insert(
      "next_trigger_at".to_string(),
      derived.and_then(|entry| entry.get("next_trigger_at")).cloned().unwrap_or(Value::Null),
    );
  }
  if projection.include_plan {
    object.insert(
      "has_pending_plan_approval".to_string(),
      derived.and_then(|entry| entry.get("has_pending_plan_approval")).cloned().unwrap_or(Value::Bool(false)),
    );
  }
  object.insert("thinking_since".to_string(), row.thinking_since.clone());
  if projection.include_run_backend {
    object.insert("run_backend".to_string(), row.run_backend.clone().map_or(Value::Null, Value::String));
  }
  if projection.display_backend
    && row.meta.profile == TaskProfile::Worker
    && let Some(backend) = &row.display_backend
  {
    object.insert("backend".to_string(), Value::String(backend.clone()));
  }
}

fn json_bytes_response(body: Vec<u8>, gzip: bool) -> Response {
  let mut response = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::CONTENT_LENGTH, body.len());
  if gzip {
    response = response
      .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
      .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
  }
  response.body(Body::from(body)).expect("a JSON response builds")
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  async fn list_body(
    features: &charliebot_registry::FeatureContext,
    rows: &[Arc<SessionMetadata>],
    derived: &DerivedSidebarMap,
    sequence_fields: &HashMap<String, Map<String, Value>>,
    memos: &SessionsListMemos,
  ) -> Value {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = sessions_list_response(&request, &features.store, rows, derived, sequence_fields, memos);
    serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
  }

  #[tokio::test]
  async fn the_list_body_renders_again_when_a_row_input_moves() {
    let (_home, features) = crate::test_support::feature_context();
    let node = crate::test_support::create_node(&features.store, "memo-row", None, TaskProfile::Manager).await;
    let mut rows = vec![Arc::new(features.store.get_session(&node.id).await.unwrap().unwrap())];
    let memos = SessionsListMemos::default();
    let mut derived = DerivedSidebarMap::new();
    derived
      .insert(node.id.clone(), json!({"has_running_tasks": false, "work_state": "idle"}).as_object().unwrap().clone());
    let sequence_fields = HashMap::new();

    let first = list_body(&features, &rows, &derived, &sequence_fields, &memos).await;
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await, first);
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(first[0]["work_state"], "idle");

    derived.get_mut(&node.id).unwrap().insert("work_state".to_string(), json!("waiting"));
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["work_state"], "waiting");
    assert_eq!(memos.state.lock().unwrap().render_calls, 2);

    let renamed = (*rows[0]).clone();
    rows[0] = Arc::new(SessionMetadata { name: "renamed".to_string(), ..renamed });
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["name"], "renamed");
    assert_eq!(memos.state.lock().unwrap().render_calls, 3);
  }

  /// The page's text is the Jinja `tojson` text of the row array, and a row keeps that text until an input moves.
  #[tokio::test]
  async fn page_text_matches_the_array_json_and_reuses_unchanged_rows() {
    let (_home, features) = crate::test_support::feature_context();
    let first = crate::test_support::create_node(&features.store, "page-text-a", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "page-text-b", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, &memo).unwrap();
    assert_eq!(text.as_str(), page_array_text(&features.store, &page, None));
    let rendered = memo.render_calls();
    assert_eq!(rendered, page.rows.len());

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    assert_eq!(active_page_json_text(&features.store, &page, None, &memo).unwrap().as_str(), text.as_str());
    assert_eq!(memo.render_calls(), rendered);

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let active = active_page_json_text(&features.store, &page, Some(&first.id), &memo).unwrap();
    assert_eq!(active.as_str(), page_array_text(&features.store, &page, Some(&first.id)));
    assert_eq!(memo.render_calls(), rendered + 1);
  }

  /// The page corpus over given rows, with the controller fields that a listing supplies for each row.
  fn page_rows_over(rows: Vec<Arc<SessionMetadata>>) -> PageRows {
    let sequence_fields = rows.iter().map(|row| (row.id.clone(), Map::new())).collect();
    PageRows { rows, derived: DerivedSidebarMap::new(), sequence_fields }
  }

  /// A row whose inputs move while it is the active session does not serve its older text to a later request.
  #[tokio::test]
  async fn a_moved_active_row_does_not_serve_its_older_text() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "moved-active-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = page_rows_over(vec![Arc::new(meta.clone())]);
    active_page_json_text(&features.store, &page, None, &memo).unwrap();

    let renamed = SessionMetadata { name: "renamed while active".to_string(), ..meta.clone() };
    let changed = page_rows_over(vec![Arc::new(renamed)]);
    let active_text = active_page_json_text(&features.store, &changed, Some(&meta.id), &memo).unwrap();
    let active: Vec<Value> = serde_json::from_str(active_text.as_str()).unwrap();
    assert_eq!(active[0]["name"], "renamed while active");
    assert_eq!(active[0]["has_unread"], false);

    let ordinary_text = active_page_json_text(&features.store, &changed, None, &memo).unwrap();
    let ordinary: Vec<Value> = serde_json::from_str(ordinary_text.as_str()).unwrap();
    assert_eq!(ordinary[0]["name"], "renamed while active");
  }

  /// A page opened without a session redirects to the row that the rendered page puts first.
  #[tokio::test]
  async fn the_redirect_row_is_the_rendered_page_first_row() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "redirect-first", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "redirect-second", None, TaskProfile::Manager).await;

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let first_row = page.rows.first().map(|row| row.id.as_str());
    assert_eq!(first_page_row_id(&features.store).unwrap().as_deref(), first_row);
  }

  /// The bytes the page must carry: `tojson` of the row values, with the active session's unread mark cleared.
  fn page_array_text(store: &Store, page: &PageRows, clear_unread: Option<&str>) -> String {
    let inputs = row_inputs(store, &page.rows, &page.derived, &page.sequence_fields, PAGE_PROJECTION).unwrap();
    let values: Vec<Value> = inputs
      .iter()
      .map(|row| {
        let mut value = store.session_metadata_value(&row.meta);
        apply_sidebar_fields(&mut value, row, PAGE_PROJECTION);
        apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
        if clear_unread == Some(row.meta.id.as_str()) {
          value.as_object_mut().unwrap().insert("has_unread".to_string(), Value::Bool(false));
        }
        value
      })
      .collect();
    charliebot_compat::jinja::tojson_text(&Value::Array(values))
  }

  #[tokio::test]
  async fn backend_display_falls_back_to_metadata_without_reading_run_records() {
    let (_home, features) = crate::test_support::feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "backend-read-count", None, TaskProfile::Manager).await;
    let record = features.store.run_metadata_path(&meta.id, "synthetic-run");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, b"this metadata must not be read").unwrap();

    let before = features.store.run_record_read_count();
    assert_eq!(run_backend(&features.store, &meta).unwrap(), None);
    assert_eq!(display_backend(&features.store, &meta).unwrap(), Some(meta.backend.clone()));
    assert_eq!(features.store.run_record_read_count() - before, 0);
  }

  #[test]
  fn listing_fields_remove_schedule_values_and_append_controller_fields() {
    let mut value = json!({"schedule_cron": null, "name": "Synthetic", "schedule_enabled": null});
    let mut fields = Map::new();
    fields.insert("schedule_cron".to_string(), json!("0 8 * * *"));
    fields.insert("sequence_state".to_string(), json!("ready"));
    apply_listing_fields(&mut value, fields);
    assert_eq!(
      value,
      json!({
        "name": "Synthetic",
        "schedule_task": null,
        "schedule_cron": "0 8 * * *",
        "sequence_state": "ready"
      })
    );
  }

  #[test]
  fn response_projection_removes_only_task_goal() {
    let (_home, features) = crate::test_support::feature_context();
    let mut meta = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    meta.task = Some(charliebot_store::metadata::TaskSpec {
      task_type: Some(charliebot_store::metadata::TaskType::Implement),
      goal: "private goal".to_string(),
      ..Default::default()
    });
    let value = metadata_value(&features.store, &meta).unwrap();
    assert!(value["task"].get("goal").is_none());
    assert_eq!(value["task"]["task_type"], "implement");
  }
}
