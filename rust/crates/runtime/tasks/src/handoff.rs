//! The switch note every automatic retry gets: the absolute paths to the context it continues from, and a
//! best-effort four-part summary of the work the previous run did.
//!
//! The note is assembled once for every retry, whether the retry continues a worker run, follows the pool's quota
//! failover, or switches an Auto session to its next member. The paths are the guarantee: the summary is an addition
//! that a failed assembly reports in the note and in the switch record without blocking the switch.

use std::collections::HashMap;

use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::layout;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::{self, RAW_LOG_NAME, RUN_EVENTS_NAME, RunRecord};
use serde_json::Value;

use crate::session_anchors;
use crate::tool_line::tool_input_first_line;

/// The `quota_failover` summary value of a run whose summary was assembled.
pub const SUMMARY_OK: &str = event_types::QUOTA_SUMMARY_OK;

/// The summary value that opens an unassembled summary; the reason follows after a colon and a space.
pub const SUMMARY_UNAVAILABLE: &str = event_types::QUOTA_SUMMARY_UNAVAILABLE;

const MAX_SUMMARY_LINE_CHARS: usize = 300;

/// Where the previous run's events are read from.
pub enum HandoffSource<'a> {
  /// A manager turn: the events the run wrote to the session's chat log.
  ManagerRun { run: &'a RunRecord },
  /// A worker run: the run's own `events.jsonl`.
  WorkerRun { run: &'a RunRecord },
}

impl HandoffSource<'_> {
  fn run(&self) -> &RunRecord {
    match self {
      HandoffSource::ManagerRun { run } | HandoffSource::WorkerRun { run } => run,
    }
  }
}

/// One retry's switch note and the summary state its `quota_failover` record carries.
pub struct Handoff {
  /// The bracketed note that opens the retry's prompt.
  pub note: String,
  /// [`SUMMARY_OK`], or an [`SUMMARY_UNAVAILABLE`] value with its reason.
  pub summary: String,
}

/// Build the switch note and summary for a retry that continues `source`'s run.
///
/// `reason` names why the retry starts a fresh conversation. The note always names the session chat log, its archives
/// and the failed run's directory as absolute paths. A worker's note and summary name the task's goal; a manager's
/// Goals reach it through its prompt, so its note and summary name none. A summary that cannot be assembled leaves a
/// line in the note and an `unavailable` value for the switch record, and the retry proceeds.
pub fn handoff(
  features: &FeatureContext,
  meta: &SessionMetadata,
  source: HandoffSource<'_>,
  reason: &str,
) -> anyhow::Result<Handoff> {
  let previous = source.run();
  let session_dir = features.store.session_dir(&meta.id);
  let chat_log = layout::chat_events_path(&session_dir);
  let archives = layout::archives_dir(&session_dir);
  let run_dir = features.store.run_dir(&meta.id, &previous.id);
  let context = format!(
    "Earlier turns' history remains readable in the session chat log at {}, its weekly archives at {}, and the failed \
     run's directory at {}, whose {RAW_LOG_NAME} holds each step's raw output.",
    chat_log.display(),
    archives.display(),
    run_dir.display(),
  );
  let (summary_text, summary_value) = match summary(features, meta, &source) {
    Ok(text) => (text, SUMMARY_OK.to_string()),
    Err(reason) => {
      (format!("Handoff summary {SUMMARY_UNAVAILABLE}: {reason}"), format!("{SUMMARY_UNAVAILABLE}: {reason}"))
    }
  };
  let note = match meta.profile {
    TaskProfile::Manager => session_anchors::manager_switch_reset_note(&meta.id, reason, &context),
    TaskProfile::Worker => session_anchors::worker_switch_reset_note(reason, task_goal(meta), &context),
  };
  Ok(Handoff { note: format!("{note}\n\n{summary_text}"), summary: summary_value })
}

/// The task goal a worker's note and summary name: its task record's goal, or its session name when it has no record.
fn task_goal(meta: &SessionMetadata) -> &str {
  meta.task.as_ref().map_or(meta.name.as_str(), |task| task.goal.as_str())
}

/// Assemble the four summary parts, or fail with the reason the previous run's events could not be read.
fn summary(features: &FeatureContext, meta: &SessionMetadata, source: &HandoffSource<'_>) -> Result<String, String> {
  if let Some(reason) = forced_summary_failure(&source.run().id) {
    return Err(reason);
  }
  let events = previous_events(features, meta, source)?;
  let goal = (meta.profile == TaskProfile::Worker).then(|| task_goal(meta));
  Ok(RunWork::collect(&events).render(goal))
}

/// The previous run's events, from the source the run kind records them in.
fn previous_events(
  features: &FeatureContext,
  meta: &SessionMetadata,
  source: &HandoffSource<'_>,
) -> Result<Vec<Value>, String> {
  match source {
    HandoffSource::ManagerRun { run } => {
      if run.input_event_ids.is_empty() {
        return Err(format!("run {} recorded no input events", run.id));
      }
      let events = features
        .store
        .load_chat_events_sync(&meta.id)
        .map_err(|error| format!("reading the session chat log: {error}"))?;
      let start = events
        .iter()
        .rposition(|event| {
          event
            .get("id")
            .and_then(CompactValue::as_str)
            .is_some_and(|id| run.input_event_ids.iter().any(|input| input == id))
        })
        .ok_or_else(|| format!("run {}'s input events are not in the session chat log", run.id))?;
      Ok(events[start + 1..].iter().map(|event| event.to_value()).collect())
    }
    HandoffSource::WorkerRun { run } => {
      let path = features.store.run_dir(&meta.id, &run.id).join(RUN_EVENTS_NAME);
      run_records::read_event_values(&path)
        .map_err(|error| format!("reading the run's events log {}: {error}", path.display()))
    }
  }
}

/// The four summary parts gathered from one run's events.
#[derive(Default)]
struct RunWork {
  steps: Vec<String>,
  completed: Vec<bool>,
  /// The call index each tool-use id names, so a result marks the call it answers.
  targets: HashMap<String, usize>,
  last_text: Option<String>,
  files: Vec<String>,
}

impl RunWork {
  fn collect(events: &[Value]) -> Self {
    let mut work = Self::default();
    for event in events {
      match event.get("type").and_then(Value::as_str) {
        Some(event_types::ASSISTANT) => work.assistant(event),
        Some(event_types::TOOL_USE) => work.tool_use(event),
        Some(event_types::USER) => work.tool_results(event),
        Some(event_types::TOOL_RESULT) => work.tool_result(event),
        _ => {}
      }
    }
    work
  }

  fn assistant(&mut self, event: &Value) {
    let Some(blocks) = event.pointer(event_types::MESSAGE_CONTENT_POINTER).and_then(Value::as_array).map(Vec::as_slice)
    else {
      return;
    };
    for block in blocks {
      match block.get("type").and_then(Value::as_str) {
        Some("text") => {
          if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.trim().is_empty()) {
            self.last_text = Some(text.trim().to_string());
          }
        }
        Some(event_types::TOOL_USE) => self.add_call(block),
        _ => {}
      }
    }
  }

  fn tool_use(&mut self, event: &Value) {
    // A standalone tool line names its tool and input; its result pairs by order, not by id.
    let call = serde_json::json!({
      "name": event.get("name").or_else(|| event.get(event_types::TOOL_NAME)).cloned().unwrap_or(Value::Null),
      "input": event.get("input").cloned().unwrap_or(Value::Null),
    });
    self.add_call(&call);
  }

  fn add_call(&mut self, block: &Value) {
    let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
    let index = self.steps.len();
    self.steps.push(format!("$ {name} {}", tool_input_first_line(block)));
    self.completed.push(false);
    if let Some(id) = block.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
      self.targets.insert(id.to_string(), index);
    }
    collect_files(block.get("input").unwrap_or(&Value::Null), &mut self.files);
  }

  fn tool_results(&mut self, event: &Value) {
    let Some(blocks) = event.pointer(event_types::MESSAGE_CONTENT_POINTER).and_then(Value::as_array).map(Vec::as_slice)
    else {
      return;
    };
    for block in blocks {
      if block.get("type").and_then(Value::as_str) == Some("tool_result") {
        self.complete(block.get(event_types::TOOL_USE_ID).and_then(Value::as_str));
      }
    }
  }

  fn tool_result(&mut self, event: &Value) {
    self.complete(event.get(event_types::TOOL_USE_ID).and_then(Value::as_str));
  }

  /// Mark the call `id` answers complete; a result that names no id completes the first open call.
  fn complete(&mut self, id: Option<&str>) {
    let index = match id {
      Some(id) => self.targets.get(id).copied(),
      None => self.completed.iter().position(|completed| !completed),
    };
    if let Some(index) = index {
      self.completed[index] = true;
    }
  }

  /// The four summary parts. The goal line is present when `goal` is.
  fn render(&self, goal: Option<&str>) -> String {
    let done = self
      .steps
      .iter()
      .zip(&self.completed)
      .filter(|(_, completed)| **completed)
      .map(|(step, _)| format!("- {}", truncate(step)))
      .collect::<Vec<_>>();
    let in_progress = self.steps.iter().zip(&self.completed).rev().find(|(_, completed)| !**completed).map_or_else(
      || self.last_text.as_deref().map_or_else(|| "none".to_string(), truncate),
      |(step, _)| format!("{} (no result yet)", truncate(step)),
    );
    let files = if self.files.is_empty() { "none".to_string() } else { self.files.join(", ") };
    let goal_line = goal.map(|goal| format!("goal: {}\n", truncate(goal))).unwrap_or_default();
    let mut text = format!("Handoff summary:\n{goal_line}done ({}):", done.len());
    if done.is_empty() {
      text.push_str(" none");
    }
    for step in done {
      text.push('\n');
      text.push_str(&step);
    }
    text.push_str(&format!("\nin progress: {in_progress}\nfiles: {files}"));
    text
  }
}

/// Collect the paths a tool input names: a singular file field or a list of paths.
fn collect_files(input: &Value, files: &mut Vec<String>) {
  let Some(object) = input.as_object() else { return };
  for key in [event_types::TOOL_INPUT_FILE_PATH, "path", "notebook_path"] {
    if let Some(path) = object.get(key).and_then(Value::as_str).filter(|path| !path.is_empty()) {
      push_once(files, path);
    }
  }
  for key in ["files", "paths", "file_paths"] {
    if let Some(paths) = object.get(key).and_then(Value::as_array) {
      for path in paths.iter().filter_map(Value::as_str).filter(|path| !path.is_empty()) {
        push_once(files, path);
      }
    }
  }
}

fn push_once(files: &mut Vec<String>, path: &str) {
  if !files.iter().any(|known| known == path) {
    files.push(path.to_string());
  }
}

fn truncate(text: &str) -> String {
  let line = text.lines().next().unwrap_or_default();
  if line.chars().count() <= MAX_SUMMARY_LINE_CHARS {
    line.to_string()
  } else {
    format!("{}…", line.chars().take(MAX_SUMMARY_LINE_CHARS).collect::<String>())
  }
}

/// The summary failures a test forces, keyed by the run whose summary it breaks.
#[cfg(any(test, feature = "test-hooks"))]
static FORCED_SUMMARY_FAILURES: std::sync::LazyLock<std::sync::Mutex<HashMap<String, String>>> =
  std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Force the summary assembly of `run_id` to fail with `reason`, as a test of the unavailable path needs.
#[cfg(any(test, feature = "test-hooks"))]
pub fn force_summary_failure_for_test(run_id: &str, reason: &str) {
  FORCED_SUMMARY_FAILURES
    .lock()
    .expect("the forced summary failure map is not poisoned")
    .insert(run_id.to_string(), reason.to_string());
}

/// Drop a forced summary failure once the test that set it no longer runs the assembly.
#[cfg(any(test, feature = "test-hooks"))]
pub fn clear_summary_failure_for_test(run_id: &str) {
  FORCED_SUMMARY_FAILURES.lock().expect("the forced summary failure map is not poisoned").remove(run_id);
}

/// The reason a test forced for this run, which every assembly of the run's summary reports.
#[cfg(any(test, feature = "test-hooks"))]
fn forced_summary_failure(run_id: &str) -> Option<String> {
  FORCED_SUMMARY_FAILURES.lock().expect("the forced summary failure map is not poisoned").get(run_id).cloned()
}

#[cfg(not(any(test, feature = "test-hooks")))]
fn forced_summary_failure(_run_id: &str) -> Option<String> {
  None
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::RunWork;

  #[test]
  fn the_summary_pairs_standalone_tool_lines_by_order() {
    let events = vec![
      json!({"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}),
      json!({"type": "tool_result", "tool_name": "Bash", "content": "ok"}),
      json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/synthetic/a.rs"}}),
    ];

    let text = RunWork::collect(&events).render(Some("fix the failing test"));

    assert_eq!(
      text,
      "Handoff summary:\ngoal: fix the failing test\ndone (1):\n- $ Bash cargo test\nin progress: $ Read \
       {\"file_path\": \"/synthetic/a.rs\"} (no result yet)\nfiles: /synthetic/a.rs"
    );
  }

  #[test]
  fn the_summary_without_a_goal_names_none() {
    let events = vec![json!({"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}})];

    let text = RunWork::collect(&events).render(None);

    assert!(text.starts_with("Handoff summary:\ndone (0):"), "{text}");
    assert!(!text.contains("goal:"), "{text}");
  }

  #[test]
  fn the_summary_pairs_assistant_blocks_by_tool_use_id() {
    let events = vec![
      json!({"type": "assistant", "message": {"content": [
        {"type": "text", "text": "Two reads."},
        {"type": "tool_use", "id": "call-1", "name": "Read", "input": {"file_path": "/synthetic/a.rs"}},
        {"type": "tool_use", "id": "call-2", "name": "Read", "input": {"file_path": "/synthetic/b.rs"}},
      ]}}),
      json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "call-2", "content": "b"},
      ]}}),
    ];

    let text = RunWork::collect(&events).render(Some("read both"));

    assert!(text.contains("done (1):\n- $ Read {\"file_path\": \"/synthetic/b.rs\"}"), "{text}");
    assert!(text.contains("in progress: $ Read {\"file_path\": \"/synthetic/a.rs\"} (no result yet)"), "{text}");
    assert!(text.contains("files: /synthetic/a.rs, /synthetic/b.rs"), "{text}");
  }
}
