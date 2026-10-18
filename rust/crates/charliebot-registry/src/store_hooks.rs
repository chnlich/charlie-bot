//! The registry's answers to the three hook traits the store calls (`charliebot_store::StoreHooks`). The store names no
//! package: it asks these adapters, which read the registered backend lifecycles and sequence controllers. The server
//! injects them at startup, when it constructs the store.

use std::collections::HashMap;
use std::sync::Arc;

use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{AccountLabelKeeper, AnchorFieldReconciler, SequenceListingFields, StoreHooks, StoreResult};
use serde_json::{Map, Value};

use crate::registry::Registry;

/// The store's hooks, answered by the registered implementations.
pub fn store_hooks(registry: Arc<Registry>) -> StoreHooks {
  let account_labels: Arc<dyn AccountLabelKeeper> =
    Arc::new(LifecycleAccountLabels { registry: Arc::clone(&registry) });
  let anchor_reconciler: Arc<dyn AnchorFieldReconciler> =
    Arc::new(LifecycleAnchors { registry: Arc::clone(&registry) });
  let sequence_listing: Arc<dyn SequenceListingFields> = Arc::new(RegisteredSequences { registry });
  StoreHooks {
    account_labels: Some(account_labels),
    anchor_reconciler: Some(anchor_reconciler),
    sequence_listing: Some(sequence_listing),
  }
}

/// The account label is a fact of the session: every registered lifecycle that keeps one reads and writes it.
struct LifecycleAccountLabels {
  registry: Arc<Registry>,
}

impl AccountLabelKeeper for LifecycleAccountLabels {
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
    let mut changed = false;
    for lifecycle in self.registry.lifecycles() {
      changed |= lifecycle.record_account_label(meta, label);
    }
    Ok(changed)
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    self.registry.lifecycles().iter().find_map(|lifecycle| lifecycle.account_label(meta))
  }
}

/// Restores the resume anchors and each lifecycle's account label from disk before an ordinary save.
struct LifecycleAnchors {
  registry: Arc<Registry>,
}

impl AnchorFieldReconciler for LifecycleAnchors {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
    if attempted.cc_session_id != disk.cc_session_id {
      tracing::warn!(session_id = %attempted.id, field = "cc_session_id", "session_anchor_write_corrected");
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
    }
    if attempted.native_backend != disk.native_backend {
      tracing::warn!(session_id = %attempted.id, field = "native_backend", "session_anchor_write_corrected");
      attempted.native_backend.clone_from(&disk.native_backend);
    }
    for lifecycle in self.registry.lifecycles() {
      let label_on_disk = lifecycle.account_label(disk);
      if lifecycle.account_label(attempted) != label_on_disk {
        tracing::warn!(
          session_id = %attempted.id,
          field = lifecycle.account_source(),
          "session_anchor_write_corrected"
        );
        lifecycle.record_account_label(attempted, label_on_disk.as_deref());
      }
    }
    Ok(())
  }
}

/// The sequence rows of the session listings, from the registered controllers.
struct RegisteredSequences {
  registry: Arc<Registry>,
}

impl SequenceListingFields for RegisteredSequences {
  fn owns_session(&self, meta: &SessionMetadata) -> bool {
    self.registry.sequence_owns_session(meta)
  }

  fn listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>> {
    Ok(self.registry.sequence_listing_fields(session_ids, now))
  }

  // No sequence controller answers these before the task tree (S3) lands, so the registered set is empty.
  fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    Ok(HashMap::new())
  }

  fn derived_archived_ids(&self) -> StoreResult<Vec<String>> {
    Ok(Vec::new())
  }
}
