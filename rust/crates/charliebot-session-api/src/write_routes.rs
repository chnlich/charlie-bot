//! Session write routes: create (a v2 task node or the operator's manager root), backend switch, and PATCH (rename or the
//! v2 task metadata mutations). Each body is checked the way FastAPI checks its request model.

// Each step answers a refused request with the response itself, so its error is the response (as `caller::resolve_caller`).
#![allow(clippy::result_large_err)]

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::{responses, validation};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::metadata_slot::MetadataFile;
use charliebot_core::{event_types, pyfmt};
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::task_errors::task_http_error;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};
use charliebot_store::{
  AGENT_CREATE_SCOPE_REFUSAL, CreateTask, PatchTask, SESSION_NOT_FOUND_DETAIL, Store, StoreError, StoreResult,
  TASK_CREATE_REQUEST_ID_REQUIRED, TaskCaller,
};
use charliebot_tasks::dispatch;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::detail::response_metadata;
use crate::routes::store_error;

const LOG_TARGET: &str = "charliebot_session_api";
const PROFILE_CHOICES: &[&str] = &["manager", "worker"];
const TASK_TYPE_CHOICES: &[&str] = &["implement", "quick-edit", "script-run", "verify", "cold-read"];
const PROFILE_REQUIRED: &str = "profile must be 'manager' or 'worker'";
const RENAME_REQUIRES_NAME: &str = "rename requires a non-empty name";

/// `models.ColdReadSpec`.
const COLD_READ_SPEC_FIELDS: &[Field] = &[
  Field::required("page_path", FieldType::String),
  Field::required("genre", FieldType::String),
  Field::required("trigger", FieldType::String),
];
const COLD_READ_SPEC: BodyModel = BodyModel::optional(COLD_READ_SPEC_FIELDS).forbid_extra();

/// `models.TaskSpec`. Its model check is `_cold_read_matches_task_type`.
const TASK_SPEC_FIELDS: &[Field] = &[
  Field::optional("goal", FieldType::String),
  Field::optional("acceptance", FieldType::StringList),
  Field::optional("context_refs", FieldType::StringList),
  Field::optional("repo_path", FieldType::NullableString),
  Field::optional("base_branch", FieldType::NullableString),
  Field::optional("task_type", FieldType::NullableEnum(TASK_TYPE_CHOICES)),
  Field::optional("keep_worktree", FieldType::Boolean),
  Field::optional("cold_read", FieldType::NullableModel { class_name: "ColdReadSpec", model: &COLD_READ_SPEC }),
];
const TASK_SPEC: BodyModel = BodyModel::optional(TASK_SPEC_FIELDS).forbid_extra().checked(cold_read_matches_task_type);

/// `models.CreateSessionRequest`. Its extra keys are the registered session slot values.
const CREATE_FIELDS: &[Field] = &[
  Field::optional("name", FieldType::NullableString),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("session_id", FieldType::NullableString),
  Field::optional("request_id", FieldType::NullableString),
  Field::optional("task_parent_id", FieldType::NullableString),
  Field::optional("profile", FieldType::NullableLiteral(PROFILE_CHOICES)),
  Field::optional("task", FieldType::NullableModel { class_name: "TaskSpec", model: &TASK_SPEC }),
  Field::optional("group", FieldType::NullableString),
];
const CREATE_SESSION: BodyModel = BodyModel::required(CREATE_FIELDS);

/// `models.PatchSessionTaskRequest`.
const PATCH_FIELDS: &[Field] = &[
  Field::optional("name", FieldType::NullableString),
  Field::optional("task_parent_id", FieldType::NullableString),
  Field::optional("profile", FieldType::NullableLiteral(PROFILE_CHOICES)),
  Field::optional("task", FieldType::NullableModel { class_name: "TaskSpec", model: &TASK_SPEC }),
  Field::optional("subtree_prompt", FieldType::NullableString),
  Field::optional("node_prompt", FieldType::NullableString),
];
const PATCH_SESSION: BodyModel = BodyModel::required(PATCH_FIELDS).forbid_extra();

/// `models.SwitchBackendRequest`.
const SWITCH_BACKEND: BodyModel = BodyModel::required(&[Field::required("backend", FieldType::String)]);

/// The validated create body. `slot_values` takes the keys that no declared field names.
#[derive(Deserialize)]
struct CreateFields {
  #[serde(default)]
  name: Option<String>,
  #[serde(default)]
  backend: Option<String>,
  #[serde(default)]
  session_id: Option<String>,
  #[serde(default)]
  request_id: Option<String>,
  #[serde(default)]
  task_parent_id: Option<String>,
  #[serde(default)]
  profile: Option<TaskProfile>,
  #[serde(default)]
  task: Option<TaskSpec>,
  #[serde(default)]
  group: Option<String>,
}

/// The validated PATCH body. An absent key is `None`, an explicit null is `Some(None)`, and a value is `Some(Some(_))`.
#[derive(Deserialize)]
struct PatchFields {
  #[serde(default)]
  name: Option<String>,
  #[serde(default, deserialize_with = "present")]
  task_parent_id: Option<Option<String>>,
  #[serde(default, deserialize_with = "present")]
  profile: Option<Option<TaskProfile>>,
  #[serde(default, deserialize_with = "present")]
  task: Option<Option<TaskSpec>>,
  #[serde(default, deserialize_with = "present")]
  subtree_prompt: Option<Option<String>>,
  #[serde(default, deserialize_with = "present")]
  node_prompt: Option<Option<String>>,
}

#[derive(Deserialize)]
struct SwitchFields {
  backend: String,
}

fn present<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
  T: Deserialize<'de>,
  D: Deserializer<'de>,
{
  Option::<T>::deserialize(deserializer).map(Some)
}

/// `TaskSpec._cold_read_matches_task_type`: a cold-read task carries `cold_read`, and no other task does.
fn cold_read_matches_task_type(task: &Map<String, Value>) -> Option<String> {
  let task_type = task.get("task_type").and_then(Value::as_str);
  let has_cold_read = task.get("cold_read").is_some_and(|value| !value.is_null());
  if task_type == Some("cold-read") {
    return (!has_cold_read).then(|| "a cold-read task requires cold_read".to_string());
  }
  has_cold_read.then(|| format!("a {} task rejects cold_read", task_type.unwrap_or("None")))
}

/// `POST /api/sessions/`: a v2 task create when a task field is set, else the operator's legacy manager-root create.
pub(crate) async fn create_session(
  State(features): State<Arc<FeatureContext>>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let body = match fastapi_body::read(&headers, body).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  match create(&features, &caller, &body).await {
    Ok(meta) => model_response(&features.store, &meta),
    Err(response) => response,
  }
}

/// `POST /api/sessions/{session_id}/backend`: switch the session's backend in place, with the sequence-binding lock and
/// the `backend_switched` audit event.
pub(crate) async fn switch_backend(
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
  match switch(&features, &session_id, parent, &body).await {
    Ok(meta) => model_response(&features.store, &meta),
    Err(response) => response,
  }
}

/// `PATCH /api/sessions/{session_id}`: a name-only PATCH renames; any other field runs the task metadata mutation.
pub(crate) async fn patch_session(
  State(features): State<Arc<FeatureContext>>,
  AxumPath(session_id): AxumPath<String>,
  headers: HeaderMap,
  body: Body,
) -> Response {
  let body = match fastapi_body::read(&headers, body).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let caller = match caller::resolve_caller(&headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  match patch(&features, &session_id, &caller, &body).await {
    Ok(detail) => responses::json(StatusCode::OK, &detail),
    Err(response) => response,
  }
}

async fn create(
  features: &FeatureContext,
  caller: &CallerIdentity,
  body: &RequestBody,
) -> Result<SessionMetadata, Response> {
  let (fields, slot_values) = create_fields(features, body)?;
  let cfg = config(features)?;
  let v2 =
    fields.request_id.is_some() || fields.task_parent_id.is_some() || fields.profile.is_some() || fields.task.is_some();
  if v2 {
    // The task-tree owner binds (parent, request_id) to one node; request_id is required there.
    let Some(request_id) = fields.request_id else {
      return Err(store_error(StoreError::invalid(TASK_CREATE_REQUEST_ID_REQUIRED)));
    };
    if let Some(requested) = fields.backend.as_deref() {
      resolve_requested_backend(Some(requested), &cfg, &features.registry, &default_backend_id(&cfg))
        .map_err(store_error)?;
    }
    let profile = match fields.profile {
      Some(profile) => profile,
      None if request_id.is_empty() => return Err(store_error(StoreError::invalid(TASK_CREATE_REQUEST_ID_REQUIRED))),
      None => return Err(store_error(StoreError::invalid(PROFILE_REQUIRED))),
    };
    let task_parent_id = fields.task_parent_id.clone();
    let request = CreateTask {
      request_id,
      task_parent_id: task_parent_id.clone(),
      profile,
      task: fields.task,
      name: fields.name,
      backend: fields.backend,
      group: fields.group,
      session_id: None,
    };
    let meta = features.store.create_task(request, task_caller(caller), slot_values).await.map_err(task_http_error)?;
    tracing::info!(
      target: LOG_TARGET,
      session_id = %meta.id,
      task_parent_id = ?task_parent_id,
      profile = profile_name(profile),
      "task_created"
    );
    return Ok(meta);
  }
  // The legacy create shape (the sidebar's new-session action) is operator scope and opens a manager root.
  if !caller.is_operator() {
    return Err(store_error(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL)));
  }
  let backend =
    resolve_requested_backend(fields.backend.as_deref(), &cfg, &features.registry, &default_backend_id(&cfg))
      .map_err(store_error)?;
  tracing::info!(target: LOG_TARGET, backend = %backend, name = ?fields.name, "creating_session");
  let request = CreateTask {
    request_id: format!("operator-create-{}", uuid::Uuid::new_v4()),
    task_parent_id: None,
    profile: TaskProfile::Manager,
    task: None,
    name: fields.name,
    backend: Some(backend),
    group: fields.group,
    session_id: fields.session_id,
  };
  features.store.create_task(request, task_caller(caller), slot_values).await.map_err(task_http_error)
}

/// The validated create body and the keys beyond its declared fields. Each extra key must be a registered session
/// slot key, as `CreateSessionRequest._registered_keys_only` checks it.
fn create_fields(
  features: &FeatureContext,
  body: &RequestBody,
) -> Result<(CreateFields, Map<String, Value>), Response> {
  let values = validated_object(body, &CREATE_SESSION)?;
  let RequestBody::Json(Value::Object(raw)) = body else {
    return Err(responses::internal_error("RuntimeError", "validated create body is not an object".to_string()));
  };
  let mut slot_values: Map<String, Value> = raw
    .iter()
    .filter(|(key, _)| !CREATE_SESSION.declares(key))
    .map(|(key, value)| (key.clone(), value.clone()))
    .collect();
  let slots = features.registry.metadata_slots();
  let registered: HashSet<String> = slots
    .iter()
    .filter(|slot| slot.on == MetadataFile::Session)
    .flat_map(|slot| slot.fields.iter().map(|field| field.name.to_string()))
    .collect();
  let mut unknown: Vec<&str> =
    slot_values.keys().map(String::as_str).filter(|key| !registered.contains(*key)).collect();
  if !unknown.is_empty() {
    unknown.sort_unstable();
    let error = validation::FieldError {
      loc: vec![validation::Loc::Field("body".to_string())],
      input: Value::Object(raw.clone()),
      kind: validation::Kind::ValueError { message: format!("unknown keys {}", list_repr(&unknown)) },
    };
    return Err(validation::response(&[error]));
  }
  for slot in slots.iter().filter(|slot| slot.on == MetadataFile::Session) {
    let mut owner_values: Map<String, Value> = slot
      .fields
      .iter()
      .filter_map(|field| slot_values.get(field.name).map(|value| (field.name.to_string(), value.clone())))
      .collect();
    let Some(validate) = features.registry.metadata_slot_validator(slot.owner, MetadataFile::Session) else {
      return Err(responses::internal_error(
        "RuntimeError",
        format!("metadata slot owner {:?} has no session request validator", slot.owner),
      ));
    };
    if let Some(message) = validate(&mut owner_values) {
      let error = validation::FieldError {
        loc: vec![validation::Loc::Field("body".to_string())],
        input: Value::Object(raw.clone()),
        kind: validation::Kind::ValueError {
          message: format!("invalid value for keys of {:?}: {message}", slot.owner),
        },
      };
      return Err(validation::response(&[error]));
    }
    slot_values.extend(owner_values);
  }
  Ok((typed(values)?, slot_values))
}

async fn switch(
  features: &FeatureContext,
  session_id: &str,
  parent: SessionMetadata,
  body: &RequestBody,
) -> Result<SessionMetadata, Response> {
  let SwitchFields { backend: target } = typed(validated_object(body, &SWITCH_BACKEND)?)?;
  let cfg = config(features)?;
  let valid = valid_backend_ids(&cfg);
  if !valid.contains(&target.as_str()) {
    return Err(store_error(unknown_backend(&target, &valid)));
  }
  let effective = if parent.backend.is_empty() { default_backend_id(&cfg) } else { parent.backend.clone() };
  if target == effective {
    return Ok(parent);
  }

  let binding = features
    .registry
    .binding_for(session_id)
    .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?;
  if let Some(binding) = binding
    && binding.dedicated_backend()
    && !same_continuation_domain(&effective, &target, &cfg, &features.registry).map_err(store_error)?
  {
    return Err(store_error(StoreError::invalid(binding.backend_lock_detail(&target))));
  }

  // The pre-rule backfill: a held native id with no recorded producer gets the effective current backend first, so the
  // next turn judges the id against the backend that produced it.
  let holds_native_id = parent.cc_session_id.as_deref().is_some_and(|id| !id.is_empty());
  if holds_native_id && parent.native_backend.as_deref().is_none_or(str::is_empty) {
    features.store.persist_native_backend(session_id, &effective).await.map_err(store_error)?;
  }

  let meta = features
    .store
    .switch_backend(session_id, &target)
    .await
    .map_err(store_error)?
    .ok_or_else(|| store_error(StoreError::not_found(SESSION_NOT_FOUND_DETAIL)))?;
  tracing::info!(target: LOG_TARGET, session_id, backend = %target, "session_backend_switched");
  let mut sidebar_fields = Map::new();
  sidebar_fields.insert("backend".to_string(), json!(target));
  features
    .store
    .broadcast_sidebar(session_id, event_types::BACKEND_SWITCHED, sidebar_fields)
    .await
    .map_err(store_error)?;

  // The audit event is the switch history; the durable metadata read here (the backfill included) is what it records.
  let durable = features.store.read_metadata_fresh(session_id).map_err(store_error)?;
  let held = durable.as_ref().filter(|meta| meta.cc_session_id.as_deref().is_some_and(|id| !id.is_empty()));
  let audit = json!({
    "type": event_types::BACKEND_SWITCHED,
    "from": effective,
    "to": target,
    "previous_native_backend": held.and_then(|meta| meta.native_backend.clone()),
    "previous_native_session_id": durable.as_ref().and_then(|meta| meta.cc_session_id.clone()),
  });
  features.store.persist_and_broadcast(session_id, audit).await.map_err(store_error)?;
  Ok(meta)
}

async fn patch(
  features: &FeatureContext,
  session_id: &str,
  caller: &CallerIdentity,
  body: &RequestBody,
) -> Result<Value, Response> {
  let values = validated_object(body, &PATCH_SESSION)?;
  let name_only = values.keys().all(|key| key == "name");
  let fields: PatchFields = typed(values)?;
  if name_only {
    return rename(features, session_id, fields.name).await;
  }
  let patch = PatchTask {
    name: fields.name,
    task_parent_id: fields.task_parent_id,
    profile: fields.profile,
    task: fields.task,
    subtree_prompt: fields.subtree_prompt,
    node_prompt: fields.node_prompt,
  };
  let guards =
    dispatch::dispatcher(features).map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?;
  let meta =
    features.store.patch_task(session_id, patch, task_caller(caller), &*guards).await.map_err(task_http_error)?;
  features.store.session_detail(&meta.id).map_err(task_http_error)
}

/// The name-only PATCH: the legacy rename, answered with the session detail form.
async fn rename(features: &FeatureContext, session_id: &str, name: Option<String>) -> Result<Value, Response> {
  let name = match name {
    Some(name) if !name.is_empty() => name,
    _ => return Err(store_error(StoreError::invalid(RENAME_REQUIRES_NAME))),
  };
  let meta = features
    .store
    .rename_session(session_id, &name)
    .await
    .map_err(store_error)?
    .ok_or_else(|| store_error(StoreError::not_found(SESSION_NOT_FOUND_DETAIL)))?;
  tracing::info!(target: LOG_TARGET, session_id, new_name = %name, "session_renamed");
  features.store.invalidate_tree_index();
  features.store.notify_task_tree_changed(session_id, Some("task_updated")).await;
  let meta = response_metadata(&features.store, &meta).map_err(store_error)?;
  rename_response(&features.store, &meta)
}

fn rename_response(store: &Store, meta: &SessionMetadata) -> Result<Value, Response> {
  let mut detail = store.session_metadata_value(meta);
  let object = detail.as_object_mut().expect("session metadata serializes to an object");
  object.insert("task_state".to_string(), json!("open"));
  if object.get("work_state").is_none_or(Value::is_null) {
    object.insert("work_state".to_string(), json!("idle"));
  }
  object.insert("archived".to_string(), json!(false));
  object.insert("ancestors".to_string(), json!([]));
  object.insert("prompt_rules".to_string(), json!({}));
  Ok(detail)
}

/// Validate a body against `model` and return its declared fields. FastAPI's 422 answers any other shape.
fn validated_object(body: &RequestBody, model: &BodyModel) -> Result<Map<String, Value>, Response> {
  match body.validate(model) {
    Ok(Some(Value::Object(values))) => Ok(values),
    Ok(other) => Err(responses::internal_error("RuntimeError", format!("validated body is {other:?}"))),
    Err(response) => Err(*response),
  }
}

/// Deserialize validated fields. Validation has already fixed their shape, so a failure here is a server fault.
pub(crate) fn typed<T: DeserializeOwned>(values: Map<String, Value>) -> Result<T, Response> {
  serde_json::from_value(Value::Object(values))
    .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))
}

pub(crate) fn config(features: &FeatureContext) -> Result<Arc<CharlieBotConfig>, Response> {
  features.config.get().map_err(|error| responses::internal_error("RuntimeError", error.to_string()))
}

fn valid_backend_ids(cfg: &CharlieBotConfig) -> Vec<&str> {
  cfg.backends.options.iter().map(|option| option.id.as_str()).collect()
}

/// `_default_backend_id`: the first configured option, else `claude`.
pub(crate) fn default_backend_id(cfg: &CharlieBotConfig) -> String {
  cfg.backends.options.first().map_or_else(|| "claude".to_string(), |option| option.id.clone())
}

/// `_resolve_requested_backend`: a configured id, a family-prefix alias (the first option of that type), or, when no
/// id was given, the fallback backend. The fallback is checked against the configured ids as the requested id is.
pub(crate) fn resolve_requested_backend(
  requested: Option<&str>,
  cfg: &CharlieBotConfig,
  registry: &Registry,
  fallback_backend: &str,
) -> StoreResult<String> {
  let valid = valid_backend_ids(cfg);
  if let Some(requested) = requested {
    if valid.contains(&requested) {
      tracing::info!(target: LOG_TARGET, backend = requested, "using_requested_backend");
      return Ok(requested.to_string());
    }
    let family = registry
      .type_for_family_prefix(requested)
      .and_then(|family_type| cfg.backends.options.iter().find(|option| option.backend_type == family_type));
    if let Some(option) = family {
      tracing::info!(target: LOG_TARGET, requested, backend = %option.id, "using_requested_backend_family_match");
      return Ok(option.id.clone());
    }
    tracing::warn!(target: LOG_TARGET, requested, valid = ?valid, "invalid_backend_requested");
    return Err(unknown_backend(requested, &valid));
  }
  if !valid.contains(&fallback_backend) {
    tracing::warn!(target: LOG_TARGET, fallback = fallback_backend, valid = ?valid, "invalid_fallback_backend");
    return Err(unknown_backend(fallback_backend, &valid));
  }
  tracing::info!(target: LOG_TARGET, fallback = fallback_backend, "using_fallback_backend");
  Ok(fallback_backend.to_string())
}

/// `_UNKNOWN_BACKEND_DETAIL`: the 400 for an id that no option answers to, naming the sorted valid ids.
fn unknown_backend(backend: &str, valid: &[&str]) -> StoreError {
  let mut sorted = valid.to_vec();
  sorted.sort_unstable();
  StoreError::invalid(format!("backend '{backend}' is not a recognized backend id; valid ids: {}", list_repr(&sorted)))
}

/// `backend_types.same_continuation_domain`: false when either id is not a configured option.
fn same_continuation_domain(
  current: &str,
  target: &str,
  cfg: &CharlieBotConfig,
  registry: &Registry,
) -> StoreResult<bool> {
  let (Some(current), Some(target)) = (cfg.get_backend_option(current), cfg.get_backend_option(target)) else {
    return Ok(false);
  };
  Ok(continuation_domain(current, cfg, registry)? == continuation_domain(target, cfg, registry)?)
}

fn continuation_domain(option: &BackendOption, cfg: &CharlieBotConfig, registry: &Registry) -> StoreResult<String> {
  let lifecycle = registry.lifecycle_for(&option.backend_type).map_err(|error| StoreError::new(error.to_string()))?;
  Ok(lifecycle.continuation_domain(option, cfg))
}

/// The caller as the task owner sees it: an agent's verified run claims, else the operator.
fn task_caller(caller: &CallerIdentity) -> TaskCaller {
  match caller.claims() {
    Some(claims) => TaskCaller::Agent { session_id: claims.session_id.clone() },
    None => TaskCaller::Operator,
  }
}

fn profile_name(profile: TaskProfile) -> &'static str {
  match profile {
    TaskProfile::Manager => "manager",
    TaskProfile::Worker => "worker",
  }
}

/// Python's repr of a list of strings, as the Python detail texts print it.
fn list_repr(items: &[&str]) -> String {
  let quoted: Vec<String> = items.iter().map(|item| pyfmt::str_repr(item)).collect();
  format!("[{}]", quoted.join(", "))
}

/// The `SessionMetadata` response form, which `response_model=models.SessionMetadata` serializes. Like every API return
/// path it carries the live run backend and busy-since (`session_store.stamp_thinking_since`).
pub(crate) fn model_response(store: &Store, meta: &SessionMetadata) -> Response {
  let value = response_metadata(store, meta).map(|meta| store.session_metadata_value(&meta));
  match value {
    Ok(value) => responses::json(StatusCode::OK, &value),
    Err(error) => store_error(error),
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use charliebot_core::run_token::RunTokenClaims;
  use charliebot_core::test_support::EventSink;
  use charliebot_store::{SIDEBAR_CHANNEL, session_channel};
  use tracing::Event;
  use tracing::field::{Field, Visit};

  use super::*;
  use crate::test_support::{answer, create_node, feature_context, install_dispatcher, json_request};

  struct LogMessage(Arc<Mutex<Vec<String>>>);

  impl EventSink for LogMessage {
    fn event(&self, event: &Event<'_>) {
      let mut message = MessageField(String::new());
      event.record(&mut message);
      self.0.lock().expect("captured logs mutex").push(format!("{} {}", event.metadata().target(), message.0));
    }
  }

  struct MessageField(String);

  impl Visit for MessageField {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
      if field.name() == "message" {
        self.0 = format!("{value:?}");
      }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
      if field.name() == "message" {
        self.0 = value.to_string();
      }
    }
  }

  /// Captures this thread's events into the returned buffer through the shared test capture, which keeps a callsite
  /// another thread reached first enabled for this thread.
  fn capture_logs() -> (Arc<Mutex<Vec<String>>>, charliebot_core::test_support::CaptureGuard) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let guard = charliebot_core::test_support::install_events(Arc::new(LogMessage(Arc::clone(&logs))));
    (logs, guard)
  }

  #[tokio::test]
  async fn create_refuses_a_task_field_without_a_request_id() {
    let (_home, features) = feature_context();
    let (headers, body) = json_request(json!({"task_parent_id": "manager"}));

    let (status, detail) = answer(create_session(State(features), headers, body).await).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail, json!({"detail": "request_id is required for task creation"}));
  }

  #[tokio::test(flavor = "current_thread")]
  async fn legacy_create_returns_the_model_logs_and_notifies_after_publishing() {
    let (_home, features) = feature_context();
    let (logs, _subscriber) = capture_logs();
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let (headers, body) = json_request(json!({"name": "Synthetic root", "group": "Synthetic group"}));

    let (status, result) = answer(create_session(State(features.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["name"], "Synthetic root");
    assert_eq!(result["group"], "Synthetic group");
    assert_eq!(result["backend"], "synthetic-a");
    assert_eq!(result["profile"], "manager");
    let session_id = result["id"].as_str().expect("created session id");
    let frame: Value = serde_json::from_str(&sidebar.recv().await.expect("task tree notification")).unwrap();
    assert_eq!(frame, json!({"type": "task_tree_changed", "session_id": session_id, "fact_type": "task_created"}));
    let events = features.store.load_chat_events_sync(session_id).expect("created chat events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"].as_str(), Some("task_created"));
    assert_eq!(events[0]["actor"].as_str(), Some("user"));
    assert!(logs.lock().expect("captured logs mutex").iter().any(|line| line.contains("creating_session")));
  }

  #[tokio::test(flavor = "current_thread")]
  async fn v2_create_returns_the_task_and_logs_its_creation() {
    let (_home, features) = feature_context();
    let (logs, _subscriber) = capture_logs();
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let (headers, body) = json_request(json!({"request_id": "api-v2-root", "profile": "manager", "name": "V2 root"}));

    let (status, result) = answer(create_session(State(features.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["name"], "V2 root");
    assert_eq!(result["profile"], "manager");
    assert_eq!(result["backend"], "synthetic-a");
    let session_id = result["id"].as_str().expect("created task id");
    let frame: Value = serde_json::from_str(&sidebar.recv().await.expect("task tree notification")).unwrap();
    assert_eq!(frame["type"], "task_tree_changed");
    assert_eq!(frame["session_id"], session_id);
    assert_eq!(frame["fact_type"], "task_created");
    let events = features.store.load_chat_events_sync(session_id).expect("created chat events");
    assert_eq!(events[0]["type"].as_str(), Some("task_created"));
    assert_eq!(events[0]["request_id"].as_str(), Some("api-v2-root"));
    assert!(logs.lock().expect("captured logs mutex").iter().any(|line| line.contains("task_created")));
  }

  #[tokio::test]
  async fn create_refuses_a_key_no_slot_registered_as_a_model_error() {
    let (_home, features) = feature_context();
    let (headers, body) = json_request(json!({"name": "Atlas", "presentation": "wide"}));

    let (status, detail) = answer(create_session(State(features), headers, body).await).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["loc"], json!(["body"]));
    assert_eq!(detail["detail"][0]["type"], "value_error");
    assert_eq!(detail["detail"][0]["msg"], "Value error, unknown keys ['presentation']");
  }

  #[tokio::test]
  async fn create_refuses_a_registered_slot_value_with_a_wrong_type() {
    let (_home, features) = feature_context();
    let (headers, body) = json_request(json!({"test_session_value": 4}));

    let (status, detail) = answer(create_session(State(features), headers, body).await).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["loc"], json!(["body"]));
    assert_eq!(detail["detail"][0]["type"], "value_error");
  }

  #[tokio::test]
  async fn create_profile_literal_errors_keep_the_python_error_type() {
    let (_home, features) = feature_context();
    let (headers, body) = json_request(json!({"request_id": "bad-profile", "profile": "reviewer"}));

    let (status, detail) = answer(create_session(State(features), headers, body).await).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "profile"]));
    assert_eq!(detail["detail"][0]["type"], "literal_error");
  }

  #[tokio::test]
  async fn create_refuses_an_unknown_backend_before_it_creates_a_node() {
    let (_home, features) = feature_context();
    let sessions_dir = features.config.get().expect("test config").sessions_dir();
    let (headers, body) = json_request(json!({"backend": "missing"}));

    let (status, detail) = answer(create_session(State(features.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail["detail"].as_str().expect("detail text").starts_with("backend 'missing' is not a recognized"));
    assert!(!sessions_dir.exists(), "a refused create writes no session");
  }

  #[tokio::test]
  async fn switch_answers_404_for_a_missing_session() {
    let (_home, features) = feature_context();
    let (headers, body) = json_request(json!({"backend": "claude"}));

    let response = switch_backend(State(features), AxumPath("ghost".to_string()), headers, body).await;
    let (status, detail) = answer(response).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(detail, json!({"detail": "Session not found"}));
  }

  #[tokio::test]
  async fn switch_refuses_an_unknown_backend_and_a_body_without_one() {
    let (_home, features) = feature_context();
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;

    let (headers, body) = json_request(json!({"backend": "missing"}));
    let (status, detail) =
      answer(switch_backend(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
      detail,
      json!({"detail": "backend 'missing' is not a recognized backend id; valid ids: ['synthetic-a', 'synthetic-b']"})
    );

    let (headers, body) = json_request(json!({}));
    let (status, detail) = answer(switch_backend(State(features), AxumPath(node.id), headers, body).await).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "backend"]));
    assert_eq!(detail["detail"][0]["type"], "missing");
  }

  #[tokio::test(flavor = "current_thread")]
  async fn backend_switch_same_target_is_quiet_and_new_target_broadcasts_and_audits() {
    let (_home, features) = feature_context();
    let (logs, _subscriber) = capture_logs();
    let node = create_node(&features.store, "switch-node", None, TaskProfile::Manager).await;
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let mut session = features.store.subscribe(&session_channel(&node.id));

    let (headers, body) = json_request(json!({"backend": "synthetic-a"}));
    let (status, result) =
      answer(switch_backend(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["backend"], "synthetic-a");
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), sidebar.recv()).await.is_err());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), session.recv()).await.is_err());

    let (headers, body) = json_request(json!({"backend": "synthetic-b"}));
    let (status, result) =
      answer(switch_backend(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["backend"], "synthetic-b");
    let sidebar_frame: Value = serde_json::from_str(&sidebar.recv().await.expect("backend sidebar broadcast")).unwrap();
    assert_eq!(sidebar_frame, json!({"type": "backend_switched", "session_id": node.id, "backend": "synthetic-b"}));
    let message_frame: Value =
      serde_json::from_str(&session.recv().await.expect("backend switch message delta")).unwrap();
    assert_eq!(message_frame["type"], "message");
    let audit_frame: Value = serde_json::from_str(&session.recv().await.expect("backend audit broadcast")).unwrap();
    assert_eq!(audit_frame["type"], "backend_switched");
    assert_eq!(audit_frame["from"], "synthetic-a");
    assert_eq!(audit_frame["to"], "synthetic-b");
    let events = features.store.load_chat_events_sync(&node.id).expect("backend audit event");
    let audit =
      events.iter().find(|event| event["type"].as_str() == Some("backend_switched")).expect("audit event written");
    assert_eq!(audit["from"].as_str(), Some("synthetic-a"));
    assert_eq!(audit["to"].as_str(), Some("synthetic-b"));
    assert!(audit["previous_native_backend"].is_null());
    assert!(logs.lock().expect("captured logs mutex").iter().any(|line| line.contains("session_backend_switched")));
  }

  #[tokio::test]
  async fn backend_switch_respects_a_dedicated_sequence_lock() {
    let (_home, features) = feature_context();
    let request = CreateTask {
      request_id: "sequence-locked-node".to_string(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some("Locked node".to_string()),
      backend: Some("synthetic-a".to_string()),
      group: None,
      session_id: Some("sequence-locked-node".to_string()),
    };
    let node = features.store.create_task(request, TaskCaller::System, Map::new()).await.unwrap();
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let mut session = features.store.subscribe(&session_channel(&node.id));
    let (headers, body) = json_request(json!({"backend": "synthetic-b"}));

    let (status, detail) =
      answer(switch_backend(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail, json!({"detail": "backend 'synthetic-b' is locked by the session API test sequence"}));
    assert_eq!(features.store.read_metadata_fresh(&node.id).unwrap().unwrap().backend, "synthetic-a");
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), sidebar.recv()).await.is_err());
    assert!(tokio::time::timeout(std::time::Duration::from_millis(10), session.recv()).await.is_err());
  }

  #[tokio::test]
  async fn backend_switch_backfills_the_native_backend_before_auditing() {
    let (_home, features) = feature_context();
    let node = create_node(&features.store, "native-switch-node", None, TaskProfile::Manager).await;
    features.store.persist_cc_session_id(&node.id, "native-conversation", None).await.unwrap();
    let mut session = features.store.subscribe(&session_channel(&node.id));
    let (headers, body) = json_request(json!({"backend": "synthetic-b"}));

    let (status, result) =
      answer(switch_backend(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["backend"], "synthetic-b");
    let message_frame: Value =
      serde_json::from_str(&session.recv().await.expect("backend switch message delta")).unwrap();
    assert_eq!(message_frame["type"], "message");
    let event: Value = serde_json::from_str(&session.recv().await.expect("backend audit broadcast")).unwrap();
    assert_eq!(event["type"], "backend_switched");
    assert_eq!(event["previous_native_backend"], "synthetic-a");
    assert_eq!(event["previous_native_session_id"], "native-conversation");
    let metadata = features.store.read_metadata_fresh(&node.id).unwrap().unwrap();
    assert_eq!(metadata.native_backend.as_deref(), Some("synthetic-a"));
  }

  #[tokio::test]
  async fn rename_answers_the_session_detail_form() {
    let (_home, features) = feature_context();
    let (logs, _subscriber) = capture_logs();
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let (headers, body) = json_request(json!({"name": "Renamed"}));

    let (status, detail) =
      answer(patch_session(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["name"], "Renamed");
    assert_eq!(detail["work_state"], "idle");
    assert_eq!(detail["task_state"], "open");
    assert_eq!(detail["prompt_rules"], json!({}));
    assert_eq!(detail["run_backend"], Value::Null);
    assert_eq!(detail["ancestors"], json!([]));
    assert_eq!(features.store.read_metadata_fresh(&node.id).expect("node read").expect("node").name, "Renamed");
    let frame: Value = serde_json::from_str(&sidebar.recv().await.expect("rename tree notification")).unwrap();
    assert_eq!(frame, json!({"type": "task_tree_changed", "session_id": node.id, "fact_type": "task_updated"}));
    assert!(logs.lock().expect("captured logs mutex").iter().any(|line| line.contains("session_renamed")));
  }

  #[tokio::test]
  async fn rename_refuses_an_empty_name_and_an_empty_body() {
    let (_home, features) = feature_context();
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;

    for payload in [json!({"name": ""}), json!({"name": null}), json!({})] {
      let (headers, body) = json_request(payload);
      let response = patch_session(State(features.clone()), AxumPath(node.id.clone()), headers, body).await;
      let (status, detail) = answer(response).await;
      assert_eq!(status, StatusCode::BAD_REQUEST);
      assert_eq!(detail, json!({"detail": "rename requires a non-empty name"}));
    }
  }

  #[tokio::test]
  async fn patch_refuses_an_undeclared_key_as_a_model_error() {
    let (_home, features) = feature_context();
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;
    let (headers, body) = json_request(json!({"name": "Renamed", "presentation": "wide"}));

    let (status, detail) = answer(patch_session(State(features), AxumPath(node.id), headers, body).await).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "presentation"]));
    assert_eq!(detail["detail"][0]["type"], "extra_forbidden");
  }

  #[tokio::test]
  async fn patch_moves_a_task_under_a_missing_parent_as_404() {
    let (_home, features) = feature_context();
    install_dispatcher(&features);
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;
    let (headers, body) = json_request(json!({"task_parent_id": "ghost"}));

    let (status, detail) = answer(patch_session(State(features), AxumPath(node.id), headers, body).await).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(detail, json!({"detail": "task ghost not found"}));
  }

  #[tokio::test]
  async fn demoting_a_manager_with_a_child_is_a_conflict_with_its_blocker() {
    let (_home, features) = feature_context();
    install_dispatcher(&features);
    let root = create_node(&features.store, "root", None, TaskProfile::Manager).await;
    create_node(&features.store, "child", Some(&root.id), TaskProfile::Worker).await;
    let (headers, body) = json_request(json!({"profile": "worker"}));

    let (status, detail) = answer(patch_session(State(features), AxumPath(root.id), headers, body).await).await;

    let blocker = "demotion to worker requires a task with no child tasks";
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(detail, json!({"detail": {"message": blocker, "blockers": [blocker]}}));
  }

  #[tokio::test]
  async fn a_prompt_edit_answers_the_detail_with_its_rule_reference() {
    let (_home, features) = feature_context();
    install_dispatcher(&features);
    let node = create_node(&features.store, "node", None, TaskProfile::Manager).await;
    let mut sidebar = features.store.subscribe(SIDEBAR_CHANNEL);
    let (headers, body) = json_request(json!({"subtree_prompt": "synthetic rule\n"}));

    let (status, detail) =
      answer(patch_session(State(features.clone()), AxumPath(node.id.clone()), headers, body).await).await;

    let expected = features.store.store_prompt_body("synthetic rule\n").expect("prompt body");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["prompt_rules"]["subtree"]["ref"], expected);
    assert_eq!(detail["task_state"], "open");
    assert_eq!(detail["name"], "node");
    let frame: Value = serde_json::from_str(&sidebar.recv().await.expect("prompt change tree notification")).unwrap();
    assert_eq!(frame, json!({"type": "task_tree_changed", "session_id": node.id, "fact_type": "prompt_changed"}));
    let events = features.store.load_chat_events_sync(&node.id).expect("prompt change events");
    assert!(events.iter().any(|event| event["type"].as_str() == Some(event_types::PROMPT_CHANGED)));
  }

  #[tokio::test]
  async fn task_field_patch_refuses_an_agent_caller() {
    let (_home, features) = feature_context();
    install_dispatcher(&features);
    let body = RequestBody::Json(json!({"task": {"goal": "Synthetic task"}}));
    let caller = CallerIdentity::Agent {
      claims: RunTokenClaims {
        session_id: "agent-task".to_string(),
        run_id: "agent-run".to_string(),
        agent: "synthetic".to_string(),
      },
    };

    let response = patch(&features, "agent-task", &caller, &body).await.unwrap_err();
    let (status, detail) = answer(response).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(detail, json!({"detail": "task metadata mutations require operator credentials"}));
  }

  #[tokio::test]
  async fn an_agent_create_outside_its_own_task_is_forbidden() {
    let (_home, features) = feature_context();
    let body = RequestBody::Json(json!({
      "request_id": "agent-outside",
      "task_parent_id": "foreign-task",
      "profile": "worker",
    }));
    let caller = CallerIdentity::Agent {
      claims: RunTokenClaims {
        session_id: "agent-task".to_string(),
        run_id: "agent-run".to_string(),
        agent: "synthetic".to_string(),
      },
    };

    let response = create(&features, &caller, &body).await.unwrap_err();
    let (status, detail) = answer(response).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(detail, json!({"detail": AGENT_CREATE_SCOPE_REFUSAL}));
  }
}
