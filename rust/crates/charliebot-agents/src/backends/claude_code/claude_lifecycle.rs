//! Process placement and account relay lifecycle for Claude CLI backends.

use std::any::Any;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{BackendLifecycle, Launch, LaunchContext, LaunchKind, LaunchRefused, LaunchWatch};
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::{
  claude_accounts, claude_config, claude_metadata, claude_relay, login_dirs, master_cc_relay,
  out_of_family_served_models,
};

pub const EXCLUDE_DYNAMIC_SECTIONS_FLAG: &str = "--exclude-dynamic-system-prompt-sections";

pub struct ClaudeCliLifecycle;

impl ClaudeCliLifecycle {
  fn checked_resume_id(&self, ctx: &LaunchContext) -> anyhow::Result<Option<String>> {
    let session_id = ctx.held_native_id.as_deref().filter(|value| !value.is_empty());
    let Some(session_id) = session_id else {
      return Ok(None);
    };
    let config_dir = login_dirs::claude_config_dir()?;
    if !claude_accounts::transcript_matches(&config_dir, session_id)?.is_empty() {
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

  fn launch(&self, ctx: &LaunchContext, resume_id: Option<String>) -> Launch {
    let mut backend_kwargs = Map::new();
    if ctx.kind == LaunchKind::Turn {
      backend_kwargs.insert(
        "extra_flags".to_string(),
        Value::Array(vec![Value::String(EXCLUDE_DYNAMIC_SECTIONS_FLAG.to_string())]),
      );
    }
    if ctx.kind == LaunchKind::Task
      && ctx.held_native_id.is_none()
      && let Some(native_id) = ctx.preassigned_native_id.as_deref()
    {
      backend_kwargs.insert("claude_session_id".to_string(), Value::String(native_id.to_string()));
    }
    Launch { backend_kwargs, resume_id, prompt: None, account_label: None, private: None }
  }
}

impl BackendLifecycle for ClaudeCliLifecycle {
  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      let resume_id = self.checked_resume_id(ctx).map_err(|error| refused(error, false, None))?;
      Ok(self.launch(ctx, resume_id))
    })
  }

  fn round_notices(&self, option: &BackendOption, events: &[Value]) -> Vec<Value> {
    let served_models = out_of_family_served_models(events, &option.model);
    if served_models.is_empty() {
      return Vec::new();
    }
    vec![json!({
      "type": "model_fallback_notice",
      "backend": option.id,
      "configured_model": option.model,
      "served_models": served_models,
    })]
  }

  fn has_round_notices(&self) -> bool {
    true
  }
}

struct PooledLaunch {
  account: claude_config::ClaudeAccount,
  relays: usize,
  decision: Arc<Mutex<Option<String>>>,
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
    let mut kwargs = Map::new();
    if ctx.kind == LaunchKind::Turn {
      kwargs.insert(
        "extra_flags".to_string(),
        Value::Array(vec![Value::String(EXCLUDE_DYNAMIC_SECTIONS_FLAG.to_string())]),
      );
    } else if ctx.kind == LaunchKind::Task {
      if first_process
        && ctx.held_native_id.is_none()
        && let Some(native_id) = ctx.preassigned_native_id.as_deref()
      {
        kwargs.insert("claude_session_id".to_string(), Value::String(native_id.to_string()));
      }
    } else {
      anyhow::bail!("unknown launch kind {:?}", ctx.kind);
    }
    Ok(kwargs)
  }

  fn pooled_launch(
    &self,
    ctx: &LaunchContext,
    account: claude_config::ClaudeAccount,
    resume_id: Option<String>,
    prompt: Option<String>,
    relays: usize,
  ) -> anyhow::Result<Launch> {
    let mut backend_kwargs = Self::launch_args(ctx, relays == 0)?;
    backend_kwargs
      .insert("claude_account".to_string(), json!({"label": account.label, "config_dir": account.config_dir}));
    let decision = Arc::new(Mutex::new(None));
    let private = PooledLaunch { account: account.clone(), relays, decision };
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
      .map_err(|error| refused(error, false, None))?;
    if let Some(account) = current.as_ref()
      && !claude_accounts::transcript_matches(Path::new(&account.config_dir), session_id)
        .map_err(|error| refused(error, false, None))?
        .is_empty()
    {
      return Ok(Some(session_id.to_string()));
    }
    let found =
      claude_accounts::find_transcript_account(&ctx.cfg, session_id).map_err(|error| refused(error, false, None))?;
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
        .map_err(|error| refused(error, false, None))?
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
    if let Some(session_id) = ctx.held_native_id.as_deref().filter(|value| !value.is_empty()) {
      let holder =
        claude_accounts::find_transcript_account(&ctx.cfg, session_id).map_err(|error| refused(error, false, None))?;
      let Some(holder) = holder else {
        let labels = claude_accounts::pool(&ctx.cfg)
          .map_err(|error| refused(error, false, None))?
          .into_iter()
          .map(|account| account.label)
          .collect::<Vec<_>>();
        return Err(LaunchRefused {
          message: format!(
            "the held conversation {session_id} has no transcript on any pool login ({})",
            labels.join(", ")
          ),
          quota_exhausted: false,
          resets_at: None,
        });
      };
      return self
        .pooled_launch(ctx, holder, Some(session_id.to_string()), None, 0)
        .map_err(|error| refused(error, false, None));
    }
    let account = claude_accounts::select(
      &self.state,
      &ctx.cfg,
      Some(&ctx.option.model),
      &HashSet::new(),
      None,
      None,
      account_pool.as_deref(),
      0.0,
    )
    .map_err(|error| refused(error, false, None))?;
    let Some(account) = account else {
      let message = claude_relay::pool_exhausted_message(&self.state, &ctx.cfg, None, account_pool.as_deref())
        .map_err(|error| refused(error, false, None))?;
      let resets_at = claude_accounts::earliest_reset(&self.state, &ctx.cfg, None, account_pool.as_deref())
        .map_err(|error| refused(error, false, None))?;
      return Err(LaunchRefused { message, quota_exhausted: true, resets_at });
    };
    self.pooled_launch(ctx, account, None, None, 0).map_err(|error| refused(error, false, None))
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
      if !claude_accounts::is_pooled(&ctx.option, &ctx.cfg) {
        return self.no_pool.place(ctx).await;
      }
      match ctx.kind {
        LaunchKind::Turn => {
          let resume_id = self.pooled_resume_id(ctx).await?;
          let (context_tokens, last_request_at) =
            (ctx.context_state)().await.map_err(|error| refused(error, false, None))?;
          let account =
            master_cc_relay::place_turn(&self.state, ctx, resume_id.as_deref(), context_tokens, last_request_at, None)
              .await?;
          let resume_id = if ctx.held_native_id.is_some() { self.pooled_resume_id(ctx).await? } else { resume_id };
          self.pooled_launch(ctx, account, resume_id, None, 0).map_err(|error| refused(error, false, None))
        }
        LaunchKind::Task => self.place_task(ctx).await,
      }
    })
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
        quota_exhausted: false,
        resets_at: None,
      })?;
      let decision = pooled.decision.lock().expect("Claude relay decision is not poisoned").clone();
      let decision = decision.ok_or_else(|| LaunchRefused {
        message: "Claude relay watch requested a move without a decision".to_string(),
        quota_exhausted: false,
        resets_at: None,
      })?;
      if decision == claude_relay::LOGIN_FAILED {
        master_cc_relay::report_login_failure(&self.state, ctx, &pooled.account)
          .await
          .map_err(|error| refused(error, false, None))?;
      }
      let account_pool = claude_accounts::option_pool(&ctx.option);
      if pooled.relays >= claude_relay::MAX_RELAYS_PER_TURN {
        let warning_has_cool_destination = if decision == claude_relay::RELAY_WARNING {
          claude_accounts::cool_destination(
            &self.state,
            &ctx.cfg,
            Some(&ctx.option.model),
            account_pool.as_deref(),
            None,
          )
          .map_err(|error| refused(error, false, None))?
          .is_some()
        } else {
          false
        };
        if !warning_has_cool_destination {
          let message = claude_relay::relay_limit_message(&self.state, &ctx.cfg, account_pool.as_deref())
            .map_err(|error| refused(error, false, None))?;
          return Err(LaunchRefused { message, quota_exhausted: false, resets_at: None });
        }
      }
      let relays = pooled.relays + 1;
      let min_headroom =
        if decision == claude_relay::RELAY_WARNING { 1.0 - claude_accounts::WARNING_UTILIZATION } else { 0.0 };
      let account = master_cc_relay::prepare_relay(
        &self.state,
        ctx,
        &pooled.account,
        native_id,
        Some(&decision),
        relays,
        min_headroom,
      )
      .await?;
      if ctx.kind == LaunchKind::Turn {
        (ctx.record_account)(account.label.clone()).await.map_err(|error| refused(error, false, None))?;
      }
      self
        .pooled_launch(
          ctx,
          account,
          native_id.map(str::to_string),
          Some(claude_relay::CONTINUATION_PROMPT.to_string()),
          relays,
        )
        .map_err(|error| refused(error, false, None))
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

  fn account_subject(&self) -> &'static str {
    "Claude account"
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

fn refused(
  error: impl std::fmt::Display,
  quota_exhausted: bool,
  resets_at: Option<charliebot_store::metadata::UtcDateTime>,
) -> LaunchRefused {
  LaunchRefused { message: error.to_string(), quota_exhausted, resets_at }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{ContextStateFn, EmitFn, RecordAccountFn, RunningTurnsFn};
  use charliebot_store::metadata::TaskProfile;
  use serde_json::json;

  use super::*;
  use crate::backends::claude_code::{event_types, turn_contribution};

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
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
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
      cwd: PathBuf::from("/synthetic/cwd"),
      held_native_id: held_native_id.map(str::to_string),
      preassigned_native_id: Some("task-session".to_string()),
      emit,
      record_account,
      context_state,
      running_turns,
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
}
