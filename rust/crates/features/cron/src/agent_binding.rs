//! The cron binding rule for an agent caller: a cron task binds a session that is the agent's own or sits below it.
//!
//! A manager agent creates a cron task whose new session sits under the agent's session, and rebinds a task to a
//! session in its own subtree. The subtree test is the store's `within_caller_subtree`, which the reparent scope rule
//! uses too. Every refusal answers 403 and names the rule that refused the request.

use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::{CreateTask, StoreError, TaskCaller};
use serde_json::{Map, json};

use crate::config::{self, ScheduledTaskConfig};

/// The refusal for an agent whose session is not a manager: manager sessions create and rebind cron tasks.
pub(crate) const MANAGER_REFUSAL: &str = "an agent may create or rebind a cron task only as a manager session";
/// The refusal for an agent that binds a cron task outside its own subtree, and for a request that clears a binding.
pub(crate) const BINDING_SCOPE_REFUSAL: &str =
  "an agent may bind a cron task only to its own session or a session below it";

/// The create rule: the caller is a manager, and a session the request names is the caller's own or below it.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_create(
  features: &FeatureContext,
  caller_session: &str,
  requested: Option<&str>,
) -> Result<(), Response> {
  require_manager(features, caller_session).await?;
  match requested {
    Some(session_id) => require_within_caller(features, caller_session, session_id).await,
    None => Ok(()),
  }
}

/// The rebind rule: the caller is a manager, and the stored binding and the new one are each the caller's own session
/// or below it. A cleared binding sits outside every subtree, so it is refused.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_rebind(
  features: &FeatureContext,
  caller_session: &str,
  stored: Option<&str>,
  requested: Option<&str>,
) -> Result<(), Response> {
  require_manager(features, caller_session).await?;
  if let Some(stored) = stored {
    require_within_caller(features, caller_session, stored).await?;
  }
  let Some(requested) = requested else {
    return Err(forbidden(BINDING_SCOPE_REFUSAL));
  };
  require_within_caller(features, caller_session, requested).await
}

/// The session that a new cron task binds: a manager child of the caller's session, by the create-subtask rule.
/// The session takes the backend the task fires on, so the scheduler's backend alignment has nothing to change.
#[allow(clippy::result_large_err)]
pub(crate) async fn create_bound_session(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  caller_session: &str,
  task: &ScheduledTaskConfig,
) -> Result<SessionMetadata, Response> {
  let backend = match config::effective_backend(task, cfg) {
    Ok(backend) => backend,
    Err(error) => return Err(responses::internal_error("RuntimeError", error.to_string())),
  };
  features
    .store
    .create_task(
      CreateTask {
        request_id: format!("cron-create:{}", uuid::Uuid::new_v4()),
        task_parent_id: Some(caller_session.to_string()),
        profile: TaskProfile::Manager,
        task: None,
        name: Some(task.name.clone()),
        backend: Some(backend),
        group: task.project.clone(),
        session_id: None,
      },
      TaskCaller::Agent { session_id: caller_session.to_string() },
      Map::new(),
    )
    .await
    .map_err(store_error)
}

#[allow(clippy::result_large_err)]
async fn require_manager(features: &FeatureContext, caller_session: &str) -> Result<(), Response> {
  match features.store.get_session(caller_session).await {
    Ok(Some(meta)) if meta.profile == TaskProfile::Manager => Ok(()),
    Ok(_) => Err(forbidden(MANAGER_REFUSAL)),
    Err(error) => Err(store_error(error)),
  }
}

#[allow(clippy::result_large_err)]
async fn require_within_caller(
  features: &FeatureContext,
  caller_session: &str,
  session_id: &str,
) -> Result<(), Response> {
  let within = match features.store.within_caller_subtree(caller_session, session_id) {
    Ok(within) => within,
    // A session the store does not hold lies outside every subtree.
    Err(error) if error.status_code() == Some(404) => false,
    Err(error) => return Err(store_error(error)),
  };
  if within { Ok(()) } else { Err(forbidden(BINDING_SCOPE_REFUSAL)) }
}

fn forbidden(rule: &str) -> Response {
  responses::fast_json(StatusCode::FORBIDDEN, &json!({"detail": rule}))
}

/// The store error as a JSON response: the status the error carries, else the internal error response.
fn store_error(error: StoreError) -> Response {
  match error.status_code() {
    Some(code) => responses::fast_json(
      StatusCode::from_u16(code).expect("the store reports HTTP status codes"),
      &json!({"detail": error.to_string()}),
    ),
    None => responses::internal_error("RuntimeError", error.to_string()),
  }
}
