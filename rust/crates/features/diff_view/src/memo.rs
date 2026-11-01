//! The diff view's two memos: the core bounded LRU memo behind the lock the shared `GitState` needs.

use std::hash::Hash;
use std::sync::Mutex;

use charliebot_core::memo::BoundedMemo;

/// The memo behind a lock, because every handler reaches it through the one shared `GitState`.
pub(crate) struct LockedMemo<K, V> {
  entries: Mutex<BoundedMemo<K, V>>,
}

impl<K: Eq + Hash, V: Clone> LockedMemo<K, V> {
  pub(crate) fn new(limit: usize) -> Self {
    Self { entries: Mutex::new(BoundedMemo::new(limit)) }
  }

  /// The value stored under `key`; a hit moves the key to the most recent end.
  pub(crate) fn get(&self, key: &K) -> Option<V> {
    self.entries.lock().expect("the memo lock is never poisoned").get(key)
  }

  /// Stores `value` under `key` as the most recent entry, then evicts past the limit.
  pub(crate) fn store(&self, key: K, value: V) {
    self.entries.lock().expect("the memo lock is never poisoned").store(key, value);
  }

  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.entries.lock().expect("the memo lock is never poisoned").len()
  }
}
