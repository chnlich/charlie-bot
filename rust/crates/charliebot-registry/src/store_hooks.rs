//! The registry's answers to the hook traits the store calls (`charliebot_store::StoreHooks`). The store names no
//! package: it asks these adapters, which read the registered backend lifecycles and sequence controllers. The server
//! injects them at startup, when it constructs the store.

use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_store::hooks::{AfterTurnWork, EventRenderer, RuntimeEventHooks};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields,
  StoreError, StoreHookFuture, StoreHooks, StoreResult,
};
use serde_json::{Map, Value};

use crate::registry::Registry;

/// The store's hooks, answered by the registered implementations.
pub fn store_hooks(registry: Arc<Registry>) -> StoreHooks {
  let account_labels: Arc<dyn AccountLabelKeeper> =
    Arc::new(LifecycleAccountLabels { registry: Arc::clone(&registry) });
  let anchor_reconciler: Arc<dyn AnchorFieldReconciler> =
    Arc::new(LifecycleAnchors { registry: Arc::clone(&registry) });
  let sequence_listing: Arc<dyn SequenceListingFields> =
    Arc::new(RegisteredSequences { registry: Arc::clone(&registry) });
  let sequence_archive: Arc<dyn SequenceArchiveHook> =
    Arc::new(RegisteredSequenceArchive { registry: Arc::clone(&registry) });
  let sequence_binding_mover: Arc<dyn SequenceBindingMover> =
    Arc::new(RegisteredSequenceBindingMover { registry: Arc::clone(&registry) });
  let runtime_events: Arc<dyn RuntimeEventHooks> = Arc::new(RegistryRuntimeEvents { registry });
  StoreHooks {
    account_labels: Some(account_labels),
    anchor_reconciler: Some(anchor_reconciler),
    sequence_listing: Some(sequence_listing),
    sequence_archive: Some(sequence_archive),
    sequence_binding_mover: Some(sequence_binding_mover),
    runtime_events: Some(runtime_events),
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

  // No registered sequence controller answers these yet, so the registered set is empty.
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

/// The sequence archive duty of a node's registered binding.
struct RegisteredSequenceArchive {
  registry: Arc<Registry>,
}

impl SequenceArchiveHook for RegisteredSequenceArchive {
  fn on_archive<'a>(&'a self, session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async move {
      let binding = self.registry.binding_for(session_id).map_err(|error| StoreError::new(error.to_string()))?;
      if let Some(binding) = binding {
        binding.on_archive().await.map_err(|error| StoreError::new(error.to_string()))?;
      }
      Ok(())
    })
  }
}

/// The bindings each registered sequence controller moves when an elone replaces a node.
struct RegisteredSequenceBindingMover {
  registry: Arc<Registry>,
}

impl SequenceBindingMover for RegisteredSequenceBindingMover {
  fn move_bindings<'a>(&'a self, source_session_id: &'a str, new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async move {
      let mut moved = false;
      for controller in self.registry.sequence_controllers() {
        moved |= controller
          .move_bindings(source_session_id, new_session_id)
          .await
          .map_err(|error| StoreError::new(error.to_string()))?;
      }
      Ok(moved)
    })
  }
}

/// Bridges feature turn contributions and event renderers into the store's runtime event hooks.
struct RegistryRuntimeEvents {
  registry: Arc<Registry>,
}

impl RuntimeEventHooks for RegistryRuntimeEvents {
  fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>> {
    let mut renderers = Vec::new();
    for (_, contribution) in self.registry.turn_contributions.pairs() {
      for (event_type, renderer_fn) in contribution.event_renderers() {
        let boxed: Arc<dyn EventRenderer> = Arc::new(move |event: &Value| {
          let res = renderer_fn(event);
          Ok(if res.is_null() { None } else { Some(res) })
        });
        renderers.push((event_type.to_string(), boxed));
      }
    }
    Ok(renderers)
  }

  fn after_turn(&self, _meta: SessionMetadata, _event: Value, _cfg: CharlieBotConfig) -> Vec<AfterTurnWork> {
    Vec::new()
  }

  fn mark_sidebar_dirty(&self, _session_id: &str) {}

  fn drop_session_runtime_state(&self, _session_id: &str) {}
}
