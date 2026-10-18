use std::collections::HashMap;
use std::sync::Mutex;

use crate::metadata::UtcDateTime;
use crate::{Store, StoreResult};

#[derive(Default)]
struct ThinkingMaps {
  busy_since: HashMap<String, UtcDateTime>,
  run_busy: HashMap<String, String>,
  run_backends: HashMap<String, String>,
}

/// In-memory busy intervals and worker display backends.
#[derive(Default)]
pub(crate) struct ThinkingState {
  maps: Mutex<ThinkingMaps>,
}

impl Store {
  /// Open a busy interval, keeping the earlier start when the session is already busy.
  pub fn mark_busy(&self, session_id: &str, since: UtcDateTime) -> StoreResult<(UtcDateTime, bool)> {
    Ok(self.mark_busy_inner(session_id, since))
  }

  fn mark_busy_inner(&self, session_id: &str, since: UtcDateTime) -> (UtcDateTime, bool) {
    let mut maps = self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned");
    let result = match maps.busy_since.get(session_id) {
      Some(existing) => (*existing, false),
      None => {
        maps.busy_since.insert(session_id.to_string(), since);
        (since, true)
      }
    };
    drop(maps);
    if result.1 {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
    result
  }

  /// Clear a busy interval when one is present.
  pub fn clear_busy(&self, session_id: &str) {
    let removed =
      self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").busy_since.remove(session_id);
    if removed.is_some() {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
  }

  /// Return the current busy interval start.
  pub fn busy_since(&self, session_id: &str) -> Option<UtcDateTime> {
    self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").busy_since.get(session_id).copied()
  }

  /// Open the busy interval owned by one worker run.
  pub fn mark_run_busy(&self, session_id: &str, run_id: &str, since: UtcDateTime) {
    self
      .thinking_state
      .maps
      .lock()
      .expect("thinking state mutex is not poisoned")
      .run_busy
      .insert(session_id.to_string(), run_id.to_string());
    self.mark_busy_inner(session_id, since);
  }

  /// Clear a worker busy interval only when the run id owns it.
  pub fn clear_run_busy(&self, session_id: &str, run_id: &str) {
    let cleared = {
      let mut maps = self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned");
      if maps.run_busy.get(session_id).map(String::as_str) != Some(run_id) {
        false
      } else {
        maps.run_busy.remove(session_id);
        maps.busy_since.remove(session_id).is_some()
      }
    };
    if cleared {
      self.runtime_hooks.mark_sidebar_dirty(session_id);
    }
  }

  /// Record the newest worker run backend for display.
  pub fn note_run_backend(&self, session_id: &str, backend: &str) {
    if !backend.is_empty() {
      self
        .thinking_state
        .maps
        .lock()
        .expect("thinking state mutex is not poisoned")
        .run_backends
        .insert(session_id.to_string(), backend.to_string());
    }
  }

  /// Return the newest worker run backend known for display.
  pub fn run_backend(&self, session_id: &str) -> Option<String> {
    self.thinking_state.maps.lock().expect("thinking state mutex is not poisoned").run_backends.get(session_id).cloned()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::make_test_store;

  #[test]
  fn busy_state_keeps_its_first_start_and_marks_sidebar_only_on_flips() {
    let (_home, store) = make_test_store();
    let first = UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    let second = UtcDateTime::parse("2026-01-01T00:01:00Z").unwrap();
    assert_eq!(store.mark_busy("synthetic", first).unwrap(), (first, true));
    assert_eq!(store.mark_busy("synthetic", second).unwrap(), (first, false));
    assert_eq!(store.busy_since("synthetic"), Some(first));
    store.clear_busy("synthetic");
    store.clear_busy("synthetic");
    assert_eq!(store.busy_since("synthetic"), None);
  }

  #[test]
  fn worker_run_finish_clears_only_the_run_that_opened_the_interval() {
    let (_home, store) = make_test_store();
    let since = UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    store.mark_run_busy("synthetic", "run-1", since);
    store.clear_run_busy("synthetic", "other-run");
    assert_eq!(store.busy_since("synthetic"), Some(since));
    store.clear_run_busy("synthetic", "run-1");
    assert_eq!(store.busy_since("synthetic"), None);
  }

  #[test]
  fn worker_run_opens_its_busy_interval_at_its_recorded_start_and_names_its_backend() {
    let (_home, store) = make_test_store();
    let started = UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    store.mark_run_busy("worker", "run-1", started);
    store.note_run_backend("worker", "fake");
    assert_eq!(store.busy_since("worker"), Some(started));
    assert_eq!(store.run_backend("worker"), Some("fake".to_string()));
    assert_eq!(store.busy_since("root"), None);
  }

  #[test]
  fn a_worker_run_finish_leaves_the_parent_busy_interval_open() {
    let (_home, store) = make_test_store();
    let queued = UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    let started = UtcDateTime::parse("2026-01-01T00:05:00Z").unwrap();
    store.mark_busy("root", queued).unwrap();
    store.mark_run_busy("worker", "run-1", started);
    store.clear_run_busy("worker", "run-1");
    assert_eq!(store.busy_since("worker"), None);
    assert_eq!(store.busy_since("root"), Some(queued));
  }

  #[test]
  fn an_empty_backend_name_keeps_the_previous_run_backend() {
    let (_home, store) = make_test_store();
    store.note_run_backend("worker", "fake");
    store.note_run_backend("worker", "");
    assert_eq!(store.run_backend("worker"), Some("fake".to_string()));
  }
}
