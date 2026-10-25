//! The bounded LRU memo of the diff view (`src/infra/memo.py` `BoundedMemo`): a hit refreshes the key's recency, and a
//! store evicts the least recently used entries past the limit.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

pub(crate) struct BoundedMemo<K, V> {
  entries: Mutex<LruCache<K, V>>,
}

impl<K: Eq + std::hash::Hash, V: Clone> BoundedMemo<K, V> {
  pub(crate) fn new(limit: usize) -> Self {
    let limit = NonZeroUsize::new(limit).expect("the memo limit is positive");
    Self { entries: Mutex::new(LruCache::new(limit)) }
  }

  /// The value stored under `key`; a hit moves the key to the most recent end.
  pub(crate) fn get(&self, key: &K) -> Option<V> {
    let mut entries = self.entries.lock().expect("the memo lock is never poisoned");
    entries.get(key).cloned()
  }

  /// Stores `value` under `key` as the most recent entry, then evicts past the limit.
  pub(crate) fn store(&self, key: K, value: V) {
    let mut entries = self.entries.lock().expect("the memo lock is never poisoned");
    entries.put(key, value);
  }

  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.entries.lock().expect("the memo lock is never poisoned").len()
  }
}

#[cfg(test)]
mod tests {
  use super::BoundedMemo;

  #[test]
  fn a_store_past_the_limit_evicts_the_least_recently_used_key() {
    let memo = BoundedMemo::new(2);
    memo.store("a", 1);
    memo.store("b", 2);
    assert_eq!(memo.get(&"a"), Some(1));
    memo.store("c", 3);
    assert_eq!(memo.len(), 2);
    assert_eq!(memo.get(&"b"), None);
    assert_eq!(memo.get(&"a"), Some(1));
    assert_eq!(memo.get(&"c"), Some(3));
  }

  #[test]
  fn storing_an_existing_key_replaces_its_value_without_growing() {
    let memo = BoundedMemo::new(2);
    memo.store("a", 1);
    memo.store("a", 2);
    assert_eq!(memo.len(), 1);
    assert_eq!(memo.get(&"a"), Some(2));
  }
}
