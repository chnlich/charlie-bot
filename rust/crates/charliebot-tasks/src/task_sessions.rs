//! Task run creation helpers (`src/runtime/task_sessions.py`).

use charliebot_core::config::CharlieBotConfig;
use charliebot_store::run_records::{RetryRunFields, RunKind, RunRecord, run_not_found_in_task_text};
use charliebot_store::{Store, StoreError, StoreResult};

/// Create a stable retry record while holding the task tree's control lock.
pub async fn create_retry(
  store: &Store,
  cfg: &CharlieBotConfig,
  session_id: &str,
  request_id: &str,
  original_run_id: &str,
  backend: Option<&str>,
  input_event_ids: Option<Vec<String>>,
) -> StoreResult<RunRecord> {
  let guard = store.control_lock().await;
  let meta = store.load_task_meta(session_id).await?;
  let state = store.task_state(session_id)?;
  if state != "open" {
    return Err(StoreError::conflict(vec![format!("task {session_id} is {state}; only open tasks accept retries")]));
  }
  let original = store
    .read_run_sync(session_id, original_run_id)?
    .ok_or_else(|| StoreError::not_found(run_not_found_in_task_text(original_run_id, session_id)))?;
  let (backend_id, model) = match backend {
    Some(backend) => {
      let option = cfg
        .get_backend_option(backend)
        .ok_or_else(|| StoreError::invalid(format!("retry backend '{backend}' is not in backends.options")))?;
      if option.model.is_empty() {
        return Err(StoreError::invalid(format!("retry '{}' has no default model", option.id)));
      }
      (Some(option.id.clone()), Some(option.model.clone()))
    }
    None => (original.backend.clone(), original.model.clone()),
  };
  let input_event_ids = input_event_ids.unwrap_or_else(|| {
    if original.kind == RunKind::ManagerTurn { original.input_event_ids.clone() } else { Vec::new() }
  });
  let task_spec_text = meta
    .task
    .as_ref()
    .map(serde_json::to_string)
    .transpose()
    .map_err(|error| StoreError::new(format!("serializing task spec for retry: {error}")))?;
  store
    .create_retry_run_locked(
      &guard,
      session_id,
      request_id,
      original_run_id,
      task_spec_text.as_deref(),
      RetryRunFields {
        kind: original.kind,
        backend: backend_id,
        model,
        repo_path: original.repo_path,
        base_branch: original.base_branch,
        branch_name: original.branch_name,
        worktree_path: original.worktree_path,
        sequence_ref: original.sequence_ref,
        review_of_run_id: original.review_of_run_id,
        input_event_ids,
      },
    )
    .await
}
