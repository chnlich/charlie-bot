//! The improve sequence controller (`src/features/improve/sequence_controller.py`). It owns every Run whose
//! `sequence_ref.kind` is `improve`. Recovery never resumes a loop: an ended loop's close step closes its child, and an
//! interrupted loop is marked for the operator to restart.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_registry::sequence::{SequenceBinding, SequenceController, SequenceTree};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::RunRecord;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::sequence::{SEQUENCE_KIND, close_ended_loop_child, reconcile_interrupted_sequences};

/// The improve controller. It is stateless: the iteration context is composed by the runner, which holds the repo root.
pub struct ImproveSequenceController;

impl SequenceController for ImproveSequenceController {
  fn sequence_kind(&self) -> &'static str {
    SEQUENCE_KIND
  }

  fn redrive<'a>(&'a self, _session_id: &'a str, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { bail!("improve iteration replay is not supported") })
  }

  /// The launch text is the full iteration context: the runner composes it with the repo root (the hook receives no
  /// startup state). This hook checks that the Run belongs to this controller, as the Python hook does.
  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    run: &'a RunRecord,
    launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async move {
      let seq = run.sequence_ref.as_ref().filter(|seq| seq.kind == SEQUENCE_KIND).with_context(|| {
        format!("iteration run {} carries no improve sequence_ref; the controller that registered it is broken", run.id)
      })?;
      tracing::debug!(target: "charliebot_improve", run_id = %run.id, owner_ref = %seq.owner_ref, "improve_launch_context");
      Ok(Some(launch_text.to_string()))
    })
  }

  fn after_run<'a>(
    &'a self,
    _session_id: &'a str,
    run: &'a RunRecord,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { close_loop_for_run(run, tree).await })
  }

  fn recover_run<'a>(
    &'a self,
    _session_id: &'a str,
    run: &'a RunRecord,
    _outcome: Option<&'a str>,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      // The close step is idempotent by its request id, so recovery counts no follow-up.
      close_loop_for_run(run, tree).await?;
      Ok(false)
    })
  }

  fn reconcile_interrupted<'a>(&'a self, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      reconcile_interrupted_sequences(tree.config(), tree, None).await?;
      Ok(())
    })
  }

  fn binding(&self, _session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    None
  }

  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    HashMap::new()
  }
}

/// Run the owning loop's close step, located through the Run's `owner_ref`: `<sessions>/<session>/loops/<loop id>`.
async fn close_loop_for_run(run: &RunRecord, tree: &dyn SequenceTree) -> anyhow::Result<()> {
  let seq = run.sequence_ref.as_ref().with_context(|| format!("run {} carries no sequence_ref", run.id))?;
  let loop_dir = Path::new(&seq.owner_ref);
  let session_id = loop_dir
    .parent()
    .and_then(Path::parent)
    .and_then(Path::file_name)
    .and_then(|name| name.to_str())
    .with_context(|| format!("owner_ref {} names no session directory", seq.owner_ref))?;
  let loop_id = loop_dir
    .file_name()
    .and_then(|name| name.to_str())
    .and_then(|name| name.parse::<i64>().ok())
    .with_context(|| format!("owner_ref {} names no loop id", seq.owner_ref))?;
  close_ended_loop_child(tree, tree.config(), session_id, loop_id).await;
  Ok(())
}

/// Register the improve controller under its sequence kind.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sequence_controller(SEQUENCE_KIND, Arc::new(ImproveSequenceController))
}
