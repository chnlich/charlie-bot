//! The no-pool lifecycle of the Claude CLI backend.

use std::fs;
use std::path::Path;

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{BackendLifecycle, Launch, LaunchContext, LaunchKind, LaunchRefused};
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use super::login_dirs;
use super::out_of_family_served_models;

pub const EXCLUDE_DYNAMIC_SECTIONS_FLAG: &str = "--exclude-dynamic-system-prompt-sections";
const POOL_REFUSAL: &str = "claude account pool is not ported yet";

/// The single-process lifecycle for Claude CLI backends without account-pool routing.
pub struct ClaudeCliLifecycle;

impl ClaudeCliLifecycle {
  fn checked_resume_id(&self, ctx: &LaunchContext) -> anyhow::Result<Option<String>> {
    let Some(session_id) = ctx.held_native_id.as_deref().filter(|value| !value.is_empty()) else {
      return Ok(None);
    };
    let config_dir = login_dirs::claude_config_dir()?;
    if transcript_exists(&config_dir, session_id)? {
      return Ok(Some(session_id.to_string()));
    }
    tracing::warn!(
      target: "charliebot_agents",
      session = ctx.session_meta.id,
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
      let resume_id = self.checked_resume_id(ctx).map_err(|error| LaunchRefused {
        message: error.to_string(),
        quota_exhausted: false,
        resets_at: None,
      })?;
      Ok(self.launch(ctx, resume_id))
    })
  }

  fn round_notices(&self, option: &BackendOption, events: &[Value]) -> Vec<Value> {
    let served_models = out_of_family_served_models(events, &option.model);
    if served_models.is_empty() {
      return Vec::new();
    }
    vec![serde_json::json!({
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

/// The cc-claude lifecycle with account-pool routing intentionally withheld from this port.
pub struct ClaudeCodeLifecycle {
  no_pool: ClaudeCliLifecycle,
}

impl ClaudeCodeLifecycle {
  pub fn new() -> Self {
    Self { no_pool: ClaudeCliLifecycle }
  }
}

impl Default for ClaudeCodeLifecycle {
  fn default() -> Self {
    Self::new()
  }
}

impl BackendLifecycle for ClaudeCodeLifecycle {
  fn continuation_domain(&self, option: &BackendOption, cfg: &CharlieBotConfig) -> String {
    if option.backend_type != "cc-claude" {
      return option.id.clone();
    }
    if has_claude_accounts(cfg) {
      return "pool".to_string();
    }
    login_dirs::claude_config_dir()
      .expect("HOME or CLAUDE_CONFIG_DIR is required for Claude continuation")
      .display()
      .to_string()
  }

  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      if ctx.option.backend_type == "cc-claude" && has_claude_accounts(&ctx.cfg) {
        return Err(LaunchRefused { message: POOL_REFUSAL.to_string(), quota_exhausted: false, resets_at: None });
      }
      self.no_pool.place(ctx).await
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
    meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
  }

  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> bool {
    if self.account_label(meta).as_deref() == label {
      return false;
    }
    let value = label.map_or(Value::Null, |value| Value::String(value.to_string()));
    meta.set_extra("claude_account", value);
    true
  }

  fn thread_native_id(&self, thread: &ThreadMetadata) -> Option<String> {
    thread.extra.get("claude_session_id").and_then(Value::as_str).map(str::to_string)
  }

  fn assign_thread_native_id(&self, thread: &mut ThreadMetadata, native_id: Option<&str>) {
    thread
      .extra
      .insert("claude_session_id".to_string(), native_id.map_or(Value::Null, |value| Value::String(value.to_string())));
  }
}

fn has_claude_accounts(cfg: &CharlieBotConfig) -> bool {
  cfg
    .section::<serde_yaml::Value>("accounts")
    .get("claude")
    .and_then(serde_yaml::Value::as_sequence)
    .is_some_and(|accounts| !accounts.is_empty())
}

fn transcript_exists(config_dir: &Path, session_id: &str) -> anyhow::Result<bool> {
  let projects = config_dir.join("projects");
  if !projects.exists() {
    return Ok(false);
  }
  for project in fs::read_dir(projects)? {
    let path = project?.path().join(format!("{session_id}.jsonl"));
    if path.is_file() {
      return Ok(true);
    }
  }
  Ok(false)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;

  use charliebot_core::config::CharlieBotConfig;
  use charliebot_registry::Registry;
  use charliebot_registry::backend::{ContextStateFn, EmitFn, LaunchKind, RecordAccountFn};
  use charliebot_store::metadata::TaskProfile;

  use super::*;

  fn config(accounts: &str) -> (tempfile::TempDir, CharlieBotConfig) {
    let home = tempfile::tempdir().unwrap();
    fs::write(
      home.path().join("config.yaml"),
      format!(
        "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\naccounts:\n{accounts}"
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
    let config = registry.load_config(home.path()).unwrap();
    (home, config)
  }

  fn context(cfg: CharlieBotConfig, kind: LaunchKind, preassigned_native_id: Option<&str>) -> LaunchContext {
    let option = cfg.backends.options[0].clone();
    let session_meta = SessionMetadata::new("session", "Session", TaskProfile::Manager);
    let emit: EmitFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let record_account: RecordAccountFn = Arc::new(|_| Box::pin(async { Ok(()) }));
    let context_state: ContextStateFn = Arc::new(|| Box::pin(async { Ok((None, None)) }));
    LaunchContext {
      cfg: Arc::new(cfg),
      option,
      session_meta,
      kind,
      cwd: PathBuf::from("/synthetic/cwd"),
      held_native_id: None,
      preassigned_native_id: preassigned_native_id.map(str::to_string),
      emit,
      record_account,
      context_state,
    }
  }

  #[tokio::test]
  async fn refuses_a_nonempty_claude_account_pool_with_the_port_boundary_message() {
    let (_home, cfg) = config("  claude:\n    - label: synthetic\n      config_dir: /synthetic/account\n");
    let ctx = context(cfg, LaunchKind::Turn, None);
    let error = ClaudeCodeLifecycle::new().place(&ctx).await.err().unwrap();
    assert_eq!(error.message, POOL_REFUSAL);
    assert!(!error.quota_exhausted);
  }

  #[tokio::test]
  async fn no_pool_launch_carries_turn_flags_and_a_preassigned_task_id() {
    let (_home, cfg) = config("  claude: []\n");
    let lifecycle = ClaudeCodeLifecycle::new();
    let turn = context(cfg.clone(), LaunchKind::Turn, None);
    let turn_launch = lifecycle.place(&turn).await.unwrap();
    assert_eq!(turn_launch.backend_kwargs["extra_flags"], serde_json::json!([EXCLUDE_DYNAMIC_SECTIONS_FLAG]));

    let task = context(cfg, LaunchKind::Task, Some("native-task-id"));
    let task_launch = lifecycle.place(&task).await.unwrap();
    assert_eq!(task_launch.backend_kwargs["claude_session_id"], "native-task-id");
    assert!(task_launch.resume_id.is_none());
  }

  #[test]
  fn account_label_and_thread_session_id_follow_their_metadata_slots() {
    let lifecycle = ClaudeCodeLifecycle::new();
    let mut session = SessionMetadata::new("s", "S", TaskProfile::Manager);
    assert!(!lifecycle.record_account_label(&mut session, None));
    assert!(lifecycle.record_account_label(&mut session, Some("synthetic")));
    assert_eq!(lifecycle.account_label(&session).as_deref(), Some("synthetic"));
    assert!(!lifecycle.record_account_label(&mut session, Some("synthetic")));
    assert!(lifecycle.record_account_label(&mut session, None));
    assert!(lifecycle.account_label(&session).is_none());
    let mut thread: ThreadMetadata = serde_json::from_value(serde_json::json!({
      "id":"thread", "session_id":"s", "description":"synthetic"
    }))
    .unwrap();
    lifecycle.assign_thread_native_id(&mut thread, Some("native-id"));
    assert_eq!(lifecycle.thread_native_id(&thread).as_deref(), Some("native-id"));
  }

  #[test]
  fn lifecycle_produces_notice_for_an_out_of_family_visible_reply() {
    let lifecycle = ClaudeCliLifecycle;
    let option = BackendOption {
      id: "fable".to_string(),
      label: "Fable".to_string(),
      model: "claude-fable-5-1".to_string(),
      prompt_overlay: None,
      backend_type: "cc-claude".to_string(),
      type_fields: serde_yaml::Mapping::new(),
    };
    let events = [serde_json::json!({
      "type":"assistant", "message":{"model":"claude-opus-4-8", "content":[{"type":"text"}]}
    })];
    let notices = lifecycle.round_notices(&option, &events);
    assert!(lifecycle.has_round_notices());
    assert_eq!(notices[0]["type"], "model_fallback_notice");
    assert_eq!(notices[0]["served_models"], serde_json::json!(["claude-opus-4-8"]));
  }
}
