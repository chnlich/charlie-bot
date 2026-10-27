//! Bounded stat-signature memos for the store's disk read paths.

use std::hash::Hash;
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;

use lru::LruCache;

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

/// A locked-caller LRU map with the same get-touch/store-evict rule as `src/infra/memo.py`, which wraps
/// `collections.OrderedDict`. `lru::LruCache` is that primitive in Rust.
pub(crate) struct BoundedMemo<K, V> {
  entries: LruCache<K, V>,
}

impl<K, V> BoundedMemo<K, V>
where
  K: Eq + Hash,
  V: Clone,
{
  pub(crate) fn new(limit: usize) -> Self {
    Self { entries: LruCache::new(NonZeroUsize::new(limit).expect("a memo limit is nonzero")) }
  }

  pub(crate) fn get(&mut self, key: &K) -> Option<V> {
    self.entries.get(key).cloned()
  }

  pub(crate) fn store(&mut self, key: K, value: V) {
    self.entries.put(key, value);
  }

  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.entries.len()
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
    assert_eq!(memo.len(), 2);
    assert_eq!(memo.get(&"a"), Some(1));
  }
}
