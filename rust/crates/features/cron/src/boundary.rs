//! The cron step-boundary waiters: one watch channel per waiting scheduler tick.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::watch;

/// What one step-chain redrive reports about a firing's steps. `Running` means a step is still working or
/// has yet to start. `Finished` means the redrive found nothing left to start. `Errored` means the redrive
/// itself returned an error.
///
/// A step whose turn failed still ends its chain as `Finished`: the failure travels in the boundary report.
/// A settled chain stays settled, so the first terminal report is the one a waiter acts on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum BoundaryState {
  #[default]
  Running,
  Finished,
  Errored,
}

impl BoundaryState {
  /// The state to publish when `later` arrives. Only a running chain moves on.
  fn merged(self, later: BoundaryState) -> BoundaryState {
    match self {
      BoundaryState::Running => later,
      BoundaryState::Finished | BoundaryState::Errored => self,
    }
  }

  /// Whether the chain has ended, so the waiter can stop waiting.
  pub(super) fn is_settled(self) -> bool {
    match self {
      BoundaryState::Running => false,
      BoundaryState::Finished | BoundaryState::Errored => true,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BoundaryNotice {
  pub generation: u64,
  pub state: BoundaryState,
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

pub(super) fn notify_boundary(leaf_id: &str, owner_ref: &str, state: BoundaryState) {
  let Some(waiters) = BOUNDARY_WAITERS.get() else { return };
  let waiters = waiters.lock().expect("cron boundary waiter mutex is not poisoned");
  let Some(sender) = waiters.get(&(leaf_id.to_string(), owner_ref.to_string())) else { return };
  let previous = *sender.borrow();
  sender.send_replace(BoundaryNotice { generation: previous.generation + 1, state: previous.state.merged(state) });
}

pub(super) fn remove_boundary(leaf_id: &str, owner_ref: &str) {
  if let Some(waiters) = BOUNDARY_WAITERS.get() {
    waiters
      .lock()
      .expect("cron boundary waiter mutex is not poisoned")
      .remove(&(leaf_id.to_string(), owner_ref.to_string()));
  }
}
