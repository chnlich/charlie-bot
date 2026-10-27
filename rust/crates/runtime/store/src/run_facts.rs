//! The bounded event-history memo shared by Store and run-record readers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::event_types::SharedChatEvent;

/// Run terminal outcomes, stop requests and withheld launches folded from one node history.
///
/// Recovery and dispatch read the same outcome and stop-request maps as Python's bounded run-facts memo.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunFacts {
  pub(crate) outcomes: HashMap<String, Option<String>>,
  pub(crate) stop_requests: HashMap<String, HashSet<Option<String>>>,
  #[cfg(test)]
  pub(crate) observed_events: usize,
}

const RUN_FACTS_MEMO_LIMIT: usize = 64;

#[derive(Clone)]
pub(crate) struct CachedRunFacts {
  pub(crate) identity: Arc<()>,
  pub(crate) events: Arc<Vec<SharedChatEvent>>,
  pub(crate) covered: usize,
  pub(crate) facts: Arc<RunFacts>,
}

/// A bounded memo keyed by the event list's identity (`Arc<()>`), which `runs.py::_RunFactsMemo` keeps as a plain
/// dict keyed by `id(events)`: lookups never touch the order, a replacement keeps its position, and eviction pops
/// the oldest insertion. That is a bounded FIFO, not the LRU `src/infra/memo.py` provides, so `lru::LruCache`
/// would move entries Python keeps in place, and no crate hashes by pointer identity. The struct stays hand-written.
#[derive(Default)]
pub(crate) struct RunFactsMemo {
  entries: VecDeque<CachedRunFacts>,
}

impl RunFactsMemo {
  pub(crate) fn get(&self, identity: &Arc<()>) -> Option<CachedRunFacts> {
    self.entries.iter().rfind(|entry| Arc::ptr_eq(&entry.identity, identity)).cloned()
  }

  pub(crate) fn insert(&mut self, entry: CachedRunFacts) {
    if let Some(existing) = self.entries.iter_mut().find(|existing| Arc::ptr_eq(&existing.identity, &entry.identity)) {
      *existing = entry;
      return;
    }
    self.entries.push_back(entry);
    while self.entries.len() > RUN_FACTS_MEMO_LIMIT {
      self.entries.pop_front();
    }
  }

  pub(crate) fn refresh_events(&mut self, identity: &Arc<()>, events: Arc<Vec<SharedChatEvent>>) {
    if let Some(entry) = self.entries.iter_mut().find(|entry| Arc::ptr_eq(&entry.identity, identity)) {
      entry.events = events;
    }
  }

  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.entries.len()
  }
}
