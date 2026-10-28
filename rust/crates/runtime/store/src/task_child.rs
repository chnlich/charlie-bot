//! Local proof that one task request created its stable task child and work Run.

use std::path::Path;

use charliebot_core::config::sessions_dir_in;
use serde_json::{Value, json};

use crate::control_events;
use crate::layout;
use crate::run_records::RUN_METADATA_NAME;

/// The v2 task-tree child that a request's stable identity binds to.
///
/// The sent-but-lost readback of `delegate` and `improve` binds to the operation's request id,
/// not to a description scan: the child's id derives from `(parent, request id)`, so two
/// intentional identical-spec siblings can never report each other's result. The candidate must
/// still be that child: the parent, worker profile, spec body and task type all match, or the
/// readback returns `None` (outcome unknown), never a fallback to an unrelated sibling.
///
/// `task_type` is `None` for a child created without a task type (the improve loop's worker
/// child), which matches a task record whose `task_type` is absent or null.
pub fn find_local_task_child(
  home: &Path,
  session_id: &str,
  description: &str,
  task_type: Option<&str>,
  request_id: Option<&str>,
) -> Option<Value> {
  let resolved_request_id =
    request_id.filter(|request_id| !request_id.is_empty()).map(str::to_string).unwrap_or_else(|| {
      control_events::derived_delegate_request_id(
        session_id,
        task_type.expect("a derived request id requires a delegation task type"),
        description,
      )
    });
  let child_id = control_events::stable_task_id(Some(session_id), &resolved_request_id);
  let session_dir = sessions_dir_in(home).join(&child_id);
  let metadata_path = layout::metadata_path(&session_dir);
  let metadata_text = std::fs::read_to_string(metadata_path).ok()?;
  let metadata = serde_json::from_str::<Value>(&metadata_text).ok()?;

  let task = metadata.get("task")?.as_object()?;
  let goal_matches = match task.get("goal") {
    Some(Value::String(goal)) => goal == description,
    None => description.is_empty(),
    Some(_) => false,
  };
  let recorded_task_type = match task.get("task_type") {
    None | Some(Value::Null) => None,
    Some(Value::String(task_type)) => Some(task_type.as_str()),
    Some(_) => return None,
  };
  let metadata_matches = metadata.get("id").and_then(Value::as_str) == Some(child_id.as_str())
    && metadata.get("name").and_then(Value::as_str).is_some()
    && metadata.get("task_parent_id").and_then(Value::as_str) == Some(session_id)
    && metadata.get("profile").and_then(Value::as_str) == Some("worker")
    && goal_matches
    && recorded_task_type == task_type;
  if !metadata_matches {
    return None;
  }

  // The authoritative Run is this operation's own work Run: the same stable
  // `(child, "<request id>:work")` binding the server launched — not "whatever run directory
  // sorts first" (a review Run of the same child is a different operation).
  let run_id = control_events::stable_run_id(&child_id, &format!("{resolved_request_id}:work"));
  let run_metadata_path = layout::runs_dir(&session_dir).join(&run_id).join(RUN_METADATA_NAME);
  let run_id = run_metadata_path.is_file().then_some(run_id);

  Some(json!({
    "session_id": child_id,
    "parent_session_id": session_id,
    "run_id": run_id,
    "thread_id": run_id,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use tempfile::TempDir;

  const SESSION_ID: &str = "s1";
  const REQUEST_ID: &str = "improve:7";
  const GOAL: &str = "optimize";

  fn child_id() -> String {
    control_events::stable_task_id(Some(SESSION_ID), REQUEST_ID)
  }

  /// A home holding one child session's metadata; the returned tempdir keeps it alive.
  fn home_with_child(metadata: Value) -> TempDir {
    let home = tempfile::tempdir().expect("a temp home");
    let session_dir = home.path().join("sessions").join(child_id());
    std::fs::create_dir_all(&session_dir).expect("the child session directory");
    std::fs::write(layout::metadata_path(&session_dir), serde_json::to_vec(&metadata).unwrap())
      .expect("the child metadata");
    home
  }

  fn worker_metadata(task: Value) -> Value {
    json!({
      "id": child_id(),
      "name": "Improve loop 7",
      "profile": "worker",
      "task_parent_id": SESSION_ID,
      "task": task,
    })
  }

  fn find(home: &TempDir, task_type: Option<&str>) -> Option<Value> {
    find_local_task_child(home.path(), SESSION_ID, GOAL, task_type, Some(REQUEST_ID))
  }

  #[test]
  fn a_matching_child_is_the_readback_value() {
    let home = home_with_child(worker_metadata(json!({ "goal": GOAL, "task_type": "implement" })));

    let child = find(&home, Some("implement")).expect("the matching child is the readback value");

    assert_eq!(child["session_id"], json!(child_id()));
    assert_eq!(child["parent_session_id"], json!(SESSION_ID));
    // No work Run on disk: the child's existence alone proves the admission.
    assert_eq!(child["run_id"], Value::Null);
    assert_eq!(child["thread_id"], Value::Null);
  }

  #[test]
  fn a_child_of_another_parent_is_not_the_readback_value() {
    let mut metadata = worker_metadata(json!({ "goal": GOAL, "task_type": "implement" }));
    metadata["task_parent_id"] = json!("another-session");
    let home = home_with_child(metadata);

    assert_eq!(find(&home, Some("implement")), None);
  }

  #[test]
  fn a_task_type_mismatch_is_not_the_readback_value() {
    let home = home_with_child(worker_metadata(json!({ "goal": GOAL, "task_type": "quick-edit" })));
    assert_eq!(find(&home, Some("implement")), None, "a differently typed child never answers");

    let typed = home_with_child(worker_metadata(json!({ "goal": GOAL, "task_type": "implement" })));
    assert_eq!(find(&typed, None), None, "a typed child never answers a task-type-less request");

    let malformed = home_with_child(worker_metadata(json!({ "goal": GOAL, "task_type": 7 })));
    assert_eq!(find(&malformed, None), None, "a malformed task type is not an untyped task");
  }

  #[test]
  fn none_matches_a_child_without_a_task_type() {
    let home = home_with_child(worker_metadata(json!({ "goal": GOAL })));

    let child = find(&home, None).expect("a task-type-less child answers a task-type-less request");

    assert_eq!(child["session_id"], json!(child_id()));
  }
}
