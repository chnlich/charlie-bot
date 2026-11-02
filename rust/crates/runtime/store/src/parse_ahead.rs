//! The parse-ahead of a session's live event log: a parse and fold taken outside the session lock, installed later as
//! the session's cached history and its first fold.
//!
//! The startup walk parses the live logs of its full-pass sessions in parallel before the node passes run one at a
//! time, so each pass starts from a warm cache. An install checks the log's identity under the session lock, so a parse
//! that an append overtook is dropped and the next read loads the file.

use std::sync::Arc;

use crate::event_types::{FileIdentity, SharedChatEvent};
use crate::run_facts::RunFacts;
use crate::store::CachedChatEvents;
use crate::task_tree::fold_live_history;
use crate::task_tree_state::NodeFacts;
use crate::{Store, StoreResult};

/// A live event log parsed outside the session lock, and the identity of the file it was parsed from.
pub struct ParsedLiveHistory {
  identity: Option<FileIdentity>,
  events: Arc<Vec<SharedChatEvent>>,
  /// The node and run facts folded from `events`, present when the session held no archived events at the parse.
  folded: Option<(NodeFacts, RunFacts)>,
}

impl Store {
  /// Parse and fold a session's live log outside its session lock, for a later [`Store::install_live_history`].
  ///
  /// The log's identity is read before and after the parse; `None` means the file changed during the read, so the parse
  /// may hold a torn view and is discarded. When the session holds no archived events, the parse also folds the node and
  /// run facts that a first read of the history folds. The caches stay as they are.
  pub fn parse_live_history(&self, session_id: &str) -> StoreResult<Option<ParsedLiveHistory>> {
    let path = self.chat_events_path(session_id);
    let identity = FileIdentity::read(&path)?;
    let events: Arc<Vec<SharedChatEvent>> =
      Arc::new(self.read_chat_events_file(&path)?.into_iter().map(Arc::new).collect());
    if FileIdentity::read(&path)? != identity {
      return Ok(None);
    }
    let folded = if self.read_archive_offset(session_id)? == 0 { Some(fold_live_history(&events)?) } else { None };
    Ok(Some(ParsedLiveHistory { identity, events, folded }))
  }

  /// Make a [`Store::parse_live_history`] result the session's cached live events, as a cold read would have loaded
  /// them, and seed its folded facts as that read's first fold would have. The install happens only while the cache is
  /// cold and the log still has the identity the parse read, checked under the session lock that every append holds;
  /// otherwise the parse is dropped and the next read loads the file. The facts are seeded only while the session still
  /// holds no archived events. Returns whether the parse was installed.
  pub fn install_live_history(&self, session_id: &str, parsed: ParsedLiveHistory) -> StoreResult<bool> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    if cache.is_some() || FileIdentity::read(&self.chat_events_path(session_id))? != parsed.identity {
      return Ok(false);
    }
    let generation = self.next_events_cache_generation();
    let list_identity = Arc::new(());
    *cache =
      Some(CachedChatEvents { generation, identity: Arc::clone(&list_identity), events: Arc::clone(&parsed.events) });
    if let Some((facts, runs)) = parsed.folded
      && self.read_archive_offset(session_id)? == 0
    {
      self.seed_node_facts(session_id, generation, list_identity, &parsed.events, facts, runs);
    }
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::store::test_support::make_test_store;

  /// A parse taken outside the session lock becomes the cache only while the log still has the identity it parsed.
  #[tokio::test]
  async fn a_parsed_history_installs_only_while_the_log_keeps_its_parsed_identity() {
    let (_home, store) = make_test_store();
    // An append to a cold cache writes only the log, so the cache stays cold throughout.
    store.append_chat_event("synthetic", json!({"id": "event-a", "type": "user"})).await.unwrap();
    let stale = store.parse_live_history("synthetic").unwrap().expect("nothing writes during the parse");
    store.append_chat_event("synthetic", json!({"id": "event-b", "type": "user"})).await.unwrap();
    assert!(!store.install_live_history("synthetic", stale).unwrap(), "a parse that misses an append is refused");

    let fresh = store.parse_live_history("synthetic").unwrap().expect("nothing writes during the parse");
    assert!(store.install_live_history("synthetic", fresh).unwrap(), "a parse of the current log installs");
    let again = store.parse_live_history("synthetic").unwrap().expect("nothing writes during the parse");
    assert!(!store.install_live_history("synthetic", again).unwrap(), "a warm cache keeps its list");
    let ids: Vec<String> = store
      .load_chat_events_shared_sync("synthetic")
      .unwrap()
      .iter()
      .map(|event| event["id"].as_str().unwrap().to_string())
      .collect();
    assert_eq!(ids, ["event-a", "event-b"]);
  }

  /// An installed parse leaves a restarted store where a cold read and its first fold leave it: the same facts, the
  /// same run facts and the same history work.
  #[tokio::test]
  async fn an_installed_parse_folds_the_facts_a_cold_read_folds() {
    let (home, writer) = make_test_store();
    for event in [
      json!({"type": "task_created", "actor": "user"}),
      json!({"type": "user", "content": "do the work"}),
      json!({"type": "run_stop_requested", "run_id": "run-1", "request_id": "stop-1"}),
      json!({"type": "run_finished", "run_id": "run-1", "outcome": "success", "input_event_ids": ["input-1"]}),
      json!({"type": "child_report", "child_session_id": "child", "child_event_id": "close-1"}),
      json!({"type": "task_closed", "outcome": "completed"}),
    ] {
      writer.append_chat_event("synthetic", event).await.unwrap();
    }
    let open = || crate::store::test_support::open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks));
    let (installed, cold) = (open().unwrap(), open().unwrap());
    let parsed = installed.parse_live_history("synthetic").unwrap().expect("nothing writes during the parse");
    assert!(installed.install_live_history("synthetic", parsed).unwrap());
    assert_eq!(installed.history_work().full_folds, 1, "the install seeds the first fold");

    let (seeded, folded) = (installed.node_facts("synthetic").unwrap(), cold.node_facts("synthetic").unwrap());
    let (seeded, folded) = (seeded.task().unwrap(), folded.task().unwrap());
    assert_eq!(seeded.task_state, folded.task_state);
    assert_eq!(seeded.has_user_request, folded.has_user_request);
    assert_eq!(seeded.run_outcomes, folded.run_outcomes);
    assert_eq!(seeded.confirmed_input_ids, folded.confirmed_input_ids);
    assert_eq!(seeded.input_candidates, folded.input_candidates);
    assert_eq!((seeded.boundary_index, seeded.boundary_open), (folded.boundary_index, folded.boundary_open));
    assert_eq!(seeded.close_events, folded.close_events);
    assert_eq!(seeded.delivered_reports, folded.delivered_reports);
    assert_eq!((seeded.events_by_id.len(), seeded.covered), (folded.events_by_id.len(), folded.covered));
    assert_eq!(installed.run_facts("synthetic").unwrap(), cold.run_facts("synthetic").unwrap());
    assert_eq!(installed.history_work(), cold.history_work(), "the install does a cold read's work, once");
  }
}
