//! Account placement and transcript adoption for Claude manager turns.

use std::collections::BTreeSet;
use std::future::Future;
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
    &ctx.base_env,
    context_tokens,
    &ctx.emit,
    &ctx.cfg,
    &owner_session_id,
    &account.label,
    trigger,
    claude_compaction::CLAUDE_COMPACTION_TIMEOUT,
    ctx.run_kind,
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
  claude_metadata::set_account(
    &mut ctx.session_meta.lock().expect("launch metadata mutex is not poisoned"),
    Some(&holder.label),
  );
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
  place_turn_with_hook(state, ctx, resume_id, context_tokens, last_request_at, now, std::future::ready(())).await
}

async fn place_turn_with_hook<F>(
  state: &ClaudeAccountStateRef,
  ctx: &LaunchContext,
  resume_id: Option<&str>,
  context_tokens: Option<i64>,
  last_request_at: Option<UtcDateTime>,
  now: Option<UtcDateTime>,
  after_running_turns: F,
) -> Result<claude_config::ClaudeAccount, LaunchRefused>
where
  F: Future<Output = ()> + Send,
{
  let pool_choice = state.pool_choice.lock().await;
  probe_reconcile_label(ctx, resume_id).await.map_err(|error| refusal(error, false, None))?;
  let running_turns = read_running_turns(ctx.running_turns.as_ref()).map_err(|error| refusal(error, false, None))?;
  after_running_turns.await;
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
  let account_changed = previous.as_ref().is_none_or(|previous| previous.label != chosen.label);
  if account_changed {
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
    tracing::info!(
      target: "charliebot_agents",
      session = %meta.id,
      account = chosen.label,
      previous = previous.as_ref().map(|account| account.label.as_str()),
      cold_cache = cold,
      "master_cc_account_chosen"
    );
  }
  drop(pool_choice);
  if account_changed {
    (ctx.record_account)(chosen.label.clone()).await.map_err(|error| refusal(error, false, None))?;
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
) -> Result<claude_config::ClaudeAccount, LaunchRefused> {
  let account_pool = claude_accounts::option_pool(&ctx.option);
  let pool_choice = if ctx.kind == LaunchKind::Turn { Some(state.pool_choice.lock().await) } else { None };
  let movement = claude_relay::relay_move(
    state,
    &ctx.cfg,
    Some(&ctx.option.model),
    current,
    session_id,
    None,
    account_pool.as_deref(),
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
  if ctx.kind == LaunchKind::Turn {
    claude_metadata::set_account(
      &mut ctx.session_meta.lock().expect("launch metadata mutex is not poisoned"),
      Some(&next.label),
    );
  }
  drop(pool_choice);
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
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::time::{Duration, UNIX_EPOCH};

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{ContextStateFn, EmitFn, RecordAccountFn, RunningTurnsFn};
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::run_records::RunKind;
  use tokio::sync::Notify;

  use crate::login_dirs;

  use super::*;

  fn config(home: &Path, accounts: &[(&str, &Path)]) -> charliebot_core::config::CharlieBotConfig {
    std::fs::create_dir_all(home).unwrap();
    let mut configured = String::new();
    let mut labels = Vec::new();
    for (label, path) in accounts {
      configured.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", path.display()));
      labels.push((*label).to_string());
    }
    std::fs::write(
      home.join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n{configured}  claude_pools:\n    alpha: [{}]\n",
        labels.join(", ")
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  fn context(
    cfg: charliebot_core::config::CharlieBotConfig,
    session_id: &str,
    account_label: Option<&str>,
    held_native_id: Option<&str>,
    running_turns: RunningTurnsFn,
    record_account: RecordAccountFn,
  ) -> LaunchContext {
    let option = cfg.backends.options[0].clone();
    let mut metadata = SessionMetadata::new(session_id, "Session", TaskProfile::Manager);
    claude_metadata::set_account(&mut metadata, account_label);
    let session_meta = Arc::new(std::sync::Mutex::new(metadata));
    let emit: EmitFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let context_state: ContextStateFn = Arc::new(|| Box::pin(async { Ok((None, None)) }));
    LaunchContext {
      cfg: Arc::new(cfg),
      option,
      session_meta,
      kind: LaunchKind::Turn,
      run_kind: RunKind::ManagerTurn,
      cwd: std::path::PathBuf::from("/synthetic/cwd"),
      base_env: Arc::new(charliebot_registry::backend::Env::new()),
      held_native_id: held_native_id.map(str::to_string),
      preassigned_native_id: None,
      emit,
      record_account,
      context_state,
      running_turns: Some(running_turns),
      run_id: None,
    }
  }

  fn record_account() -> RecordAccountFn {
    Arc::new(|_| Box::pin(async { Ok(()) }))
  }

  fn account_credentials(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join(login_dirs::CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#)
      .unwrap();
  }

  async fn both_placements_reach_hook(
    arrivals: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
  ) {
    let notified = notify.notified();
    if arrivals.fetch_add(1, Ordering::SeqCst) == 1 {
      notify.notify_waiters();
    }
    if arrivals.load(Ordering::SeqCst) < 2 {
      let _ = tokio::time::timeout(Duration::from_millis(200), notified).await;
    }
    seen.lock().unwrap().push(arrivals.load(Ordering::SeqCst));
  }

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

  #[test]
  fn in_place_account_label_is_busy_without_recording_it_first() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let state = claude_accounts::ClaudeAccountState::new();
    let mut running = SessionMetadata::new("running", "Running", TaskProfile::Manager);
    claude_metadata::set_account(&mut running, Some("a"));
    let running = Arc::new(std::sync::Mutex::new(running));
    let running_turns = {
      let running = Arc::clone(&running);
      Arc::new(move || vec![running.lock().unwrap().clone()]) as RunningTurnsFn
    };
    let current = SessionMetadata::new("current", "Current", TaskProfile::Manager);

    let (chosen, _) = choose_turn_account(
      &state,
      &cfg,
      &current,
      Some("claude-fable-5-1"),
      None,
      None,
      Some("alpha"),
      &running_turns(),
    )
    .unwrap();

    assert_eq!(chosen.unwrap().label, "b");
  }

  #[test]
  fn choose_turn_account_replays_the_2026_10_19_line_bounce() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let labels = ["main", "ext-1", "ext-2", "ext-3", "ext-4", "invite-1"];
    let directories = labels.map(|label| root.path().join(label));
    for directory in &directories {
      account_credentials(directory);
    }
    let accounts =
      labels.iter().zip(&directories).map(|(label, directory)| (*label, directory.as_path())).collect::<Vec<_>>();
    let cfg = config(&home, &accounts);
    let state = claude_accounts::ClaudeAccountState::new();
    let now = UtcDateTime::parse("2026-10-19T23:23:37Z").unwrap();
    let readings =
      [("ext-3", 0.07), ("invite-1", 0.23), ("ext-4", 0.47), ("ext-1", 0.73), ("main", 0.96), ("ext-2", 0.90)];
    for (label, utilization) in readings {
      let info = serde_json::json!({"unifiedWindows":{"five_hour":{"utilization":utilization}}});
      claude_accounts::observe_rate_limit(&state, label, &info, Some(now));
    }
    let running = ["ext-3", "invite-1", "ext-4", "ext-1", "main"]
      .into_iter()
      .enumerate()
      .map(|(index, label)| {
        let mut holder = SessionMetadata::new(format!("other-{index}"), "Other", TaskProfile::Manager);
        claude_metadata::set_account(&mut holder, Some(label));
        holder
      })
      .collect::<Vec<_>>();
    let mut current = SessionMetadata::new("current", "Current", TaskProfile::Manager);
    claude_metadata::set_account(&mut current, Some("ext-4"));
    let last_request = UtcDateTime::parse("2026-10-19T23:13:37Z").unwrap();

    let (chosen, cold) = choose_turn_account(
      &state,
      &cfg,
      &current,
      Some("claude-fable-5-1"),
      Some(last_request),
      Some(now),
      Some("alpha"),
      &running,
    )
    .unwrap();

    assert_eq!(chosen.map(|account| account.label), Some("ext-3".to_string()));
    assert!(!cold);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn concurrent_placements_choose_distinct_accounts() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let metadata_a = Arc::new(std::sync::Mutex::new(SessionMetadata::new("turn-a", "A", TaskProfile::Manager)));
    let metadata_b = Arc::new(std::sync::Mutex::new(SessionMetadata::new("turn-b", "B", TaskProfile::Manager)));
    let running_a: RunningTurnsFn = {
      let metadata_b = Arc::clone(&metadata_b);
      Arc::new(move || vec![metadata_b.lock().unwrap().clone()])
    };
    let running_b: RunningTurnsFn = {
      let metadata_a = Arc::clone(&metadata_a);
      Arc::new(move || vec![metadata_a.lock().unwrap().clone()])
    };
    let mut ctx_a = context(cfg.clone(), "turn-a", None, None, running_a, record_account());
    let mut ctx_b = context(cfg, "turn-b", None, None, running_b, record_account());
    ctx_a.session_meta = Arc::clone(&metadata_a);
    ctx_b.session_meta = Arc::clone(&metadata_b);
    let arrivals = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let placement_a = {
      let state = Arc::clone(&state);
      let arrivals = Arc::clone(&arrivals);
      let notify = Arc::clone(&notify);
      let seen = Arc::clone(&seen);
      tokio::spawn(async move {
        place_turn_with_hook(&state, &ctx_a, None, None, None, None, both_placements_reach_hook(arrivals, notify, seen))
          .await
          .unwrap()
      })
    };
    let placement_b = {
      let state = Arc::clone(&state);
      let arrivals = Arc::clone(&arrivals);
      let notify = Arc::clone(&notify);
      let seen = Arc::clone(&seen);
      tokio::spawn(async move {
        place_turn_with_hook(&state, &ctx_b, None, None, None, None, both_placements_reach_hook(arrivals, notify, seen))
          .await
          .unwrap()
      })
    };
    let account_a = placement_a.await.unwrap();
    let account_b = placement_b.await.unwrap();
    assert_ne!(account_a.label, account_b.label);
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, [1, 2]);
  }

  #[tokio::test]
  async fn newer_transcript_refusal_adopts_holder_under_pool_lock() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    claude_accounts::record_auth_failure(&state, "a", None);
    let source = account_a.join("projects/p/session-id.jsonl");
    let destination = account_b.join("projects/p/session-id.jsonl");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, "same transcript\n").unwrap();
    std::fs::write(&destination, "same transcript\n").unwrap();
    std::fs::File::open(&source)
      .unwrap()
      .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000)))
      .unwrap();
    std::fs::File::open(&destination)
      .unwrap()
      .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)))
      .unwrap();
    let account_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record_account: RecordAccountFn = {
      let state = Arc::clone(&state);
      let account_calls = Arc::clone(&account_calls);
      Arc::new(move |_| {
        let state = Arc::clone(&state);
        let account_calls = Arc::clone(&account_calls);
        Box::pin(async move {
          account_calls.lock().unwrap().push(state.pool_choice.try_lock().is_err());
          Ok(())
        })
      })
    };
    let running_turns: RunningTurnsFn = Arc::new(Vec::new);
    let ctx = context(cfg, "turn", Some("a"), Some("session-id"), running_turns, record_account);
    tokio::time::timeout(Duration::from_secs(5), place_turn(&state, &ctx, Some("session-id"), None, None, None))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(claude_metadata::account_of(&session_metadata(&ctx)).as_deref(), Some("b"));
    assert_eq!(*account_calls.lock().unwrap(), [true, false]);
  }
}
