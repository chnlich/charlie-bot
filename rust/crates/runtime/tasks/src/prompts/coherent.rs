//! The launch-time coherent snapshot over the task tree.
//!
//! The ancestor chain, the refs and the status Goals of the session and its parent are captured under the control lock.
//! The templates, the supplements, the overlay and the memory are read outside it. Before a pass settles, the view is
//! captured again and the sources are assembled again; any change starts another pass, and a source that keeps moving
//! fails the launch.

use anyhow::{Context, anyhow};
use charliebot_core::RepoRoot;
use charliebot_core::approval_policy::ApprovalRuleRef;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::home::PROMPT_BODIES_DIR_NAME;
use charliebot_registry::Registry;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::{ControlGuard, Store};
use std::sync::Arc;

use super::sources::{approval_rule_refs, manager_session_rule_segments};
use super::{
  COHERENCE_PASSES, ChainLink, OverlayError, PromptEnv, PromptSnapshot, RuleInputs, SessionGoals, TaskPromptError,
  assemble_snapshot, build_segments,
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

/// What one pass captures under the control lock: the ancestor chain, this node's node rule ref, and the status Goals.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedView {
  chain: Vec<ChainLink>,
  node_ref: Option<String>,
  goals: SessionGoals,
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

/// The status Goals of `meta` and of its nearest parent. The caller holds the control lock. A parent that stores an
/// unreadable status fails the capture, as the registry's hook reports it.
pub async fn capture_session_goals(
  store: &Store,
  registry: &Registry,
  meta: &SessionMetadata,
) -> anyhow::Result<SessionGoals> {
  let parent = match store.ancestor_ids(&meta.id)?.first() {
    Some(parent_id) => {
      let parent =
        store.load_meta(parent_id).await?.ok_or_else(|| anyhow!("task {parent_id} vanished during prompt assembly"))?;
      registry.status_goal(&parent)?
    }
    None => None,
  };
  Ok(SessionGoals { parent, own: registry.status_goal(meta)? })
}

/// Reloads `session_id` and captures its view inside one control-lock hold, so the view and the body built from the
/// returned metadata come from the same tree. A task that vanished mid-assembly fails.
async fn capture_under_control_lock(
  store: &Store,
  registry: &Registry,
  session_id: &str,
) -> anyhow::Result<(CapturedView, SessionMetadata)> {
  let guard = store.control_lock().await;
  let fresh =
    store.load_meta(session_id).await?.ok_or_else(|| anyhow!("task {session_id} vanished during prompt assembly"))?;
  let (chain, node_ref) = capture_prompt_chain(store, &guard, &fresh).await?;
  let goals = capture_session_goals(store, registry, &fresh).await?;
  Ok((CapturedView { chain, node_ref, goals }, fresh))
}

/// The assembly inputs of one captured view under the launch's overlay.
fn view_inputs<'a>(view: &'a CapturedView, overlay: Option<&'a str>) -> RuleInputs<'a> {
  RuleInputs { overlay, chain: &view.chain, node_ref: view.node_ref.as_deref(), goals: &view.goals }
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
    let (view, fresh) = capture_under_control_lock(store, env.registry, &meta.id).await?;
    let inputs = view_inputs(&view, overlay.as_deref());
    let (built, overlay_error) = build_segments(env, &fresh, kind, &inputs)?;
    let candidate = assemble_snapshot(&built);
    // The mutable-source recheck: assemble again, and any difference means no coherent view existed yet.
    let (rebuilt, overlay_error_again) = build_segments(env, &fresh, kind, &inputs)?;
    if candidate != assemble_snapshot(&rebuilt) {
      continue;
    }
    let (view_again, _) = capture_under_control_lock(store, env.registry, &meta.id).await?;
    if view_again != view {
      // A rule, an ancestor or a Goal moved: rebuild from the new view.
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

/// The exact rules a manager session would receive at its next start, without chat history or host-state writes.
pub async fn current_session_approval_rule_refs(
  store: &Store,
  env: &PromptEnv<'_>,
  meta: &SessionMetadata,
) -> anyhow::Result<Vec<ApprovalRuleRef>> {
  for _ in 0..COHERENCE_PASSES {
    let (view, fresh) = capture_under_control_lock(store, env.registry, &meta.id).await?;
    let inputs = view_inputs(&view, None);
    let candidate = approval_rule_segments(env, &fresh, &inputs)?;
    let rebuilt = approval_rule_segments(env, &fresh, &inputs)?;
    if candidate != rebuilt {
      continue;
    }
    let (view_again, _) = capture_under_control_lock(store, env.registry, &meta.id).await?;
    if view_again != view {
      continue;
    }
    return Ok(approval_rule_refs(&fresh.id, &candidate));
  }
  Err(
    TaskPromptError::new(format!(
      "approval rule sources for task {} did not settle across {COHERENCE_PASSES} coherent-view attempts",
      meta.id
    ))
    .into(),
  )
}

/// The manager's current session rules, built from the same source readers as its next prompt.
fn approval_rule_segments(
  env: &PromptEnv<'_>,
  meta: &SessionMetadata,
  inputs: &RuleInputs<'_>,
) -> anyhow::Result<Vec<super::RuleSegment>> {
  let (mut segments, host) = manager_session_rule_segments(env, meta)?;
  segments.extend(host);
  let bodies_dir = env.cfg.charliebot_home.join(PROMPT_BODIES_DIR_NAME);
  segments.extend(super::sources::local_rule_segments(&bodies_dir, inputs.chain, inputs.node_ref, &meta.id)?);
  if let Some(goal) = super::sources::goal_segment(super::MANAGER_KINDS[0], meta, inputs) {
    segments.push(goal);
  }
  Ok(segments)
}

/// The launch path's coherent snapshot, offloading each blocking source read to a blocking thread.
/// The API preview keeps its borrowed environment and existing path.
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
    let (view, fresh) = capture_under_control_lock(store, &env.registry, &meta.id).await?;
    let (built, overlay_error) =
      build_segments_off_thread(&env, fresh.clone(), kind, overlay.as_deref(), view.clone()).await?;
    let candidate = assemble_snapshot(&built);
    let (rebuilt, overlay_error_again) =
      build_segments_off_thread(&env, fresh, kind, overlay.as_deref(), view.clone()).await?;
    if candidate != assemble_snapshot(&rebuilt) {
      continue;
    }
    let (view_again, _) = capture_under_control_lock(store, &env.registry, &meta.id).await?;
    if view_again != view {
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
  view: CapturedView,
) -> anyhow::Result<(Vec<super::RuleSegment>, Option<OverlayError>)> {
  let cfg = Arc::clone(&env.cfg);
  let registry = Arc::clone(&env.registry);
  let checkout = Arc::clone(&env.checkout);
  let kind = kind.to_string();
  let overlay = overlay.map(str::to_string);
  tokio::task::spawn_blocking(move || {
    let env = PromptEnv { cfg: &cfg, registry: &registry, checkout: &checkout };
    build_segments(&env, &meta, &kind, &view_inputs(&view, overlay.as_deref()))
  })
  .await
  .map_err(anyhow::Error::new)
  .context("joining blocking prompt source builder")?
}
