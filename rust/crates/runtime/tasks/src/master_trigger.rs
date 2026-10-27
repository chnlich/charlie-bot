//! Resolve a task's successor chain and dispatch a wake to its active tail.

use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::SessionStatus;
use charliebot_store::{StoreError, StoreResult};
use serde_json::{Map, Value};

use crate::dispatch::{self, Dispatcher};

/// The input to a master trigger, with the Python entry point's defaults.
pub struct TriggerMasterRequest<'a> {
  pub session_id: &'a str,
  pub summary: &'a str,
  pub input_id: Option<&'a str>,
  pub event_type: &'a str,
  pub actor: &'a str,
  pub from_session: Option<&'a str>,
  pub from_session_name: Option<&'a str>,
  pub pull_back: bool,
}

impl<'a> TriggerMasterRequest<'a> {
  /// Set the required source session and summary with the Python defaults.
  pub fn new(session_id: &'a str, summary: &'a str) -> TriggerMasterRequest<'a> {
    TriggerMasterRequest {
      session_id,
      summary,
      input_id: None,
      event_type: event_types::AGENT_MESSAGE,
      actor: "agent",
      from_session: None,
      from_session_name: None,
      pull_back: true,
    }
  }
}

/// Resolve a task's successor tail and admit then dispatch one wake there.
pub async fn trigger_master(features: &FeatureContext, request: TriggerMasterRequest<'_>) -> StoreResult<()> {
  let Some(resolved) = features.store.resolve_successor_chain(request.session_id)?.last().cloned() else {
    tracing::error!(target: "charliebot_tasks", session = request.session_id, "trigger_master_session_not_found");
    return Ok(());
  };
  if resolved.id != request.session_id {
    tracing::info!(
      target: "charliebot_tasks",
      session = request.session_id,
      resolved_session = %resolved.id,
      "trigger_master_redirected_to_successor"
    );
  }
  if resolved.status == SessionStatus::Archived && resolved.successor_session_id.is_none() {
    if !request.pull_back {
      tracing::info!(
        target: "charliebot_tasks",
        session = request.session_id,
        resolved_session = %resolved.id,
        "wake_skipped_archived"
      );
      return Ok(());
    }
    features.store.unarchive_session(&resolved.id).await?;
    tracing::info!(
      target: "charliebot_tasks",
      session = request.session_id,
      resolved_session = %resolved.id,
      "wake_pulled_back_archived_session"
    );
  }
  let Some(node) = features.store.get_session(&resolved.id).await? else {
    tracing::error!(target: "charliebot_tasks", session = %resolved.id, "trigger_master_session_not_found");
    return Ok(());
  };
  let dispatcher = dispatch::dispatcher(features).map_err(|error| StoreError::new(error.to_string()))?;
  wake_task_node(&features.store, dispatcher.as_ref(), &node.id, request).await
}

async fn wake_task_node(
  store: &charliebot_store::Store,
  dispatcher: &Dispatcher,
  node_id: &str,
  request: TriggerMasterRequest<'_>,
) -> StoreResult<()> {
  let mut from_session = request.from_session;
  let mut from_session_name = request.from_session_name;
  if node_id != request.session_id && from_session.is_none() {
    let predecessor = store
      .get_session(request.session_id)
      .await?
      .ok_or_else(|| StoreError::new(format!("successor chain lost source session {}", request.session_id)))?;
    from_session = Some(&predecessor.id);
    from_session_name = Some(&predecessor.name);
    let fields = input_fields(request.input_id, from_session, from_session_name);
    dispatcher.admit_input(node_id, request.event_type, request.summary, request.actor, fields).await?;
    dispatcher.dispatch_pending(node_id).await?;
    return Ok(());
  }
  let fields = input_fields(request.input_id, from_session, from_session_name);
  dispatcher.admit_input(node_id, request.event_type, request.summary, request.actor, fields).await?;
  dispatcher.dispatch_pending(node_id).await?;
  Ok(())
}

fn input_fields(input_id: Option<&str>, from_session: Option<&str>, from_session_name: Option<&str>) -> Value {
  let mut fields = Map::new();
  if let Some(input_id) = input_id {
    fields.insert("input_id".to_string(), Value::String(input_id.to_string()));
  }
  if let Some(from_session) = from_session {
    fields.insert("from_session".to_string(), Value::String(from_session.to_string()));
  }
  if let Some(from_session_name) = from_session_name {
    fields.insert("from_session_name".to_string(), Value::String(from_session_name.to_string()));
  }
  Value::Object(fields)
}
