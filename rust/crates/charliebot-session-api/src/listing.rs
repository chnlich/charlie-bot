//! Session listing projections shared by sidebar routes.

use std::collections::VecDeque;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_registry::Registry;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::{Store, StoreError, StoreResult};
use flate2::Compression;
use flate2::write::GzEncoder;
use http::HeaderValue;
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

#[derive(Default)]
struct SessionsListMemoState {
  whole_body: Option<(Vec<Value>, Vec<u8>)>,
  gzip: VecDeque<(Vec<u8>, Vec<u8>)>,
}

/// The render and gzip memos owned by one sidebar listing route.
#[derive(Default)]
pub struct SessionsListMemos {
  state: Mutex<SessionsListMemoState>,
}

/// Fetch active, non-scheduled sessions and their sidebar overlays.
pub fn active_listing_corpus(
  store: &Store,
  registry: &Registry,
) -> StoreResult<(Vec<SessionMetadata>, DerivedSidebarMap)> {
  let listed = store.list_sessions(Some(SessionStatus::Active), None, Some(false))?;
  let derived = sidebar::resolve_sidebar_state(store, registry, &listed.rows, true, true, true, false)?;
  Ok((listed.rows, derived))
}

/// Render sidebar rows with sequence fields and route-local body and gzip memos.
pub fn sessions_list_response(
  request: &Request<Body>,
  store: &Store,
  registry: &Registry,
  rows: &[SessionMetadata],
  derived: &DerivedSidebarMap,
  memos: &SessionsListMemos,
) -> Response {
  let values = match listed_values(
    store,
    registry,
    rows,
    derived,
    SidebarProjection {
      include_running: true,
      include_trigger: true,
      include_plan: true,
      display_backend: true,
      include_run_backend: false,
    },
  ) {
    Ok(values) => values,
    Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
  };
  let body = {
    let mut state = memos.state.lock().expect("sessions list memo mutex is not poisoned");
    let body = match &state.whole_body {
      Some((cached, body)) if *cached == values => body.clone(),
      _ => {
        let body = charliebot_compat::json::fast_json_bytes(&Value::Array(values.clone()));
        state.whole_body = Some((values, body.clone()));
        body
      }
    };
    let wants_gzip = request
      .headers()
      .get(header::ACCEPT_ENCODING)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| value.contains("gzip"));
    if wants_gzip {
      if let Some(index) = state.gzip.iter().position(|(plain, _)| *plain == body) {
        let entry = state.gzip.remove(index).expect("the gzip memo position names an entry");
        let compressed = entry.1.clone();
        state.gzip.push_back(entry);
        return json_bytes_response(compressed, true);
      }
      let mut encoder = GzEncoder::new(Vec::new(), Compression::new(1));
      use std::io::Write;
      encoder.write_all(&body).expect("writing to a Vec cannot fail");
      let compressed = encoder.finish().expect("finishing into a Vec cannot fail");
      state.gzip.push_back((body, compressed.clone()));
      while state.gzip.len() > GZIP_MEMO_LIMIT {
        state.gzip.pop_front();
      }
      return json_bytes_response(compressed, true);
    }
    body
  };
  json_bytes_response(body, false)
}

/// Render metadata as the Python response model does, excluding `task.goal`.
pub(crate) fn metadata_value(meta: &SessionMetadata) -> StoreResult<Value> {
  let mut value = serde_json::to_value(meta).map_err(|error| StoreError::new(error.to_string()))?;
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

pub(crate) fn run_backend(store: &Store, session_id: &str) -> StoreResult<Option<String>> {
  if let Some(backend) = store.run_backend(session_id) {
    return Ok(Some(backend));
  }
  Ok(store.newest_run_record_sync(session_id)?.and_then(|record| record.backend))
}

pub(crate) fn listed_values(
  store: &Store,
  registry: &Registry,
  rows: &[SessionMetadata],
  derived: &DerivedSidebarMap,
  projection: SidebarProjection,
) -> StoreResult<Vec<Value>> {
  let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
  let listing_fields = registry.sequence_listing_fields(&ids, UtcDateTime::now());
  rows
    .iter()
    .map(|row| {
      let mut value = metadata_value(row)?;
      apply_sidebar_fields(&mut value, row, derived.get(&row.id), store, projection)?;
      apply_listing_fields(&mut value, listing_fields.get(&row.id).cloned().unwrap_or_default());
      Ok(value)
    })
    .collect()
}

fn apply_sidebar_fields(
  value: &mut Value,
  meta: &SessionMetadata,
  derived: Option<&Map<String, Value>>,
  store: &Store,
  projection: SidebarProjection,
) -> StoreResult<()> {
  let object = value.as_object_mut().expect("session metadata serializes to an object");
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
  object.insert(
    "thinking_since".to_string(),
    store.busy_since(&meta.id).map_or(Value::Null, |value| json!(value.to_python_string())),
  );
  let run_backend =
    if projection.include_run_backend || projection.display_backend { run_backend(store, &meta.id)? } else { None };
  if projection.include_run_backend {
    object.insert("run_backend".to_string(), run_backend.clone().map_or(Value::Null, Value::String));
  }
  if projection.display_backend
    && meta.profile == TaskProfile::Worker
    && let Some(backend) = run_backend
  {
    object.insert("backend".to_string(), Value::String(backend));
  }
  Ok(())
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
    let mut meta = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    meta.task = Some(charliebot_store::metadata::TaskSpec {
      task_type: Some(charliebot_store::metadata::TaskType::Implement),
      goal: "private goal".to_string(),
      ..Default::default()
    });
    let value = metadata_value(&meta).unwrap();
    assert!(value["task"].get("goal").is_none());
    assert_eq!(value["task"]["task_type"], "implement");
  }
}
