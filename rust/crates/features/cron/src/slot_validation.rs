//! The validator of the cron metadata slot: it checks the scheduled-task field and each task's schedule entry.

use charliebot_store::metadata::{
  LAST_RUN_STATUS, LAST_SCHEDULED_CRON, LAST_SCHEDULED_RUN, SCHEDULED_TASK, TASK_SCHEDULE_SLOTS,
};
use serde_json::{Map, Value};

pub(crate) fn validate_cron_fields(values: &mut Map<String, Value>) -> Option<String> {
  if values.get(SCHEDULED_TASK).is_some_and(|value| !value.is_null() && !value.is_string()) {
    return Some(format!("{SCHEDULED_TASK} must be a string or null"));
  }
  match values.get(TASK_SCHEDULE_SLOTS) {
    None | Some(Value::Null) => None,
    Some(Value::Object(entries)) => entries.iter().find_map(|(task, entry)| validate_task_entry(task, entry)),
    Some(_) => Some(format!("{TASK_SCHEDULE_SLOTS} must be an object keyed by task name, or null")),
  }
}

/// The reason a task's schedule entry is malformed, or `None` when every key holds a valid value.
fn validate_task_entry(task: &str, entry: &Value) -> Option<String> {
  let Some(entry) = entry.as_object() else {
    return Some(format!("{TASK_SCHEDULE_SLOTS} entry of task {task:?} must be an object"));
  };
  for name in [LAST_SCHEDULED_RUN, LAST_SCHEDULED_CRON] {
    if entry.get(name).is_some_and(|value| !value.is_null() && !value.is_string()) {
      return Some(format!("{name} of task {task:?} must be a string or null"));
    }
  }
  if entry.get(LAST_RUN_STATUS).is_some_and(|value| {
    !value.is_null() && !matches!(value.as_str(), Some("running" | "success" | "failed" | "skipped"))
  }) {
    return Some(format!("{LAST_RUN_STATUS} of task {task:?} must be a LastRunStatus or null"));
  }
  None
}
