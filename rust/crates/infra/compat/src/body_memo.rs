//! The bounded memo of response bodies: at most `limit` bodies, each no longer than the memo's byte cap.
//!
//! A response body's bytes follow from the version of the resource the response answers for, so a repeat request that
//! names the same version answers from the memo instead of reading the file or deflating the body again. The memo
//! drops entries in insertion order, so a full memo frees the entry stored longest ago. A body past the cap is not
//! kept: the caller sends it and the next request produces it again, which keeps one large response from pinning the
//! memo's memory.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use bytes::Bytes;

/// The bodies one key space keeps, oldest first.
pub struct BodyMemo<K> {
  bodies: HashMap<K, Bytes>,
  /// The keys in insertion order, so a full memo drops the entry stored longest ago.
  order: VecDeque<K>,
  limit: usize,
  max_body_bytes: usize,
}

impl<K: Clone + Eq + Hash> BodyMemo<K> {
  /// A memo that holds at most `limit` bodies, each no longer than `max_body_bytes`.
  pub fn new(limit: usize, max_body_bytes: usize) -> Self {
    Self { bodies: HashMap::new(), order: VecDeque::new(), limit, max_body_bytes }
  }

  /// The body stored under `key`, or `None` when the memo holds none.
  pub fn get(&self, key: &K) -> Option<Bytes> {
    self.bodies.get(key).cloned()
  }

  /// The storage of `body` under `key`, which drops a body longer than the memo's cap. Storing a key the memo already
  /// holds replaces its body and keeps the key's place in the insertion order.
  pub fn store(&mut self, key: K, body: Bytes) {
    if body.len() > self.max_body_bytes {
      return;
    }
    if self.bodies.insert(key.clone(), body).is_none() {
      self.order.push_back(key);
      while self.order.len() > self.limit {
        if let Some(oldest) = self.order.pop_front() {
          self.bodies.remove(&oldest);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use bytes::Bytes;

  use super::BodyMemo;

  fn body(text: &str) -> Bytes {
    Bytes::from(text.to_string())
  }

  #[test]
  fn a_store_past_the_limit_drops_the_entry_stored_longest_ago() {
    let mut memo = BodyMemo::new(2, 16);
    memo.store("a", body("one"));
    memo.store("b", body("two"));
    assert_eq!(memo.get(&"a"), Some(body("one")));
    memo.store("c", body("three"));
    assert_eq!(memo.get(&"a"), None);
    assert_eq!(memo.get(&"b"), Some(body("two")));
    assert_eq!(memo.get(&"c"), Some(body("three")));
  }

  #[test]
  fn a_body_past_the_cap_is_not_kept() {
    let mut memo = BodyMemo::new(2, 3);
    memo.store("long", body("four"));
    memo.store("short", body("ok"));
    assert_eq!(memo.get(&"long"), None);
    assert_eq!(memo.get(&"short"), Some(body("ok")));
  }

  #[test]
  fn storing_a_held_key_replaces_the_body_and_keeps_its_place_in_the_order() {
    let mut memo = BodyMemo::new(2, 16);
    memo.store("a", body("one"));
    memo.store("b", body("two"));
    memo.store("a", body("three"));
    assert_eq!(memo.get(&"a"), Some(body("three")));
    memo.store("c", body("four"));
    assert_eq!(memo.get(&"a"), None);
    assert_eq!(memo.get(&"b"), Some(body("two")));
    assert_eq!(memo.get(&"c"), Some(body("four")));
  }
}
