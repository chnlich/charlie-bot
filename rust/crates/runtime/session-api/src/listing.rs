//! Session listing projections shared by sidebar routes.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_registry::Registry;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::{Store, StoreResult};
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

/// Fetch active, non-scheduled sessions and their sidebar overlays.
pub fn active_listing_corpus(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
) -> StoreResult<(Vec<Arc<SessionMetadata>>, DerivedSidebarMap)> {
  let listed = store.list_sessions(Some(SessionStatus::Active), None, Some(false))?;
  let derived = sidebar::resolve_sidebar_state(store, registry, &listed.rows, true, true, true, false)?;
  Ok((listed.rows, derived))
}

/// The chat page's sidebar corpus: active, unscheduled sessions outside every owned subtree, with their overlays.
pub struct PageRows {
  rows: Vec<Arc<SessionMetadata>>,
  derived: DerivedSidebarMap,
}

impl PageRows {
  /// The first row's id: the session that a page opened without one redirects to.
  pub fn first_id(&self) -> Option<&str> {
    self.rows.first().map(|row| row.id.as_str())
  }
}

/// Read the chat page's sidebar corpus without rendering a row.
pub fn active_page_rows(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageRows> {
  let (mut rows, derived) = active_listing_corpus(store, registry)?;
  let roots = store.subtree_roots_shared()?;
  rows.retain(|row| {
    !roots.sequence.contains_key(&row.id) && !roots.views.values().any(|members| members.contains_key(&row.id))
  });
  Ok(PageRows { rows, derived })
}

/// Render the chat page's sidebar rows.
pub fn active_page_values(store: &Store, registry: &Registry, page: &PageRows) -> StoreResult<Vec<Value>> {
  let projection = SidebarProjection {
    include_running: true,
    include_trigger: true,
    include_plan: false,
    display_backend: true,
    include_run_backend: true,
  };
  row_inputs(store, registry, &page.rows, &page.derived, projection)?
    .into_iter()
    .map(|row| {
      let mut value = store.session_metadata_value(&row.meta);
      apply_sidebar_fields(&mut value, &row, projection);
      apply_listing_fields(&mut value, row.listing.expect("every sidebar row has sequence listing fields"));
      Ok(value)
    })
    .collect()
}

/// Render sidebar rows with sequence fields and route-local body and gzip memos.
pub fn sessions_list_response(
  request: &Request<Body>,
  store: &Store,
  registry: &Registry,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  memos: &SessionsListMemos,
) -> Response {
  let projection = SidebarProjection {
    include_running: true,
    include_trigger: true,
    include_plan: true,
    display_backend: true,
    include_run_backend: false,
  };
  let inputs = match row_inputs(store, registry, rows, derived, projection) {
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
  registry: &Registry,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  projection: SidebarProjection,
) -> StoreResult<Vec<Value>> {
  row_inputs(store, registry, rows, derived, projection)?.iter().map(|row| render_row(store, row, projection)).collect()
}

/// Read every input that the rows of one listing render from.
fn row_inputs(
  store: &Store,
  registry: &Registry,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  projection: SidebarProjection,
) -> StoreResult<Vec<RowInputs>> {
  let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
  let listing_fields = registry.sequence_listing_fields(&ids, UtcDateTime::now());
  rows
    .iter()
    .map(|row| {
      Ok(RowInputs {
        meta: Arc::clone(row),
        derived: derived.get(&row.id).cloned(),
        thinking_since: store.busy_since(&row.id).map_or(Value::Null, |value| json!(value.to_python_string())),
        run_backend: if projection.include_run_backend { run_backend(store, row)? } else { None },
        display_backend: if projection.display_backend { display_backend(store, row)? } else { None },
        listing: listing_fields.get(&row.id).cloned(),
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
    memos: &SessionsListMemos,
  ) -> Value {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = sessions_list_response(&request, &features.store, &features.registry, rows, derived, memos);
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

    let first = list_body(&features, &rows, &derived, &memos).await;
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(list_body(&features, &rows, &derived, &memos).await, first);
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(first[0]["work_state"], "idle");

    derived.get_mut(&node.id).unwrap().insert("work_state".to_string(), json!("waiting"));
    assert_eq!(list_body(&features, &rows, &derived, &memos).await[0]["work_state"], "waiting");
    assert_eq!(memos.state.lock().unwrap().render_calls, 2);

    let renamed = (*rows[0]).clone();
    rows[0] = Arc::new(SessionMetadata { name: "renamed".to_string(), ..renamed });
    assert_eq!(list_body(&features, &rows, &derived, &memos).await[0]["name"], "renamed");
    assert_eq!(memos.state.lock().unwrap().render_calls, 3);
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
