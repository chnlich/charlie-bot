//! Local proof that one delegation request created its stable task child and work Run.

use std::path::Path;

use charliebot_store::control_events;
use serde_json::{Value, json};

pub(super) fn find_local_task_child(
  home: &Path,
  session_id: &str,
  description: &str,
  task_type: &str,
  request_id: Option<&str>,
) -> Option<Value> {
  let resolved_request_id = request_id
    .filter(|request_id| !request_id.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| control_events::derived_delegate_request_id(session_id, task_type, description));
  let child_id = control_events::stable_task_id(Some(session_id), &resolved_request_id);
  let metadata_path = home.join("sessions").join(&child_id).join("metadata.json");
  let metadata_text = std::fs::read_to_string(metadata_path).ok()?;
  let metadata = serde_json::from_str::<Value>(&metadata_text).ok()?;

  let task = metadata.get("task")?.as_object()?;
  let goal_matches = match task.get("goal") {
    Some(Value::String(goal)) => goal == description,
    None => description.is_empty(),
    Some(_) => false,
  };
  let metadata_matches = metadata.get("id").and_then(Value::as_str) == Some(child_id.as_str())
    && metadata.get("name").and_then(Value::as_str).is_some()
    && metadata.get("task_parent_id").and_then(Value::as_str) == Some(session_id)
    && metadata.get("profile").and_then(Value::as_str) == Some("worker")
    && goal_matches
    && task.get("task_type").and_then(Value::as_str) == Some(task_type);
  if !metadata_matches {
    return None;
  }

  let run_id = control_events::stable_run_id(&child_id, &format!("{resolved_request_id}:work"));
  let run_metadata_path =
    home.join("sessions").join(&child_id).join("data").join("runs").join(&run_id).join("metadata.json");
  let run_id = run_metadata_path.is_file().then_some(run_id);

  Some(json!({
    "session_id": child_id,
    "parent_session_id": session_id,
    "run_id": run_id,
    "thread_id": run_id,
  }))
}
