//! Account placement and transcript adoption for Claude manager turns.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use super::{claude_accounts, claude_compaction, claude_config, claude_metadata, claude_relay};
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::backend::{LaunchContext, LaunchKind, LaunchRefused, RunningTurnsFn};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};

pub const GUARD_REFUSED_NEWER_TRANSCRIPT: &str = "guard_refused_newer_transcript";

fn read_running_turns(callback: Option<&RunningTurnsFn>) -> anyhow::Result<Vec<SessionMetadata>> {
  let callback =
    callback.ok_or_else(|| anyhow::anyhow!("LaunchContext.running_turns is required for a pooled manager turn"))?;
  Ok(callback())
}

fn session_metadata(ctx: &LaunchContext) -> SessionMetadata {
  ctx.session_meta.lock().expect("launch metadata mutex is not poisoned").clone()
}

async fn compact_session_transcript(
  ctx: &LaunchContext,
  session_id: &str,
  account: &claude_config::ClaudeAccount,
  trigger: &str,
  context_tokens: Option<i64>,
) -> anyhow::Result<bool> {
  let owner_session_id = session_metadata(ctx).id;
  claude_compaction::compact_with_sonnet(
    session_id,
    &ctx.cwd,
    Path::new(&account.config_dir),
    context_tokens,
    &ctx.emit,
    &ctx.cfg,
    Some(&owner_session_id),
    &account.label,
    trigger,
    claude_compaction::CLAUDE_COMPACTION_TIMEOUT,
  )
  .await
}

#[allow(clippy::too_many_arguments)]
pub fn choose_turn_account(
  state: &claude_accounts::ClaudeAccountState,
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
  model: Option<&str>,
  last_request_at: Option<UtcDateTime>,
  now: Option<UtcDateTime>,
  account_pool: Option<&str>,
  running_turns: &[SessionMetadata],
) -> anyhow::Result<(Option<claude_config::ClaudeAccount>, bool)> {
  let moment = claude_accounts::now_or(now);
  let current_label = claude_metadata::account_of(session_meta);
  let current = claude_accounts::account_by_label(cfg, current_label.as_deref())?;
  let cold = claude_compaction::cache_expired(last_request_at, Some(moment));
  let busy = running_turns
    .iter()
    .filter(|metadata| metadata.id != session_meta.id)
    .filter_map(claude_metadata::account_of)
    .collect::<BTreeSet<_>>();
  if let Some(current) = current.as_ref()
    && claude_accounts::in_pool(cfg, &current.label, account_pool)?
    && !busy.contains(&current.label)
    && !cold
    && claude_accounts::healthy(state, current, Some(moment))
  {
    let reading = claude_accounts::latest_reading(state, &current.label, model, Some(moment));
    let rejected = reading.as_ref().is_some_and(|reading| reading.rejected_until.is_some_and(|reset| reset > moment));
    if reading.as_ref().is_none_or(|reading| reading.utilization < claude_accounts::WARNING_UTILIZATION && !rejected) {
      return Ok((Some(current.clone()), cold));
    }
  }
  let busy = busy.into_iter().collect::<BTreeSet<_>>();
  Ok((
    claude_accounts::select(state, cfg, model, &Default::default(), Some(&busy), Some(moment), account_pool, 0.0)?,
    cold,
  ))
}

pub async fn adopt_transcript_holder(
  ctx: &LaunchContext,
  session_id: Option<&str>,
  holder: &claude_config::ClaudeAccount,
  previous_label: Option<&str>,
  reason: &str,
) -> anyhow::Result<()> {
  (ctx.record_account)(holder.label.clone()).await?;
  tracing::warn!(
    target: "charliebot_agents",
    session = %session_metadata(ctx).id,
    cc_session_id = session_id,
    adopted = holder.label,
    previous = previous_label,
    reason,
    "master_cc_account_label_reconciled"
  );
  Ok(())
}

async fn probe_reconcile_label(ctx: &LaunchContext, session_id: Option<&str>) -> anyhow::Result<()> {
  let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
    return Ok(());
  };
  let meta = session_metadata(ctx);
  let label = claude_accounts::account_by_label(&ctx.cfg, claude_metadata::account_of(&meta).as_deref())?;
  let Some(label) = label else {
    return Ok(());
  };
  let label_copy = claude_accounts::transcript_path(Path::new(&label.config_dir), session_id)?;
  let newest = claude_accounts::newest_transcript_copy(&ctx.cfg, session_id)?;
  let (Some(label_copy), Some((newest_account, newest_copy))) = (label_copy, newest) else {
    return Ok(());
  };
  if newest_copy == label_copy || !claude_accounts::transcript_lineage_split(&label_copy, &newest_copy)? {
    return Ok(());
  }
  adopt_transcript_holder(ctx, Some(session_id), &newest_account, Some(&label.label), "forked_or_stale_lineage").await
}

pub async fn place_turn(
  state: &ClaudeAccountStateRef,
  ctx: &LaunchContext,
  resume_id: Option<&str>,
  context_tokens: Option<i64>,
  last_request_at: Option<UtcDateTime>,
  now: Option<UtcDateTime>,
) -> Result<claude_config::ClaudeAccount, LaunchRefused> {
  probe_reconcile_label(ctx, resume_id).await.map_err(|error| refusal(error, false, None))?;
  let running_turns = read_running_turns(ctx.running_turns.as_ref()).map_err(|error| refusal(error, false, None))?;
  let meta = session_metadata(ctx);
  let account_pool = claude_accounts::option_pool(&ctx.option);
  let (chosen, cold) = choose_turn_account(
    state,
    &ctx.cfg,
    &meta,
    Some(&ctx.option.model),
    last_request_at,
    now,
    account_pool.as_deref(),
    &running_turns,
  )
  .map_err(|error| refusal(error, false, None))?;
  let Some(chosen) = chosen else {
    let message = claude_relay::pool_exhausted_message(state, &ctx.cfg, now, account_pool.as_deref())
      .map_err(|error| refusal(error, false, None))?;
    let resets_at = claude_accounts::earliest_reset(state, &ctx.cfg, now, account_pool.as_deref())
      .map_err(|error| refusal(error, false, None))?;
    return Err(LaunchRefused { message, quota_exhausted: true, resets_at });
  };
  let previous = claude_accounts::account_by_label(&ctx.cfg, claude_metadata::account_of(&meta).as_deref())
    .map_err(|error| refusal(error, false, None))?;
  if previous.as_ref().is_none_or(|previous| previous.label != chosen.label) {
    if let (Some(resume_id), Some(previous)) = (resume_id.filter(|id| !id.is_empty()), previous.as_ref())
      && let Err(error) =
        claude_accounts::move_transcript(resume_id, Path::new(&previous.config_dir), Path::new(&chosen.config_dir))
    {
      if !claude_accounts::is_newer_transcript_refusal(&error) {
        return Err(LaunchRefused {
          message: format!("Claude account switch failed: {error}"),
          quota_exhausted: false,
          resets_at: None,
        });
      }
      adopt_transcript_holder(ctx, Some(resume_id), &chosen, Some(&previous.label), GUARD_REFUSED_NEWER_TRANSCRIPT)
        .await
        .map_err(|error| refusal(error, false, None))?;
    }
    claude_metadata::set_account(
      &mut ctx.session_meta.lock().expect("launch metadata mutex is not poisoned"),
      Some(&chosen.label),
    );
    (ctx.record_account)(chosen.label.clone()).await.map_err(|error| refusal(error, false, None))?;
    tracing::info!(
      target: "charliebot_agents",
      session = %meta.id,
      account = chosen.label,
      previous = previous.as_ref().map(|account| account.label.as_str()),
      cold_cache = cold,
      "master_cc_account_chosen"
    );
  }
  report_empty_credentials(state, ctx).await.map_err(|error| refusal(error, false, None))?;
  if resume_id.is_some_and(|id| !id.is_empty())
    && cold
    && claude_compaction::expired_cache_compaction_wanted(
      &ctx.cfg,
      Some(&ctx.option.model),
      context_tokens,
      last_request_at,
      now,
    )
  {
    compact_session_transcript(
      ctx,
      resume_id.expect("resume id was checked"),
      &chosen,
      "expired_cache",
      context_tokens,
    )
    .await
    .map_err(|error| refusal(error, false, None))?;
  }
  Ok(chosen)
}

async fn report_login_required(
  ctx: &LaunchContext,
  account: &claude_config::ClaudeAccount,
  reason: &str,
) -> anyhow::Result<()> {
  tracing::error!(
    target: "charliebot_agents",
    session = %session_metadata(ctx).id,
    account = account.label,
    config_dir = account.config_dir,
    reason,
    "claude_account_login_required"
  );
  (ctx.emit)(claude_relay::login_required_event(account, reason)).await
}

pub async fn report_login_failure(
  state: &ClaudeAccountStateRef,
  ctx: &LaunchContext,
  account: &claude_config::ClaudeAccount,
) -> anyhow::Result<()> {
  claude_accounts::record_auth_failure(state, &account.label, None);
  report_login_required(ctx, account, claude_relay::LOGIN_REASON_AUTH_FAILED).await
}

async fn report_empty_credentials(state: &ClaudeAccountStateRef, ctx: &LaunchContext) -> anyhow::Result<()> {
  for account in claude_accounts::pool(&ctx.cfg)? {
    let present = claude_accounts::credentials_present(&account);
    if claude_accounts::login_notice_due(state, &account.label, !present) {
      report_login_required(ctx, &account, claude_relay::LOGIN_REASON_EMPTY_CREDENTIALS).await?;
    }
  }
  Ok(())
}

pub async fn prepare_relay(
  state: &ClaudeAccountStateRef,
  ctx: &LaunchContext,
  current: &claude_config::ClaudeAccount,
  session_id: Option<&str>,
  reason: Option<&str>,
  relays: usize,
  min_headroom: f64,
) -> Result<claude_config::ClaudeAccount, LaunchRefused> {
  let account_pool = claude_accounts::option_pool(&ctx.option);
  let movement = claude_relay::relay_move(
    state,
    &ctx.cfg,
    Some(&ctx.option.model),
    current,
    session_id,
    None,
    account_pool.as_deref(),
    min_headroom,
  )
  .map_err(|error| refusal(error, false, None))?;
  let Some(next) = movement.account else {
    let message = movement.error.expect("failed account move has an error");
    if let Some(holder) = movement.refused_holder.filter(|_| ctx.kind == LaunchKind::Turn) {
      adopt_transcript_holder(ctx, session_id, &holder, Some(&current.label), GUARD_REFUSED_NEWER_TRANSCRIPT)
        .await
        .map_err(|error| refusal(error, false, None))?;
      return Ok(holder);
    }
    let resets_at = if movement.pool_exhausted {
      claude_accounts::earliest_reset(state, &ctx.cfg, None, account_pool.as_deref())
        .map_err(|error| refusal(error, false, None))?
    } else {
      None
    };
    return Err(LaunchRefused { message, quota_exhausted: movement.pool_exhausted, resets_at });
  };
  let meta = session_metadata(ctx);
  match ctx.kind {
    LaunchKind::Turn => tracing::warn!(
      target: "charliebot_agents",
      session = %meta.id,
      cc_session_id = session_id,
      reason,
      from_account = current.label,
      to_account = next.label,
      "master_cc_account_relay"
    ),
    LaunchKind::Task => tracing::warn!(
      target: "charliebot_agents",
      session = %meta.id,
      reason,
      from_account = current.label,
      to_account = next.label,
      relays,
      "worker_account_relay"
    ),
  }
  let (context_tokens, _) = (ctx.context_state)().await.map_err(|error| refusal(error, false, None))?;
  if ctx.kind == LaunchKind::Turn
    && claude_compaction::relay_compaction_wanted(&ctx.cfg, Some(&ctx.option.model), context_tokens)
  {
    let session_id = session_id.expect("a successful relay has a session id");
    compact_session_transcript(ctx, session_id, &next, "relay", context_tokens)
      .await
      .map_err(|error| refusal(error, false, None))?;
  }
  Ok(next)
}

fn refusal(error: anyhow::Error, quota_exhausted: bool, resets_at: Option<UtcDateTime>) -> LaunchRefused {
  LaunchRefused { message: error.to_string(), quota_exhausted, resets_at }
}

pub type ClaudeAccountStateRef = Arc<claude_accounts::ClaudeAccountState>;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn selection_rejects_a_turn_context_without_running_turns() {
    let error = read_running_turns(None).unwrap_err();
    assert!(error.to_string().contains("running_turns"));
  }

  #[test]
  fn split_transcript_tail_selects_the_newest_holder_before_placement() {
    let base = tempfile::tempdir().unwrap();
    let old = base.path().join("old/projects/p/session.jsonl");
    let newest = base.path().join("new/projects/p/session.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::create_dir_all(newest.parent().unwrap()).unwrap();
    std::fs::write(&old, "old-line\n").unwrap();
    std::fs::write(&newest, "new-line\n").unwrap();
    assert!(claude_accounts::transcript_lineage_split(&old, &newest).unwrap());
  }
}
