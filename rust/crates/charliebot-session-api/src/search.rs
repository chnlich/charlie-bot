//! Read-only session name and chat-content search.

use std::cmp::Reverse;
use std::fs;

use charliebot_store::metadata::{SessionMetadata, SessionStatus};
use charliebot_store::{Store, StoreResult};

const SEARCH_RESULT_LIMIT: usize = 200;

/// Search every session name and active-session chat content, newest first.
pub(crate) fn search_sessions(store: &Store, query: &str) -> StoreResult<Vec<SessionMetadata>> {
  if query.trim().is_empty() {
    return Ok(store.list_sessions(Some(SessionStatus::Active), None, None)?.rows);
  }

  let needle = query.to_lowercase();
  let all = store.load_session_metas(None)?;
  let mut names: Vec<SessionMetadata> =
    all.iter().filter(|meta| meta.name.to_lowercase().contains(&needle)).cloned().collect();
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);

  let mut content_hits = Vec::new();
  for meta in &all {
    if meta.status != SessionStatus::Active || meta.name.to_lowercase().contains(&needle) {
      continue;
    }
    let path = store.chat_events_path(&meta.id);
    match fs::read_to_string(&path) {
      Ok(content) if content.to_lowercase().contains(&needle) => content_hits.push(meta.clone()),
      Ok(_) => {}
      Err(error) => tracing::debug!(session_id = %meta.id, error = %error, "search_read_failed"),
    }
  }

  names.extend(content_hits);
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);
  Ok(names)
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::fs;
  use std::sync::Arc;

  use charliebot_store::hooks::EmptyRuntimeEventHooks;
  use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
  use charliebot_store::{
    AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields, Store,
    StoreHookFuture, StoreHooks, StoreResult,
  };
  use serde_json::{Map, Value};

  use super::search_sessions;

  struct TestLabels;

  impl AccountLabelKeeper for TestLabels {
    fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> StoreResult<bool> {
      Ok(false)
    }

    fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
      None
    }
  }

  struct TestAnchors;

  impl AnchorFieldReconciler for TestAnchors {
    fn reconcile(&self, _disk: &SessionMetadata, _attempted: &mut SessionMetadata) -> StoreResult<()> {
      Ok(())
    }
  }

  struct TestSequence;

  impl SequenceListingFields for TestSequence {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }

    fn listing_fields(
      &self,
      _session_ids: &[String],
      _now: UtcDateTime,
    ) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  struct TestArchive;

  impl SequenceArchiveHook for TestArchive {
    fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  struct TestBindingMover;

  impl SequenceBindingMover for TestBindingMover {
    fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(false) })
    }
  }

  fn test_store(home: &std::path::Path) -> Store {
    let config = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      config,
      Vec::new(),
      StoreHooks {
        account_labels: Some(Arc::new(TestLabels)),
        anchor_reconciler: Some(Arc::new(TestAnchors)),
        sequence_listing: Some(Arc::new(TestSequence)),
        runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
        sequence_archive: Some(Arc::new(TestArchive)),
        sequence_binding_mover: Some(Arc::new(TestBindingMover)),
      },
    )
    .unwrap()
  }

  fn write_session(store: &Store, id: &str, name: &str, status: SessionStatus, updated_at: &str) {
    let mut meta = SessionMetadata::new(id, name, TaskProfile::Manager);
    meta.status = status;
    meta.updated_at = UtcDateTime::parse(updated_at).unwrap();
    let path = store.metadata_path(id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, meta.to_json_string(&[], true)).unwrap();
  }

  #[test]
  fn blank_search_returns_active_sessions_newest_first() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "old", "old", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "archived", "archived", SessionStatus::Archived, "2026-08-01T12:02:00Z");
    write_session(&store, "new", "new", SessionStatus::Active, "2026-08-01T12:01:00Z");

    let rows = search_sessions(&store, "  \t").unwrap();
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["new", "old"]);
  }

  /// `test_archived_pagination.py::test_search_cap_keeps_content_hits_above_the_cap_line`.
  #[test]
  fn content_hits_compete_with_name_hits_at_the_two_hundred_row_cap() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    for index in 0..200 {
      write_session(
        &store,
        &format!("archived-{index:03}"),
        &format!("needle-{index:03}"),
        SessionStatus::Archived,
        &format!("2026-08-01T{:02}:{:02}:00Z", 12 + index / 60, index % 60),
      );
    }
    write_session(&store, "above", "unrelated above", SessionStatus::Active, "2026-08-02T04:40:00Z");
    write_session(&store, "below", "unrelated below", SessionStatus::Active, "2026-08-01T11:59:00Z");
    for id in ["above", "below"] {
      let path = store.chat_events_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, "{\"content\":\"needle in the events\"}\n").unwrap();
    }

    let rows = search_sessions(&store, "needle").unwrap();
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(rows.len(), 200);
    assert!(ids.contains(&"above"));
    assert!(!ids.contains(&"below"));
    assert!(!ids.contains(&"archived-000"));
  }

  /// `test_archived_pagination.py::test_search_match_memo_refreshes_when_chat_content_or_names_move`.
  #[test]
  fn search_reads_new_chat_content_and_names_on_the_next_request() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "quiet", "quiet", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "carrier", "carrier", SessionStatus::Active, "2026-08-01T12:01:00Z");
    assert!(search_sessions(&store, "needle").unwrap().is_empty());

    let events = store.chat_events_path("carrier");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, "{\"content\":\"needle in the events\"}\n").unwrap();
    let content_hits = search_sessions(&store, "needle").unwrap();
    assert_eq!(content_hits.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["carrier"]);

    write_session(&store, "named", "needle name", SessionStatus::Archived, "2026-08-01T12:02:00Z");
    let all_hits = search_sessions(&store, "needle").unwrap();
    assert_eq!(all_hits.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["named", "carrier"]);
  }
}
