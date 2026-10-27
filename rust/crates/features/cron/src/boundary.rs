//! The cron step-boundary waiters: one watch channel per waiting scheduler tick (`src/features/cron/scheduler.py`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BoundaryNotice {
  pub generation: u64,
  pub done: bool,
  pub failed: bool,
}

type BoundaryKey = (String, String);
static BOUNDARY_WAITERS: OnceLock<Mutex<HashMap<BoundaryKey, watch::Sender<BoundaryNotice>>>> = OnceLock::new();

pub(super) fn subscribe_boundary(leaf_id: &str, owner_ref: &str) -> watch::Receiver<BoundaryNotice> {
  let waiters = BOUNDARY_WAITERS.get_or_init(|| Mutex::new(HashMap::new()));
  let mut waiters = waiters.lock().expect("cron boundary waiter mutex is not poisoned");
  waiters
    .entry((leaf_id.to_string(), owner_ref.to_string()))
    .or_insert_with(|| watch::channel(BoundaryNotice::default()).0)
    .subscribe()
}

pub(super) fn notify_boundary(leaf_id: &str, owner_ref: &str, done: bool, failed: bool) {
  let Some(waiters) = BOUNDARY_WAITERS.get() else { return };
  let waiters = waiters.lock().expect("cron boundary waiter mutex is not poisoned");
  let Some(sender) = waiters.get(&(leaf_id.to_string(), owner_ref.to_string())) else { return };
  let previous = *sender.borrow();
  sender.send_replace(BoundaryNotice {
    generation: previous.generation + 1,
    done: previous.done || done,
    failed: previous.failed || failed,
  });
}

pub(super) fn remove_boundary(leaf_id: &str, owner_ref: &str) {
  if let Some(waiters) = BOUNDARY_WAITERS.get() {
    waiters
      .lock()
      .expect("cron boundary waiter mutex is not poisoned")
      .remove(&(leaf_id.to_string(), owner_ref.to_string()));
  }
}
