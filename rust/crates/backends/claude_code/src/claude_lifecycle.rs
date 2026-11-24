//! Process placement and account relay lifecycle for Claude CLI backends.

use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::{
  BackendLifecycle, CLAUDE_SESSION_ID_FIELD, EXTRA_FLAGS_BACKEND_KWARG, IMPROVE_TOKEN_MIN_SECONDS, IdleArmContext,
  Launch, LaunchContext, LaunchKind, LaunchRefused, LaunchWatch, MemberFailure, QuotaOutlook, ReattachHold,
};
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata};
use charliebot_store::run_records::RunKind;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::{
  EXCLUDE_DYNAMIC_SECTIONS_FLAG, claude_accounts, claude_config, claude_idle_compaction, claude_metadata, claude_relay,
  login_dirs, master_cc_relay, midturn_delivery, out_of_family_served_models,
};

pub struct ClaudeCliLifecycle;

impl ClaudeCliLifecycle {
  fn checked_resume_id(&self, ctx: &LaunchContext) -> anyhow::Result<Option<String>> {
    self.checked_resume_id_for(ctx, &login_dirs::claude_config_dir()?)
  }

  fn checked_resume_id_for(&self, ctx: &LaunchContext, config_dir: &Path) -> anyhow::Result<Option<String>> {
    let session_id = ctx.held_native_id.as_deref().filter(|value| !value.is_empty());
    let Some(session_id) = session_id else {
      return Ok(None);
    };
    // A keyed round keeps its conversation on the improve host, so no local login holds its transcript. The held id
    // resumes there, and the host's `claude --resume` fails the round when the transcript is missing.
    if keyed_round(ctx) {
      return Ok(Some(session_id.to_string()));
    }
    if !claude_accounts::transcript_matches(config_dir, session_id)?.is_empty() {
      return Ok(Some(session_id.to_string()));
    }
    tracing::warn!(
      target: "charliebot_agents",
      session = %ctx.session_meta.lock().expect("launch metadata mutex is not poisoned").id,
      cc_session_id = session_id,
      config_dir = %config_dir.display(),
      "master_cc_resume_transcript_missing"
    );
    Ok(None)
  }

  fn launch(&self, ctx: &LaunchContext, resume_id: Option<String>) -> anyhow::Result<Launch> {
    let backend_kwargs = launch_kwargs(ctx, true)?;
    Ok(Launch { backend_kwargs, resume_id, prompt: None, account_label: None, private: None })
  }
}

/// The launch arguments one Claude process receives: its run kind, the manager turn's flags, and the run id. A manager
/// turn's run id names its read-budget hook entries. A task run's run id names its compaction-context hook, so a task
/// launch without one fails. A relayed process continues the same run.
fn launch_kwargs(ctx: &LaunchContext, first_process: bool) -> anyhow::Result<Map<String, Value>> {
  let mut kwargs = Map::new();
  kwargs.insert("run_kind".to_string(), json!(ctx.run_kind));
  if ctx.kind == LaunchKind::Turn {
    kwargs.insert(
      EXTRA_FLAGS_BACKEND_KWARG.to_string(),
      Value::Array(vec![Value::String(EXCLUDE_DYNAMIC_SECTIONS_FLAG.to_string())]),
    );
    midturn_delivery::insert_launch_kwargs(&mut kwargs, ctx)?;
    if ctx.run_kind == RunKind::ManagerTurn {
      let Some(run_id) = ctx.run_id.as_deref().filter(|value| !value.is_empty()) else {
        anyhow::bail!("manager turn Claude launch requires a CharlieBot run id for the read-budget hook");
      };
      kwargs.insert("run_id".to_string(), Value::String(run_id.to_string()));
      kwargs.insert("can_delegate".to_string(), Value::Bool(ctx.can_delegate));
    }
  } else if ctx.kind == LaunchKind::Task {
    let Some(run_id) = ctx.run_id.as_deref().filter(|value| !value.is_empty()) else {
      anyhow::bail!("task Claude launch requires a CharlieBot run id for the compaction-context hook");
    };
    kwargs.insert("run_id".to_string(), Value::String(run_id.to_string()));
    if first_process
      && ctx.held_native_id.is_none()
      && let Some(native_id) = ctx.preassigned_native_id.as_deref()
    {
      kwargs.insert(CLAUDE_SESSION_ID_FIELD.to_string(), Value::String(native_id.to_string()));
    }
  } else {
    anyhow::bail!("unknown launch kind {:?}", ctx.kind);
  }
  Ok(kwargs)
}

impl BackendLifecycle for ClaudeCliLifecycle {
  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      let resume_id = self.checked_resume_id(ctx).map_err(|error| refused(error, MemberFailure::Other, None))?;
      self.launch(ctx, resume_id).map_err(|error| refused(error, MemberFailure::Other, None))
    })
  }

  fn midturn_launch_kwargs(
    &self,
    meta: &SessionMetadata,
    turn: bool,
    run_kind: RunKind,
    run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    midturn_delivery::launch_kwargs(meta, turn, run_kind, run_id)
  }

  fn round_notices(&self, option: &BackendOption, events: &[Value]) -> Vec<Value> {
    let served_models = out_of_family_served_models(events, &option.model);
    if served_models.is_empty() {
      return Vec::new();
    }
    vec![json!({
      "type": event_types::MODEL_FALLBACK_NOTICE,
      "backend": option.id,
      event_types::CONFIGURED_MODEL: option.model,
      event_types::SERVED_MODELS: served_models,
    })]
  }

  fn has_round_notices(&self) -> bool {
    true
  }

  fn disarm_idle_compaction(&self, session_id: &str) {
    claude_idle_compaction::IdleCompaction::shared().disarm(session_id);
  }

  fn settle_idle_compaction<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { claude_idle_compaction::IdleCompaction::shared().settle_for_turn(session_id).await })
  }
}

struct PooledLaunch {
  account: claude_config::ClaudeAccount,
  relays: usize,
  /// The logins this run has launched on, `account` among them. A relay never picks one of them again.
  used: HashSet<String>,
  decision: Arc<Mutex<Option<String>>>,
  /// The reservation the run's placement took on `account`. Held until the launch drops, so the login counts the
  /// run until the round ends or the relay replaces the launch. Nothing reads it. Holding it is the point.
  #[allow(dead_code)]
  handle: Option<Arc<claude_accounts::ReservationHandle>>,
}

/// The cc-claude lifecycle and its one server-owned account observation state.
pub struct ClaudeCodeLifecycle {
  no_pool: ClaudeCliLifecycle,
  state: Arc<claude_accounts::ClaudeAccountState>,
}

impl ClaudeCodeLifecycle {
  pub fn new() -> Self {
    Self::with_state(Arc::new(claude_accounts::ClaudeAccountState::new()))
  }

  pub fn with_state(state: Arc<claude_accounts::ClaudeAccountState>) -> Self {
    Self { no_pool: ClaudeCliLifecycle, state }
  }

  fn launch_args(ctx: &LaunchContext, first_process: bool) -> anyhow::Result<Map<String, Value>> {
    launch_kwargs(ctx, first_process)
  }

  /// The launch on `account`. `used` holds the logins the run launched on before this one. `account` joins them.
  #[allow(clippy::too_many_arguments)]
  fn pooled_launch(
    &self,
    ctx: &LaunchContext,
    account: claude_config::ClaudeAccount,
    resume_id: Option<String>,
    prompt: Option<String>,
    relays: usize,
    handle: Option<Arc<claude_accounts::ReservationHandle>>,
    mut used: HashSet<String>,
  ) -> anyhow::Result<Launch> {
    let mut backend_kwargs = Self::launch_args(ctx, relays == 0)?;
    backend_kwargs
      .insert("claude_account".to_string(), json!({"label": account.label, "config_dir": account.config_dir}));
    let decision = Arc::new(Mutex::new(None));
    used.insert(account.label.clone());
    let private = PooledLaunch { account: account.clone(), relays, used, decision, handle };
    Ok(Launch {
      backend_kwargs,
      resume_id,
      prompt,
      account_label: Some(account.label),
      private: Some(Arc::new(private) as Arc<dyn Any + Send + Sync>),
    })
  }

  async fn pooled_resume_id(&self, ctx: &LaunchContext) -> Result<Option<String>, LaunchRefused> {
    let session_id = ctx.held_native_id.as_deref().filter(|value| !value.is_empty());
    let Some(session_id) = session_id else {
      return Ok(None);
    };
    let _pool_choice = self.state.pool_choice.lock().await;
    let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned").clone();
    let previous_label = claude_metadata::account_of(&meta);
    let current = claude_accounts::account_by_label(&ctx.cfg, previous_label.as_deref())
      .map_err(|error| refused(error, MemberFailure::Other, None))?;
    if let Some(account) = current.as_ref()
      && !claude_accounts::transcript_matches(Path::new(&account.config_dir), session_id)
        .map_err(|error| refused(error, MemberFailure::Other, None))?
        .is_empty()
    {
      return Ok(Some(session_id.to_string()));
    }
    let found = claude_accounts::newest_transcript_copy(&ctx.cfg, session_id)
      .map_err(|error| refused(error, MemberFailure::Other, None))?
      .map(|(account, _path)| account);
    if let Some(found) = found {
      tracing::info!(
        target: "charliebot_agents",
        session = %meta.id,
        cc_session_id = session_id,
        account = found.label.as_str(),
        previous_account = previous_label.as_deref(),
        "master_cc_resume_transcript_found_in_pool"
      );
      if previous_label.as_deref() != Some(found.label.as_str()) {
        claude_metadata::set_account(
          &mut ctx.session_meta.lock().expect("launch metadata mutex is not poisoned"),
          Some(&found.label),
        );
      }
      return Ok(Some(session_id.to_string()));
    }
    tracing::warn!(
      target: "charliebot_agents",
      session = %meta.id,
      cc_session_id = session_id,
      config_dir = current.as_ref().map(|account| account.config_dir.as_str()),
      pool = ?claude_accounts::pool(&ctx.cfg)
        .map_err(|error| refused(error, MemberFailure::Other, None))?
        .iter()
        .map(|account| account.label.as_str())
        .collect::<Vec<_>>(),
      "master_cc_resume_transcript_missing"
    );
    Ok(None)
  }

  fn pooled_state(launch: &Launch) -> Option<&PooledLaunch> {
    launch.private.as_ref()?.downcast_ref::<PooledLaunch>()
  }

  async fn place_task(&self, ctx: &LaunchContext) -> Result<Launch, LaunchRefused> {
    let account_pool = claude_accounts::option_pool(&ctx.option);
    let keyed = keyed_round(ctx);
    let held = ctx.held_native_id.as_deref().filter(|value| !value.is_empty());
    // A run that stays on this host resumes the login that holds its transcript. A keyed round keeps its transcript
    // on the improve host, so it picks its login by the ordinary pool choice and resumes the held id there.
    if !keyed && let Some(session_id) = held {
      let holder = claude_accounts::newest_transcript_copy(&ctx.cfg, session_id)
        .map_err(|error| refused(error, MemberFailure::Other, None))?
        .map(|(account, _path)| account);
      let Some(holder) = holder else {
        let labels = claude_accounts::pool(&ctx.cfg)
          .map_err(|error| refused(error, MemberFailure::Other, None))?
          .into_iter()
          .map(|account| account.label)
          .collect::<Vec<_>>();
        return Err(LaunchRefused {
          message: format!(
            "the held conversation {session_id} has no transcript on any pool login ({})",
            labels.join(", ")
          ),
          failure: MemberFailure::Other,
          resets_at: None,
        });
      };
      return self
        .pooled_launch(
          ctx,
          holder.clone(),
          Some(session_id.to_string()),
          None,
          0,
          Some(claude_accounts::reserve_on(
            &self.state,
            &claude_accounts::run_key(ctx),
            &holder.label,
            "redirect",
            None,
          )),
          HashSet::new(),
        )
        .map_err(|error| refused(error, MemberFailure::Other, None));
    }
    // A keyed round ships its login's token to its host, so a login whose token lives too little for the round is
    // left out. When that is the only reason no login is usable, the round fails as a token shortage.
    // No member is recorded out, because its other logins may serve other rounds.
    let short = if keyed {
      claude_accounts::short_token_logins(&ctx.cfg, account_pool.as_deref(), None)
        .map_err(|error| refused(error, MemberFailure::Other, None))?
    } else {
      HashSet::new()
    };
    let reservation = claude_accounts::reserve_pick(
      &self.state,
      &ctx.cfg,
      Some(&ctx.option.model),
      &short,
      None,
      None,
      account_pool.as_deref(),
      &claude_accounts::run_key(ctx),
    )
    .map_err(|error| refused(error, MemberFailure::Other, None))?;
    let Some(reservation) = reservation else {
      let no_login =
        claude_accounts::no_usable_login(&self.state, &ctx.cfg, Some(&ctx.option.model), None, account_pool.as_deref())
          .map_err(|error| refused(error, MemberFailure::Other, None))?;
      if keyed && !short.is_empty() && no_login.failure != MemberFailure::Quota {
        let mut labels = short.iter().map(String::as_str).collect::<Vec<_>>();
        labels.sort_unstable();
        return Err(LaunchRefused {
          message: format!(
            "no login of the pool has {IMPROVE_TOKEN_MIN_SECONDS} seconds of access-token life for this round; \
             short: {}",
            labels.join(", ")
          ),
          failure: MemberFailure::TokenShort,
          resets_at: None,
        });
      }
      let message = claude_relay::pool_exhausted_message(
        &self.state,
        &ctx.cfg,
        Some(&ctx.option.model),
        None,
        account_pool.as_deref(),
      )
      .map_err(|error| refused(error, MemberFailure::Other, None))?;
      return Err(LaunchRefused { message, failure: no_login.failure, resets_at: no_login.recovers_at });
    };
    self
      .pooled_launch(
        ctx,
        reservation.account,
        held.map(str::to_string),
        None,
        0,
        Some(reservation.handle),
        HashSet::new(),
      )
      .map_err(|error| refused(error, MemberFailure::Other, None))
  }
}

impl Default for ClaudeCodeLifecycle {
  fn default() -> Self {
    Self::new()
  }
}

impl BackendLifecycle for ClaudeCodeLifecycle {
  fn continuation_domain(&self, option: &BackendOption, cfg: &CharlieBotConfig) -> String {
    claude_accounts::continuation_domain(option, cfg).expect("Claude continuation domain is configured")
  }

  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      if ctx.kind == LaunchKind::Turn {
        let session_id = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned").id.clone();
        claude_idle_compaction::IdleCompaction::shared()
          .settle_for_turn(&session_id)
          .await
          .map_err(|error| refused(error, MemberFailure::Other, None))?;
      }
      if !claude_accounts::is_pooled(&ctx.option, &ctx.cfg) {
        return self.no_pool.place(ctx).await;
      }
      match ctx.kind {
        LaunchKind::Turn => {
          let resume_id = self.pooled_resume_id(ctx).await?;
          let (context_tokens, last_request_at) =
            (ctx.context_state)().await.map_err(|error| refused(error, MemberFailure::Other, None))?;
          let placement =
            master_cc_relay::place_turn(&self.state, ctx, resume_id.as_deref(), context_tokens, last_request_at, None)
              .await?;
          let resume_id = if ctx.held_native_id.is_some() { self.pooled_resume_id(ctx).await? } else { resume_id };
          self
            .pooled_launch(ctx, placement.account, resume_id, None, 0, placement.handle, HashSet::new())
            .map_err(|error| refused(error, MemberFailure::Other, None))
        }
        LaunchKind::Task => self.place_task(ctx).await,
      }
    })
  }

  fn midturn_launch_kwargs(
    &self,
    meta: &SessionMetadata,
    turn: bool,
    run_kind: RunKind,
    run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    midturn_delivery::launch_kwargs(meta, turn, run_kind, run_id)
  }

  fn arm_idle_compaction<'a>(&'a self, ctx: IdleArmContext<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
    claude_idle_compaction::arm_for_round(ctx)
  }

  fn disarm_idle_compaction(&self, session_id: &str) {
    claude_idle_compaction::IdleCompaction::shared().disarm(session_id);
  }

  fn settle_idle_compaction<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { claude_idle_compaction::IdleCompaction::shared().settle_for_turn(session_id).await })
  }

  /// The pool answers whether any login can serve the option's model now.
  /// A login can serve it when its readings sit below full, or when none is read yet.
  /// The pool also gives the earliest reset among the logins that its rejections hold out.
  fn quota_outlook(&self, option: &BackendOption, cfg: &CharlieBotConfig) -> anyhow::Result<Option<QuotaOutlook>> {
    if !claude_accounts::is_pooled(option, cfg) {
      return Ok(None);
    }
    let now = claude_accounts::now_or(None);
    let can_run =
      claude_accounts::member_reading(&self.state, cfg, option, now)?.is_none_or(|reading| !reading.exhausted);
    let earliest_reset = claude_accounts::earliest_reset(
      &self.state,
      cfg,
      Some(&option.model),
      Some(now),
      claude_accounts::option_pool(option).as_deref(),
    )?;
    Ok(Some(QuotaOutlook { can_run, earliest_reset }))
  }

  /// The account directory picked by usage from the option's configured login pool. Returns `Ok(Ok(None))` when no
  /// `accounts.claude` logins are configured, or `Ok(Err(reason))` when no pooled login is usable.
  fn resolve_account_dir(
    &self,
    option: &BackendOption,
    cfg: &CharlieBotConfig,
  ) -> anyhow::Result<Result<Option<String>, String>> {
    if !claude_accounts::is_pooled(option, cfg) {
      return Ok(Ok(None));
    }
    let account_pool = claude_accounts::option_pool(option);
    if let Some(account) = claude_accounts::select(
      &self.state,
      cfg,
      Some(&option.model),
      &HashSet::new(),
      None,
      None,
      account_pool.as_deref(),
      0.0,
    )? {
      return Ok(Ok(Some(account.config_dir)));
    }
    let message =
      claude_relay::pool_exhausted_message(&self.state, cfg, Some(&option.model), None, account_pool.as_deref())?;
    Ok(Err(message))
  }

  /// The hold the login that a recovered run's process already uses takes for that run, read from the process's
  /// environment. A process whose environment names no pool login is an unknown login: the follow continues without
  /// a hold.
  fn reattach_hold(&self, cfg: &CharlieBotConfig, pid: u32, run_id: &str) -> ReattachHold {
    match claude_accounts::reattach_hold(&self.state, cfg, pid, run_id, None) {
      Ok(Some(handle)) => ReattachHold { private: Some(Arc::new(handle) as Arc<dyn Any + Send + Sync>) },
      Ok(None) => {
        if claude_accounts::pool(cfg).is_ok_and(|pool| !pool.is_empty()) {
          tracing::error!(target: "charliebot_agents", pid, run_id, "claude_reattach_login_unknown");
        }
        ReattachHold::none()
      }
      Err(error) => {
        tracing::error!(target: "charliebot_agents", pid, run_id, error = %error, "claude_reattach_login_unknown");
        ReattachHold::none()
      }
    }
  }

  fn watch(&self, ctx: &LaunchContext, launch: &Launch) -> Option<Box<dyn LaunchWatch>> {
    let pooled = Self::pooled_state(launch)?;
    Some(Box::new(claude_relay::RelayWatch::new(
      pooled.account.label.clone(),
      Some(ctx.option.model.clone()),
      Arc::clone(&ctx.cfg),
      Arc::clone(&self.state),
      claude_accounts::option_pool(&ctx.option),
      Arc::clone(&pooled.decision),
      keyed_round(ctx),
    )))
  }

  fn next_launch<'a>(
    &'a self,
    ctx: &'a LaunchContext,
    launch: &'a Launch,
    native_id: Option<&'a str>,
  ) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      let pooled = Self::pooled_state(launch).ok_or_else(|| LaunchRefused {
        message: "Claude relay launch has no account state".to_string(),
        failure: MemberFailure::Other,
        resets_at: None,
      })?;
      let decision = pooled.decision.lock().expect("Claude relay decision is not poisoned").clone();
      let decision = decision.ok_or_else(|| LaunchRefused {
        message: "Claude relay watch requested a move without a decision".to_string(),
        failure: MemberFailure::Other,
        resets_at: None,
      })?;
      let account_pool = claude_accounts::option_pool(&ctx.option);
      // A keyed round forwards its login's token once, when it starts, so it cannot relay: its decision ends the round.
      // A lost forwarded token says nothing about the local login it was copied from, so that login is not marked.
      if keyed_round(ctx) {
        return Err(
          claude_relay::keyed_round_refusal(
            &self.state,
            &ctx.cfg,
            Some(&ctx.option.model),
            account_pool.as_deref(),
            &decision,
          )
          .map_err(|error| refused(error, MemberFailure::Other, None))?,
        );
      }
      if decision == claude_relay::LOGIN_FAILED {
        master_cc_relay::report_login_failure(&self.state, ctx, &pooled.account)
          .await
          .map_err(|error| refused(error, MemberFailure::Other, None))?;
      }
      if let Some(cap_refusal) = claude_relay::relay_cap_refusal(
        &self.state,
        &ctx.cfg,
        Some(&ctx.option.model),
        pooled.relays,
        &decision,
        account_pool.as_deref(),
      )
      .map_err(|error| refused(error, MemberFailure::Other, None))?
      {
        return Err(cap_refusal);
      }
      let relays = pooled.relays + 1;
      let placement = master_cc_relay::prepare_relay(
        &self.state,
        ctx,
        &pooled.account,
        native_id,
        Some(&decision),
        &pooled.used,
        relays,
      )
      .await?;
      if ctx.kind == LaunchKind::Turn {
        (ctx.record_account)(placement.account.label.clone())
          .await
          .map_err(|error| refused(error, MemberFailure::Other, None))?;
      }
      self
        .pooled_launch(
          ctx,
          placement.account,
          native_id.map(str::to_string),
          Some(claude_relay::CONTINUATION_PROMPT.to_string()),
          relays,
          placement.handle,
          pooled.used.clone(),
        )
        .map_err(|error| refused(error, MemberFailure::Other, None))
    })
  }

  fn after_round<'a>(
    &'a self,
    ctx: &'a LaunchContext,
    native_id: Option<&'a str>,
    succeeded: bool,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let has_account =
        claude_metadata::account_of(&ctx.session_meta.lock().expect("launch metadata mutex is not poisoned")).is_some();
      if ctx.kind == LaunchKind::Turn
        && succeeded
        && has_account
        && let Some(session_id) = native_id.filter(|id| !id.is_empty())
      {
        let cfg = Arc::clone(&ctx.cfg);
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || claude_accounts::retire_transcript_copies(&cfg, &session_id, 2)).await??;
      }
      Ok(())
    })
  }

  fn round_notices(&self, option: &BackendOption, events: &[Value]) -> Vec<Value> {
    self.no_pool.round_notices(option, events)
  }

  fn has_round_notices(&self) -> bool {
    true
  }

  fn account_source(&self) -> &'static str {
    "claude_account"
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    claude_metadata::account_of(meta)
  }

  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> bool {
    if self.account_label(meta).as_deref() == label {
      return false;
    }
    claude_metadata::set_account(meta, label);
    true
  }

  fn thread_native_id(&self, thread: &ThreadMetadata) -> Option<String> {
    claude_metadata::session_id_of(thread)
  }

  fn assign_thread_native_id(&self, thread: &mut ThreadMetadata, native_id: Option<&str>) {
    claude_metadata::set_session_id(thread, native_id);
  }
}

/// Whether the run is a keyed round: its task names an improve host, and the round's login travels to that host.
fn keyed_round(ctx: &LaunchContext) -> bool {
  let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned");
  meta.improve_host().is_some()
}

fn refused(
  error: impl std::fmt::Display,
  failure: MemberFailure,
  resets_at: Option<charliebot_store::metadata::UtcDateTime>,
) -> LaunchRefused {
  LaunchRefused { message: error.to_string(), failure, resets_at }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;

  use charliebot_agents::backend_availability::BackendAvailability;
  use charliebot_agents::model_pool::{LaneStart, RotationPositions, Selector};
  use charliebot_registry::Registry;
  use charliebot_registry::backend::{ContextStateFn, EmitFn, LaunchArgs, RecordAccountFn, RunningTurnsFn};
  use charliebot_store::metadata::{TaskApprover, TaskProfile, TaskSpec, UtcDateTime};
  use charliebot_store::run_records::RunKind;
  use serde_json::json;

  use super::*;
  use crate::{ClaudeCodeBackend, event_types, turn_contribution};

  fn disabled_cgroup() -> charliebot_agents::transport::cgroup::SessionCgroupConfig {
    charliebot_agents::transport::cgroup::SessionCgroupConfig::disabled()
  }

  fn config(home: &Path, accounts: &[(&str, &Path)]) -> CharlieBotConfig {
    fs::create_dir_all(home).unwrap();
    let mut configured = String::new();
    let mut labels = Vec::new();
    for (label, path) in accounts {
      configured.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", path.display()));
      labels.push((*label).to_string());
    }
    fs::write(
      home.join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n      account_pool: alpha\naccounts:\n  claude:\n{configured}  claude_pools:\n    alpha: [{}]\n",
        labels.join(", ")
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  fn unpooled_config(home: &Path) -> CharlieBotConfig {
    fs::create_dir_all(home).unwrap();
    fs::write(
      home.join("config.yaml"),
      "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n",
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  fn account_credentials(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join(login_dirs::CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#).unwrap();
  }

  fn context(
    cfg: CharlieBotConfig,
    kind: LaunchKind,
    account_label: Option<&str>,
    held_native_id: Option<&str>,
    running_turns: Option<RunningTurnsFn>,
  ) -> LaunchContext {
    let option = cfg.backends.options[0].clone();
    let mut metadata = SessionMetadata::new("session", "Session", TaskProfile::Manager);
    claude_metadata::set_account(&mut metadata, account_label);
    let emit: EmitFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let session_meta = Arc::new(Mutex::new(metadata));
    let record_account: RecordAccountFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let context_state: ContextStateFn = Arc::new(|| Box::pin(async { Ok((None, None)) }));
    LaunchContext {
      cfg: Arc::new(cfg),
      option,
      session_meta,
      kind,
      run_kind: match kind {
        LaunchKind::Turn => RunKind::ManagerTurn,
        LaunchKind::Task => RunKind::Work,
      },
      cwd: PathBuf::from("/synthetic/cwd"),
      base_env: Arc::new(charliebot_registry::backend::Env::new()),
      held_native_id: held_native_id.map(str::to_string),
      preassigned_native_id: Some("task-session".to_string()),
      emit,
      record_account,
      context_state,
      running_turns,
      run_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
      can_delegate: false,
    }
  }

  #[tokio::test]
  async fn placement_adopts_a_split_newest_transcript_before_account_selection() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let transcript_a = account_a.join("projects/cwd/session-id.jsonl");
    let transcript_b = account_b.join("projects/cwd/session-id.jsonl");
    fs::create_dir_all(transcript_a.parent().unwrap()).unwrap();
    fs::write(&transcript_a, "old lineage row\n").unwrap();
    fs::create_dir_all(transcript_b.parent().unwrap()).unwrap();
    fs::write(&transcript_b, "new fork row\n").unwrap();
    let last_request = charliebot_store::metadata::UtcDateTime::now();
    let running_turns: RunningTurnsFn = Arc::new(Vec::new);
    let mut ctx = context(cfg, LaunchKind::Turn, Some("a"), Some("session-id"), Some(running_turns));
    ctx.context_state = Arc::new(move || Box::pin(async move { Ok((None, Some(last_request))) }));
    let lifecycle = ClaudeCodeLifecycle::new();
    let launch = lifecycle.place(&ctx).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("b"));
    assert_eq!(claude_metadata::account_of(&ctx.session_meta.lock().unwrap()).as_deref(), Some("b"));
  }

  fn write_copy(path: &Path, text: &str, modified: std::time::SystemTime) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::File::options().write(true).open(path).unwrap().set_modified(modified).unwrap();
  }

  /// Pool order names login "a" first, and its copy is the older one. Login "b" holds the newer copy.
  fn split_copies(root: &Path) -> (CharlieBotConfig, PathBuf, PathBuf) {
    let account_a = root.join("account-a");
    let account_b = root.join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&root.join("home"), &[("a", &account_a), ("b", &account_b)]);
    let older = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    let newer = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    write_copy(&account_a.join("projects/cwd/session-id.jsonl"), "old lineage row\n", older);
    write_copy(&account_b.join("projects/cwd/session-id.jsonl"), "new fork row\n", newer);
    (cfg, account_a, account_b)
  }

  /// A held task resumes on the login whose copy is newest, not on the first login in pool order that holds a copy.
  #[tokio::test]
  async fn a_held_task_resumes_on_the_login_with_the_newest_copy() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, _account_a, _account_b) = split_copies(root.path());
    let ctx = context(cfg, LaunchKind::Task, None, Some("session-id"), None);
    let launch = ClaudeCodeLifecycle::new().place(&ctx).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("b"));
  }

  /// The turn resume's holder lookup takes the newest copy too, and records that login on the session.
  #[tokio::test]
  async fn the_resume_fallback_takes_the_login_with_the_newest_copy() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, _account_a, _account_b) = split_copies(root.path());
    let ctx = context(cfg, LaunchKind::Turn, None, Some("session-id"), None);
    let resume = ClaudeCodeLifecycle::new().pooled_resume_id(&ctx).await.unwrap();
    assert_eq!(resume.as_deref(), Some("session-id"));
    assert_eq!(claude_metadata::account_of(&ctx.session_meta.lock().unwrap()).as_deref(), Some("b"));
  }

  /// A relay copies the transcript and keeps its modification time. When the destination login then appends to its
  /// copy, that copy is the newest, and the next resume reads it.
  #[tokio::test]
  async fn a_relayed_copy_that_the_destination_appends_to_is_the_one_resumed() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&root.path().join("home"), &[("a", &account_a), ("b", &account_b)]);
    let older = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    write_copy(&account_a.join("projects/cwd/session-id.jsonl"), "first row\n", older);
    claude_accounts::move_transcript("session-id", &account_a, &account_b).unwrap();
    fs::OpenOptions::new()
      .append(true)
      .open(account_b.join("projects/cwd/session-id.jsonl"))
      .unwrap()
      .write_all(b"second row\n")
      .unwrap();
    let ctx = context(cfg, LaunchKind::Task, None, Some("session-id"), None);
    let launch = ClaudeCodeLifecycle::new().place(&ctx).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("b"));
  }

  #[test]
  fn cli_variants_and_unpooled_cc_claude_reuse_transcript_checked_flag_launches() {
    // The cc-anthropic-compatible and cc-openai-compatible registrations share ClaudeCliLifecycle.
    let root = tempfile::tempdir().unwrap();
    let cfg = unpooled_config(&root.path().join("home"));
    let cc_session_id = "synthetic-cc-session";
    let ctx = context(cfg.clone(), LaunchKind::Turn, None, Some(cc_session_id), Some(Arc::new(Vec::new)));
    let config_dir = root.path().join("claude-default");
    let cli = ClaudeCliLifecycle;
    let claude = ClaudeCodeLifecycle::new();
    let missing = cli.checked_resume_id_for(&ctx, &config_dir).unwrap();
    let missing_launch = cli.launch(&ctx, missing).unwrap();
    assert_eq!(missing_launch.resume_id, None);
    let cc_missing = claude.no_pool.checked_resume_id_for(&ctx, &config_dir).unwrap();
    let cc_missing_launch = claude.no_pool.launch(&ctx, cc_missing).unwrap();
    assert_eq!(cc_missing_launch.resume_id, None);

    let transcript = config_dir.join("projects/synthetic-project").join(format!("{cc_session_id}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "synthetic transcript\n").unwrap();
    let present = cli.checked_resume_id_for(&ctx, &config_dir).unwrap();
    assert_eq!(present.as_deref(), Some(cc_session_id));
    let cli_launch = cli.launch(&ctx, present.clone()).unwrap();

    let cc_resume = claude.no_pool.checked_resume_id_for(&ctx, &config_dir).unwrap();
    assert_eq!(cc_resume.as_deref(), Some(cc_session_id));
    let claude_launch = claude.no_pool.launch(&ctx, cc_resume).unwrap();
    let expected = json!({
      "extra_flags": [EXCLUDE_DYNAMIC_SECTIONS_FLAG],
      "run_kind": "manager_turn",
      "midturn_delivery": true,
      "midturn_run_id": "11111111-2222-3333-4444-555555555555",
      "run_id": "11111111-2222-3333-4444-555555555555",
      "can_delegate": false,
    });

    let fresh_ctx = context(cfg, LaunchKind::Turn, None, None, Some(Arc::new(Vec::new)));
    let fresh = cli.checked_resume_id_for(&fresh_ctx, &config_dir).unwrap();
    let fresh_launch = cli.launch(&fresh_ctx, fresh).unwrap();
    for launch in [&missing_launch, &cc_missing_launch, &cli_launch, &fresh_launch, &claude_launch] {
      assert_eq!(json!(launch.backend_kwargs), expected);
      assert_eq!(launch.account_label, None);
    }
    assert_eq!(cli_launch.resume_id.as_deref(), Some(cc_session_id));
    assert_eq!(claude_launch.resume_id.as_deref(), Some(cc_session_id));
    assert!(claude.watch(&ctx, &claude_launch).is_none());
  }

  #[tokio::test]
  async fn pooled_tasks_carry_no_running_turn_callback() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_dir = root.path().join("account");
    account_credentials(&account_dir);
    let cfg = config(&home, &[("a", &account_dir)]);
    let ctx = context(cfg, LaunchKind::Task, None, None, None);
    let launch = ClaudeCodeLifecycle::new().place(&ctx).await.unwrap();
    assert!(ctx.running_turns.is_none());
    assert_eq!(launch.backend_kwargs["claude_session_id"], "task-session");
    assert_eq!(launch.account_label.as_deref(), Some("a"));
  }

  #[tokio::test]
  async fn a_keyed_task_with_only_short_logins_is_refused_as_token_short() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    let expires_at_ms = UtcDateTime::now().into_inner().unix_timestamp() * 1000;
    for account in [&account_a, &account_b] {
      fs::create_dir_all(account).unwrap();
      fs::write(
        account.join(login_dirs::CREDENTIALS_FILE),
        json!({"claudeAiOauth": {"accessToken": "synthetic", "expiresAt": expires_at_ms}}).to_string(),
      )
      .unwrap();
    }
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let ctx = context(cfg, LaunchKind::Task, None, None, None);
    ctx.session_meta.lock().unwrap().task = Some(TaskSpec {
      goal: String::new(),
      repo_path: None,
      base_branch: None,
      task_type: None,
      keep_worktree: false,
      cold_read: None,
      approver: TaskApprover::User,
      improve_host: Some("remote-test-host".to_string()),
    });

    let refusal = match ClaudeCodeLifecycle::new().place(&ctx).await {
      Err(refusal) => refusal,
      Ok(_) => panic!("the task started without a login that has enough token life"),
    };

    assert_eq!(refusal.failure, MemberFailure::TokenShort);
    assert!(refusal.message.contains("no login of the pool has"), "the refusal does not name the token-life condition");
  }

  /// Credentials whose access token lives longer than a keyed round needs.
  fn long_lived_credentials(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let expires_at_ms = (UtcDateTime::now().into_inner().unix_timestamp() + 86_400) * 1000;
    fs::write(
      path.join(login_dirs::CREDENTIALS_FILE),
      json!({"claudeAiOauth": {"accessToken": "synthetic", "expiresAt": expires_at_ms}}).to_string(),
    )
    .unwrap();
  }

  #[tokio::test]
  async fn a_keyed_task_keeps_its_held_conversation_on_the_pooled_path() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    long_lived_credentials(&account_a);
    long_lived_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let held = "held-conversation";

    // An unkeyed run searches the local logins for the transcript, finds none, and refuses the task.
    let unkeyed = context(cfg.clone(), LaunchKind::Task, None, Some(held), None);
    let refusal = match ClaudeCodeLifecycle::new().place(&unkeyed).await {
      Err(refusal) => refusal,
      Ok(_) => panic!("the unkeyed task resumed a conversation that no local login holds"),
    };
    assert!(refusal.message.contains("has no transcript on any pool login"), "{}", refusal.message);

    // A keyed run keeps the id: its transcript is on the improve host, and the run forwards a chosen login's token.
    let keyed = context(cfg, LaunchKind::Task, None, Some(held), None);
    keyed.session_meta.lock().unwrap().task = Some(TaskSpec {
      goal: String::new(),
      repo_path: None,
      base_branch: None,
      task_type: None,
      keep_worktree: false,
      cold_read: None,
      approver: TaskApprover::User,
      improve_host: Some("remote-test-host".to_string()),
    });
    let launch = ClaudeCodeLifecycle::new().place(&keyed).await.unwrap();
    assert_eq!(launch.resume_id.as_deref(), Some(held));
    assert!(launch.account_label.is_some(), "the keyed task picks its login by the ordinary pool choice");
  }

  /// A keyed round's login counts the round's handle, a concurrent pick sees it, and the handle ends with the round.
  #[tokio::test]
  async fn a_keyed_rounds_login_counts_a_handle_that_concurrent_picks_see() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    long_lived_credentials(&account_a);
    long_lived_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let mut keyed = context(cfg.clone(), LaunchKind::Task, None, None, None);
    keyed.session_meta.lock().unwrap().task = Some(TaskSpec {
      goal: String::new(),
      repo_path: None,
      base_branch: None,
      task_type: None,
      keep_worktree: false,
      cold_read: None,
      approver: TaskApprover::User,
      improve_host: Some("remote-test-host".to_string()),
    });
    keyed.run_id = Some("keyed-run".to_string());

    let launch = lifecycle.place(&keyed).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("a"));
    let counts = state.reservation_counts(UtcDateTime::now());
    assert_eq!(counts, std::collections::HashMap::from([("a".to_string(), 1)]));

    // A concurrent pick sees the handle: "b"'s unreserved headroom outranks "a"'s, where the tie band would have
    // kept the pool order on "a".
    let mut plain = context(cfg, LaunchKind::Task, None, None, None);
    plain.run_id = Some("plain-run".to_string());
    let plain_launch = lifecycle.place(&plain).await.unwrap();
    assert_eq!(plain_launch.account_label.as_deref(), Some("b"));

    drop((launch, plain_launch));
    assert!(state.reservation_counts(UtcDateTime::now()).is_empty(), "the handle ends with the round");
  }

  /// A task that returns to the login holding its transcript registers a handle there.
  #[tokio::test]
  async fn a_task_returning_to_its_transcript_holder_holds_a_handle() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let account_a = root.path().join("account-a");
    let account_b = root.path().join("account-b");
    account_credentials(&account_a);
    account_credentials(&account_b);
    let cfg = config(&home, &[("a", &account_a), ("b", &account_b)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let transcript = account_a.join("projects/cwd/held-session.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "held transcript\n").unwrap();

    let ctx = context(cfg, LaunchKind::Task, None, Some("held-session"), None);
    let launch = lifecycle.place(&ctx).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("a"));
    let counts = state.reservation_counts(UtcDateTime::now());
    assert_eq!(counts, std::collections::HashMap::from([("a".to_string(), 1)]));
  }

  #[test]
  fn a_keyed_run_keeps_its_held_conversation_on_the_plain_path() {
    let root = tempfile::tempdir().unwrap();
    let cfg = unpooled_config(&root.path().join("home"));
    let config_dir = root.path().join("claude-default");
    let held = "held-conversation";
    let ctx = context(cfg, LaunchKind::Task, None, Some(held), None);

    // An unkeyed run drops the id that no local login holds, and logs `master_cc_resume_transcript_missing`.
    assert_eq!(ClaudeCliLifecycle.checked_resume_id_for(&ctx, &config_dir).unwrap(), None);

    // A keyed run keeps the id: its transcript is on the improve host, so no local search decides.
    ctx.session_meta.lock().unwrap().task = Some(TaskSpec {
      goal: String::new(),
      repo_path: None,
      base_branch: None,
      task_type: None,
      keep_worktree: false,
      cold_read: None,
      approver: TaskApprover::User,
      improve_host: Some("remote-test-host".to_string()),
    });
    assert_eq!(ClaudeCliLifecycle.checked_resume_id_for(&ctx, &config_dir).unwrap().as_deref(), Some(held));
  }

  /// A pool of the login a round starts on, `current`, and a login `fresh` with no reading yet.
  fn current_and_fresh(root: &Path) -> (CharlieBotConfig, PathBuf) {
    let current = root.join("current");
    let fresh = root.join("fresh");
    account_credentials(&current);
    account_credentials(&fresh);
    (config(&root.join("home"), &[("current", &current), ("fresh", &fresh)]), current)
  }

  /// A config whose priority pool `cheap` holds two Claude members, `claude-haiku` then `claude-sonnet`. Both are paid
  /// by the subscription and run on the login pool `alpha` of `accounts`.
  fn cheap_config(home: &Path, accounts: &[(&str, &Path)]) -> CharlieBotConfig {
    fs::create_dir_all(home).unwrap();
    let mut configured = String::new();
    let mut labels = Vec::new();
    for (label, path) in accounts {
      configured.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", path.display()));
      labels.push((*label).to_string());
    }
    fs::write(
      home.join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: claude-haiku\n      label: Haiku\n      type: cc-claude\n      model: claude-haiku-5-5\n      account_pool: alpha\n      billing: subscription\n    - id: claude-sonnet\n      label: Sonnet\n      type: cc-claude\n      model: claude-sonnet-5-5\n      account_pool: alpha\n      billing: subscription\n  pools:\n    - {{id: cheap, label: Cheap, rule: priority, members: [claude-haiku, claude-sonnet]}}\naccounts:\n  claude:\n{configured}  claude_pools:\n    alpha: [{}]\n",
        labels.join(", ")
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("backends.claude_code", crate::register).unwrap();
    registry.load_config(home).unwrap()
  }

  /// The launch of the pool's `current` login from a task context, keyed to an improve host when `keyed` is set.
  fn current_launch(
    lifecycle: &ClaudeCodeLifecycle,
    cfg: CharlieBotConfig,
    current: &Path,
    keyed: bool,
  ) -> (LaunchContext, Launch) {
    let ctx = context(cfg, LaunchKind::Task, None, None, None);
    if keyed {
      ctx.session_meta.lock().unwrap().task = Some(TaskSpec {
        goal: String::new(),
        repo_path: None,
        base_branch: None,
        task_type: None,
        keep_worktree: false,
        cold_read: None,
        approver: TaskApprover::User,
        improve_host: Some("remote-test-host".to_string()),
      });
    }
    let account =
      claude_config::ClaudeAccount { label: "current".to_string(), config_dir: current.display().to_string() };
    let launch = lifecycle.pooled_launch(&ctx, account, None, None, 0, None, HashSet::new()).unwrap();
    (ctx, launch)
  }

  /// A rate-limit event for the round's login: a window at `utilization`, resetting in 2030.
  fn rate_limit(status: &str, utilization: f64) -> Value {
    let reset = UtcDateTime::parse("2030-01-01T00:00:00Z").unwrap().into_inner().unix_timestamp() as f64;
    json!({
      "type": "rate_limit_event",
      "rate_limit_info": {
        "status": status,
        "resetsAt": reset,
        "unifiedWindows": {"five_hour": {"utilization": utilization}}
      }
    })
  }

  fn tool_result() -> Value {
    json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}})
  }

  fn lost_login() -> Value {
    json!({
      "type": "assistant",
      "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "Failed to authenticate"}]},
      "error": "authentication_failed",
      "is_api_error_message": true,
      "api_error_status": 401
    })
  }

  fn clean_result() -> Value {
    json!({"type": "result", "subtype": "success", "is_error": false, "result": "done"})
  }

  /// After a run ends cleanly on a login that rejected a request, the pool's next real pick skips that login: its
  /// headroom is zero until the reset. The run ends with no refusal, so no member is recorded out.
  #[tokio::test]
  async fn a_new_pick_after_a_clean_end_skips_the_login_that_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, _current) = current_and_fresh(root.path());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let first = context(cfg.clone(), LaunchKind::Task, None, None, None);
    let launch = lifecycle.place(&first).await.unwrap();
    assert_eq!(launch.account_label.as_deref(), Some("current"));

    let mut watch = lifecycle.watch(&first, &launch).expect("a pooled launch carries a watch");
    assert!(!watch.observe(&rate_limit("rejected", 1.0)));
    assert!(!watch.observe(&clean_result()));
    assert!(!watch.wants_next(0, ""), "the clean end is not relayed");

    let mut next = context(cfg, LaunchKind::Task, None, None, None);
    next.run_id = Some("next-run".to_string());
    let next_launch = lifecycle.place(&next).await.expect("the pool still has a login with headroom");
    assert_eq!(next_launch.account_label.as_deref(), Some("fresh"));
  }

  #[tokio::test]
  async fn a_keyed_round_at_the_warning_line_is_not_relayed() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, current) = current_and_fresh(root.path());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    claude_accounts::observe_rate_limit(
      &state,
      "fresh",
      &json!({"unifiedWindows": {"five_hour": {"utilization": 0.20}}}),
      None,
      None,
    );
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let (ctx, launch) = current_launch(&lifecycle, cfg, &current, true);
    let mut watch = lifecycle.watch(&ctx, &launch).expect("a pooled launch has a watch");

    assert!(!watch.observe(&rate_limit("allowed_warning", 0.97)), "the keyed watch arms no warning relay");
    assert!(!watch.observe(&tool_result()), "the keyed round is not stopped at its next tool result");
    assert!(!watch.wants_next(0, ""), "the keyed round starts no relay after it exits");
    let reading =
      claude_accounts::latest_reading(&state, "current", None, None).expect("the watch records the reading");
    assert_eq!(reading.utilization, 0.97);
  }

  #[tokio::test]
  async fn an_unkeyed_round_at_the_warning_line_still_relays() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, current) = current_and_fresh(root.path());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    claude_accounts::observe_rate_limit(
      &state,
      "fresh",
      &json!({"unifiedWindows": {"five_hour": {"utilization": 0.20}}}),
      None,
      None,
    );
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let (ctx, launch) = current_launch(&lifecycle, cfg, &current, false);
    let mut watch = lifecycle.watch(&ctx, &launch).expect("a pooled launch has a watch");

    assert!(!watch.observe(&rate_limit("allowed_warning", 0.97)));
    assert!(watch.observe(&tool_result()), "the unkeyed round stops at its next tool result to relay");
  }

  #[tokio::test]
  async fn a_keyed_round_rejected_on_quota_ends_with_the_pool_reset() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, current) = current_and_fresh(root.path());
    let lifecycle = ClaudeCodeLifecycle::new();
    let (ctx, launch) = current_launch(&lifecycle, cfg, &current, true);
    let mut watch = lifecycle.watch(&ctx, &launch).expect("a pooled launch has a watch");

    assert!(!watch.observe(&rate_limit("rejected", 1.0)));
    assert!(watch.wants_next(1, ""));
    let refusal = match lifecycle.next_launch(&ctx, &launch, None).await {
      Err(refusal) => refusal,
      Ok(_) => panic!("the keyed round relayed after its login was rejected on quota"),
    };

    assert_eq!(refusal.failure, MemberFailure::Quota);
    assert_eq!(refusal.resets_at.map(|value| value.to_str()).as_deref(), Some("2030-01-01T00:00:00Z"));
    assert!(refusal.failure.fails_over(), "the pool moves the round to its next member");
  }

  #[tokio::test]
  async fn a_keyed_round_whose_login_is_lost_ends_as_a_token_shortage() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, current) = current_and_fresh(root.path());
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let (ctx, launch) = current_launch(&lifecycle, cfg, &current, true);
    let mut watch = lifecycle.watch(&ctx, &launch).expect("a pooled launch has a watch");

    assert!(!watch.observe(&lost_login()));
    assert!(watch.wants_next(1, ""));
    let refusal = match lifecycle.next_launch(&ctx, &launch, None).await {
      Err(refusal) => refusal,
      Ok(_) => panic!("the keyed round relayed after its login was lost"),
    };

    assert_eq!(refusal.failure, MemberFailure::TokenShort);
    assert_eq!(refusal.resets_at, None);
    assert!(refusal.failure.fails_over(), "the pool moves the round to its next member");
    assert!(!refusal.failure.records_out(), "a token shortage records no member out");
    let account =
      claude_config::ClaudeAccount { label: "current".to_string(), config_dir: current.display().to_string() };
    assert!(
      claude_accounts::healthy(&state, &account, None),
      "the forwarded copy failed, not the local login, so the login stays selectable"
    );
  }

  #[tokio::test]
  async fn a_keyed_round_rejected_on_quota_moves_to_the_next_cheap_member() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let current = root.path().join("current");
    let fresh = root.path().join("fresh");
    account_credentials(&current);
    account_credentials(&fresh);
    let cfg = cheap_config(&home, &[("current", &current), ("fresh", &fresh)]);
    let lifecycle = ClaudeCodeLifecycle::new();
    let (ctx, launch) = current_launch(&lifecycle, cfg.clone(), &current, true);
    let mut watch = lifecycle.watch(&ctx, &launch).expect("a pooled launch has a watch");

    assert!(!watch.observe(&rate_limit("rejected", 1.0)));
    assert!(watch.wants_next(1, ""));
    let refusal = match lifecycle.next_launch(&ctx, &launch, None).await {
      Err(refusal) => refusal,
      Ok(_) => panic!("the keyed round relayed after its login was rejected on quota"),
    };
    assert!(refusal.failure.fails_over(), "the failed round moves to the pool's next member");

    // The failover chain of a failed run records its member out, for the failure's reason.
    // The pool's next pick leaves out every member the chain has left, `claude-haiku` among them.
    let mut registry = Registry::new();
    registry.register_package("backends.claude_code", crate::register).unwrap();
    let availability = Arc::new(BackendAvailability::load(&home).unwrap());
    let reason = refusal.failure.availability_reason().expect("a quota failure records its member out");
    availability.record(&cfg, &registry, "claude-haiku", refusal.resets_at, reason, &refusal.message).unwrap();
    let selector = Selector::new(&cfg, &registry, availability, Arc::new(RotationPositions::new(LaneStart::Head)));
    let left = ["claude-haiku".to_string()];
    let next = selector.select("cheap", &left).unwrap().expect("the cheap pool holds a second member");
    assert_eq!(next.id, "claude-sonnet");
  }

  #[tokio::test]
  async fn only_a_successful_labeled_turn_retires_old_transcript_copies() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let dirs = [root.path().join("a"), root.path().join("b"), root.path().join("c")];
    let labels = [("a", dirs[0].as_path()), ("b", dirs[1].as_path()), ("c", dirs[2].as_path())];
    let cfg = config(&home, &labels);
    for dir in &dirs {
      let path = dir.join("projects/cwd/session-id.jsonl");
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, "synthetic transcript").unwrap();
    }
    let lifecycle = ClaudeCodeLifecycle::new();
    let successful_turn = context(cfg.clone(), LaunchKind::Turn, Some("a"), None, Some(Arc::new(Vec::new)));
    lifecycle.after_round(&successful_turn, Some("session-id"), true).await.unwrap();
    assert_eq!(dirs.iter().filter(|dir| dir.join("projects/cwd/session-id.jsonl").exists()).count(), 2);

    for dir in &dirs {
      fs::write(dir.join("projects/cwd/session-id.jsonl"), "restored transcript").unwrap();
    }
    let failed_turn = context(cfg.clone(), LaunchKind::Turn, Some("a"), None, Some(Arc::new(Vec::new)));
    lifecycle.after_round(&failed_turn, Some("session-id"), false).await.unwrap();
    let task = context(cfg, LaunchKind::Task, Some("a"), None, None);
    lifecycle.after_round(&task, Some("session-id"), true).await.unwrap();
    assert_eq!(dirs.iter().filter(|dir| dir.join("projects/cwd/session-id.jsonl").exists()).count(), 3);
  }

  #[test]
  fn login_required_renderer_keeps_account_details_out_of_chat() {
    let event = json!({"type":"claude_account_login_required","account":"synthetic","config_dir":"/synthetic/login"});
    let rendered = turn_contribution::render_login_required(&event);
    assert_eq!(rendered["kind"], event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED);
    assert!(rendered["content"].as_str().is_some_and(|content| !content.is_empty()));
  }

  #[tokio::test]
  async fn a_turn_start_cancels_the_pending_idle_timer() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    account_credentials(&account_dir);
    let cfg = config(&root.path().join("home"), &[("a", &account_dir)]);
    let ctx = context(cfg, LaunchKind::Turn, None, None, Some(Arc::new(Vec::new)));
    let session_id = "idle-turn-start";
    ctx.session_meta.lock().unwrap().id = session_id.to_string();
    let idle = claude_idle_compaction::IdleCompaction::shared();
    idle.arm(claude_idle_compaction::test_support::timer_input(
      session_id,
      UtcDateTime::now(),
      claude_idle_compaction::test_support::no_candidate(),
    ));
    let timer = idle.timer_abort_handle(session_id).expect("the round armed a timer");

    ClaudeCodeLifecycle::new().place(&ctx).await.unwrap();

    assert!(!idle.has_timer(session_id));
    tokio::task::yield_now().await;
    assert!(timer.is_finished(), "the pending timer is cancelled");
  }

  #[tokio::test]
  async fn place_waits_for_a_running_idle_compaction_before_it_starts_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    account_credentials(&account_dir);
    let cfg = config(&root.path().join("home"), &[("a", &account_dir)]);
    let ctx = context(cfg, LaunchKind::Turn, None, None, Some(Arc::new(Vec::new)));
    let session_id = "idle-turn-wait";
    ctx.session_meta.lock().unwrap().id = session_id.to_string();
    let idle = claude_idle_compaction::IdleCompaction::shared();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let run: claude_idle_compaction::RunFn = {
      let started = Arc::clone(&started);
      let release = Arc::clone(&release);
      Arc::new(move || {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Box::pin(async move {
          started.notify_one();
          release.notified().await;
          Ok(())
        }) as futures_util::future::BoxFuture<'static, anyhow::Result<()>>
      })
    };
    let request = UtcDateTime::now();
    idle.arm(claude_idle_compaction::test_support::candidate_input(
      session_id,
      claude_idle_compaction::test_support::armed_facts(request, "synthetic-claude-session"),
      false,
      run,
    ));
    let compaction = idle.fire_at(session_id, UtcDateTime::now()).unwrap().expect("every idle condition holds");
    started.notified().await;

    let placed = tokio::spawn(async move { ClaudeCodeLifecycle::new().place(&ctx).await });
    for _ in 0..3 {
      tokio::task::yield_now().await;
    }
    assert!(!placed.is_finished(), "the turn waits for the running compaction");

    release.notify_one();
    placed.await.unwrap().unwrap();
    assert!(compaction.is_finished(), "the turn starts after the compaction ends");
    compaction.await.unwrap();
  }

  #[test]
  fn pooled_relay_turn_uses_the_same_midturn_launch_arguments() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account");
    account_credentials(&account_dir);
    let cfg = config(&root.path().join("home"), &[("a", &account_dir)]);
    let ctx = context(cfg.clone(), LaunchKind::Turn, None, None, Some(Arc::new(Vec::new)));
    let account =
      claude_config::ClaudeAccount { label: "a".to_string(), config_dir: account_dir.display().to_string() };
    let lifecycle = ClaudeCodeLifecycle::new();

    let first = lifecycle.pooled_launch(&ctx, account.clone(), None, None, 0, None, HashSet::new()).unwrap();
    let relay =
      lifecycle.pooled_launch(&ctx, account, None, Some("continue".to_string()), 1, None, HashSet::new()).unwrap();

    assert_eq!(first.backend_kwargs["midturn_delivery"], json!(true));
    assert_eq!(relay.backend_kwargs["midturn_delivery"], json!(true));
    let run_id = ctx.run_id.clone().unwrap();
    assert_eq!(first.backend_kwargs["midturn_run_id"], json!(run_id));
    assert_eq!(relay.backend_kwargs["midturn_run_id"], json!(run_id));
  }

  /// A thread turn takes the stream-json input path a web turn takes, so a message sent during the turn reaches it.
  #[tokio::test]
  async fn thread_turn_takes_the_stream_json_input_path_like_a_web_turn() {
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
    charliebot_registry::thread_origin::register_origin_field("discord", "discord_origin");
    charliebot_registry::thread_origin::register_origin_field("feishu", "feishu_origin");
    for (field, origin) in [
      ("slack_origin", json!({"team_id": "T", "channel_id": "C", "thread_ts": "1.0"})),
      ("discord_origin", json!({"guild_id": "G", "parent_channel_id": "C", "thread_id": "1"})),
      ("feishu_origin", json!({"tenant_key": "T", "chat_id": "oc_1", "root_id": "om_1"})),
    ] {
      let root = tempfile::tempdir().unwrap();
      let cfg = unpooled_config(&root.path().join("home"));
      let ctx = context(cfg.clone(), LaunchKind::Turn, None, None, Some(Arc::new(Vec::new)));
      let mut meta = charliebot_store::metadata::SessionMetadata::new("s1", "thread", TaskProfile::Manager);
      meta.extra.insert(field.to_string(), origin);
      *ctx.session_meta.lock().expect("launch metadata mutex is not poisoned") = meta.clone();
      let lifecycle = ClaudeCodeLifecycle::new();

      let launch = lifecycle.place(&ctx).await.unwrap();
      assert_eq!(launch.backend_kwargs["midturn_delivery"], json!(true), "{field}: the thread turn is gated");
      assert_eq!(launch.backend_kwargs["midturn_run_id"], json!(ctx.run_id.clone().unwrap()), "{field}");
      let backend = ClaudeCodeBackend::from_launch(
        None,
        None,
        false,
        false,
        None,
        LaunchArgs {
          extra_flags: Vec::new(),
          buffer_limit: None,
          instructions_content: None,
          instructions_file: None,
          resume_session_id: None,
          log_dir: None,
          cgroup_session_id: Some(meta.id.clone()),
          remote_launch: None,
          backend_kwargs: launch.backend_kwargs.clone(),
          manager_turn: false,
        },
        disabled_cgroup(),
      )
      .unwrap();
      let command = backend.build_command("prompt").unwrap();
      assert!(command.iter().any(|value| value == "--input-format"), "{field}: {command:?}");
      assert!(command.iter().any(|value| value == "--replay-user-messages"), "{field}: {command:?}");
    }
  }

  /// A worker whose pool has no usable login refuses the launch as an access refusal, and names the earliest
  /// authentication-failure disable end rather than a quota reset.
  #[tokio::test]
  async fn a_task_with_no_usable_login_carries_the_refusal_and_the_earliest_recovery() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let a = root.path().join("a");
    let b = root.path().join("b");
    account_credentials(&a);
    account_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let failed = UtcDateTime::now();
    claude_accounts::record_auth_failure(&state, "a", Some(failed));
    claude_accounts::record_auth_failure(&state, "b", Some(failed));
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));
    let ctx = context(cfg, LaunchKind::Task, None, None, None);

    let refused = match lifecycle.place(&ctx).await {
      Err(refused) => refused,
      Ok(_) => panic!("no login can take the run"),
    };

    assert_eq!(refused.failure, MemberFailure::AccessRefused);
    assert_eq!(
      refused.resets_at,
      Some(UtcDateTime::from(failed.into_inner() + claude_accounts::AUTH_FAILURE_COOLDOWN))
    );
  }

  #[test]
  fn resolve_account_dir_picks_a_pooled_login_and_refuses_when_none_is_usable() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let a = root.path().join("a");
    let b = root.path().join("b");
    account_credentials(&a);
    account_credentials(&b);
    let cfg = config(&home, &[("a", &a), ("b", &b)]);
    let option = cfg.get_backend_option("fable").unwrap();
    let state = Arc::new(claude_accounts::ClaudeAccountState::new());
    let lifecycle = ClaudeCodeLifecycle::with_state(Arc::clone(&state));

    let dir = lifecycle.resolve_account_dir(option, &cfg).unwrap().unwrap();
    assert!(
      dir.as_deref() == Some(a.to_string_lossy().as_ref()) || dir.as_deref() == Some(b.to_string_lossy().as_ref()),
      "{dir:?}"
    );

    let failed = UtcDateTime::now();
    claude_accounts::record_auth_failure(&state, "a", Some(failed));
    claude_accounts::record_auth_failure(&state, "b", Some(failed));
    let refusal = lifecycle.resolve_account_dir(option, &cfg).unwrap().unwrap_err();
    assert!(!refusal.is_empty());

    let unpooled = unpooled_config(&root.path().join("unpooled"));
    assert_eq!(
      lifecycle.resolve_account_dir(unpooled.get_backend_option("fable").unwrap(), &unpooled).unwrap(),
      Ok(None)
    );
  }
}
