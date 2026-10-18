//! Sequence hooks (`src/runtime/hooks/sequence_controllers.py`): a package that owns durable sequences registers a
//! controller. The controller owns every run whose sequence kind it declares, and it answers for its sessions in the
//! listings and in the bindings the runtime asks for.
//!
//! The run-record and task-tree methods of the Python controller (`redrive`, `launch_context`, `after_run`,
//! `recover_run`, `reconcile_interrupted`) need the run store (S2) and the task tree (S3). They join this trait with
//! those parts; `rust/docs/registry.md` lists them.

use std::collections::HashMap;
use std::sync::Arc;

use charliebot_store::StoreResult;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::registry::{Registry, RegistryError};

/// A node's binding to a sequence (`sequence_controllers.SequenceBinding`). A scheduled task's binding is the loaded
/// task whose session id names the node.
pub trait SequenceBinding: Send + Sync {
  fn name(&self) -> &str;
  /// True when the sequence gives the node a backend of its own.
  fn dedicated_backend(&self) -> bool;
  /// The error detail when a backend switch to `target_backend` is refused.
  fn backend_lock_detail(&self, target_backend: &str) -> String;
  /// Runs the node's wake duties and returns an optional turn-input prefix.
  fn on_wake<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    input_events: &'a [Value],
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>>;
  /// Runs the sequence's archive duty for this node.
  fn on_archive<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// A package's runtime interface for one kind of durable sequence (`sequence_controllers.SequenceController`).
pub trait SequenceController: Send + Sync {
  /// The sequence kind this controller owns: the `sequence_ref.kind` of its runs.
  fn sequence_kind(&self) -> &'static str;

  /// The binding of `session_id`, if this controller binds it.
  fn binding(&self, session_id: &str) -> Option<Arc<dyn SequenceBinding>>;

  /// Whether `meta` carries this controller's legacy ownership marker.
  fn owns_session(&self, meta: &SessionMetadata) -> bool;

  /// This controller's per-row listing fields for one batch of session ids.
  fn listing_fields(&self, session_ids: &[String], now: UtcDateTime) -> HashMap<String, Map<String, Value>>;

  /// Moves every binding of `source` onto `new`, the node that replaced it. `true` when a binding moved.
  fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async { Ok(false) })
  }
}

impl Registry {
  /// Register a sequence controller (`sequence_controllers.register_sequence_controller`). A repeated name fails.
  pub fn sequence_controller(
    &mut self,
    name: &'static str,
    controller: Arc<dyn SequenceController>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.sequence_controllers.insert(package, name, controller)
  }

  /// The registered controllers in registration order.
  pub fn sequence_controllers(&self) -> impl Iterator<Item = &Arc<dyn SequenceController>> {
    self.sequence_controllers.values()
  }

  /// Whether a registered controller owns `meta`.
  pub fn sequence_owns_session(&self, meta: &SessionMetadata) -> bool {
    self.sequence_controllers().any(|controller| controller.owns_session(meta))
  }

  /// The binding of `session_id`. Two controllers that both bind one session fail.
  pub fn binding_for(&self, session_id: &str) -> anyhow::Result<Option<Arc<dyn SequenceBinding>>> {
    let mut found: Option<Arc<dyn SequenceBinding>> = None;
    for controller in self.sequence_controllers() {
      let Some(binding) = controller.binding(session_id) else {
        continue;
      };
      if found.is_some() {
        anyhow::bail!("multiple sequence controllers bind session {session_id:?}");
      }
      found = Some(binding);
    }
    Ok(found)
  }

  /// Merges every controller's listing fields into one row map. Every requested id has an entry, possibly empty.
  pub fn sequence_listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> HashMap<String, Map<String, Value>> {
    let mut ids = session_ids.to_vec();
    ids.sort();
    ids.dedup();
    let mut fields: HashMap<String, Map<String, Value>> = ids.iter().map(|id| (id.clone(), Map::new())).collect();
    for controller in self.sequence_controllers() {
      for (session_id, controller_fields) in controller.listing_fields(&ids, now) {
        fields.entry(session_id).or_default().extend(controller_fields);
      }
    }
    fields
  }
}

/// The task-tree handle a controller calls. The run-store and dispatcher members land with S2 and S3; this build
/// exposes the session metadata read that every member needs.
pub trait SequenceTree: Send + Sync {
  fn load_meta<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<Option<SessionMetadata>>>;
}
