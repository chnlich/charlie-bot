//! The registry's answers to the hook traits the store calls (`charliebot_store::StoreHooks`). The store names no
//! package: it asks these adapters, which read the registered backend lifecycles and sequence controllers. The server
//! injects them at startup, when it constructs the store.

use std::collections::{HashMap, HashSet};
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

  fn sequence_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
    Ok(subtree_roots(metas, |meta| self.registry.sequence_owns_session(meta), false))
  }

  fn view_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    let mut view_roots: HashMap<&'static str, HashSet<String>> = HashMap::new();
    for meta in metas {
      for contribution in self.registry.sidebar_contributions() {
        if let Some(view) = contribution.view_member(meta) {
          view_roots.entry(view).or_default().insert(meta.id.clone());
        }
      }
    }
    Ok(
      view_roots
        .into_iter()
        .map(|(view, roots)| {
          let members = subtree_roots(metas, |meta| roots.contains(&meta.id), true);
          (view.to_string(), members)
        })
        .collect(),
    )
  }
}

fn subtree_roots(
  metas: &[SessionMetadata],
  is_root: impl Fn(&SessionMetadata) -> bool,
  include_root: bool,
) -> HashMap<String, String> {
  let by_id: HashMap<&str, &SessionMetadata> = metas.iter().map(|meta| (meta.id.as_str(), meta)).collect();
  let mut roots: HashMap<String, String> = HashMap::new();
  for start in metas {
    let mut chain = Vec::new();
    let mut seen = HashSet::from([start.id.as_str()]);
    let mut root = if include_root && is_root(start) { Some(start.id.as_str()) } else { None };
    let mut parent_id = start.task_parent_id.as_deref();
    while root.is_none() {
      let Some(parent) = parent_id else { break };
      if let Some(root_id) = roots.get(parent) {
        root = Some(root_id.as_str());
        break;
      }
      let Some(parent_meta) = by_id.get(parent).copied() else { break };
      if !seen.insert(parent_meta.id.as_str()) {
        break;
      }
      if is_root(parent_meta) {
        root = Some(parent_meta.id.as_str());
        break;
      }
      chain.push(parent_meta.id.as_str());
      parent_id = parent_meta.task_parent_id.as_deref();
    }
    if let Some(root_id) = root {
      let root_owned = root_id.to_string();
      for member in chain.into_iter().chain(std::iter::once(start.id.as_str())) {
        roots.insert(member.to_string(), root_owned.clone());
      }
      if include_root {
        roots.insert(root_owned.clone(), root_owned);
      }
    }
  }
  roots
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

  fn mark_sidebar_dirty(&self, session_id: &str) {
    charliebot_store::sidebar_state::mark_sidebar_dirty(session_id);
  }

  fn drop_session_runtime_state(&self, _session_id: &str) {}
}
