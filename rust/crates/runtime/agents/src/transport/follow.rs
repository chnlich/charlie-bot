//! Tail-follow reads complete raw NDJSON lines and checkpoints after handling their events.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use memchr::memchr_iter;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::Instant;

use super::cursor::RawCursorWriter;
use super::spawn;

pub const DEFAULT_BUFFER_LIMIT: usize = 1024 * 1024 * 1024;
pub const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const READ_CHUNK_BYTES: usize = 64 * 1024;

#[cfg(test)]
thread_local! {
  static FOLLOW_METADATA_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
  static FOLLOW_EVENT_METADATA_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Async callback invoked once when the raw log exceeds its silence threshold.
pub type SilenceCallback = Box<dyn FnMut() -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> + Send>;

/// Counters returned after a follow loop drains or reaches its post-result timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowSummary {
  /// Last complete-line boundary seen in the raw log.
  pub final_offset: u64,
  /// Whether a result event started the post-result clock.
  pub saw_result: bool,
}

/// Follow a raw log from `start_offset`, processing each event before its line cursor advances.
#[allow(clippy::too_many_arguments)]
pub async fn tail_follow_events<T, H, Fut, L>(
  raw_path: &Path,
  mut translate: T,
  mut handle_event: H,
  mut is_alive: L,
  cursor_path: Option<&Path>,
  start_offset: u64,
  post_result_timeout: Duration,
  buffer_limit: usize,
  poll_interval: Duration,
  mut on_silence: Option<SilenceCallback>,
  silence_threshold: Duration,
) -> anyhow::Result<FollowSummary>
where
  T: FnMut(Value) -> anyhow::Result<Vec<Value>>,
  H: FnMut(Value) -> Fut,
  Fut: Future<Output = io::Result<()>>,
  L: FnMut() -> bool,
{
  while !raw_path.exists() {
    if !is_alive() {
      return Ok(FollowSummary { final_offset: start_offset, saw_result: false });
    }
    tokio::time::sleep(poll_interval).await;
  }

  let mut file = tokio::fs::File::open(raw_path).await?;
  let clamp_timestamps = !super::final_stdout::is_capture(raw_path.parent().expect("raw log has a parent"));
  file.seek(std::io::SeekFrom::Start(start_offset)).await?;
  let mut read_to = start_offset;
  let mut offset = start_offset;
  let mut line_length = 0u64;
  let mut partial_line = Vec::new();
  let mut partial_has_content = false;
  let mut oversized_line = false;
  let mut saw_result = false;
  let mut last_growth = Instant::now();
  let initial_mtime = file_metadata(&file).await?.modified().ok();
  let mut last_output_at = subtract_age(Instant::now(), initial_mtime);
  let mut silence_reported = false;
  let mut pending_tool_calls = HashSet::new();
  let mut cursor_writer = cursor_path.map(RawCursorWriter::new);
  let mut bytes = vec![0; READ_CHUNK_BYTES];

  loop {
    let size = file_metadata(&file).await?.len();
    if size > read_to {
      let now = Instant::now();
      last_growth = now;
      let mtime = file_metadata(&file).await?.modified().ok();
      last_output_at = subtract_age(now, mtime);
      while read_to < size {
        let limit = usize::try_from((size - read_to).min(READ_CHUNK_BYTES as u64)).expect("bounded read length");
        let count = file.read(&mut bytes[..limit]).await?;
        if count == 0 {
          break;
        }
        let chunk_offset = read_to;
        read_to += count as u64;
        let mut start = 0;
        // `base.py:470-502` scans a mapped line view; borrow complete chunk lines here and buffer only cross-chunk lines.
        for index in memchr_iter(b'\n', &bytes[..count]) {
          let segment = &bytes[start..index];
          line_length = line_length.saturating_add(segment.len() as u64);
          if !oversized_line && !partial_line.is_empty() {
            partial_line.extend_from_slice(segment);
          }
          let line_end = chunk_offset + index as u64;
          if line_length > buffer_limit as u64 {
            tracing::debug!(
              target: "charliebot_agents",
              bytes = line_length,
              limit = buffer_limit,
              "backend_line_over_limit"
            );
          } else if !oversized_line {
            let parsed = if partial_line.is_empty() {
              spawn::parse_backend_line(segment)
            } else {
              spawn::parse_backend_line(&partial_line)
            };
            if let Some(event) = parsed {
              let event_mtime = event_file_metadata(&file)?.modified().ok();
              for mut translated in translate(event)? {
                track_pending_tool_calls(&translated, &mut pending_tool_calls);
                if clamp_timestamps {
                  clamp_event_timestamp(&mut translated, event_mtime);
                }
                let is_result = translated.get("type").and_then(Value::as_str) == Some("result");
                // A delivered input starts a same-process second turn: its result re-arms the post-result clock
                // (`base.py` resets `saw_result` on every INPUT_DELIVERED event).
                let is_input_delivered =
                  translated.get("type").and_then(Value::as_str) == Some(charliebot_core::event_types::INPUT_DELIVERED);
                handle_event(translated).await?;
                if is_input_delivered {
                  saw_result = false;
                }
                if !saw_result && is_result {
                  if pending_tool_calls.is_empty() {
                    saw_result = true;
                    last_growth = Instant::now();
                  } else {
                    tracing::debug!(
                      target: "charliebot_agents",
                      pending = pending_tool_calls.len(),
                      tool_ids = ?pending_tool_calls,
                      "backend_result_suppressed_pending_tools"
                    );
                  }
                }
              }
              if let Some(writer) = cursor_writer.as_mut() {
                writer.write(line_end + 1)?;
              }
            }
          }
          offset = line_end + 1;
          line_length = 0;
          partial_line.clear();
          partial_has_content = false;
          oversized_line = false;
          start = index + 1;
        }
        let trailing = &bytes[start..count];
        line_length = line_length.saturating_add(trailing.len() as u64);
        partial_has_content |= trailing.iter().any(|byte| !byte.is_ascii_whitespace());
        if line_length > buffer_limit as u64 {
          oversized_line = true;
          partial_line.clear();
        } else if !oversized_line {
          partial_line.extend_from_slice(trailing);
        }
      }
      continue;
    }

    if saw_result && Instant::now().duration_since(last_growth) > post_result_timeout {
      tracing::warn!(target: "charliebot_agents", timeout = post_result_timeout.as_secs_f64(), "backend_post_result_timeout");
      break;
    }
    if !is_alive() {
      // The child can no longer write, so one final metadata check drains the
      // lines it wrote just before exiting instead of breaking with them unread.
      let final_size = file_metadata(&file).await?.len();
      if final_size == read_to {
        break;
      }
      continue;
    }
    if let Some(callback) = on_silence.as_mut()
      && !silence_reported
      && Instant::now().duration_since(last_output_at) > silence_threshold
    {
      silence_reported = true;
      callback().await?;
    }
    tokio::time::sleep(poll_interval).await;
  }

  if line_length > 0 && partial_has_content {
    tracing::warn!(target: "charliebot_agents", bytes = line_length, "raw_trailing_torn_line_dropped");
  }
  if let Some(writer) = cursor_writer.as_mut() {
    writer.close()?;
  }
  Ok(FollowSummary { final_offset: offset, saw_result })
}

async fn file_metadata(file: &tokio::fs::File) -> io::Result<std::fs::Metadata> {
  #[cfg(test)]
  FOLLOW_METADATA_CALLS.with(|count| count.set(count.get() + 1));
  file.metadata().await
}

fn event_file_metadata(file: &tokio::fs::File) -> io::Result<std::fs::Metadata> {
  #[cfg(test)]
  FOLLOW_EVENT_METADATA_CALLS.with(|count| count.set(count.get() + 1));
  // `base.py:518` fstats each valid event; keep that timestamp point without Tokio's blocking-pool metadata hop.
  // SAFETY: the Tokio file owns this stable descriptor for the borrow; ManuallyDrop prevents a second close.
  let borrowed = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(file.as_raw_fd()) });
  borrowed.metadata()
}

fn subtract_age(now: Instant, mtime: Option<SystemTime>) -> Instant {
  let age = mtime.and_then(|time| SystemTime::now().duration_since(time).ok()).unwrap_or(Duration::ZERO);
  now.checked_sub(age).unwrap_or(now)
}

fn track_pending_tool_calls(event: &Value, pending: &mut HashSet<String>) {
  match event.get("type").and_then(Value::as_str) {
    Some("assistant") => {
      if let Some(items) = event.pointer("/message/content").and_then(Value::as_array) {
        for item in items {
          if item.get("type").and_then(Value::as_str) == Some("tool_use") {
            insert_id(item.get("id"), pending);
          }
        }
      }
    }
    Some("tool_use") => insert_id(event.get("id"), pending),
    Some("tool_result") => remove_id(event.get("tool_use_id"), pending),
    Some("user") => {
      if let Some(items) = event.pointer("/message/content").and_then(Value::as_array) {
        for item in items {
          if item.get("type").and_then(Value::as_str) == Some("tool_result") {
            remove_id(item.get("tool_use_id"), pending);
          }
        }
      }
    }
    _ => {}
  }
}

fn insert_id(value: Option<&Value>, pending: &mut HashSet<String>) {
  if let Some(id) = value.and_then(Value::as_str).filter(|id| !id.is_empty()) {
    pending.insert(id.to_string());
  }
}

fn remove_id(value: Option<&Value>, pending: &mut HashSet<String>) {
  if let Some(id) = value.and_then(Value::as_str).filter(|id| !id.is_empty()) {
    pending.remove(id);
  }
}

fn clamp_event_timestamp(event: &mut Value, mtime: Option<SystemTime>) {
  if event.get("timestamp").is_some_and(charliebot_core::pyfmt::python_truthy) {
    return;
  }
  let now = OffsetDateTime::now_utc();
  let mtime = mtime.map(OffsetDateTime::from).unwrap_or(now);
  let clamped = if now < mtime { now } else { mtime };
  let value = python_isoformat(clamped);
  event
    .as_object_mut()
    .expect("translated backend event is an object")
    .insert("timestamp".to_string(), Value::String(value));
}

fn python_isoformat(value: OffsetDateTime) -> String {
  charliebot_core::timefmt::isoformat(value)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::transport::cursor::read_raw_cursor;
  use std::fs;
  use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  };

  fn default_follow<'a>(
    raw_path: &'a Path,
    events: &'a mut Vec<Value>,
    cursor: Option<&'a Path>,
    start_offset: u64,
  ) -> impl Future<Output = anyhow::Result<FollowSummary>> + 'a {
    tail_follow_events(
      raw_path,
      |event| Ok(vec![event]),
      |event| {
        events.push(event);
        async { Ok(()) }
      },
      || false,
      cursor,
      start_offset,
      Duration::from_secs(30),
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(3600),
    )
  }

  #[test]
  fn a_clamped_timestamp_is_python_isoformat_in_utc() {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_micros(1_792_000_000_500_000);
    let mut event = serde_json::json!({"type": "assistant"});
    clamp_event_timestamp(&mut event, Some(mtime));
    assert_eq!(event["timestamp"], "2026-10-14T17:46:40.500000+00:00");
  }

  #[tokio::test]
  async fn keeps_partial_lines_and_drops_a_torn_final_line() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"assistant\",\"seq\":1}\n{\"type\":\"assistant\",\"seq\":2").unwrap();
    let mut events = Vec::new();
    default_follow(&raw, &mut events, None, 0).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["seq"], 1);
    assert!(events[0]["timestamp"].is_string());
  }

  #[tokio::test]
  async fn completes_a_partial_line_after_a_later_append() {
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"assistant\",\"seq\":9").unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_handler = Arc::clone(&events);
    let alive = Arc::new(AtomicBool::new(true));
    let alive_for_handler = Arc::clone(&alive);
    let alive_for_probe = Arc::clone(&alive);
    let (partial_read, partial_read_rx) = oneshot::channel();
    let partial_read = Arc::new(Mutex::new(Some(partial_read)));
    let raw_for_follow = raw.clone();
    let task = tokio::spawn(async move {
      tail_follow_events(
        &raw_for_follow,
        |event| Ok(vec![event]),
        move |event| {
          events_for_handler.lock().unwrap().push(event);
          alive_for_handler.store(false, Ordering::SeqCst);
          async { Ok(()) }
        },
        move || {
          if let Some(sender) = partial_read.lock().expect("partial-read signal is not poisoned").take() {
            sender.send(()).expect("test waits for the partial-line read");
          }
          alive_for_probe.load(Ordering::SeqCst)
        },
        None,
        0,
        Duration::from_secs(1),
        DEFAULT_BUFFER_LIMIT,
        Duration::from_millis(2),
        None,
        Duration::from_secs(1),
      )
      .await
    });
    tokio::time::timeout(Duration::from_secs(30), partial_read_rx).await.unwrap().unwrap();
    use std::io::Write;
    fs::OpenOptions::new().append(true).open(&raw).unwrap().write_all(b"}\n").unwrap();
    tokio::time::timeout(Duration::from_secs(30), task).await.unwrap().unwrap().unwrap();
    assert_eq!(events.lock().unwrap()[0]["seq"], 9);
  }

  #[tokio::test]
  async fn cursor_advances_after_the_event_handler_finishes() {
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let cursor = root.path().join("agent.raw.cursor");
    fs::write(&raw, b"{\"type\":\"assistant\",\"seq\":1}\n").unwrap();
    let (handler_entered, handler_entered_rx) = oneshot::channel();
    let (handler_release, handler_release_rx) = oneshot::channel();
    let handler_entered = Arc::new(Mutex::new(Some(handler_entered)));
    let handler_release_rx = Arc::new(Mutex::new(Some(handler_release_rx)));
    let expected_offset = fs::metadata(&raw).unwrap().len();
    let task = tokio::spawn({
      let handler_entered = Arc::clone(&handler_entered);
      let handler_release_rx = Arc::clone(&handler_release_rx);
      let cursor_for_follow = cursor.clone();
      let cursor_for_handler = cursor.clone();
      async move {
        tail_follow_events(
          &raw,
          |event| Ok(vec![event]),
          move |_event| {
            let entered = handler_entered
              .lock()
              .expect("event-handler channel state is not poisoned")
              .take()
              .expect("event handler runs once");
            let release = handler_release_rx
              .lock()
              .expect("event-handler channel state is not poisoned")
              .take()
              .expect("event handler runs once");
            let cursor = cursor_for_handler.clone();
            async move {
              assert_eq!(read_raw_cursor(&cursor), 0);
              entered.send(()).expect("test waits for the event handler");
              release.await.expect("test releases the event handler");
              Ok(())
            }
          },
          || false,
          Some(&cursor_for_follow),
          0,
          Duration::from_secs(1),
          DEFAULT_BUFFER_LIMIT,
          Duration::from_millis(1),
          None,
          Duration::from_secs(1),
        )
        .await
      }
    });
    tokio::time::timeout(Duration::from_secs(30), handler_entered_rx).await.unwrap().unwrap();
    assert_eq!(read_raw_cursor(&cursor), 0);
    handler_release.send(()).expect("event handler is waiting");
    tokio::time::timeout(Duration::from_secs(30), task).await.unwrap().unwrap().unwrap();
    assert_eq!(read_raw_cursor(&cursor), expected_offset);
  }

  #[tokio::test]
  async fn result_waits_for_wrapped_and_flat_tool_results() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(
      &raw,
      b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"wrapped\"}]}}\n{\"type\":\"result\"}\n{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"wrapped\"}]}}\n{\"type\":\"result\"}\n",
    )
    .unwrap();
    let wrapped_summary = tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| async { Ok(()) },
      || false,
      None,
      0,
      Duration::ZERO,
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(wrapped_summary.saw_result);

    fs::write(&raw, b"{\"type\":\"tool_use\",\"id\":\"flat\"}\n{\"type\":\"result\"}\n").unwrap();
    let alive_checks = Arc::new(AtomicUsize::new(0));
    let checks = Arc::clone(&alive_checks);
    let summary = tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| async { Ok(()) },
      move || checks.fetch_add(1, Ordering::SeqCst) == 0,
      None,
      0,
      Duration::ZERO,
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(!summary.saw_result);
    assert_eq!(alive_checks.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn an_oversized_complete_line_is_skipped() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"assistant\",\"pad\":\"123456789\"}\n{\"type\":\"result\"}\n").unwrap();
    let cursor = root.path().join("agent.raw.cursor");
    let mut events = Vec::new();
    tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |event| {
        events.push(event);
        async { Ok(()) }
      },
      || false,
      Some(&cursor),
      0,
      Duration::from_secs(1),
      20,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "result");
    assert_eq!(read_raw_cursor(&cursor), fs::metadata(raw).unwrap().len());
  }

  #[tokio::test]
  async fn malformed_and_oversized_lines_move_only_the_in_memory_offset() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let cursor = root.path().join("agent.raw.cursor");
    fs::write(&raw, b"not-json\n{\"pad\":\"this line is too large\"}\n").unwrap();
    let summary = tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| async { Ok(()) },
      || false,
      Some(&cursor),
      0,
      Duration::from_secs(1),
      8,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(summary.final_offset, fs::metadata(&raw).unwrap().len());
    assert!(!cursor.exists());
  }

  #[tokio::test]
  async fn first_result_ends_a_live_follow_after_the_timeout() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"result\"}\n").unwrap();
    let summary = tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| async { Ok(()) },
      || true,
      None,
      0,
      Duration::from_millis(5),
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(summary.saw_result);
  }

  #[tokio::test]
  async fn silence_callback_uses_initial_mtime_and_runs_once() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"assistant\"}\n").unwrap();
    let old = std::time::UNIX_EPOCH + Duration::from_secs(1);
    fs::File::open(&raw).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_callback = Arc::clone(&calls);
    let calls_for_liveness = Arc::clone(&calls);
    let callback: SilenceCallback = Box::new(move || {
      let calls = Arc::clone(&calls_for_callback);
      Box::pin(async move {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })
    });
    tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| async { Ok(()) },
      move || calls_for_liveness.load(Ordering::SeqCst) == 0,
      None,
      0,
      Duration::from_secs(1),
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      Some(callback),
      Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
  }

  #[tokio::test]
  async fn returns_translator_errors_before_advancing_the_cursor() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let cursor = root.path().join("agent.raw.cursor");
    fs::write(&raw, b"{\"type\":\"assistant\"}\n").unwrap();
    let error = tail_follow_events(
      &raw,
      |_| Err(anyhow::anyhow!("synthetic translator failure")),
      |_| async { Ok(()) },
      || false,
      Some(&cursor),
      0,
      Duration::from_secs(1),
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "synthetic translator failure");
    assert!(!cursor.exists());
  }

  #[tokio::test(flavor = "current_thread")]
  async fn one_growth_round_reads_metadata_once_for_five_thousand_events() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let line = b"{\"type\":\"assistant\",\"timestamp\":\"2026-10-22T00:00:00+00:00\"}\n";
    fs::write(&raw, line.repeat(5000)).unwrap();
    FOLLOW_METADATA_CALLS.with(|count| count.set(0));
    FOLLOW_EVENT_METADATA_CALLS.with(|count| count.set(0));
    let handled = AtomicUsize::new(0);

    tail_follow_events(
      &raw,
      |event| Ok(vec![event]),
      |_| {
        handled.fetch_add(1, Ordering::Relaxed);
        async { Ok(()) }
      },
      || false,
      None,
      0,
      Duration::from_secs(1),
      DEFAULT_BUFFER_LIMIT,
      Duration::from_millis(1),
      None,
      Duration::from_secs(1),
    )
    .await
    .unwrap();

    assert_eq!(handled.load(Ordering::Relaxed), 5000);
    FOLLOW_METADATA_CALLS.with(|count| assert_eq!(count.get(), 5));
    FOLLOW_EVENT_METADATA_CALLS.with(|count| assert_eq!(count.get(), 5000));
  }
}
