//! Session fork and elone (`src/runtime/session_fork.py`). A fork copies one session's chat history into a new manager
//! session. A clone of a manager source is a sibling at the source's task-tree position. An elone replaces a manager
//! source in place: the child takes over the source's decomposition edge and subtree, the source closes archived, and
//! the scheduled bindings of the source move to the child. An elone of a worker source is an ordinary fork, and its
//! source only archives.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use charliebot_core::{event_types, fsutil, timefmt};
use serde_json::json;
use uuid::Uuid;

use crate::control_events::{self, ACTOR_USER};
use crate::metadata::{EventRef, SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
use crate::sidebar_state;
use crate::task_tree::archived_close_event;
use crate::{ControlGuard, SESSION_NOT_FOUND_DETAIL, Store, StoreError, StoreHookFuture, StoreResult, ndjson};

/// The opener of a clone's bootstrap prompt (`session_fork.FORK_BOOTSTRAP_OPENER`).
pub const FORK_BOOTSTRAP_OPENER: &str = "This session continues a prior conversation.";
/// The opener of an elone's bootstrap prompt (`session_fork.ELONE_BOOTSTRAP_OPENER`).
pub const ELONE_BOOTSTRAP_OPENER: &str =
  "You're taking over because the user wasn't satisfied with the previous session.";
/// Where the earlier history stays readable. The fork and elone bootstraps and the context-reset note share it.
pub const HISTORY_LOCATION_NOTE: &str = concat!(
  "Earlier turns' history remains readable in this session's chat log, ",
  "data/chat_events.jsonl in the working directory."
);

/// The bytes that `_REFERENCE_LINE_WS` strips from the edges of a line when it checks for a blank line.
const REFERENCE_LINE_WS: &[u8] = b" \t\r\n\x0b\x0c";

/// The dispatcher's share of an elone: it redelivers the close reports that one node still owes
/// (`TaskTreeManager._redeliver_subtree_close_reports`). The dispatcher implements it, and the elone receives it.
pub trait CloseReportRedelivery: Send + Sync {
  /// Redeliver the close reports `session_id` owes its recipients. The caller holds the control lock through `guard`.
  fn redeliver_close_reports_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
  ) -> StoreHookFuture<'a, ()>;
}

impl Store {
  /// `SessionFork.fork_session`: copy the source's history up to `event_index` (all of it when `None`) into a new
  /// session. A closed parent task refuses the clone of a manager source before anything is written.
  pub async fn fork_session(
    &self,
    parent_id: &str,
    event_index: Option<i64>,
    backend: Option<&str>,
  ) -> StoreResult<SessionMetadata> {
    if let Some(blocker) = self.clone_blocker(parent_id).await? {
      return Err(StoreError::conflict(vec![blocker]));
    }
    let meta = self.spawn_with_history(parent_id, event_index, backend, "C").await?;
    log_spawn("session_cloned", &meta, parent_id, event_index);
    Ok(meta)
  }

  /// `SessionFork.elone_session`: a successor that starts from the source's history up to `event_index`. The source
  /// archives, and its `successor_session_id` names the most recent successor.
  pub async fn elone_session(
    &self,
    parent_id: &str,
    event_index: i64,
    backend: Option<&str>,
    redelivery: &dyn CloseReportRedelivery,
  ) -> StoreResult<SessionMetadata> {
    let fresh_parent = self.read_metadata_fresh(parent_id)?.ok_or_else(session_not_found)?;
    if fresh_parent.profile != TaskProfile::Manager {
      return self.elone_ordinary_session(parent_id, event_index, backend).await;
    }
    let meta = self.run_elone_replacement(&fresh_parent, event_index, backend, redelivery).await?;
    log_spawn("session_eloned", &meta, parent_id, Some(event_index));
    Ok(meta)
  }

  /// The clone refusal for one source: a manager source whose parent task is not open.
  async fn clone_blocker(&self, source_id: &str) -> StoreResult<Option<String>> {
    let Some(source) = self.load_meta(source_id).await? else { return Ok(None) };
    let Some(parent_task) = source.task_parent_id.as_deref() else { return Ok(None) };
    if source.profile != TaskProfile::Manager {
      return Ok(None);
    }
    let state = self.task_state(parent_task)?;
    if state != "open" {
      return Ok(Some(format!("parent task {parent_task} is {state}; the clone writes nothing")));
    }
    Ok(None)
  }

  /// `TaskTreeManager.run_elone_replacement`: replace one manager source with its elone child under the control lock.
  /// The steps run in order, and a failed step names the steps that completed in the log.
  async fn run_elone_replacement(
    &self,
    source: &SessionMetadata,
    event_index: i64,
    backend: Option<&str>,
    redelivery: &dyn CloseReportRedelivery,
  ) -> StoreResult<SessionMetadata> {
    let guard = self.control_lock().await;
    let mut completed = Vec::new();
    let result = self.replace_source(&guard, source, event_index, backend, redelivery, &mut completed).await;
    if let Err(error) = &result {
      tracing::warn!(
        source_id = %source.id,
        completed = ?completed,
        error = %error,
        "elone_replacement_failed"
      );
    }
    result
  }

  async fn replace_source(
    &self,
    guard: &ControlGuard,
    source: &SessionMetadata,
    event_index: i64,
    backend: Option<&str>,
    redelivery: &dyn CloseReportRedelivery,
    completed: &mut Vec<&'static str>,
  ) -> StoreResult<SessionMetadata> {
    let blockers = self.elone_blockers(source)?;
    if !blockers.is_empty() {
      return Err(StoreError::conflict(blockers));
    }
    completed.push("validated the source");
    self.redeliver_subtree_close_reports(guard, &source.id, redelivery).await?;
    completed.push("redelivered the subtree's undelivered close reports");
    let new_meta = self.spawn_with_history(&source.id, Some(event_index), backend, "E").await?;
    completed.push("published the new node");
    self.move_source_children(&source.id, &new_meta.id).await?;
    completed.push("moved the source's child tasks");
    self.close_source_archived(guard, &source.id, &new_meta.id).await?;
    completed.push("closed the source");
    self.archive_source(&source.id, &new_meta.id).await?;
    completed.push("archived the source with its successor pointer");
    self.rebind_elone_bindings_locked(guard, &source.id, &new_meta.id).await?;
    completed.push("rebound the scheduled tasks");
    Ok(new_meta)
  }

  /// `TaskTreeManager._elone_blockers`: an unfinished run on the source, or a closed parent task.
  fn elone_blockers(&self, source: &SessionMetadata) -> StoreResult<Vec<String>> {
    let mut blockers: Vec<String> =
      self.run_blockers(&source.id)?.into_iter().map(|blocker| format!("{}: {}", source.id, blocker.reason)).collect();
    if let Some(parent_task) = source.task_parent_id.as_deref() {
      let state = self.task_state(parent_task)?;
      if state != "open" {
        blockers.push(format!("parent task {parent_task} is {state}"));
      }
    }
    Ok(blockers)
  }

  /// `TaskTreeManager._redeliver_subtree_close_reports`: each node below the source delivers the reports it owes
  /// before the source closes, so no close report reaches a closed node.
  async fn redeliver_subtree_close_reports(
    &self,
    guard: &ControlGuard,
    source_id: &str,
    redelivery: &dyn CloseReportRedelivery,
  ) -> StoreResult<()> {
    self.invalidate_tree_index();
    for descendant_id in self.descendant_ids(source_id)? {
      redelivery.redeliver_close_reports_locked(guard, &descendant_id).await?;
    }
    Ok(())
  }

  /// `TaskTreeManager._move_source_children`: each direct child's decomposition edge points at the elone child.
  async fn move_source_children(&self, source_id: &str, new_id: &str) -> StoreResult<()> {
    self.invalidate_tree_index();
    for child_id in self.child_ids(source_id)? {
      self
        .mutate_fresh(&child_id, false, true, |meta| {
          meta.task_parent_id = Some(new_id.to_string());
          meta.updated_at = UtcDateTime::now();
          Ok(true)
        })
        .await?
        .ok_or_else(|| StoreError::not_found(format!("task {child_id} not found")))?;
    }
    self.invalidate_tree_index();
    Ok(())
  }

  /// `TaskTreeManager._close_source_archived`: close an open source with the archive format, which reports to nobody.
  async fn close_source_archived(&self, guard: &ControlGuard, source_id: &str, new_id: &str) -> StoreResult<()> {
    if self.task_state(source_id)? != "open" {
      return Ok(());
    }
    let request_id = format!("elone-{}", Uuid::new_v4());
    let summary = format!("archived by elone successor {new_id}");
    let event = archived_close_event(source_id, &request_id, ACTOR_USER, &summary, &[], None);
    self.append_control_fact(guard, source_id, event).await?;
    self.invalidate_tree_index();
    Ok(())
  }

  /// `SessionFork._archive_source`: the stored-status archive and the successor pointer, latest takeover wins.
  async fn archive_source(&self, source_id: &str, new_id: &str) -> StoreResult<()> {
    {
      let _session_guard = self.lock_for(source_id).await;
      if let Some(mut fresh_source) = self.get_session(source_id).await? {
        fresh_source.status = SessionStatus::Archived;
        fresh_source.successor_session_id = Some(new_id.to_string());
        fresh_source.updated_at = UtcDateTime::now();
        self.save_metadata_locked(&mut fresh_source, false)?;
      }
    }
    self.drop_session_runtime_state(source_id);
    Ok(())
  }

  /// `SessionFork._elone_ordinary_session`: a worker source's elone is a manager-root child and an archived source.
  async fn elone_ordinary_session(
    &self,
    parent_id: &str,
    event_index: i64,
    backend: Option<&str>,
  ) -> StoreResult<SessionMetadata> {
    let meta = self.spawn_with_history(parent_id, Some(event_index), backend, "E").await?;
    self.archive_source(parent_id, &meta.id).await?;
    log_spawn("session_eloned", &meta, parent_id, Some(event_index));
    Ok(meta)
  }

  /// `SessionFork._spawn_with_history`: create a manager session whose chat log opens with the parent's raw event lines
  /// for `[0, end)`, then the `clone_start` marker, then the child's creation fact.
  async fn spawn_with_history(
    &self,
    parent_id: &str,
    event_index: Option<i64>,
    backend: Option<&str>,
    name_prefix: &str,
  ) -> StoreResult<SessionMetadata> {
    let parent = self.get_session(parent_id).await?.ok_or_else(session_not_found)?;
    let count = self.get_chat_event_count_sync(parent_id)?;
    let end = match event_index {
      None => count,
      Some(index) => {
        if index < 0 || index >= count as i64 {
          return Err(StoreError::invalid(format!(
            "event_index {index} out of range for parent session {parent_id} with {count} events"
          )));
        }
        index as usize + 1
      }
    };

    // A manager source's child keeps the source's tree position: elone replaces the node in place, and clone adds a
    // sibling under the same parent. The task spec is a deep copy, so the two nodes never share one value.
    let holds_position = parent.profile == TaskProfile::Manager;
    let mut meta =
      SessionMetadata::new(Uuid::new_v4().to_string(), format!("{name_prefix}{}", parent.name), TaskProfile::Manager);
    meta.parent_session_id = Some(parent_id.to_string());
    meta.backend = backend.filter(|backend| !backend.is_empty()).unwrap_or(&parent.backend).to_string();
    meta.group = parent.group.clone();
    if holds_position {
      meta.task_parent_id = parent.task_parent_id.clone();
      meta.task = parent.task.clone();
      meta.subtree_prompt_ref = parent.subtree_prompt_ref.clone();
      meta.node_prompt_ref = parent.node_prompt_ref.clone();
    }
    let created_event_id = Uuid::new_v4().to_string();
    let created_event = control_events::build_task_created_event(
      ACTOR_USER,
      &meta.id,
      &format!("history-copy-{}", meta.id),
      meta.task_parent_id.as_deref(),
      None,
      Some(&created_event_id),
      None,
    );
    meta.created_by_event = Some(EventRef { session_id: meta.id.clone(), event_id: Some(created_event_id) });
    let session_dir = self.session_dir(&meta.id);
    fs::create_dir_all(session_dir.join("data"))
      .map_err(|error| StoreError::new(format!("{}: {error}", session_dir.display())))?;

    let clone_event = json!({
      "type": event_types::CLONE_START,
      "parent_session_id": parent_id,
      "parent_session_name": parent.name,
      "timestamp": timefmt::utc_now_iso(),
    });
    // The child log is born whole: the prefix, the marker and the creation fact land through one atomic stream.
    let tail = format!("{}{}", ndjson::encode_line(&clone_event), ndjson::encode_line(&created_event));
    self.write_history_file(&self.chat_events_path(&meta.id), parent_id, end, &tail)?;
    {
      let _session_guard = self.lock_for(&meta.id).await;
      self.save_metadata_locked(&mut meta, false)?;
    }
    self.invalidate_tree_index();
    self.runtime_hooks.copy_on_fork(&self.session_dir(parent_id), &session_dir)?;
    // A contribution's copy lands after the metadata write, so a poll between the two re-probes the copied files.
    sidebar_state::mark_sidebar_dirty(&meta.id);
    self.broadcast_task_tree_changed(&meta.id, Some(event_types::TASK_CREATED)).await?;
    Ok(meta)
  }

  /// `SessionFork._write_history_file_sync`: write the parent's raw lines `[0, end)` and then `tail` into `path`.
  ///
  /// The lines come from the archive files first, then the live log. The archive offset decides the split, and
  /// both sides spend raw lines against the same budget. A corrupt line, or undecodable bytes in a file the copy reads,
  /// fails the write and leaves no child log behind.
  fn write_history_file(&self, path: &Path, parent_id: &str, end: usize, tail: &str) -> StoreResult<()> {
    let archive_take = self.read_archive_offset(parent_id)?.min(end);
    let live_take = end - archive_take;
    let parent = path.parent().expect("a chat log path has a parent directory");
    fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;

    fsutil::atomic_write_stream(path, |out: &mut BufWriter<File>| -> StoreResult<()> {
      let mut raw_left = archive_take;
      let mut archived = 0;
      if raw_left > 0 {
        for source in self.archive_paths(parent_id)?.iter() {
          if raw_left == 0 {
            break;
          }
          let (raw, appended) = copy_reference_file(out, source, raw_left)?;
          raw_left -= raw;
          archived += appended;
        }
      }
      if archived != archive_take {
        return Err(StoreError::invalid(format!(
          "loaded {archived} archived parent events for requested range [0, {archive_take})"
        )));
      }

      let live_path = self.chat_events_path(parent_id);
      let mut live = 0;
      if live_take > 0 && live_path.exists() {
        live = copy_reference_file(out, &live_path, live_take)?.1;
      }
      if live != live_take {
        return Err(StoreError::invalid(format!(
          "loaded {live} live parent events for requested range [0, {live_take})"
        )));
      }
      out.write_all(tail.as_bytes())?;
      Ok(())
    })
  }
}

/// `SessionFork._stream_reference_file`: copy the non-blank lines among the first `take` raw lines of `source` into
/// `out`. Returns `(raw, appended)`: the raw lines spent against the budget, blank lines included, and the lines
/// written. The whole file must decode as UTF-8, including the lines past the budget, so each line is checked as it
/// is read. A non-blank line that is not wrapped in `{}` fails the copy. A CR before the newline is dropped.
fn copy_reference_file(out: &mut impl Write, source: &Path, take: usize) -> StoreResult<(usize, usize)> {
  let file = File::open(source).map_err(|error| StoreError::new(format!("{}: {error}", source.display())))?;
  let mut reader = BufReader::new(file);
  let mut line = Vec::new();
  let mut raw = 0;
  let mut appended = 0;
  loop {
    line.clear();
    let read =
      reader.read_until(b'\n', &mut line).map_err(|error| StoreError::new(format!("{}: {error}", source.display())))?;
    if read == 0 {
      break;
    }
    std::str::from_utf8(&line).map_err(|error| StoreError::invalid(format!("{}: {error}", source.display())))?;
    if raw >= take {
      continue;
    }
    raw += 1;
    let content = line.strip_suffix(b"\n").unwrap_or(&line);
    let Some(first) = content.iter().position(|byte| !REFERENCE_LINE_WS.contains(byte)) else { continue };
    let last = content
      .iter()
      .rposition(|byte| !REFERENCE_LINE_WS.contains(byte))
      .expect("a line with a non-blank byte has a last one");
    if content[first] != b'{' || content[last] != b'}' {
      let snippet: String = String::from_utf8_lossy(content).trim().chars().take(80).collect();
      return Err(StoreError::invalid(format!("parent event line is not a serialized event object: {snippet:?}")));
    }
    let write_end = if content.last() == Some(&b'\r') { content.len() - 1 } else { content.len() };
    out.write_all(&content[..write_end])?;
    out.write_all(b"\n")?;
    appended += 1;
  }
  Ok((raw, appended))
}

fn session_not_found() -> StoreError {
  StoreError::not_found(SESSION_NOT_FOUND_DETAIL)
}

/// `SessionFork._log_spawn`: one log shape for both spawn flows, distinguished by the event name.
fn log_spawn(event: &'static str, meta: &SessionMetadata, parent_id: &str, event_index: Option<i64>) {
  tracing::info!(
    event,
    new_session = %meta.id,
    parent = parent_id,
    event_index = ?event_index,
    backend = %meta.backend,
    "session_spawned"
  );
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;

  use charliebot_core::event_types;
  use serde_json::{Map, Value, json};

  use super::*;
  use crate::metadata::TaskSpec;
  use crate::task_tree::tests::{create_node, make_store, queued_run};
  use crate::{CreateTask, TaskCaller};

  /// A redelivery with no reports owed, for elone tests whose subtree has no undelivered close reports.
  struct NoRedelivery;

  impl CloseReportRedelivery for NoRedelivery {
    fn redeliver_close_reports_locked<'a>(
      &'a self,
      _guard: &'a ControlGuard,
      _session_id: &'a str,
    ) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  async fn create_with_goal(
    store: &Store,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
    goal: &str,
  ) -> SessionMetadata {
    store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: Some(TaskSpec { goal: goal.to_string(), ..TaskSpec::default() }),
          name: Some(request_id.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  fn user_event(session_id: &str, content: &str) -> Value {
    json!({"type": event_types::USER, "actor": "user", "source_session_id": session_id, "content": content})
  }

  async fn append_user(store: &Store, session_id: &str, content: &str) {
    store.append_chat_event(session_id, user_event(session_id, content)).await.unwrap();
  }

  /// The session directories under the home, the set a refused fork or elone must leave unchanged.
  fn session_dirs(store: &Store) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(store.config().sessions_dir())
      .unwrap()
      .map(|entry| entry.unwrap().path())
      .filter(|path| path.is_dir())
      .collect();
    dirs.sort();
    dirs
  }

  /// Every child chat log the store has written, in the sessions directory.
  fn chat_logs(store: &Store) -> Vec<PathBuf> {
    session_dirs(store)
      .into_iter()
      .map(|dir| dir.join("data").join("chat_events.jsonl"))
      .filter(|path| path.is_file())
      .collect()
  }

  /// Write one archived close fact, as the completion owner's archive writes it.
  async fn archive_task(store: &Store, session_id: &str) {
    let guard = store.control_lock().await;
    let event = archived_close_event(session_id, "elone-test-archive", ACTOR_USER, "archived by the user", &[], None);
    store.append_control_fact(&guard, session_id, event).await.unwrap();
    drop(guard);
    store.invalidate_tree_index();
  }

  #[tokio::test]
  async fn fork_copies_parent_prefix_and_clone_marker_into_child_log() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let parent = create_node(&store, "parent", None, TaskProfile::Manager).await;
    append_user(&store, &parent.id, "e0").await;
    append_user(&store, &parent.id, "e1").await;
    // A third user event past the fork point proves the copied prefix stops there.
    append_user(&store, &parent.id, "e2").await;
    let parent_events = store.load_chat_events_sync(&parent.id).unwrap();

    let child = store.fork_session(&parent.id, Some(2), None).await.unwrap();

    let child_events = store.load_chat_events_sync(&child.id).unwrap();
    // The parent holds its creation fact, then e0, e1 and e2; the cut at index 2 keeps the first three.
    assert_eq!(child_events.len(), 5);
    assert_eq!(child_events[..3], parent_events[..3]);
    assert_eq!(child_events[1]["content"].as_str(), Some("e0"));
    assert_eq!(child_events[2]["content"].as_str(), Some("e1"));
    assert_eq!(child_events[3]["type"].as_str(), Some(event_types::CLONE_START));
    assert_eq!(child_events[3]["parent_session_id"].as_str(), Some(parent.id.as_str()));
    assert_eq!(child_events[4]["type"].as_str(), Some(event_types::TASK_CREATED));
    assert_eq!(child_events[4]["source_session_id"].as_str(), Some(child.id.as_str()));
  }

  #[tokio::test]
  async fn fork_rejects_corrupt_parent_line_and_writes_no_child_log() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let parent = create_node(&store, "parent", None, TaskProfile::Manager).await;
    append_user(&store, &parent.id, "ok").await;
    let mut log = fs::OpenOptions::new().append(true).open(store.chat_events_path(&parent.id)).unwrap();
    std::io::Write::write_all(&mut log, b"{truncated\n").unwrap();
    let logs_before = chat_logs(&store);

    let error = store.fork_session(&parent.id, None, None).await.unwrap_err();

    assert!(matches!(&error, StoreError::Invalid(message) if message.contains("not a serialized event object")));
    assert_eq!(chat_logs(&store), logs_before);
  }

  #[tokio::test]
  async fn fork_copies_non_ascii_lines_verbatim_and_undecodable_bytes_raise() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let parent = create_node(&store, "parent", None, TaskProfile::Manager).await;
    append_user(&store, &parent.id, "ok").await;
    // A valid non-ASCII line passes the UTF-8 check, and the copy keeps its raw bytes.
    let non_ascii = format!("{}\n", json!({"type": event_types::USER, "content": "中文"}));
    let mut log = fs::OpenOptions::new().append(true).open(store.chat_events_path(&parent.id)).unwrap();
    std::io::Write::write_all(&mut log, non_ascii.as_bytes()).unwrap();
    drop(log);
    let parent_bytes = fs::read(store.chat_events_path(&parent.id)).unwrap();

    let child = store.fork_session(&parent.id, None, None).await.unwrap();

    let child_bytes = fs::read(store.chat_events_path(&child.id)).unwrap();
    assert!(child_bytes.starts_with(&parent_bytes));
    let marker_lines: Vec<Value> = std::str::from_utf8(&child_bytes[parent_bytes.len()..])
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    let types: Vec<&str> = marker_lines.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(types, [event_types::CLONE_START, event_types::TASK_CREATED]);

    // Undecodable bytes raise at fork time, and the failed fork writes no child chat log.
    let other = create_node(&store, "other", None, TaskProfile::Manager).await;
    append_user(&store, &other.id, "ok").await;
    let mut log = fs::OpenOptions::new().append(true).open(store.chat_events_path(&other.id)).unwrap();
    std::io::Write::write_all(&mut log, b"\xff\xfe\n").unwrap();
    drop(log);
    let logs_before = chat_logs(&store);

    let error = store.fork_session(&other.id, None, None).await.unwrap_err();

    assert!(matches!(error, StoreError::Invalid(_)));
    assert_eq!(chat_logs(&store), logs_before);
  }

  #[tokio::test]
  async fn fork_refuses_an_event_index_outside_the_parent_log_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let parent = create_node(&store, "parent", None, TaskProfile::Manager).await;
    let dirs_before = session_dirs(&store);

    for index in [-1, 1] {
      let error = store.fork_session(&parent.id, Some(index), None).await.unwrap_err();
      assert!(matches!(&error, StoreError::Invalid(message) if message.contains("out of range")));
    }
    assert_eq!(session_dirs(&store), dirs_before);
  }

  #[tokio::test]
  async fn elone_writes_successor_pointer_and_archives_parent() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let parent = create_node(&store, "parent", None, TaskProfile::Manager).await;

    let child = store.elone_session(&parent.id, 0, None, &NoRedelivery).await.unwrap();

    let fresh_parent = store.read_metadata_fresh(&parent.id).unwrap().unwrap();
    assert_eq!(fresh_parent.successor_session_id.as_deref(), Some(child.id.as_str()));
    assert_eq!(fresh_parent.status, SessionStatus::Archived);
  }

  #[tokio::test]
  async fn successor_chain_walks_across_three_elone_generations() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let gen0 = create_node(&store, "G0", None, TaskProfile::Manager).await;

    let gen1 = store.elone_session(&gen0.id, 0, None, &NoRedelivery).await.unwrap();
    let gen2 = store.elone_session(&gen1.id, 0, None, &NoRedelivery).await.unwrap();
    let gen3 = store.elone_session(&gen2.id, 0, None, &NoRedelivery).await.unwrap();

    let resolved = store.resolve_successor_chain(&gen0.id).unwrap();
    assert_eq!(resolved.last().unwrap().id, gen3.id);
  }

  #[tokio::test]
  async fn clone_child_manager_copies_position_and_stays_childless() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_with_goal(&store, "root", None, TaskProfile::Manager, "project").await;
    let source = create_with_goal(&store, "source", Some(&root.id), TaskProfile::Manager, "feature").await;
    let source_child = create_node(&store, "source-child", Some(&source.id), TaskProfile::Manager).await;
    store
      .mutate_fresh(&source.id, false, true, |meta| {
        meta.subtree_prompt_ref = Some("sha-subtree".to_string());
        meta.node_prompt_ref = Some("sha-node".to_string());
        meta.group = Some("quant".to_string());
        Ok(true)
      })
      .await
      .unwrap();

    let clone = store.fork_session(&source.id, None, None).await.unwrap();

    let fresh = store.get_session(&clone.id).await.unwrap().unwrap();
    assert_eq!(fresh.task_parent_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(fresh.profile, TaskProfile::Manager);
    assert_eq!(fresh.task.as_ref().map(|task| task.goal.as_str()), Some("feature"));
    assert_eq!(fresh.group.as_deref(), Some("quant"));
    assert_eq!(fresh.subtree_prompt_ref.as_deref(), Some("sha-subtree"));
    assert_eq!(fresh.node_prompt_ref.as_deref(), Some("sha-node"));
    assert_eq!(fresh.parent_session_id.as_deref(), Some(source.id.as_str()));
    assert!(store.child_ids(&clone.id).unwrap().is_empty());
    // The source keeps its position and its subtree.
    let fresh_source = store.get_session(&source.id).await.unwrap().unwrap();
    assert_eq!(fresh_source.task_parent_id.as_deref(), Some(root.id.as_str()));
    let fresh_child = store.get_session(&source_child.id).await.unwrap().unwrap();
    assert_eq!(fresh_child.task_parent_id.as_deref(), Some(source.id.as_str()));
    assert_eq!(store.task_state(&source.id).unwrap(), "open");
  }

  #[tokio::test]
  async fn clone_root_manager_is_a_root() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_with_goal(&store, "root", None, TaskProfile::Manager, "project").await;

    let clone = store.fork_session(&root.id, None, None).await.unwrap();

    let fresh = store.get_session(&clone.id).await.unwrap().unwrap();
    assert_eq!(fresh.task_parent_id, None);
  }

  #[tokio::test]
  async fn elone_moves_subtree_and_archives_source() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    let still_child = create_node(&store, "c1", Some(&source.id), TaskProfile::Manager).await;
    let running = create_node(&store, "c2", Some(&source.id), TaskProfile::Worker).await;
    {
      let guard = store.control_lock().await;
      store.register_run_locked(&guard, &running.id, queued_run(&running.id, "run-queued"), None).await.unwrap();
    }

    let new = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap();

    assert_eq!(
      store.get_session(&still_child.id).await.unwrap().unwrap().task_parent_id.as_deref(),
      Some(new.id.as_str())
    );
    assert_eq!(store.get_session(&running.id).await.unwrap().unwrap().task_parent_id.as_deref(), Some(new.id.as_str()));
    let mut children = store.child_ids(&new.id).unwrap();
    children.sort();
    let mut expected = vec![still_child.id.clone(), running.id.clone()];
    expected.sort();
    assert_eq!(children, expected);
    let closes = store
      .load_all_facts(&source.id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
      .collect::<Vec<_>>();
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0]["outcome"].as_str(), Some("archived"));
    assert_eq!(closes[0]["report_to"].to_value(), Value::Null);
    let fresh_source = store.read_metadata_fresh(&source.id).unwrap().unwrap();
    assert_eq!(fresh_source.status, SessionStatus::Archived);
    assert_eq!(fresh_source.successor_session_id.as_deref(), Some(new.id.as_str()));
  }

  #[tokio::test]
  async fn elone_moves_children_created_after_the_cut() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    // The cut at index 0 copies only the creation fact, and this child is created after that point.
    let late_child = create_node(&store, "late-child", Some(&source.id), TaskProfile::Manager).await;

    let new = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap();

    assert_eq!(
      store.get_session(&late_child.id).await.unwrap().unwrap().task_parent_id.as_deref(),
      Some(new.id.as_str())
    );
    assert_eq!(store.child_ids(&new.id).unwrap(), vec![late_child.id.clone()]);
    let copied: Vec<String> = store
      .load_chat_events_sync(&new.id)
      .unwrap()
      .iter()
      .map(|event| event["type"].as_str().unwrap().to_string())
      .collect();
    assert_eq!(copied, [event_types::TASK_CREATED, event_types::CLONE_START, event_types::TASK_CREATED]);
  }

  #[tokio::test]
  async fn elone_rebinds_scheduled_bindings_and_copies_cron_bookkeeping() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let source = create_node(&store, "source", None, TaskProfile::Manager).await;
    let mut cron = Map::new();
    cron.insert("last_scheduled_run".to_string(), json!("2026-01-02T03:04:05+00:00"));
    cron.insert("last_scheduled_cron".to_string(), json!("0 3 * * *"));
    cron.insert("last_run_status".to_string(), json!("success"));
    {
      let guard = store.control_lock().await;
      store.update_slot_fields_locked(&guard, &source.id, "cron", cron).await.unwrap();
    }

    // The test store's binding mover reports that it moved a binding, so the elone copies the scheduler fields.
    let new = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap();

    let fresh = store.get_session(&new.id).await.unwrap().unwrap();
    assert_eq!(fresh.extra("last_scheduled_run"), Some(&json!("2026-01-02T03:04:05+00:00")));
    assert_eq!(fresh.extra("last_scheduled_cron"), Some(&json!("0 3 * * *")));
    assert_eq!(fresh.extra("last_run_status"), Some(&json!("success")));
  }

  #[tokio::test]
  async fn elone_with_unfinished_run_refuses_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let source = create_node(&store, "source", None, TaskProfile::Manager).await;
    {
      let guard = store.control_lock().await;
      store.register_run_locked(&guard, &source.id, queued_run(&source.id, "run-queued"), None).await.unwrap();
    }
    let dirs_before = session_dirs(&store);
    let source_before = store.read_metadata_fresh(&source.id).unwrap().unwrap();

    let error = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap_err();

    assert!(matches!(&error, StoreError::Conflict(blockers) if blockers.len() == 1));
    assert_eq!(session_dirs(&store), dirs_before);
    let source_after = store.read_metadata_fresh(&source.id).unwrap().unwrap();
    assert_eq!(source_after.status, source_before.status);
    assert_eq!(source_after.successor_session_id, None);
  }

  #[tokio::test]
  async fn worker_source_clone_and_elone_keep_the_ordinary_shape() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let worker = create_node(&store, "worker", Some(&root.id), TaskProfile::Worker).await;

    let clone = store.fork_session(&worker.id, None, None).await.unwrap();
    let fresh_clone = store.get_session(&clone.id).await.unwrap().unwrap();
    assert_eq!(fresh_clone.profile, TaskProfile::Manager);
    assert_eq!(fresh_clone.task_parent_id, None);
    assert!(fresh_clone.name.starts_with('C'));

    let elone = store.elone_session(&worker.id, 0, None, &NoRedelivery).await.unwrap();
    let fresh_elone = store.get_session(&elone.id).await.unwrap().unwrap();
    assert_eq!(fresh_elone.profile, TaskProfile::Manager);
    assert_eq!(fresh_elone.task_parent_id, None);
    assert!(fresh_elone.name.starts_with('E'));
    let fresh_worker = store.read_metadata_fresh(&worker.id).unwrap().unwrap();
    assert_eq!(fresh_worker.status, SessionStatus::Archived);
    assert_eq!(fresh_worker.successor_session_id.as_deref(), Some(elone.id.as_str()));
  }

  #[tokio::test]
  async fn clone_and_elone_write_nothing_into_the_tree_parent_log() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    let log_before = fs::read(store.chat_events_path(&root.id)).unwrap();

    store.fork_session(&source.id, None, None).await.unwrap();
    store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap();

    assert_eq!(fs::read(store.chat_events_path(&root.id)).unwrap(), log_before);
  }

  #[tokio::test]
  async fn closed_parent_refuses_clone_and_elone() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    archive_task(&store, &root.id).await;
    let dirs_before = session_dirs(&store);

    let clone_error = store.fork_session(&source.id, None, None).await.unwrap_err();
    assert!(matches!(&clone_error, StoreError::Conflict(blockers) if blockers.len() == 1));
    let elone_error = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap_err();
    assert!(matches!(&elone_error, StoreError::Conflict(blockers) if blockers.len() == 1));

    assert_eq!(session_dirs(&store), dirs_before);
  }

  #[tokio::test]
  async fn copied_user_message_does_not_authorize_the_successor() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    append_user(&store, &source.id, "the user's instruction").await;

    let clone = store.fork_session(&source.id, None, None).await.unwrap();

    // The copied user message belongs to the source, so it does not authorize the clone.
    assert!(store.check_task_authorization(&clone.id).is_err());
  }

  #[tokio::test]
  async fn archived_source_successor_starts_open() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let source = create_node(&store, "source", None, TaskProfile::Manager).await;
    archive_task(&store, &source.id).await;
    assert_eq!(store.task_state(&source.id).unwrap(), "archived");

    let new = store.elone_session(&source.id, 0, None, &NoRedelivery).await.unwrap();

    assert_eq!(store.task_state(&new.id).unwrap(), "open");
  }

  #[tokio::test]
  async fn clone_of_an_archived_source_under_an_open_parent_starts_open() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let source = create_node(&store, "source", Some(&root.id), TaskProfile::Manager).await;
    archive_task(&store, &source.id).await;
    assert_eq!(store.task_state(&source.id).unwrap(), "archived");

    let clone = store.fork_session(&source.id, None, None).await.unwrap();

    assert_eq!(store.task_state(&clone.id).unwrap(), "open");
  }
}
