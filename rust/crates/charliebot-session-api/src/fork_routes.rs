//! The fork and elone routes (`src/runtime/api/sessions.py`, `fork_session` and `elone_session`). Each creates a successor
//! session from the source's chat history, then admits the bootstrap prompt as the caller's input and dispatches the
//! successor's first run.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::task_errors::task_http_error;
use charliebot_store::StoreResult;
use charliebot_store::control_events::{ACTOR_AGENT, ACTOR_USER};
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::session_fork::{ELONE_BOOTSTRAP_OPENER, FORK_BOOTSTRAP_OPENER, HISTORY_LOCATION_NOTE};
use charliebot_tasks::dispatch::{self, Dispatcher};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::routes::store_error;
use crate::write_routes::{config, default_backend_id, resolve_requested_backend, typed};

/// `models.ForkSessionRequest`: the fork body is optional, and each field has a default.
const FORK_FIELDS: &[Field] =
  &[Field::optional("event_index", FieldType::NullableInteger), Field::optional("backend", FieldType::NullableString)];
const FORK_BODY: BodyModel = BodyModel::optional(FORK_FIELDS);

/// `models.EloneSessionRequest`: the elone body is required, and `event_index` has no default.
const ELONE_FIELDS: &[Field] =
  &[Field::required("event_index", FieldType::Integer), Field::optional("backend", FieldType::NullableString)];
const ELONE_BODY: BodyModel = BodyModel::required(ELONE_FIELDS);

#[derive(Default, Deserialize)]
struct ForkFields {
  #[serde(default)]
  event_index: Option<i64>,
  #[serde(default)]
  backend: Option<String>,
}

#[derive(Deserialize)]
struct EloneFields {
  event_index: i64,
  #[serde(default)]
  backend: Option<String>,
}

/// `POST /api/sessions/{session_id}/fork`: a clone of the session's history, opened by the fork bootstrap.
async fn fork_session(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let body = match fastapi_body::read(&headers, body).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let parent = match features.store.require_session(&session_id).await {
    Ok(parent) => parent,
    Err(error) => return store_error(error),
  };
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let dispatch = match dispatch::dispatcher(&features) {
    Ok(dispatch) => dispatch,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let fields: ForkFields = match fork_fields(&body) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let cfg = match config(&features) {
    Ok(cfg) => cfg,
    Err(response) => return response,
  };
  let backend = match resolve_requested_backend(
    fields.backend.as_deref(),
    &cfg,
    &features.registry,
    &parent_backend_or_default(&parent, &cfg),
  ) {
    Ok(backend) => backend,
    Err(error) => return task_http_error(error),
  };
  let meta = match features.store.fork_session(&session_id, fields.event_index, Some(&backend)).await {
    Ok(meta) => meta,
    Err(error) => return task_http_error(error),
  };
  let directive = format!(
    "Get oriented from that log, summarize where things stand, and wait for the user's next instruction. {}",
    successor_tree_note(&meta.id)
  );
  if let Err(error) =
    start_successor_run(&dispatch, &caller, &meta, &format!("{FORK_BOOTSTRAP_OPENER} "), &directive).await
  {
    return task_http_error(error);
  }
  spawn_response(&features, &meta)
}

/// `POST /api/sessions/{session_id}/elone`: a fresh successor that starts from the history up to `event_index`.
async fn elone_session(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let body = match fastapi_body::read(&headers, body).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let parent = match features.store.require_session(&session_id).await {
    Ok(parent) => parent,
    Err(error) => return store_error(error),
  };
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let dispatch = match dispatch::dispatcher(&features) {
    Ok(dispatch) => dispatch,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let fields: EloneFields = match elone_fields(&body) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let cfg = match config(&features) {
    Ok(cfg) => cfg,
    Err(response) => return response,
  };
  let backend = match resolve_requested_backend(
    fields.backend.as_deref(),
    &cfg,
    &features.registry,
    &parent_backend_or_default(&parent, &cfg),
  ) {
    Ok(backend) => backend,
    Err(error) => return task_http_error(error),
  };
  let meta = match features.store.elone_session(&session_id, fields.event_index, Some(&backend), &*dispatch).await {
    Ok(meta) => meta,
    Err(error) => return task_http_error(error),
  };
  let prompt_head = format!(
    "{ELONE_BOOTSTRAP_OPENER} The dissatisfaction is usually with the most recent exchange before the takeover point. "
  );
  let directive = format!(
    "Understand what the user wanted and where it went wrong, then give your read and a better approach. \
     Confirm with the user before acting. {}",
    successor_tree_note(&meta.id)
  );
  if let Err(error) = start_successor_run(&dispatch, &caller, &meta, &prompt_head, &directive).await {
    return task_http_error(error);
  }
  spawn_response(&features, &meta)
}

/// The successor as its spawn returned it. Python answers with the in-memory model, which holds every session slot
/// field at its default and has no live-run fields, so the response is the spawn's metadata with those defaults.
fn spawn_response(features: &FeatureContext, meta: &SessionMetadata) -> Response {
  let value = features.store.session_metadata_value(meta);
  responses::json(StatusCode::OK, &value)
}

/// The fork body's fields. An absent body, which FastAPI passes as None, takes every default.
fn fork_fields(body: &RequestBody) -> Result<ForkFields, Box<Response>> {
  match body.validate(&FORK_BODY) {
    Ok(None) => Ok(ForkFields::default()),
    Ok(Some(Value::Object(values))) => typed(values).map_err(Box::new),
    Ok(Some(_)) => unreachable!("a validated model body is an object"),
    Err(response) => Err(response),
  }
}

/// The elone body's fields. The body is required, so the validated value is always an object.
fn elone_fields(body: &RequestBody) -> Result<EloneFields, Box<Response>> {
  match body.validate(&ELONE_BODY) {
    Ok(Some(Value::Object(values))) => typed(values).map_err(Box::new),
    Ok(_) => unreachable!("a required model body validates to an object"),
    Err(response) => Err(response),
  }
}

/// The backend a successor inherits when the request names none: the parent's, or the default when it has none.
fn parent_backend_or_default(parent: &SessionMetadata, cfg: &CharlieBotConfig) -> String {
  if parent.backend.is_empty() { default_backend_id(cfg) } else { parent.backend.clone() }
}

/// `_successor_tree_note`: the two tree sentences that every fork and elone bootstrap appends.
fn successor_tree_note(new_session_id: &str) -> String {
  format!(
    "Run `charliebot session tree --root {new_session_id}` to list your current child tasks. \
     Child tasks in the copied history can belong to the source session."
  )
}

/// `_start_successor_run`: admit the successor's bootstrap prompt as the caller's input, then dispatch its first run.
/// The prompt is the opener, the history note and the directive, joined by single spaces.
async fn start_successor_run(
  dispatch: &Dispatcher,
  caller: &CallerIdentity,
  meta: &SessionMetadata,
  prompt_head: &str,
  directive: &str,
) -> StoreResult<()> {
  let prompt = format!("{prompt_head}{HISTORY_LOCATION_NOTE} {directive}");
  let event_type = dispatch::input_event_type_for_caller(caller);
  let (from_session, from_session_name) =
    if event_type == event_types::USER { (None, None) } else { dispatch::agent_provenance(caller) };
  let actor = if event_type == event_types::USER { ACTOR_USER } else { ACTOR_AGENT };
  let fields = json!({"from_session": from_session, "from_session_name": from_session_name});
  dispatch.admit_input(&meta.id, event_type, &prompt, actor, fields).await?;
  dispatch.dispatch_pending(&meta.id).await?;
  Ok(())
}

/// The fork and elone routes, merged into the session router.
pub(crate) fn routes() -> Router<Arc<FeatureContext>> {
  Router::new()
    .route("/api/sessions/{session_id}/fork", post_only(fork_session))
    .route("/api/sessions/{session_id}/elone", post_only(elone_session))
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::Mutex;

  use axum::http::StatusCode;
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::{ChatEvent, CreateTask, Store, TaskCaller};
  use charliebot_tasks::dispatch::DispatchExecutor;
  use futures_util::future::BoxFuture;
  use serde_json::Map;

  use super::*;
  use crate::test_support::{answer, feature_context, install_dispatcher, json_request};

  /// Records the session and input contents of every launched run; each run is reported as `run-1`.
  #[derive(Default)]
  struct RecordingExecutor {
    launches: Mutex<Vec<(String, Vec<String>)>>,
  }

  impl DispatchExecutor for RecordingExecutor {
    fn launch<'a>(
      &'a self,
      session_id: &'a str,
      pending_inputs: &'a [ChatEvent],
      _launch_run_id: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<Option<String>>> {
      let contents = pending_inputs
        .iter()
        .map(|input| input["content"].as_str().expect("a pending input carries its content").to_string())
        .collect();
      self.launches.lock().expect("launch log").push((session_id.to_string(), contents));
      Box::pin(async { Ok(Some("run-1".to_string())) })
    }
  }

  /// A feature context whose dispatcher launches through a recording executor.
  fn recording_features() -> (tempfile::TempDir, Arc<FeatureContext>, Arc<RecordingExecutor>) {
    let (home, features) = feature_context();
    install_dispatcher(&features);
    let executor = Arc::new(RecordingExecutor::default());
    dispatch::dispatcher(&features).expect("installed dispatcher").set_executor(Some(executor.clone()));
    (home, features, executor)
  }

  /// A manager root pinned to `backend`, with one user message in its chat log.
  async fn seed_parent(store: &Store, backend: &str) -> SessionMetadata {
    let request = CreateTask {
      request_id: "parent".to_string(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some("Parent".to_string()),
      backend: Some(backend.to_string()),
      group: None,
      session_id: None,
    };
    let parent = store.create_task(request, TaskCaller::System, Map::new()).await.expect("parent session");
    let message =
      json!({"type": event_types::USER, "actor": "user", "source_session_id": parent.id, "content": "hello"});
    store.append_chat_event(&parent.id, message).await.expect("parent message");
    parent
  }

  async fn post_route(route: &str, features: &Arc<FeatureContext>, session_id: &str, body: Value) -> Response {
    let (headers, body) = json_request(body);
    let session_id = session_id.to_string();
    match route {
      "fork" => fork_session(State(Arc::clone(features)), AxumPath(session_id), headers, body).await,
      "elone" => elone_session(State(Arc::clone(features)), AxumPath(session_id), headers, body).await,
      _ => unreachable!("the route tests post to fork or elone"),
    }
  }

  /// The session directory names under the home, the set a refused fork or elone must leave unchanged.
  fn session_dir_names(features: &FeatureContext) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(features.store.config().sessions_dir())
      .expect("sessions directory")
      .map(|entry| entry.expect("session directory entry").file_name().to_string_lossy().into_owned())
      .collect();
    names.sort();
    names
  }

  /// The successor's only launch is its bootstrap prompt, which opens with `opener` and names the successor's id.
  fn assert_bootstrap_launch(executor: &RecordingExecutor, child: &str, opener: &str) {
    let launches = executor.launches.lock().expect("launch log");
    assert_eq!(launches.len(), 1);
    let (launched_id, prompts) = &launches[0];
    assert_eq!(launched_id, child);
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].starts_with(opener));
    assert!(prompts[0].contains(child));
  }

  /// Backend validation precedes every side effect: the route answers 400, creates no session directory,
  /// launches nothing, and leaves the parent's status unchanged.
  async fn assert_unresolvable_backend_persists_nothing(route: &str, parent_backend: &str, body: Value) {
    let (_home, features, executor) = recording_features();
    let parent = seed_parent(&features.store, parent_backend).await;
    let dirs_before = session_dir_names(&features);

    let (status, _) = answer(post_route(route, &features, &parent.id, body).await).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(session_dir_names(&features), dirs_before);
    assert!(executor.launches.lock().expect("launch log").is_empty());
    let parent_after = features.store.require_session(&parent.id).await.expect("parent session");
    assert_eq!(parent_after.status, parent.status);
  }

  #[tokio::test]
  async fn fork_inherits_the_parent_backend_and_launches_only_its_bootstrap() {
    let (_home, features, executor) = recording_features();
    let parent = seed_parent(&features.store, "synthetic-b").await;

    let (status, meta) = answer(post_route("fork", &features, &parent.id, json!({})).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["backend"], "synthetic-b");
    assert_bootstrap_launch(&executor, meta["id"].as_str().expect("child id"), FORK_BOOTSTRAP_OPENER);
  }

  #[tokio::test]
  async fn elone_inherits_the_parent_backend_and_launches_only_its_bootstrap() {
    let (_home, features, executor) = recording_features();
    let parent = seed_parent(&features.store, "synthetic-b").await;

    let (status, meta) = answer(post_route("elone", &features, &parent.id, json!({"event_index": 1})).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["backend"], "synthetic-b");
    assert_bootstrap_launch(&executor, meta["id"].as_str().expect("child id"), ELONE_BOOTSTRAP_OPENER);
  }

  #[tokio::test]
  async fn fork_refuses_an_explicit_unresolvable_backend_and_persists_nothing() {
    let body = json!({"event_index": 1, "backend": "missing-backend"});
    assert_unresolvable_backend_persists_nothing("fork", "synthetic-a", body).await;
  }

  #[tokio::test]
  async fn fork_refuses_an_inherited_unresolvable_backend_and_persists_nothing() {
    assert_unresolvable_backend_persists_nothing("fork", "missing-backend", json!({"event_index": 1})).await;
  }

  #[tokio::test]
  async fn elone_refuses_an_explicit_unresolvable_backend_and_persists_nothing() {
    let body = json!({"event_index": 1, "backend": "missing-backend"});
    assert_unresolvable_backend_persists_nothing("elone", "synthetic-a", body).await;
  }

  #[tokio::test]
  async fn elone_refuses_an_inherited_unresolvable_backend_and_persists_nothing() {
    assert_unresolvable_backend_persists_nothing("elone", "missing-backend", json!({"event_index": 1})).await;
  }
}
