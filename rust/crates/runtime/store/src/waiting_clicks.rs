//! The status click lines of a user message: each one marks done the Waiting on you item at its 1-based position in
//! the list version it names. The task fold reads them into a session's click facts, and an append reads them to mark
//! the session's sidebar row for a fresh probe.

use crate::compact::CompactValue;
use crate::event_types::ChatEvent;
use crate::task_tree_state::valid_waiting_version;

/// The list version and 1-based position named by each matching status click line.
pub(crate) fn status_waiting_clicks(event: &ChatEvent, session_id: &str) -> Vec<(String, usize)> {
  const HEADER: &str = "[Status · Waiting on you]";

  let Some(content) = event.get("content").and_then(CompactValue::as_str) else {
    return Vec::new();
  };
  let mut lines = content.lines();
  if lines.next() != Some(HEADER) {
    return Vec::new();
  }
  let mut clicks = Vec::new();
  for line in lines {
    let Some((number, reference)) = line.split_once(". ▸ Waiting on you · session ") else {
      continue;
    };
    if number.parse::<usize>().ok().is_none_or(|number| number == 0) {
      continue;
    }
    let Some((owner, reference)) = reference.split_once(" · list ") else {
      continue;
    };
    if owner != session_id {
      continue;
    }
    let Some((version, reference)) = reference.split_once(" · since ") else {
      continue;
    };
    if !valid_waiting_version(version) {
      continue;
    }
    let Some((since, position)) = reference.rsplit_once(" · #") else {
      continue;
    };
    if since.is_empty() || position.is_empty() || !position.bytes().all(|byte| byte.is_ascii_digit()) {
      continue;
    }
    let Ok(position) = position.parse::<usize>() else {
      continue;
    };
    if position > 0 {
      clicks.push((version.to_string(), position));
    }
  }
  clicks
}
