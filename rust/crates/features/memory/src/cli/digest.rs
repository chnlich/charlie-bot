//! `charliebot memory user-digest`: the memory curator's cross-session user-message digest
//! (`prompts/cron/memory_curator/user_message_digest.py`).
//!
//! One line per user message from the last 7 days: `<YYYY-MM-DD> <session-short-id> [NEW] <text>`,
//! `NEW` marking messages from the last 24 hours. Read-only, capped at 120K characters with the
//! oldest lines dropped first.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::event_types;
use charliebot_core::home;
use charliebot_store::layout;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::Value;

const WINDOW_S: f64 = 7.0 * 86400.0;
const NEW_S: f64 = 86400.0;
const TEXT_CAP: usize = 400;
const TOTAL_CAP: usize = 120_000;

/// Print the digest of the live sessions.
pub fn run() -> Result<(), CliExit> {
  let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|elapsed| elapsed.as_secs_f64())
    .map_err(|error| CliExit::error(error.to_string()))?;
  print!("{}", digest(&home_dir, now)?);
  Ok(())
}

/// The digest text: one line per user message, ending with a newline.
pub fn digest(home_dir: &Path, now: f64) -> Result<String, CliExit> {
  let sessions = config::sessions_dir_in(home_dir);
  let entries = match fs::read_dir(&sessions) {
    Ok(entries) => entries,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("\n".to_string()),
    Err(error) => return Err(CliExit::error(format!("read {}: {error}", sessions.display()))),
  };
  let mut rows: Vec<(f64, String, String)> = Vec::new();
  for entry in entries {
    let entry = entry.map_err(|error| CliExit::error(format!("read {}: {error}", sessions.display())))?;
    let session_dir = entry.path();
    if !session_dir.is_dir() {
      continue;
    }
    let events_path = layout::chat_events_path(&session_dir);
    let metadata = match fs::metadata(&events_path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(CliExit::error(format!("read {}: {error}", events_path.display()))),
    };
    let modified =
      metadata.modified().map_err(|error| CliExit::error(format!("read {}: {error}", events_path.display())))?;
    if now - system_time_seconds(modified) > WINDOW_S {
      continue;
    }
    let session_id: String = entry.file_name().to_string_lossy().chars().take(8).collect();
    let text = fs::read_to_string(&events_path)
      .map_err(|error| CliExit::error(format!("read {}: {error}", events_path.display())))?;
    for line in text.lines() {
      let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
        continue;
      };
      if event_kind(&event) != Some(event_types::USER) {
        continue;
      }
      let Some(content) = event.get("content").and_then(Value::as_str) else {
        continue;
      };
      if content.trim().is_empty() || content.starts_with('/') {
        continue;
      }
      let timestamp = event.get("timestamp").and_then(Value::as_str).unwrap_or("").replace('Z', "+00:00");
      let Some(at) = parse_timestamp(&timestamp) else {
        continue;
      };
      if now - at > WINDOW_S {
        continue;
      }
      rows.push((at, session_id.clone(), cap_text(content)));
    }
  }
  rows.sort_by(|left, right| {
    left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1)).then_with(|| left.2.cmp(&right.2))
  });
  let mut out: Vec<String> = rows
    .iter()
    .map(|(at, session_id, text)| {
      let new = if now - at <= NEW_S { "NEW " } else { "" };
      format!("{} {session_id} {new}{text}", local_date(*at))
    })
    .collect();
  while out.iter().map(|line| line.chars().count() + 1).sum::<usize>() > TOTAL_CAP {
    out.remove(0);
  }
  Ok(out.join("\n") + "\n")
}

/// `event.get('type') or event.get('event_type')`: an empty `type` falls back to `event_type`.
fn event_kind(event: &serde_json::Map<String, Value>) -> Option<&str> {
  match event.get("type") {
    Some(Value::String(text)) if !text.is_empty() => Some(text.as_str()),
    Some(Value::String(_)) | Some(Value::Null) | None => event.get("event_type").and_then(Value::as_str),
    Some(_) => None,
  }
}

fn cap_text(content: &str) -> String {
  content.chars().take(TEXT_CAP).collect::<String>().replace('\n', " ")
}

/// `datetime.fromtimestamp(t)` in the process's local timezone, formatted as `%Y-%m-%d`.
fn local_date(at: f64) -> String {
  Local
    .timestamp_opt(at.floor() as i64, 0)
    .single()
    .map(|moment| moment.format("%Y-%m-%d").to_string())
    .unwrap_or_default()
}

fn system_time_seconds(time: SystemTime) -> f64 {
  match time.duration_since(UNIX_EPOCH) {
    Ok(elapsed) => elapsed.as_secs_f64(),
    Err(error) => -(error.duration().as_secs_f64()),
  }
}

/// `datetime.datetime.fromisoformat` after the script's `Z` replacement; a value without an offset is
/// local time, as Python reads it.
fn parse_timestamp(text: &str) -> Option<f64> {
  if let Ok(moment) = DateTime::parse_from_rfc3339(text) {
    return Some(moment.timestamp_micros() as f64 / 1_000_000.0);
  }
  for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
    if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
      return Local.from_local_datetime(&naive).earliest().map(|moment| moment.timestamp_micros() as f64 / 1_000_000.0);
    }
  }
  let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
  let naive = date.and_hms_opt(0, 0, 0)?;
  Local.from_local_datetime(&naive).earliest().map(|moment| moment.timestamp_micros() as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  /// The fixed "now" every test reads: 2026-10-28T12:00:00Z.
  fn now() -> f64 {
    DateTime::parse_from_rfc3339("2026-10-28T12:00:00Z").unwrap().timestamp() as f64
  }

  struct Fixture {
    home: tempfile::TempDir,
  }

  impl Fixture {
    fn new() -> Fixture {
      Fixture { home: tempfile::tempdir().unwrap() }
    }

    fn session(&self, name: &str, events: &[Value]) -> PathBuf {
      let session_dir = self.home.path().join("sessions").join(name);
      let path = layout::chat_events_path(&session_dir);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let text: String = events.iter().map(|event| format!("{event}\n")).collect();
      std::fs::write(&path, text).unwrap();
      session_dir
    }

    fn digest(&self) -> String {
      digest(self.home.path(), now()).unwrap()
    }
  }

  fn user_event(content: &str, timestamp: &str) -> Value {
    serde_json::json!({"type": "user", "content": content, "timestamp": timestamp})
  }

  #[test]
  fn one_line_per_user_message_with_the_new_marker() {
    let fixture = Fixture::new();
    fixture.session(
      "12345678-session-id",
      &[
        user_event("recent message", "2026-10-28T06:00:00+00:00"),
        user_event("older message", "2026-10-24T06:00:00+00:00"),
      ],
    );

    let output = fixture.digest();
    let lines: Vec<&str> = output.lines().collect();

    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("12345678 older message"), "{output}");
    assert!(lines[1].ends_with("12345678 NEW recent message"), "{output}");
    assert!(lines[0].starts_with("2026-10-"), "{output}");
    assert!(output.ends_with('\n'));
  }

  #[test]
  fn non_user_slash_blank_and_malformed_events_are_skipped() {
    let fixture = Fixture::new();
    let lines = [
      serde_json::json!({"type": "assistant", "content": "assistant text", "timestamp": "2026-10-28T06:00:00Z"}),
      serde_json::json!({"event_type": "user", "content": "/command", "timestamp": "2026-10-28T06:00:00Z"}),
      serde_json::json!({"type": "user", "content": "   ", "timestamp": "2026-10-28T06:00:00Z"}),
      serde_json::json!({"type": "user", "content": "no timestamp", "timestamp": "not-a-time"}),
      serde_json::json!({"type": "user", "content": "kept", "timestamp": "2026-10-28T06:00:00Z"}),
      serde_json::json!({"event_type": "user", "content": "kept by event_type", "timestamp": "2026-10-28T06:00:00Z"}),
    ];
    let session_dir = fixture.home.path().join("sessions").join("87654321-session-id");
    let path = layout::chat_events_path(&session_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, format!("{text}not json\n")).unwrap();

    let output = fixture.digest();
    let kept: Vec<&str> = output.lines().collect();

    assert_eq!(kept.len(), 2, "{output}");
    assert!(kept[0].ends_with("kept"), "{output}");
    assert!(kept[1].ends_with("kept by event_type"), "{output}");
  }

  #[test]
  fn messages_older_than_the_window_and_stale_files_are_skipped() {
    let fixture = Fixture::new();
    fixture.session("11111111-session-id", &[user_event("outside the window", "2026-10-01T06:00:00Z")]);
    let stale = fixture.session("22222222-session-id", &[user_event("stale file", "2026-10-28T06:00:00Z")]);
    let stale_path = layout::chat_events_path(&stale);
    let file = fs::OpenOptions::new().write(true).open(&stale_path).unwrap();
    let old = UNIX_EPOCH + std::time::Duration::from_secs_f64(now() - 8.0 * 86400.0);
    file.set_modified(old).unwrap();

    assert_eq!(fixture.digest(), "\n");
  }

  #[test]
  fn text_is_capped_at_four_hundred_characters_and_newlines_become_spaces() {
    let fixture = Fixture::new();
    let content = format!("{}\nsecond line", "word ".repeat(100));
    fixture.session("33333333-session-id", &[user_event(&content, "2026-10-28T06:00:00Z")]);

    let output = fixture.digest();
    let text = output.lines().next().unwrap().split_once("NEW ").unwrap().1;

    assert_eq!(text.chars().count(), TEXT_CAP);
    assert!(!text.contains('\n'));
  }

  #[test]
  fn the_total_cap_drops_the_oldest_lines() {
    let fixture = Fixture::new();
    let events: Vec<Value> = (0..400)
      .map(|index| {
        let at = DateTime::from_timestamp((now() - (400 - index) as f64 * 60.0) as i64, 0).unwrap();
        user_event(&format!("{index:03} {}", "x".repeat(1000)), &at.to_rfc3339())
      })
      .collect();
    fixture.session("44444444-session-id", &events);

    let output = fixture.digest();
    let total: usize = output.lines().map(|line| line.chars().count() + 1).sum();

    assert!(total <= TOTAL_CAP, "{total}");
    assert!(output.contains("399 "), "the newest line survives");
    assert!(!output.lines().any(|line| line.contains(" 000 ")), "the oldest line is dropped");
  }
}
