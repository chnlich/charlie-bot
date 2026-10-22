//! Bounded stat-signature memos for the store's disk read paths.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::os::unix::fs::MetadataExt;

/// The Python memo signature shared by the NDJSON, archive, and trigger readers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSignature {
  pub(crate) mtime_ns: i128,
  pub(crate) size: u64,
}

impl FileSignature {
  pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
    Self {
      mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
      size: metadata.size(),
    }
  }
}

/// A locked-caller LRU map with the same get-touch/store-evict rule as `src/infra/memo.py`.
pub(crate) struct BoundedMemo<K, V> {
  limit: usize,
  entries: HashMap<K, (u64, V)>,
  order: VecDeque<(K, u64)>,
  next_order: u64,
}

impl<K, V> BoundedMemo<K, V>
where
  K: Clone + Eq + Hash,
  V: Clone,
{
  pub(crate) fn new(limit: usize) -> Self {
    Self { limit, entries: HashMap::new(), order: VecDeque::new(), next_order: 0 }
  }

  pub(crate) fn get(&mut self, key: &K) -> Option<V> {
    let order = self.next_order;
    self.next_order = self.next_order.checked_add(1).expect("memo LRU order counter is exhausted");
    let (stored_order, value) = self.entries.get_mut(key)?;
    *stored_order = order;
    let value = value.clone();
    self.order.push_back((key.clone(), order));
    self.compact_if_needed();
    Some(value)
  }

  pub(crate) fn store(&mut self, key: K, value: V) {
    let order = self.next_order;
    self.next_order = self.next_order.checked_add(1).expect("memo LRU order counter is exhausted");
    self.entries.insert(key.clone(), (order, value));
    self.order.push_back((key, order));
    while self.entries.len() > self.limit {
      let (oldest, oldest_order) = self.order.pop_front().expect("bounded memo order is non-empty");
      if self.entries.get(&oldest).is_some_and(|(order, _)| *order == oldest_order) {
        self.entries.remove(&oldest);
      }
    }
    self.compact_if_needed();
  }

  fn compact_if_needed(&mut self) {
    let threshold = self.limit.saturating_mul(4).max(64);
    if self.order.len() > threshold {
      self.order.retain(|(key, order)| self.entries.get(key).is_some_and(|(stored, _)| stored == order));
    }
  }
}

/// A bounded LRU whose entries are served only while `(mtime_ns, size)` still matches.
pub(crate) struct StatSignatureMemo<K, V> {
  entries: BoundedMemo<K, (FileSignature, V)>,
}

impl<K, V> StatSignatureMemo<K, V>
where
  K: Clone + Eq + Hash,
  V: Clone,
{
  pub(crate) fn new(limit: usize) -> Self {
    Self { entries: BoundedMemo::new(limit) }
  }

  pub(crate) fn fresh(&mut self, key: &K, signature: FileSignature) -> Option<V> {
    let (stored_signature, value) = self.entries.get(key)?;
    (stored_signature == signature).then_some(value)
  }

  pub(crate) fn record(&mut self, key: K, signature: FileSignature, value: V) {
    self.entries.store(key, (signature, value));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bounded_memo_touches_hits_and_evicts_the_oldest_entry() {
    let mut memo = BoundedMemo::new(2);
    memo.store("a", 1);
    memo.store("b", 2);
    assert_eq!(memo.get(&"a"), Some(1));
    memo.store("c", 3);
    assert_eq!(memo.get(&"a"), Some(1));
    assert_eq!(memo.get(&"b"), None);
    assert_eq!(memo.get(&"c"), Some(3));

    for _ in 0..1000 {
      assert_eq!(memo.get(&"a"), Some(1));
      memo.store("c", 3);
    }
    assert_eq!(memo.entries.len(), 2);
    assert_eq!(memo.get(&"a"), Some(1));
  }
}
