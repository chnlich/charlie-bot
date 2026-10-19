use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::fsutil;
use serde_json::Value;

use crate::metadata::{SessionMetadata, UtcDateTime};
use crate::ndjson;
use crate::store::CachedChatEvents;
use crate::{ControlGuard, Store, StoreError, StoreResult};

/// One parsed chat-log event.
pub type ChatEvent = Value;

/// A stable cache identity plus either the full event list or its newly appended suffix.
pub struct ChatEventsDelta {
  pub generation: u64,
  pub start_index: usize,
  pub event_count: usize,
  pub events: Vec<ChatEvent>,
}

impl Store {
  /// Append one event, injecting its id and Python `datetime.isoformat()` timestamp when absent.
  pub async fn append_chat_event(&self, session_id: &str, mut event: Value) -> StoreResult<ChatEvent> {
    let object = event.as_object_mut().ok_or_else(|| StoreError::new("chat event must be a JSON object"))?;
    if !object.contains_key("id") {
      object.insert("id".to_string(), Value::String(uuid::Uuid::new_v4().to_string()));
    }
    if !object.contains_key("timestamp") {
      object.insert("timestamp".to_string(), Value::String(python_event_timestamp(UtcDateTime::now())));
    }
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    ndjson::append(&self.chat_events_path(session_id), &event)?;
    if let Some(cached) = cache.as_mut() {
      Arc::make_mut(&mut cached.events).push(event.clone());
    }
    Ok(event)
  }

  /// Append one control fact while the caller holds the tree-wide control lock, then notify the sidebar of its type.
  pub async fn append_control_fact(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    event: Value,
  ) -> StoreResult<ChatEvent> {
    self.require_control_guard(guard)?;
    let event = self.append_chat_event(session_id, event).await?;
    let fact_type = event.get("type").and_then(Value::as_str);
    self.notify_task_tree_changed(session_id, fact_type).await;
    Ok(event)
  }

  /// Read the live event log, using the in-memory parsed-event cache after the first read.
  pub fn load_chat_events_sync(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    Ok(self.load_chat_events_shared_sync(session_id)?.as_ref().clone())
  }

  /// Read the cached event list as an immutable shared snapshot.
  pub fn load_chat_events_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    self.load_chat_events_shared_locked(session_id, &mut cache)
  }

  /// Read the live event cache from `covered` when its identity still matches, otherwise from the beginning.
  pub fn load_chat_events_delta_sync(
    &self,
    session_id: &str,
    known_generation: Option<u64>,
    covered: usize,
  ) -> StoreResult<ChatEventsDelta> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let _snapshot = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let cached = cache.as_ref().expect("event cache entry was loaded above");
    let start_index =
      if known_generation == Some(cached.generation) && covered <= cached.events.len() { covered } else { 0 };
    Ok(ChatEventsDelta {
      generation: cached.generation,
      start_index,
      event_count: cached.events.len(),
      events: cached.events[start_index..].to_vec(),
    })
  }

  /// Read archived facts and the live events, sharing the live cache when no archive is present.
  pub fn load_all_facts_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let live = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let archive_offset = self.read_archive_offset(session_id)?;
    if archive_offset == 0 {
      return Ok(live);
    }
    let mut events = self.load_archive_range(session_id, 0, archive_offset)?;
    events.extend(live.iter().cloned());
    Ok(Arc::new(events))
  }

  /// Read the latest live-log events without priming the event cache.
  pub fn load_chat_events_tail(&self, session_id: &str, limit: usize) -> StoreResult<(Vec<ChatEvent>, usize, bool)> {
    let path = self.chat_events_path(session_id);
    let raw = match fs::read(&path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0, false)),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    let total = if raw.is_empty() {
      0
    } else {
      raw.iter().filter(|byte| **byte == b'\n').count() + usize::from(!raw.ends_with(b"\n"))
    };
    let has_more = total > limit;
    if limit == 0 || total == 0 {
      return Ok((Vec::new(), total, has_more));
    }
    let mut lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
    if raw.ends_with(b"\n") {
      lines.pop();
    }
    let nonblank: Vec<&[u8]> = lines.into_iter().filter(|line| !line.iter().all(u8::is_ascii_whitespace)).collect();
    let start = nonblank.len().saturating_sub(limit);
    let events = nonblank[start..].iter().filter_map(|line| ndjson::parse_line(line)).collect();
    Ok((events, total, has_more))
  }

  /// Read all durable facts, archived segments first and the live log last, composed the way
  /// `task_sessions.fact_history` composes it: the archived count is the total event count minus
  /// the live event count, the archived events are the range read from 0 to that count, and
  /// archive files beyond that count stay unread.
  pub fn load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    let live = self.load_chat_events_sync(session_id)?;
    let total = self.get_chat_event_count_sync(session_id)?;
    let archived_count = total.saturating_sub(live.len());
    if archived_count == 0 {
      return Ok(live);
    }
    let mut events = self.load_chat_events_range(session_id, 0, archived_count)?.0;
    events.extend(live);
    Ok(events)
  }

  /// Read the global event-index range `[start, end)` and say whether earlier indices exist.
  ///
  /// Global indices are `archive_offset + line_in_live_file`. With a positive archive offset the live
  /// half numbers physical lines — a blank or a malformed line consumes its index and parses to nothing,
  /// the same inconsistency `ndjson.parse_ndjson_range`'s islice shows on the unarchived cold path. An
  /// unarchived warm cache instead numbers its own parsed-event indices, matching Python's cache branch.
  pub fn load_chat_events_range(
    &self,
    session_id: &str,
    start: usize,
    end: usize,
  ) -> StoreResult<(Vec<ChatEvent>, bool)> {
    if end <= start {
      return Ok((Vec::new(), start > 0));
    }
    let archive_offset = self.read_archive_offset(session_id)?;
    if end <= archive_offset {
      return Ok((self.load_archive_range(session_id, start, end)?, start > 0));
    }
    if start >= archive_offset {
      let rel_start = start - archive_offset;
      let rel_end = end - archive_offset;
      let events = if archive_offset > 0 {
        let lines = self.live_range_lines(session_id)?;
        slice_live_lines(&lines, rel_start, rel_end)
      } else {
        match self.cached_chat_events(session_id) {
          Some(cached) => cached.iter().skip(rel_start).take(rel_end.saturating_sub(rel_start)).cloned().collect(),
          None => {
            let lines = self.live_lines_newline_split(session_id)?;
            slice_live_lines(&lines, rel_start, rel_end)
          }
        }
      };
      return Ok((events, start > 0));
    }
    let mut out = self.load_archive_range(session_id, start, archive_offset)?;
    let live_end = end - archive_offset;
    let lines = self.live_range_lines(session_id)?;
    out.extend(slice_live_lines(&lines, 0, live_end));
    Ok((out, start > 0))
  }

  /// Read one archive's parsed events. An unreadable file logs and contributes nothing, the
  /// pre-memo reader's behavior on open failure in `_archive_file_events`.
  fn read_archive_file(&self, path: &std::path::Path, session_id: &str) -> Vec<ChatEvent> {
    match ndjson::read_file(path) {
      Ok(events) => events,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "archive_read_failed");
        Vec::new()
      }
    }
  }

  /// Read the parsed events at global indices `[start, end)` from the archive files in
  /// chronological order, stopping once the covered extent passes `end`.
  fn load_archive_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<Vec<ChatEvent>> {
    let mut events = Vec::new();
    let mut pos = 0usize;
    for path in self.archive_paths(session_id)? {
      let file_events = self.read_archive_file(&path, session_id);
      let file_end = pos + file_events.len();
      if pos < end && file_end > start {
        events.extend(file_events[start.max(pos) - pos..end.min(file_end) - pos].iter().cloned());
      }
      pos = file_end;
      if pos >= end {
        break;
      }
    }
    Ok(events)
  }

  /// Read the live log as parsed events with one slot per physical line, split the way Python's
  /// universal-newline reader splits (`\r\n` and a lone `\r` terminate a line too). A read
  /// failure logs and answers an empty page, the `_live_range_lines` behavior on an OSError.
  fn live_range_lines(&self, session_id: &str) -> StoreResult<Vec<Option<ChatEvent>>> {
    let path = self.chat_events_path(session_id);
    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "live_range_read_failed");
        return Ok(Vec::new());
      }
    };
    let text = String::from_utf8(bytes).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    let mut lines = Vec::new();
    let mut segment = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
      match character {
        '\n' => {
          lines.push(ndjson::parse_line(segment.as_bytes()));
          segment.clear();
        }
        '\r' => {
          if chars.peek() == Some(&'\n') {
            chars.next();
          }
          lines.push(ndjson::parse_line(segment.as_bytes()));
          segment.clear();
        }
        _ => segment.push(character),
      }
    }
    lines.push(ndjson::parse_line(segment.as_bytes()));
    Ok(lines)
  }

  /// Read the live log as parsed events with one slot per physical line, split on `\n` only —
  /// the domain `ndjson.parse_ndjson_range` numbers on the unarchived cold path.
  fn live_lines_newline_split(&self, session_id: &str) -> StoreResult<Vec<Option<ChatEvent>>> {
    let path = self.chat_events_path(session_id);
    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    Ok(bytes.split(|byte| *byte == b'\n').map(ndjson::parse_line).collect())
  }

  /// Find the first event with `id` in archived and live history.
  pub fn find_event_by_id(&self, session_id: &str, event_id: &str) -> StoreResult<Option<ChatEvent>> {
    Ok(
      self
        .load_all_facts(session_id)?
        .into_iter()
        .find(|event| event.get("id").and_then(Value::as_str) == Some(event_id)),
    )
  }

  /// Return the total event-index extent, using parsed cache length or physical live lines.
  pub fn get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize> {
    let offset = self.read_archive_offset(session_id)?;
    if let Some(events) = self.cached_chat_events(session_id) {
      return Ok(offset + events.len());
    }
    Ok(offset + ndjson::count_lines(&self.chat_events_path(session_id))?)
  }

  /// Discard the parsed live-log cache after a rewrite.
  pub fn clear_chat_events_cache(&self, session_id: &str) {
    let cache_entry = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id).cloned();
    let Some(cache_entry) = cache_entry else { return };
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    *cache = None;
  }

  fn load_chat_events_shared_locked(
    &self,
    session_id: &str,
    cache: &mut Option<CachedChatEvents>,
  ) -> StoreResult<Arc<Vec<ChatEvent>>> {
    if let Some(cached) = cache.as_ref() {
      return Ok(Arc::clone(&cached.events));
    }
    let events = Arc::new(self.read_chat_events_file(&self.chat_events_path(session_id))?);
    *cache = Some(CachedChatEvents { generation: self.next_events_cache_generation(), events: Arc::clone(&events) });
    Ok(events)
  }

  fn read_chat_events_file(&self, path: &Path) -> StoreResult<Vec<ChatEvent>> {
    #[cfg(test)]
    if let Some(hook) = self.event_read_hook.lock().expect("event read hook mutex is not poisoned").clone() {
      hook();
    }
    ndjson::read_file(path)
  }

  pub(crate) fn cached_chat_events(&self, session_id: &str) -> Option<Arc<Vec<ChatEvent>>> {
    let cache_entry = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id).cloned()?;
    let cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    cache.as_ref().map(|cached| Arc::clone(&cached.events))
  }

  /// Archive the oldest consecutive events before `cutoff` and advance metadata `archive_offset`.
  pub async fn archive_events_before(&self, session_id: &str, cutoff: UtcDateTime) -> StoreResult<Option<PathBuf>> {
    let live_path = self.chat_events_path(session_id);
    let raw = match fs::read(&live_path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", live_path.display()))),
    };
    let mut archived_raw = Vec::new();
    let mut kept_raw = Vec::new();
    let mut split_reached = false;
    let mut lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
    if raw.is_empty() {
      lines.clear();
    } else if raw.ends_with(b"\n") {
      lines.pop();
    }
    for line in lines {
      let line = line.strip_suffix(b"\r").unwrap_or(line);
      if split_reached {
        kept_raw.push(line.to_vec());
        continue;
      }
      if line.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      let Some(event) = ndjson::parse_line(line) else {
        split_reached = true;
        kept_raw.push(line.to_vec());
        continue;
      };
      let Some(timestamp) = event.get("timestamp").and_then(Value::as_str) else {
        split_reached = true;
        kept_raw.push(line.to_vec());
        continue;
      };
      let parsed = match UtcDateTime::parse(timestamp) {
        Ok(value) => value,
        Err(_) => {
          split_reached = true;
          kept_raw.push(line.to_vec());
          continue;
        }
      };
      if parsed < cutoff {
        archived_raw.push(line.to_vec());
      } else {
        split_reached = true;
        kept_raw.push(line.to_vec());
      }
    }
    if archived_raw.is_empty() {
      return Ok(None);
    }
    let date = cutoff.into_inner().date();
    let (iso_year, iso_week, _) = date.to_iso_week_date();
    let archive_dir = live_path.parent().expect("event log path has a parent").join("archives");
    fs::create_dir_all(&archive_dir).map_err(|error| StoreError::new(format!("{}: {error}", archive_dir.display())))?;
    let archive_path = archive_dir.join(format!("chat_events.{iso_year}-W{iso_week:02}.jsonl"));
    let mut archive = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&archive_path)
      .map_err(|error| StoreError::new(format!("{}: {error}", archive_path.display())))?;
    for line in &archived_raw {
      archive
        .write_all(line)
        .and_then(|()| archive.write_all(b"\n"))
        .map_err(|error| StoreError::new(format!("{}: {error}", archive_path.display())))?;
    }
    drop(archive);
    let mut replacement = String::new();
    for line in kept_raw {
      replacement.push_str(&String::from_utf8_lossy(&line));
      replacement.push('\n');
    }
    fsutil::atomic_write_text(&live_path, &replacement)
      .map_err(|error| StoreError::new(format!("{}: {error}", live_path.display())))?;
    let archived_count = archived_raw.len() as i64;
    self
      .mutate_fresh(session_id, false, true, |meta: &mut SessionMetadata| {
        meta.archive_offset += archived_count;
        Ok(true)
      })
      .await?;
    self.clear_chat_events_cache(session_id);
    Ok(Some(archive_path))
  }

  pub(crate) fn read_archive_offset(&self, session_id: &str) -> StoreResult<usize> {
    if let Some(meta) = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").get(session_id) {
      return Ok(meta.value.archive_offset.max(0) as usize);
    }
    let meta = match self.read_metadata_fresh(session_id) {
      Ok(meta) => meta,
      Err(error) => {
        // `read_archive_offset_sync` falls back to 0 when metadata.json is missing or unreadable.
        tracing::debug!(session_id, error = %error, "archive_offset_read_failed");
        return Ok(0);
      }
    };
    Ok(meta.map_or(0, |meta| meta.archive_offset.max(0) as usize))
  }

  fn archive_paths(&self, session_id: &str) -> StoreResult<Vec<PathBuf>> {
    let dir = self.session_dir(session_id).join("data").join("archives");
    let entries = match fs::read_dir(&dir) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => {
        tracing::debug!(path = %dir.display(), error = %error, "archive_dir_stat_failed");
        return Ok(Vec::new());
      }
    };
    let mut paths = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", dir.display())))?;
      let name = entry.file_name();
      // The archive glob `chat_events.*.jsonl`: a bare `chat_events.jsonl` is not an archive.
      let name = name.to_string_lossy();
      if name.strip_prefix("chat_events.").and_then(|rest| rest.strip_suffix(".jsonl")).is_some() {
        paths.push(entry.path());
      }
    }
    paths.sort();
    Ok(paths)
  }
}

/// Take the physical-line slots in `[start, end)`, dropping the slots that parsed to nothing.
fn slice_live_lines(lines: &[Option<ChatEvent>], start: usize, end: usize) -> Vec<ChatEvent> {
  lines.iter().skip(start).take(end.saturating_sub(start)).flatten().cloned().collect()
}

fn python_event_timestamp(value: UtcDateTime) -> String {
  let datetime = value.into_inner();
  let base = format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
    datetime.year(),
    u8::from(datetime.month()),
    datetime.day(),
    datetime.hour(),
    datetime.minute(),
    datetime.second(),
  );
  if datetime.microsecond() == 0 {
    format!("{base}+00:00")
  } else {
    format!("{base}.{:06}+00:00", datetime.microsecond())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::make_test_store;
  use serde_json::json;
  use std::collections::BTreeSet;
  use std::sync::{Mutex, mpsc};
  use std::time::Duration;

  #[test]
  fn event_timestamp_matches_python_isoformat_offset() {
    let value = UtcDateTime::parse("2026-01-01T00:00:00.123400Z").unwrap();
    assert_eq!(python_event_timestamp(value), "2026-01-01T00:00:00.123400+00:00");
  }

  #[test]
  fn archive_path_uses_the_cutoff_iso_week() {
    let value = UtcDateTime::parse("2021-01-01T12:00:00Z").unwrap();
    let (year, week, _) = value.into_inner().date().to_iso_week_date();
    assert_eq!((year, week), (2020, 53));
  }

  #[tokio::test]
  async fn event_count_tail_and_global_range_reads_agree() {
    let (_home, store) = make_test_store();
    let mut expected = Vec::new();
    for index in 0..5 {
      expected.push(store.append_chat_event("synthetic", json!({"type": "user", "content": index})).await.unwrap());
    }

    assert_eq!(store.get_chat_event_count_sync("synthetic").unwrap(), expected.len());
    let (tail, total, has_more) = store.load_chat_events_tail("synthetic", 2).unwrap();
    assert_eq!(tail, expected[3..]);
    assert_eq!(total, 5);
    assert!(has_more);

    let (range, has_earlier) = store.load_chat_events_range("synthetic", 1, 4).unwrap();
    assert_eq!(range, expected[1..4]);
    assert!(has_earlier);
    let (full, has_earlier) = store.load_chat_events_range("synthetic", 0, 5).unwrap();
    assert_eq!(full, expected);
    assert!(!has_earlier);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn cold_read_of_one_session_does_not_block_another_sessions_append() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    *store.event_read_hook.lock().unwrap() = Some(Arc::new(move || {
      entered_tx.send(()).unwrap();
      release_rx.lock().unwrap().recv().unwrap();
    }));

    let reader_store = Arc::clone(&store);
    let reader = tokio::task::spawn_blocking(move || reader_store.load_chat_events_shared_sync("synthetic-a"));
    let entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
    if !entered {
      let _ = release_tx.send(());
      reader.await.unwrap().unwrap();
      panic!("cold read did not reach its file-read hook");
    }

    let append_store = Arc::clone(&store);
    let mut append = tokio::spawn(async move {
      append_store.append_chat_event("synthetic-b", json!({"id": "event-b", "type": "user"})).await
    });
    let append_result = tokio::time::timeout(Duration::from_secs(1), &mut append).await;
    let appended_while_read_held = append_result.is_ok();
    release_tx.send(()).unwrap();
    reader.await.unwrap().unwrap();
    let appended = match append_result {
      Ok(result) => result.unwrap().unwrap(),
      Err(_) => append.await.unwrap().unwrap(),
    };

    assert!(appended_while_read_held, "session B append waited for session A file read");
    assert_eq!(appended["id"], "event-b");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn concurrent_appends_during_cold_reads_land_once_in_cache_and_file() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    let session_id = "synthetic-same-session";
    let path = store.chat_events_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    for iteration in 0..200 {
      store.clear_chat_events_cache(session_id);
      std::fs::write(&path, b"").unwrap();
      let (entered_tx, entered_rx) = mpsc::channel();
      let (release_tx, release_rx) = mpsc::channel();
      let release_rx = Arc::new(Mutex::new(release_rx));
      *store.event_read_hook.lock().unwrap() = Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
      }));

      let reader_store = Arc::clone(&store);
      let reader = tokio::task::spawn_blocking(move || reader_store.load_chat_events_shared_sync(session_id));
      let entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
      if !entered {
        let _ = release_tx.send(());
        reader.await.unwrap().unwrap();
        panic!("iteration {iteration}: cold read did not reach its file-read hook");
      }

      let (started_tx, started_rx) = mpsc::channel();
      let mut appends = Vec::new();
      let expected: BTreeSet<String> = (0..2).map(|index| format!("event-{iteration}-{index}")).collect();
      for index in 0..2 {
        let append_store = Arc::clone(&store);
        let started_tx = started_tx.clone();
        let event_id = format!("event-{iteration}-{index}");
        appends.push(tokio::spawn(async move {
          started_tx.send(()).unwrap();
          append_store.append_chat_event(session_id, json!({"id": event_id, "type": "user"})).await
        }));
      }
      drop(started_tx);
      for _ in 0..2 {
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
      }
      release_tx.send(()).unwrap();
      reader.await.unwrap().unwrap();
      for append in appends {
        append.await.unwrap().unwrap();
      }

      let cached = store.load_chat_events_shared_sync(session_id).unwrap();
      let cache_ids: BTreeSet<String> = cached.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect();
      let file_events = ndjson::read_file(&path).unwrap();
      let file_ids: BTreeSet<String> =
        file_events.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect();
      assert_eq!(cached.len(), expected.len(), "iteration {iteration}: cached event count");
      assert_eq!(file_events.len(), expected.len(), "iteration {iteration}: file event count");
      assert_eq!(cache_ids, expected, "iteration {iteration}: cache ids");
      assert_eq!(file_ids, expected, "iteration {iteration}: file ids");
      assert_eq!(cached.as_slice(), file_events.as_slice(), "iteration {iteration}: cache and file order");
    }
  }

  #[tokio::test]
  async fn shared_reads_reuse_the_snapshot_until_append() {
    let (_home, store) = make_test_store();
    store.append_chat_event("synthetic-shared", json!({"id": "first", "type": "user"})).await.unwrap();

    let first = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    let second = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    store.append_chat_event("synthetic-shared", json!({"id": "second", "type": "user"})).await.unwrap();
    let appended = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    assert!(!Arc::ptr_eq(&first, &appended));
    assert_eq!(first.len(), 1);
    assert_eq!(appended.last().unwrap()["id"], "second");
  }

  mod parity {
    use super::*;
    use crate::{StoreHooks, StoreResult};
    use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct Labels;
    impl crate::AccountLabelKeeper for Labels {
      fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
        let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
        if meta.extra("claude_account") == Some(&value) {
          return Ok(false);
        }
        meta.set_extra("claude_account", value);
        Ok(true)
      }

      fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
        meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
      }
    }

    struct Anchors;
    impl crate::AnchorFieldReconciler for Anchors {
      fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
        attempted.cc_session_id.clone_from(&disk.cc_session_id);
        attempted.native_backend.clone_from(&disk.native_backend);
        Ok(())
      }
    }

    struct Listing;
    impl crate::SequenceListingFields for Listing {
      fn owns_session(&self, _meta: &SessionMetadata) -> bool {
        false
      }
      fn listing_fields(
        &self,
        _: &[String],
        _: UtcDateTime,
      ) -> StoreResult<HashMap<String, serde_json::Map<String, Value>>> {
        Ok(HashMap::new())
      }
      fn sequence_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
        Ok(HashMap::new())
      }
      fn view_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
        Ok(HashMap::new())
      }
    }

    fn test_store(home: &std::path::Path) -> Store {
      let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
      Store::new(
        cfg,
        vec![MetadataSlot {
          owner: "probe",
          on: MetadataFile::Session,
          after: Some("group"),
          fields: vec![SlotField { name: "probe_note", default: Value::Null }],
        }],
        StoreHooks {
          account_labels: Some(Arc::new(Labels)),
          anchor_reconciler: Some(Arc::new(Anchors)),
          sequence_listing: Some(Arc::new(Listing)),
          runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
          sequence_archive: Some(Arc::new(crate::store::test_support::SequenceArchive)),
          sequence_binding_mover: Some(Arc::new(crate::store::test_support::BindingMover)),
        },
      )
      .unwrap()
    }

    fn seed_metadata(store: &Store, session_id: &str) {
      let meta = SessionMetadata::new(session_id, "seed", crate::metadata::TaskProfile::Manager);
      let path = store.metadata_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, meta.to_json_string(&store.slots, true)).unwrap();
    }

    fn ids(events: &[ChatEvent]) -> Vec<String> {
      events.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect()
    }

    /// `test_recycle.py::test_recycle_archives_old_chat_events_and_advances_offset` at store level:
    /// the old head lands in a weekly archive file, the live file keeps the retained tail, the
    /// offset advances, and the next append continues the global numbering.
    #[tokio::test]
    async fn archiving_the_old_head_advances_the_offset_and_continues_global_indexes() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..6 {
        body.push_str(&format!("{{\"id\": \"old-{index}\", \"timestamp\": \"2025-02-01T00:00:00+00:00\"}}\n"));
      }
      for index in 0..3 {
        body.push_str(&format!("{{\"id\": \"new-{index}\", \"timestamp\": \"2025-02-05T00:00:0{}+00:00\"}}\n", index));
      }
      std::fs::write(&path, body).unwrap();

      let archive = store
        .archive_events_before(session_id, UtcDateTime::parse("2025-02-04T00:00:00Z").unwrap())
        .await
        .unwrap()
        .unwrap();
      assert!(archive.ends_with("chat_events.2025-W06.jsonl"), "{}", archive.display());
      let archive_text = std::fs::read_to_string(&archive).unwrap();
      let archive_ids: Vec<String> = archive_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string())
        .collect();
      assert_eq!(archive_ids, (0..6).map(|index| format!("old-{index}")).collect::<Vec<_>>());
      let live_text = std::fs::read_to_string(&path).unwrap();
      let live_ids: Vec<String> = live_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string())
        .collect();
      assert_eq!(live_ids, (0..3).map(|index| format!("new-{index}")).collect::<Vec<_>>());
      assert_eq!(store.read_metadata_fresh(session_id).unwrap().unwrap().archive_offset, 6);
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 9);
      assert_eq!(ids(&store.load_all_facts(session_id).unwrap()), {
        let mut all: Vec<String> = (0..6).map(|index| format!("old-{index}")).collect();
        all.extend((0..3).map(|index| format!("new-{index}")));
        all
      });
      let appended = store
        .append_chat_event(session_id, json!({"type": "user", "content": "after", "id": "after-1"}))
        .await
        .unwrap();
      let _ = appended;
      // Global index 9 is the first appended event after six archived rows and three retained rows.
      assert_eq!(store.load_chat_events_range(session_id, 9, 10).unwrap().0[0]["id"], "after-1");
    }

    /// `test_recycle.py::test_recycle_noop_when_nothing_old` at store level: nothing archived, no
    /// archive file, the offset stays 0.
    #[tokio::test]
    async fn archiving_without_old_events_is_a_noop() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "{\"id\": \"future\", \"timestamp\": \"2026-05-10T01:00:00+00:00\"}\n").unwrap();
      let archive =
        store.archive_events_before(session_id, UtcDateTime::parse("2026-05-10T00:00:00Z").unwrap()).await.unwrap();
      assert!(archive.is_none());
      assert_eq!(store.read_metadata_fresh(session_id).unwrap().unwrap().archive_offset, 0);
      assert_eq!(store.load_chat_events_sync(session_id).unwrap().len(), 1);
    }

    /// `test_recycle.py::test_live_range_walk_matches_full_build_across_line_shapes`: blank and
    /// malformed lines consume an index, a CRLF pair terminates one line, and the final line
    /// carries no newline.
    #[tokio::test]
    async fn range_reads_number_physical_lines_when_archive_offset_is_positive() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..6 {
        body.push_str(&format!("{{\"id\": \"old-{index}\", \"timestamp\": \"2025-02-01T00:00:00+00:00\"}}\n"));
      }
      std::fs::write(&path, body).unwrap();
      store.archive_events_before(session_id, UtcDateTime::parse("2025-02-03T00:00:00Z").unwrap()).await.unwrap();

      std::fs::write(&path, b"{\"id\": \"a0\"}\n\n{bad json\n{\"id\": \"a1\"}\r\n{\"id\": \"a2\"}\n{\"id\": \"a3\"}")
        .unwrap();
      let spec: Vec<Option<&str>> = vec![Some("a0"), None, None, Some("a1"), Some("a2"), Some("a3")];
      let expected = |start: usize, end: usize| -> Vec<String> {
        spec
          .iter()
          .skip(start.max(6) - 6)
          .take(end.saturating_sub(start))
          .filter_map(|slot| *slot)
          .map(str::to_string)
          .collect()
      };
      for (start, end) in [(6usize, 7usize), (7, 9), (8, 10), (6, 10), (9, 12), (11, 15), (7, 7)] {
        let (events, has_more) = store.load_chat_events_range(session_id, start, end).unwrap();
        assert_eq!(ids(&events), expected(start, end), "window [{start}, {end})");
        assert_eq!(has_more, start > 0, "window [{start}, {end})");
      }
    }

    /// The unarchived cold path numbers physical lines (`ndjson.parse_ndjson_range`'s islice),
    /// and the warm cache numbers its own parsed-event indices.
    #[tokio::test]
    async fn unarchived_range_reads_cold_numbers_physical_lines_and_warm_numbers_parsed_events() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, b"{\"id\": \"a\"}\n\n{bad\n{\"id\": \"b\"}\n{\"id\": \"c\"}").unwrap();

      let (cold, has_more) = store.load_chat_events_range(session_id, 3, 5).unwrap();
      assert_eq!(ids(&cold), vec!["b", "c"]);
      assert!(has_more);
      // The blank and the malformed line each consumed a physical index.
      let (cold_head, _) = store.load_chat_events_range(session_id, 1, 3).unwrap();
      assert!(cold_head.is_empty());

      store.load_chat_events_sync(session_id).unwrap();
      let (warm, _) = store.load_chat_events_range(session_id, 1, 2).unwrap();
      assert_eq!(ids(&warm), vec!["b"]);
      let (warm_end, _) = store.load_chat_events_range(session_id, 2, 6).unwrap();
      assert_eq!(ids(&warm_end), vec!["c"]);
    }

    /// `fact_history` composition: the archived count is the total event count minus the live
    /// count, archive files beyond that count stay unread — one stale archive row beyond the
    /// offset never reaches the facts.
    #[tokio::test]
    async fn load_all_facts_stops_reading_archives_at_the_archive_offset() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      let archive_dir = path.parent().unwrap().join("archives");
      std::fs::create_dir_all(&archive_dir).unwrap();
      let mut archive_body = String::new();
      for index in 0..4 {
        archive_body.push_str(&format!("{{\"id\": \"archived-{index}\"}}\n"));
      }
      std::fs::write(archive_dir.join("chat_events.2025-W06.jsonl"), archive_body).unwrap();
      std::fs::write(&path, b"{\"id\": \"live-0\"}\n{\"id\": \"live-1\"}\n{\"id\": \"live-2\"}\n").unwrap();

      let mut meta = store.read_metadata_fresh(session_id).unwrap().unwrap();
      meta.archive_offset = 2;
      let metadata_path = store.metadata_path(session_id);
      std::fs::write(&metadata_path, meta.to_json_string(&store.slots, true)).unwrap();

      let facts = store.load_all_facts(session_id).unwrap();
      assert_eq!(ids(&facts), ["archived-0", "archived-1", "live-0", "live-1", "live-2"]);
    }

    /// `core/test_ndjson.py`'s count contract through the store: the total is the archive offset
    /// plus the live file's physical lines, an unterminated final line included.
    #[tokio::test]
    async fn event_count_is_the_offset_plus_physical_live_lines() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, b"{\"id\": \"a\"}\n{bad}\n\n{\"id\": \"b\"}").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 4);
    }

    /// `core/test_ndjson.py::test_count_ndjson_lines_memo_recounts_after_append` at store level: the
    /// count follows a line appended outside the store.
    #[tokio::test]
    async fn event_count_recounts_after_an_external_append() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "{\"i\": 0}\n{\"i\": 1}\n{\"i\": 2}\n{\"i\": 3}\n{\"i\": 4}\n").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 5);
      let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
      std::io::Write::write_all(&mut file, b"{\"i\": 5}\n").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 6);
    }
  }
}
