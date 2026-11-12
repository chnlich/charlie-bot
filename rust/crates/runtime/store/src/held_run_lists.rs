//! Run listings held in memory for the startup walk.
//!
//! A node pass lists its session's run records several times (the pass itself, its follow-ups, the dispatch of pending
//! inputs), and each listing reads every run's metadata file. The walk lists each full-pass session once in its
//! parallel phase and holds the listing; while the hold lasts, run-record reads of that session answer from it. The
//! store is the only writer of run records during the walk, and each of its run-record writes and session deletions
//! drops the session's listing first. So every read still returns what the disk holds. The hold ends with the walk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::run_record::RunRecord;

/// The held listings, each in the listing's `(started_at, id)` order; `None` outside a hold.
#[derive(Default)]
pub(crate) struct HeldRunLists(Mutex<Option<HashMap<String, Arc<Vec<RunRecord>>>>>);

impl HeldRunLists {
  /// Start a hold; it ends when the returned guard drops, which frees the listings.
  pub(crate) fn hold(&self) -> RunListHold<'_> {
    self.0.lock().expect("held run lists mutex is not poisoned").get_or_insert_with(HashMap::new);
    RunListHold { lists: self }
  }

  /// Keep `records` as the session's listing while a hold lasts; outside a hold, keep nothing.
  pub(crate) fn keep(&self, session_id: &str, records: Arc<Vec<RunRecord>>) {
    if let Some(lists) = self.0.lock().expect("held run lists mutex is not poisoned").as_mut() {
      lists.insert(session_id.to_string(), records);
    }
  }

  /// The session's held listing, while a hold holds one.
  pub(crate) fn list(&self, session_id: &str) -> Option<Arc<Vec<RunRecord>>> {
    self.0.lock().expect("held run lists mutex is not poisoned").as_ref()?.get(session_id).cloned()
  }

  /// Drop the session's listing ahead of a change to its run records.
  pub(crate) fn evict(&self, session_id: &str) {
    if let Some(lists) = self.0.lock().expect("held run lists mutex is not poisoned").as_mut() {
      lists.remove(session_id);
    }
  }
}

/// A hold of run listings; dropping it ends the hold and frees the listings.
pub struct RunListHold<'a> {
  lists: &'a HeldRunLists,
}

impl Drop for RunListHold<'_> {
  fn drop(&mut self) {
    self.lists.0.lock().expect("held run lists mutex is not poisoned").take();
  }
}
