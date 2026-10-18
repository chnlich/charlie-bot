use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use charliebot_core::fsutil;
use serde_json::Value;

use crate::metadata::{SessionMetadata, UtcDateTime};
use crate::ndjson;
use crate::{ControlGuard, Store, StoreError, StoreResult};

/// One parsed chat-log event.
pub type ChatEvent = Value;

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
    ndjson::append(&self.chat_events_path(session_id), &event)?;
    let mut cache = self.events_cache.lock().expect("event cache mutex is not poisoned");
    if let Some(events) = cache.get_mut(session_id) {
      events.push(event.clone());
    }
    Ok(event)
  }

  /// Append one control fact while the caller holds the tree-wide control lock.
  pub async fn append_control_fact(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    event: Value,
  ) -> StoreResult<ChatEvent> {
    self.require_control_guard(guard)?;
    self.append_chat_event(session_id, event).await
  }

  /// Read the live event log, using the in-memory parsed-event cache after the first read.
  pub fn load_chat_events_sync(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    if let Some(events) = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id) {
      return Ok(events.clone());
    }
    let events = ndjson::read_file(&self.chat_events_path(session_id))?;
    self.events_cache.lock().expect("event cache mutex is not poisoned").insert(session_id.to_string(), events.clone());
    Ok(events)
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

  /// Read all durable facts, archived segments first and the live log last.
  pub fn load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    let mut events = Vec::new();
    for path in self.archive_paths(session_id)? {
      events.extend(ndjson::read_file(&path)?);
    }
    events.extend(self.load_chat_events_sync(session_id)?);
    Ok(events)
  }

  /// Read the global event-index range `[start, end)` and say whether earlier indices exist.
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
    let mut out = Vec::new();
    let mut archive_index = 0usize;
    if start < archive_offset {
      for path in self.archive_paths(session_id)? {
        let archived = ndjson::read_file(&path)?;
        let file_end = archive_index + archived.len();
        if archive_index < end && file_end > start {
          let from = start.saturating_sub(archive_index).min(archived.len());
          let to = end.saturating_sub(archive_index).min(archived.len());
          out.extend(archived[from..to].iter().cloned());
        }
        archive_index = file_end;
        if archive_index >= end.min(archive_offset) {
          break;
        }
      }
    }
    if end > archive_offset {
      let live = self.load_chat_events_sync(session_id)?;
      let live_start = start.saturating_sub(archive_offset);
      let live_end = end.saturating_sub(archive_offset).min(live.len());
      if live_start < live_end {
        out.extend(live[live_start..live_end].iter().cloned());
      }
    }
    Ok((out, start > 0))
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
    if let Some(events) = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id) {
      return Ok(offset + events.len());
    }
    Ok(offset + ndjson::count_lines(&self.chat_events_path(session_id))?)
  }

  /// Discard the parsed live-log cache after a rewrite.
  pub fn clear_chat_events_cache(&self, session_id: &str) {
    self.events_cache.lock().expect("event cache mutex is not poisoned").remove(session_id);
  }

  /// Return the latest persisted context reading and the newest assistant/result request time.
  pub fn context_state(&self, session_id: &str) -> StoreResult<(Option<i64>, Option<UtcDateTime>)> {
    let events = self.load_chat_events_sync(session_id)?;
    let mut reading_kind = "";
    let mut context_tokens = None;
    let mut post_compact_tokens = None;
    let mut last_request_at = None;
    for event in &events {
      let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
      if matches!(event_type, "assistant" | "result")
        && let Some(raw) = event.get("timestamp").and_then(Value::as_str)
        && let Ok(parsed) = UtcDateTime::parse(raw)
      {
        last_request_at = Some(parsed);
      }
      if event_type == "assistant" && !truthy(event.get("parent_tool_use_id")) {
        let usage = event.get("message").and_then(|message| message.get("usage"));
        let prompt_tokens = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
          .iter()
          .map(|key| usage.and_then(|usage| usage.get(*key)).and_then(Value::as_i64).unwrap_or(0))
          .sum::<i64>();
        if prompt_tokens > 0 {
          context_tokens = Some(prompt_tokens);
          post_compact_tokens = None;
          reading_kind = "assistant";
        }
      } else if event_type == "system" {
        match event.get("subtype").and_then(Value::as_str) {
          Some("compact_boundary") if reading_kind == "assistant" => {
            // A boundary without an integer post_tokens leaves the earlier boundary's count in place.
            if let Some(tokens) =
              event.get("compact_metadata").and_then(|value| value.get("post_tokens")).and_then(Value::as_i64)
            {
              post_compact_tokens = Some(tokens);
            }
          }
          Some("context_reading") => {
            if let Some(reading) = event.get("context_reading").and_then(Value::as_object) {
              reading_kind = "resolved";
              context_tokens = reading.get("context_tokens").and_then(Value::as_i64);
            }
          }
          _ => {}
        }
      } else if event_type == "result"
        && let Some(snapshot) = event.get("context_snapshot").and_then(Value::as_object)
      {
        let tokens = snapshot.get("tokens").and_then(Value::as_object);
        let sum = ["input", "output", "reasoning", "cache_read", "cache_write"]
          .iter()
          .map(|key| tokens.and_then(|tokens| tokens.get(*key)).and_then(Value::as_i64).unwrap_or(0))
          .sum::<i64>();
        reading_kind = "snapshot";
        context_tokens = Some(sum);
      }
    }
    if reading_kind == "assistant" {
      context_tokens = post_compact_tokens.or(context_tokens);
    }
    Ok((context_tokens, last_request_at))
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
    Ok(self.read_metadata_fresh(session_id)?.map_or(0, |meta| meta.archive_offset.max(0) as usize))
  }

  fn archive_paths(&self, session_id: &str) -> StoreResult<Vec<PathBuf>> {
    let dir = self.session_dir(session_id).join("data").join("archives");
    let entries = match fs::read_dir(&dir) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", dir.display()))),
    };
    let mut paths = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", dir.display())))?;
      let name = entry.file_name();
      if name.to_string_lossy().starts_with("chat_events.") && name.to_string_lossy().ends_with(".jsonl") {
        paths.push(entry.path());
      }
    }
    paths.sort();
    Ok(paths)
  }
}

fn truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) | Some(Value::Bool(false)) => false,
    Some(Value::String(text)) => !text.is_empty(),
    Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
    Some(Value::Array(items)) => !items.is_empty(),
    Some(Value::Object(items)) => !items.is_empty(),
    Some(Value::Bool(true)) => true,
  }
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
}
