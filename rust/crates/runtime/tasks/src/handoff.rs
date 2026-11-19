//! The switch note an automatic switch gets: the absolute paths to the context it continues from, and a best-effort
//! four-part summary of the work the previous run did.
//!
//! The note is assembled once for every switch: a retry (a worker's, or a pool's mid-turn quota failover to its next
//! member) and a manager turn that starts on the member its pool switched to. The paths are the guarantee: the summary
//! is an addition that a failed assembly reports in the note and in the switch record without blocking the switch.
//!
//! A turn-start switch persists its note in the run's `handoff_note.md`. The file first holds the switch's
//! `quota_failover` event as the pending intent, and the finished note replaces it before the event's summary is
//! recorded, so a launch that finds the intent completes the note before it starts.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use charliebot_agents::backend_availability;
use charliebot_core::event_types;
use charliebot_core::fsutil;
use charliebot_registry::context::FeatureContext;
use charliebot_store::compact::CompactValue;
use charliebot_store::layout;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::{self, RAW_LOG_NAME, RUN_EVENTS_NAME, RunRecord};
use charliebot_store::session_fork::HISTORY_LOCATION_NOTE;
use serde_json::Value;

use crate::prompts::HANDOFF_NOTE_FILENAME;
use crate::session_anchors;
use crate::tool_line::tool_input_first_line;

/// The `quota_failover` summary value of a run whose summary was assembled.
pub const SUMMARY_OK: &str = event_types::QUOTA_SUMMARY_OK;

/// The summary value that opens an unassembled summary; the reason follows after a colon and a space.
pub const SUMMARY_UNAVAILABLE: &str = event_types::QUOTA_SUMMARY_UNAVAILABLE;

/// The character cap on one summary line: the goal, a step, or the in-progress step. At four bytes a character at most,
/// a capped line stays far below [`MAX_SUMMARY_PART_BYTES`].
const MAX_SUMMARY_LINE_CHARS: usize = 300;

/// The byte bound on each part of the summary: the goal line, the done list, the in-progress line and the files list.
///
/// The summary sits in the launch prompt, and the kernel caps one argv string at 128 KiB (`MAX_ARG_STRLEN`), failing a
/// longer one with E2BIG. At 8 KiB a part, the four parts total at most 32 KiB, a quarter of that cap. A part over the
/// bound keeps its newest entries, because the next run needs the latest steps and the in-progress step first. The
/// part then states how many older entries it left out and names the log that holds the full list.
const MAX_SUMMARY_PART_BYTES: usize = 8 * 1024;

/// Where the previous run's events are read from.
pub enum HandoffSource<'a> {
  /// A manager turn that failed mid-turn: the events the run wrote to the session's chat log.
  ManagerRun { run: &'a RunRecord },
  /// A manager turn that starts on the member its pool switched to: the events the previous launched turn wrote to the
  /// session's chat log.
  ManagerTurnStart { run: &'a RunRecord },
  /// A worker run: the run's own `events.jsonl`.
  WorkerRun { run: &'a RunRecord },
}

impl HandoffSource<'_> {
  fn run(&self) -> &RunRecord {
    match self {
      HandoffSource::ManagerRun { run }
      | HandoffSource::ManagerTurnStart { run }
      | HandoffSource::WorkerRun { run } => run,
    }
  }

  /// The log the previous run's events are read from, which a cut summary part names.
  fn full_list_log(&self) -> FullListLog {
    match self {
      HandoffSource::ManagerRun { .. } | HandoffSource::ManagerTurnStart { .. } => FullListLog::SessionChat,
      HandoffSource::WorkerRun { .. } => FullListLog::RunEvents,
    }
  }
}

/// The log that holds a run's full step and file lists. A cut summary part names it, so the next run knows where the
/// omitted entries are.
#[derive(Clone, Copy)]
enum FullListLog {
  /// A worker run's events log.
  RunEvents,
  /// A manager turn's session chat log.
  SessionChat,
}

impl FullListLog {
  fn name(self) -> &'static str {
    match self {
      FullListLog::RunEvents => "the run's events log",
      FullListLog::SessionChat => "the session chat log",
    }
  }
}

/// One switch's note and the summary state its `quota_failover` record carries.
pub struct Handoff {
  /// The bracketed note that opens the run's prompt.
  pub note: String,
  /// [`SUMMARY_OK`], or an [`SUMMARY_UNAVAILABLE`] value with its reason.
  pub summary: String,
}

/// Build the switch note and summary for a run that continues `source`'s run.
///
/// `reason` names why the run starts a fresh conversation. The note always names the session chat log, its archives
/// and the source run's directory as absolute paths: "the failed run's directory" for a retry, and "the previous run's
/// directory" for a turn-start switch. A worker's note and summary name the task's goal; a manager's Goals reach it
/// through its prompt, so its note and summary name none. A summary that cannot be assembled leaves a line in the note
/// and an `unavailable` value for the switch record, and the run proceeds.
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
  let (lead, run_label) = match source {
    HandoffSource::ManagerTurnStart { .. } => (
      format!("{HISTORY_LOCATION_NOTE} Its absolute paths are the session chat log at"),
      "the previous run's directory",
    ),
    HandoffSource::ManagerRun { .. } | HandoffSource::WorkerRun { .. } => {
      ("Earlier turns' history remains readable in the session chat log at".to_string(), "the failed run's directory")
    }
  };
  let context = format!(
    "{lead} {}, its weekly archives at {}, and {run_label} at {}, whose {RAW_LOG_NAME} holds each step's raw output.",
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

/// The handoff of a manager turn-start switch. `intent` is the switch's `quota_failover` event, and `previous` is the
/// last manager turn that launched on the session before the switch, or `None` when no such turn launched.
pub fn turn_start(
  features: &FeatureContext,
  meta: &SessionMetadata,
  intent: &Value,
  previous: Option<&RunRecord>,
) -> anyhow::Result<Handoff> {
  let recovers_at = intent_text(intent, event_types::QUOTA_FAILOVER_RECOVERS_AT)?;
  let reason = turn_start_reason(
    intent_text(intent, event_types::QUOTA_FAILOVER_SET)?,
    intent_text(intent, "from")?,
    intent_text(intent, "to")?,
    intent_text(intent, event_types::QUOTA_FAILOVER_REASON)?,
    (!recovers_at.is_empty()).then_some(recovers_at),
  );
  match previous {
    Some(previous) => handoff(features, meta, HandoffSource::ManagerTurnStart { run: previous }, &reason),
    None => {
      let why = "no manager turn launched on the session before this switch";
      Ok(Handoff {
        note: format!(
          "{}\n\nHandoff summary {SUMMARY_UNAVAILABLE}: {why}",
          session_anchors::manager_context_reset_note(&meta.id, &reason)
        ),
        summary: format!("{SUMMARY_UNAVAILABLE}: {why}"),
      })
    }
  }
}

/// The reason clause a turn-start switch's note opens with: the pool, the member the session left, the member it runs
/// on, and why the member left.
pub fn turn_start_reason(pool: &str, from: &str, to: &str, reason: &str, recovers_at: Option<&str>) -> String {
  let switch = format!("pool '{pool}' switched from {from} to {to}");
  match (reason, recovers_at) {
    (event_types::QUOTA_REASON_SUBSCRIPTION_RESET, _) => format!("{switch}: subscription quota reset"),
    (event_types::QUOTA_REASON_NEAR_EXHAUSTION, None) => format!("{switch}: the member is near quota exhaustion"),
    (event_types::QUOTA_REASON_NEAR_EXHAUSTION, Some(at)) => {
      format!("{switch}: the member is near quota exhaustion, resets at {at}")
    }
    _ => {
      let cause = backend_availability::switch_cause(reason);
      match recovers_at {
        Some(at) => format!("{switch}: {cause}, {} {at}", backend_availability::switch_recovery_label(reason)),
        None => format!("{switch}: {cause}"),
      }
    }
  }
}

/// A text field of a switch intent. The intent is written by this module's caller, so a missing field is a fault.
fn intent_text<'a>(intent: &'a Value, key: &str) -> anyhow::Result<&'a str> {
  intent.get(key).and_then(Value::as_str).with_context(|| format!("the switch intent names no {key}: {intent}"))
}

/// What a manager run's handoff note file holds: a turn-start switch's pending intent, or its finished note.
pub enum HandoffNote {
  /// The switch's `quota_failover` event, written before its note was assembled.
  Pending(Value),
  /// The finished note, which the launch places ahead of the prompt.
  Finished(String),
}

/// Persist a turn-start switch's pending intent in its run's directory, before the switch's note is assembled.
pub fn write_pending_switch(run_dir: &Path, intent: &Value) -> anyhow::Result<()> {
  write_note_file(run_dir, &intent.to_string())
}

/// Persist a turn-start switch's finished note in its run's directory, before the switch's summary is recorded.
pub fn write_finished_note(run_dir: &Path, note: &str) -> anyhow::Result<()> {
  write_note_file(run_dir, note)
}

fn write_note_file(run_dir: &Path, text: &str) -> anyhow::Result<()> {
  let path = run_dir.join(HANDOFF_NOTE_FILENAME);
  fsutil::atomic_write_text_synced(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// Read a run's handoff note file, or `None` when the run carries no turn-start switch. A pending intent is a JSON
/// object, and a finished note opens with its context-reset line, so the first character tells them apart.
pub fn read_note(run_dir: &Path) -> anyhow::Result<Option<HandoffNote>> {
  let path = run_dir.join(HANDOFF_NOTE_FILENAME);
  let text = match std::fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
  };
  if text.starts_with('{') {
    let intent = serde_json::from_str(&text)
      .with_context(|| format!("parsing the pending switch intent in {}", path.display()))?;
    return Ok(Some(HandoffNote::Pending(intent)));
  }
  Ok(Some(HandoffNote::Finished(text)))
}

/// Assemble the four summary parts, or fail with the reason the previous run's events could not be read.
fn summary(features: &FeatureContext, meta: &SessionMetadata, source: &HandoffSource<'_>) -> Result<String, String> {
  if let Some(reason) = forced_summary_failure(&source.run().id) {
    return Err(reason);
  }
  let events = previous_events(features, meta, source)?;
  let goal = (meta.profile == TaskProfile::Worker).then(|| task_goal(meta));
  Ok(RunWork::collect(&events).located_in(source.full_list_log()).render(goal))
}

/// The previous run's events, from the source the run kind records them in.
fn previous_events(
  features: &FeatureContext,
  meta: &SessionMetadata,
  source: &HandoffSource<'_>,
) -> Result<Vec<Value>, String> {
  match source {
    HandoffSource::ManagerRun { run } | HandoffSource::ManagerTurnStart { run } => {
      if run.input_event_ids.is_empty() {
        return Err(format!("run {} recorded no input events", run.id));
      }
      let events = features
        .store
        .load_chat_events_shared_sync(&meta.id)
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
  /// The log a cut part names as holding the full lists. [`RunWork::located_in`] sets it.
  full_list_log: Option<FullListLog>,
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

  /// Name the log that holds the full lists, which a list part cut to the bound points to.
  fn located_in(mut self, log: FullListLog) -> Self {
    self.full_list_log = Some(log);
    self
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
      if block.get("type").and_then(Value::as_str) == Some(event_types::TOOL_RESULT) {
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

  /// The four summary parts. The goal line is present when `goal` is. A list part over
  /// [`MAX_SUMMARY_PART_BYTES`] keeps its newest entries and states how many older ones it left out.
  fn render(&self, goal: Option<&str>) -> String {
    let goal_line = goal.map(|goal| format!("goal: {}\n", truncate(goal))).unwrap_or_default();
    let in_progress = self.steps.iter().zip(&self.completed).rev().find(|(_, completed)| !**completed).map_or_else(
      || self.last_text.as_deref().map_or_else(|| "none".to_string(), truncate),
      |(step, _)| format!("{} (no result yet)", truncate(step)),
    );
    format!("Handoff summary:\n{goal_line}{}\nin progress: {in_progress}\n{}", self.done_part(), self.files_part())
  }

  /// The done part: one `- ` line per finished step, oldest first.
  fn done_part(&self) -> String {
    let lines = self
      .steps
      .iter()
      .zip(&self.completed)
      .filter(|(_, completed)| **completed)
      .map(|(step, _)| format!("- {}", truncate(step)))
      .collect::<Vec<_>>();
    let whole = if lines.is_empty() {
      "done (0): none".to_string()
    } else {
      format!("done ({}):\n{}", lines.len(), lines.join("\n"))
    };
    if whole.len() <= MAX_SUMMARY_PART_BYTES {
      return whole;
    }
    // The label is sized at its widest count, so the kept lines fit beside any count it states.
    let label_width = self.cut_done_label(lines.len(), lines.len()).len();
    let oldest = oldest_kept(&lines, MAX_SUMMARY_PART_BYTES - label_width, 1);
    let label = self.cut_done_label(lines.len(), oldest);
    if oldest == lines.len() { label } else { format!("{label}\n{}", lines[oldest..].join("\n")) }
  }

  /// The files part: the touched paths in first-touch order, or `none`.
  fn files_part(&self) -> String {
    if self.files.is_empty() {
      return "files: none".to_string();
    }
    let whole = format!("files: {}", self.files.join(", "));
    if whole.len() <= MAX_SUMMARY_PART_BYTES {
      return whole;
    }
    // Each path is counted with its ", " separator. The oldest kept path has none, so the count runs two bytes high
    // and the part still stays within the bound.
    let label_width = self.cut_files_label(self.files.len(), self.files.len()).len();
    let oldest = oldest_kept(&self.files, MAX_SUMMARY_PART_BYTES - label_width, 2);
    let label = self.cut_files_label(self.files.len(), oldest);
    if oldest == self.files.len() { label } else { format!("{label} {}", self.files[oldest..].join(", ")) }
  }

  /// The done label of a cut part: the finished-step count, the older steps it left out, and the full list's log.
  fn cut_done_label(&self, total: usize, omitted: usize) -> String {
    format!("done ({total}; {omitted} earlier steps omitted; the full list is in {}):", self.log_name())
  }

  /// The files label of a cut part: the touched-file count, the older files it left out, and the full list's log.
  fn cut_files_label(&self, total: usize, omitted: usize) -> String {
    format!("files ({total}; {omitted} earlier files omitted; the full list is in {}):", self.log_name())
  }

  /// The name of the log that holds the full lists. Only a cut part reads it.
  fn log_name(&self) -> &'static str {
    self.full_list_log.expect("a summary that cuts a list part names its full list with located_in").name()
  }
}

/// The index of the oldest entry a list part keeps: the newest entries whose bytes, each with `separator` bytes before
/// it, fit `budget`. `entries` are oldest first.
fn oldest_kept(entries: &[String], budget: usize, separator: usize) -> usize {
  let mut used = 0;
  let mut oldest = entries.len();
  for entry in entries.iter().rev() {
    used += separator + entry.len();
    if used > budget {
      break;
    }
    oldest -= 1;
  }
  oldest
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

  use super::{FullListLog, MAX_SUMMARY_PART_BYTES, RunWork};

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

  #[test]
  fn a_900_command_run_summary_keeps_its_newest_steps_within_the_part_bound() {
    let mut events = Vec::new();
    for index in 0..900 {
      let id = format!("call-{index}");
      // Each command runs past the line cap and touches its own file, so both list parts exceed the bound.
      let command = format!("cargo test --package synthetic-{index:04} -- {}", "x".repeat(320));
      let path = format!("/synthetic/crates/module_{index:04}/src/lib.rs");
      events.push(json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command, "file_path": path}},
      ]}}));
      events.push(json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id, "content": "ok"},
      ]}}));
    }
    let in_progress = "cargo clippy --package synthetic-in-progress";
    events.push(json!({"type": "assistant", "message": {"content": [
      {"type": "tool_use", "id": "call-900", "name": "Bash", "input": {"command": in_progress}},
    ]}}));

    let text = RunWork::collect(&events).located_in(FullListLog::RunEvents).render(Some("synthetic goal"));

    assert!(text.len() <= 4 * MAX_SUMMARY_PART_BYTES, "{text}");
    let done_part = &text[text.find("done (").unwrap()..text.find("\nin progress: ").unwrap()];
    let files_part = &text[text.rfind("\nfiles").unwrap() + 1..];
    assert!(done_part.len() <= MAX_SUMMARY_PART_BYTES, "{done_part}");
    assert!(files_part.len() <= MAX_SUMMARY_PART_BYTES, "{files_part}");

    let kept_steps = done_part.matches("- $ Bash cargo test --package synthetic-").count();
    let omitted_steps = 900 - kept_steps;
    assert!(kept_steps > 0 && omitted_steps > 0, "{done_part}");
    let done_label = done_part.lines().next().unwrap();
    assert!(states_number(done_label, 900) && states_number(done_label, omitted_steps), "{done_label}");
    assert!(done_label.contains(FullListLog::RunEvents.name()), "{done_label}");
    assert!(done_part.contains("synthetic-0899"), "the newest finished step stays: {done_part}");
    assert!(!text.contains("synthetic-0000"), "the oldest finished step is cut: {text}");
    assert!(
      text.contains("in progress: $ Bash cargo clippy --package synthetic-in-progress (no result yet)"),
      "{text}"
    );

    let kept_files = files_part.matches("/synthetic/crates/module_").count();
    let omitted_files = 900 - kept_files;
    assert!(kept_files > 0 && omitted_files > 0, "{files_part}");
    assert!(states_number(files_part, 900) && states_number(files_part, omitted_files), "{files_part}");
    assert!(files_part.contains(FullListLog::RunEvents.name()), "{files_part}");
    assert!(files_part.contains("module_0899"), "the newest touched file stays: {files_part}");
    assert!(!files_part.contains("module_0000"), "the oldest touched file is cut: {files_part}");
  }

  /// Whether `text` states `number` as a whole number, so `90` does not match `900`.
  fn states_number(text: &str, number: usize) -> bool {
    text.split(|c: char| !c.is_ascii_digit()).any(|token| token == number.to_string())
  }
}
