//! The per-draft send lock.
//!
//! One lock per `(session, draft)` wraps the sendability check and the platform send, so a double click sends one
//! message and its second request reads the logged `draft_sent` as a conflict. A lock lives only while a request
//! holds it: the map keeps a weak reference, and an entry whose request has ended is dropped.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

static LOCKS: LazyLock<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Take the lock for one draft, waiting while another request holds it.
pub async fn acquire(session_id: &str, draft_id: &str) -> OwnedMutexGuard<()> {
  let key = format!("{session_id}/{draft_id}");
  let lock = {
    let mut locks = LOCKS.lock().expect("the draft lock map mutex is not poisoned");
    locks.retain(|_, weak| weak.strong_count() > 0);
    match locks.get(&key).and_then(Weak::upgrade) {
      Some(lock) => lock,
      None => {
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
      }
    }
  };
  lock.lock_owned().await
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  #[tokio::test]
  async fn one_draft_lock_serializes_two_holders_and_another_draft_does_not_wait() {
    let first = acquire("session-lock", "draft-lock").await;
    let other = tokio::time::timeout(Duration::from_secs(1), acquire("session-lock", "other-draft")).await;
    assert!(other.is_ok(), "a different draft takes its own lock without waiting");
    drop(other.unwrap());

    let waiting = tokio::spawn(async move { acquire("session-lock", "draft-lock").await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiting.is_finished(), "the second holder of one draft waits");
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(1), waiting).await.expect("the lock releases");
    drop(second.unwrap());
  }
}
