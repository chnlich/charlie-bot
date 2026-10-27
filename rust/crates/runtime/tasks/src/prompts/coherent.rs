//! The launch-time coherent snapshot over the task tree (`src/runtime/task_execution.py:279-378`).
//!
//! The ancestor chain and the refs are captured under the control lock. The templates, the supplements, the overlay and
//! the memory are read outside it. Before a pass settles, the chain is captured again and the sources are assembled
//! again; any change starts another pass, and a source that keeps moving fails the launch.

use anyhow::{Context, anyhow};
use charliebot_core::RepoRoot;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::{ControlGuard, Store};
use std::sync::Arc;

use super::{
  COHERENCE_PASSES, ChainLink, OverlayError, PromptEnv, PromptSnapshot, RuleInputs, TaskPromptError, assemble_snapshot,
  build_segments,
};

/// The coherent snapshot of one launch: the snapshot, the overlay read error (if any), and whether the overlay is
/// declared (`declared`). An undeclared overlay is `false`; `none` is declared with no overlay.
pub struct CoherentSnapshot {
  pub snapshot: PromptSnapshot,
  pub overlay_error: Option<OverlayError>,
  pub declared: bool,
}

struct LaunchPromptEnv {
  cfg: Arc<CharlieBotConfig>,
  registry: Arc<Registry>,
  checkout: Arc<RepoRoot>,
}

/// The three-state overlay judgment of a launch: `(overlay name or None, declared)`. An undeclared overlay is
/// `(None, false)`; `none` is `(None, true)`; any other name is `(Some(name), true)`.
pub fn resolve_launch_overlay(option: &BackendOption) -> (Option<String>, bool) {
  match option.prompt_overlay.as_deref() {
    None => (None, false),
    Some("none") => (None, true),
    Some(name) => (Some(name.to_string()), true),
  }
}

/// The subtree refs from the root down to `meta`, with `meta`'s own subtree ref last, and `meta`'s node rule ref. The
/// caller holds `control`, so the ancestor walk and the metadata reads see one tree.
pub async fn capture_prompt_chain(
  store: &Store,
  _control: &ControlGuard,
  meta: &SessionMetadata,
) -> anyhow::Result<(Vec<ChainLink>, Option<String>)> {
  let mut chain = Vec::new();
  // `ancestor_ids` lists the nearest ancestor first; the chain starts at the root.
  for ancestor_id in store.ancestor_ids(&meta.id)?.iter().rev() {
    let ancestor = store
      .load_meta(ancestor_id)
      .await?
      .ok_or_else(|| anyhow!("task {ancestor_id} vanished during prompt assembly"))?;
    chain.push(ChainLink { session_id: ancestor_id.clone(), subtree_ref: ancestor.subtree_prompt_ref });
  }
  chain.push(ChainLink { session_id: meta.id.clone(), subtree_ref: meta.subtree_prompt_ref.clone() });
  Ok((chain, meta.node_prompt_ref.clone()))
}

/// Reloads `session_id` and captures its chain inside one control-lock hold, so the chain and the body built from the
/// returned metadata come from the same view. A task that vanished mid-assembly fails.
async fn capture_under_control_lock(
  store: &Store,
  session_id: &str,
) -> anyhow::Result<(Vec<ChainLink>, Option<String>, SessionMetadata)> {
  let guard = store.control_lock().await;
  let fresh =
    store.load_meta(session_id).await?.ok_or_else(|| anyhow!("task {session_id} vanished during prompt assembly"))?;
  let (chain, node_ref) = capture_prompt_chain(store, &guard, &fresh).await?;
  Ok((chain, node_ref, fresh))
}

/// The coherent snapshot for one run kind of `meta` under `option` (`assemble_coherent_snapshot`). A snapshot whose
/// sources do not settle within the bounded passes is a launch-blocking error.
pub async fn assemble_coherent_snapshot(
  store: &Store,
  env: &PromptEnv<'_>,
  meta: &SessionMetadata,
  kind: &str,
  option: &BackendOption,
) -> anyhow::Result<CoherentSnapshot> {
  let (overlay, declared) = resolve_launch_overlay(option);
  for _ in 0..COHERENCE_PASSES {
    let (chain, node_ref, fresh) = capture_under_control_lock(store, &meta.id).await?;
    let inputs = RuleInputs { overlay: overlay.as_deref(), chain: &chain, node_ref: node_ref.as_deref() };
    let (built, overlay_error) = build_segments(env, &fresh, kind, &inputs)?;
    let candidate = assemble_snapshot(&built);
    // The mutable-source recheck: assemble again, and any difference means no coherent view existed yet.
    let (rebuilt, overlay_error_again) = build_segments(env, &fresh, kind, &inputs)?;
    if candidate != assemble_snapshot(&rebuilt) {
      continue;
    }
    let (chain_again, node_ref_again, _) = capture_under_control_lock(store, &meta.id).await?;
    if (&chain_again, &node_ref_again) != (&chain, &node_ref) {
      // A rule or an ancestor moved: rebuild from the new view.
      continue;
    }
    return Ok(CoherentSnapshot {
      snapshot: candidate,
      overlay_error: overlay_error.or(overlay_error_again),
      declared,
    });
  }
  Err(
    TaskPromptError::new(format!(
      "prompt sources for task {} did not settle across {COHERENCE_PASSES} coherent-view attempts",
      meta.id
    ))
    .into(),
  )
}

/// The launch path's coherent snapshot, offloading each blocking source read as Python does with `asyncio.to_thread`
/// (`src/runtime/task_execution.py:355-365`). The API preview keeps its borrowed environment and existing path.
pub async fn assemble_coherent_snapshot_for_launch(
  store: &Store,
  cfg: Arc<CharlieBotConfig>,
  registry: Arc<Registry>,
  checkout: Arc<RepoRoot>,
  meta: &SessionMetadata,
  kind: &str,
  option: &BackendOption,
) -> anyhow::Result<CoherentSnapshot> {
  let (overlay, declared) = resolve_launch_overlay(option);
  let env = LaunchPromptEnv { cfg, registry, checkout };
  for _ in 0..COHERENCE_PASSES {
    let (chain, node_ref, fresh) = capture_under_control_lock(store, &meta.id).await?;
    let (built, overlay_error) =
      build_segments_off_thread(&env, fresh.clone(), kind, overlay.as_deref(), chain.clone(), node_ref.clone()).await?;
    let candidate = assemble_snapshot(&built);
    let (rebuilt, overlay_error_again) =
      build_segments_off_thread(&env, fresh, kind, overlay.as_deref(), chain.clone(), node_ref.clone()).await?;
    if candidate != assemble_snapshot(&rebuilt) {
      continue;
    }
    let (chain_again, node_ref_again, _) = capture_under_control_lock(store, &meta.id).await?;
    if (&chain_again, &node_ref_again) != (&chain, &node_ref) {
      continue;
    }
    return Ok(CoherentSnapshot {
      snapshot: candidate,
      overlay_error: overlay_error.or(overlay_error_again),
      declared,
    });
  }
  Err(
    TaskPromptError::new(format!(
      "prompt sources for task {} did not settle across {COHERENCE_PASSES} coherent-view attempts",
      meta.id
    ))
    .into(),
  )
}

async fn build_segments_off_thread(
  env: &LaunchPromptEnv,
  meta: SessionMetadata,
  kind: &str,
  overlay: Option<&str>,
  chain: Vec<ChainLink>,
  node_ref: Option<String>,
) -> anyhow::Result<(Vec<super::RuleSegment>, Option<OverlayError>)> {
  let cfg = Arc::clone(&env.cfg);
  let registry = Arc::clone(&env.registry);
  let checkout = Arc::clone(&env.checkout);
  let kind = kind.to_string();
  let overlay = overlay.map(str::to_string);
  tokio::task::spawn_blocking(move || {
    let env = PromptEnv { cfg: &cfg, registry: &registry, checkout: &checkout };
    let inputs = RuleInputs { overlay: overlay.as_deref(), chain: &chain, node_ref: node_ref.as_deref() };
    build_segments(&env, &meta, &kind, &inputs)
  })
  .await
  .map_err(anyhow::Error::new)
  .context("joining blocking prompt source builder")?
}
