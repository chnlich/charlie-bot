//! The batch window of a cron-bound session: the two latest fires of one cron task in the session's chat log, or the
//! session's creation time when the latest fire is the first.
//!
//! A master-mode cron fire admits a `scheduled_trigger` whose id is `cron:<task>:<firing>` and whose timestamp is the
//! admission time. The same session receives other `scheduled_trigger` events (self-wakes and close-blocked notices),
//! so only the task's own id prefix selects a fire.

use std::path::Path;

use anyhow::{Context, Result, bail};
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::layout::metadata_path;
use charliebot_store::metadata::read_session_metadata;
use jiff::Timestamp;

/// The batch window: failures at or after `since` and before `until`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
  pub since: Timestamp,
  pub until: Timestamp,
}

/// The window between the two latest fires of `task` in `session_id`'s chat log. The first fire starts at the
/// session's creation time, so consecutive windows share a boundary.
pub fn cron_window(home: &Path, session_id: &str, task: &str) -> Result<Window> {
  let session_dir = sessions_dir_in(home).join(session_id);
  let path = metadata_path(&session_dir);
  let metadata = read_session_metadata(&path)
    .map_err(|error| anyhow::anyhow!("reading the metadata of session {session_id}: {error}"))?
    .with_context(|| format!("session {session_id} has no metadata at {}", path.display()))?;
  let created_at = metadata
    .created_at
    .to_str()
    .parse::<Timestamp>()
    .with_context(|| format!("reading the creation time of session {session_id}"))?;

  let mut fires = fires(&session_dir, task)?;
  fires.sort();
  fires.dedup();
  let until = *fires
    .last()
    .with_context(|| format!("session {session_id} has no scheduled_trigger with id prefix cron:{task}:"))?;
  let since = if fires.len() >= 2 { fires[fires.len() - 2] } else { created_at };
  if since >= until {
    bail!("session {session_id}: the fires of cron:{task} leave no window between {since} and {until}");
  }
  Ok(Window { since, until })
}

/// Every timestamp of this task's fire events across the session's archives and live log.
fn fires(session_dir: &Path, task: &str) -> Result<Vec<Timestamp>> {
  let prefix = format!("cron:{task}:");
  let paths = chat_events::session_log_paths(session_dir)
    .with_context(|| format!("listing the chat logs of {}", session_dir.display()))?;
  let mut fires = Vec::new();
  for path in paths {
    let lines = chat_events::read_lines(&path, LineRule::LossyLines)
      .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
    for line in lines {
      let line = line.map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
      let Ok(event) = line.decode() else { continue };
      if event.get("type").and_then(serde_json::Value::as_str) != Some(event_types::SCHEDULED_TRIGGER) {
        continue;
      }
      let Some(id) = event.get("id").and_then(serde_json::Value::as_str) else { continue };
      if !id.starts_with(&prefix) {
        continue;
      }
      let Some(timestamp) = event.get("timestamp").and_then(serde_json::Value::as_str) else { continue };
      fires.push(timestamp.parse::<Timestamp>().with_context(|| format!("reading fire {id} in {}", path.display()))?);
    }
  }
  Ok(fires)
}

#[cfg(test)]
mod tests {
  use std::fs::{self, OpenOptions};
  use std::io::Write;
  use std::path::PathBuf;

  use charliebot_store::layout::chat_events_path;

  use super::*;

  fn session(root: &Path, created_at: &str) -> PathBuf {
    let dir = root.join("sessions").join("triage");
    fs::create_dir_all(dir.join("data")).unwrap();
    let metadata = format!(
      r#"{{"id":"triage","name":"triage","schema_version":2,"profile":"manager","created_at":"{created_at}"}}"#
    );
    fs::write(metadata_path(&dir), metadata).unwrap();
    dir
  }

  fn event(dir: &Path, id: &str, timestamp: &str) {
    let line = format!(r#"{{"id":"{id}","type":"scheduled_trigger","timestamp":"{timestamp}","content":"wake"}}"#);
    let mut file = OpenOptions::new().create(true).append(true).open(chat_events_path(dir)).unwrap();
    writeln!(file, "{line}").unwrap();
  }

  fn fire(dir: &Path, timestamp: &str) {
    event(dir, &format!("cron:failure-triage:{timestamp}"), timestamp);
  }

  fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
  }

  #[test]
  fn adjacent_fires_give_windows_that_meet() {
    let root = tempfile::tempdir().unwrap();
    let dir = session(root.path(), "2026-10-31T07:00:00Z");
    fire(&dir, "2026-10-31T15:07:00+00:00");
    let first = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(first, Window { since: at("2026-10-31T07:00:00Z"), until: at("2026-10-31T15:07:00Z") });

    fire(&dir, "2026-11-01T16:07:00+00:00");
    let second = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(second.since, first.until);
    assert_eq!(second.until, at("2026-11-01T16:07:00Z"));
  }

  #[test]
  fn a_self_wake_and_a_close_blocked_notice_between_fires_leave_the_window_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let dir = session(root.path(), "2026-10-31T07:00:00Z");
    fire(&dir, "2026-10-31T15:07:00+00:00");
    event(&dir, "trigger-2f9c", "2026-11-01T03:00:00+00:00");
    event(&dir, "close-blocked:request-8a1d", "2026-11-01T09:00:00+00:00");
    fire(&dir, "2026-11-01T16:07:00+00:00");
    let window = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(window.since, at("2026-10-31T15:07:00Z"));
    assert_eq!(window.until, at("2026-11-01T16:07:00Z"));
  }

  #[test]
  fn the_monday_to_thursday_catch_up_keeps_consecutive_windows_adjacent() {
    let root = tempfile::tempdir().unwrap();
    let dir = session(root.path(), "2026-10-26T07:00:00Z");
    fire(&dir, "2026-10-26T15:07:00+00:00");
    fire(&dir, "2026-10-28T17:00:00+00:00");
    let catch_up = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(catch_up, Window { since: at("2026-10-26T15:07:00Z"), until: at("2026-10-28T17:00:00Z") });

    fire(&dir, "2026-10-29T15:07:00+00:00");
    let next = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(next, Window { since: at("2026-10-28T17:00:00Z"), until: at("2026-10-29T15:07:00Z") });
  }

  #[test]
  fn the_first_fire_starts_at_the_session_creation_time() {
    let root = tempfile::tempdir().unwrap();
    let dir = session(root.path(), "2026-10-31T07:13:44Z");
    fire(&dir, "2026-10-31T15:07:00+00:00");
    let window = cron_window(root.path(), "triage", "failure-triage").unwrap();
    assert_eq!(window, Window { since: at("2026-10-31T07:13:44Z"), until: at("2026-10-31T15:07:00Z") });
  }

  #[test]
  fn a_session_with_no_matching_fire_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let dir = session(root.path(), "2026-10-31T07:00:00Z");
    event(&dir, "trigger-2f9c", "2026-11-01T03:00:00+00:00");
    let error = cron_window(root.path(), "triage", "failure-triage").unwrap_err();
    assert!(error.to_string().contains("no scheduled_trigger"), "{error}");
  }
}
