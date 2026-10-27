//! Tests for Store-owned thinking state.

#[cfg(test)]
mod tests {
  use crate::metadata::UtcDateTime;
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
