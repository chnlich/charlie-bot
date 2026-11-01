//! The bounded LRU memo of `src/infra/memo.py`: a hit refreshes the key's recency, and a store evicts the least
//! recently used entries past the limit. The caller holds it behind the lock its own sharing needs.

use std::hash::Hash;
use std::num::NonZeroUsize;

use lru::LruCache;

/// The memo of `src/infra/memo.py` `BoundedMemo`, a map over `collections.OrderedDict`: `get` moves the key to the
/// most recent end, and `store` evicts past the limit. `lru::LruCache` is that primitive in Rust.
pub struct BoundedMemo<K, V> {
  entries: LruCache<K, V>,
}

impl<K, V> BoundedMemo<K, V>
where
  K: Eq + Hash,
  V: Clone,
{
  /// A memo that holds at most `limit` entries; `limit` must be positive.
  pub fn new(limit: usize) -> Self {
    Self { entries: LruCache::new(NonZeroUsize::new(limit).expect("a memo limit is nonzero")) }
  }

  /// The value stored under `key`; a hit moves the key to the most recent end.
  pub fn get(&mut self, key: &K) -> Option<V> {
    self.entries.get(key).cloned()
  }

  /// Stores `value` under `key` as the most recent entry, then evicts past the limit.
  pub fn store(&mut self, key: K, value: V) {
    self.entries.put(key, value);
  }

  /// The number of entries held now.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether no entry is held.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::BoundedMemo;

  #[test]
  fn a_store_past_the_limit_evicts_the_least_recently_used_key() {
    let mut memo = BoundedMemo::new(2);
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
    let mut memo = BoundedMemo::new(2);
    memo.store("a", 1);
    memo.store("a", 2);
    assert_eq!(memo.len(), 1);
    assert_eq!(memo.get(&"a"), Some(2));
  }
}
