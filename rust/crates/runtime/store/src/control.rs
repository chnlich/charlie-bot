use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

/// Proof that the caller owns this store's tree-wide control lock.
///
/// The guard is intentionally not cloneable. Locked operations accept it by reference and never
/// acquire the lock again, so control-event writes cannot recurse through the lock.
pub struct ControlGuard {
  pub(crate) store_id: Uuid,
  _guard: OwnedMutexGuard<()>,
}

pub(crate) struct ControlLock {
  store_id: Uuid,
  mutex: Arc<Mutex<()>>,
}

impl ControlLock {
  pub(crate) fn new(store_id: Uuid) -> Self {
    Self { store_id, mutex: Arc::new(Mutex::new(())) }
  }

  pub(crate) async fn lock(&self) -> ControlGuard {
    let guard = Arc::clone(&self.mutex).lock_owned().await;
    ControlGuard { store_id: self.store_id, _guard: guard }
  }

  pub(crate) fn validate(&self, guard: &ControlGuard) -> Result<(), String> {
    if guard.store_id == self.store_id { Ok(()) } else { Err("control guard belongs to another store".to_string()) }
  }
}
