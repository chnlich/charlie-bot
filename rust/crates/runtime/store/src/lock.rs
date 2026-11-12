//! The lock and poison rule for the store's in-memory caches.

use std::sync::{Mutex, MutexGuard};

/// Locks `mutex`, naming `what` in the panic that a poisoned lock raises.
///
/// A panic that abandoned a write may leave the guarded state half-written, so every later reader
/// must fail loud rather than serve it.
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
  mutex.lock().unwrap_or_else(|_| panic!("{what} is not poisoned"))
}
