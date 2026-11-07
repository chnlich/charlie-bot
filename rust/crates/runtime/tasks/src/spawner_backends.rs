//! Backend-resolution refusals used by the manager spawner.

use std::collections::HashSet;
use std::fmt::{Display, Formatter};

use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{AUTO_BACKEND_ID, BackendOption, CharlieBotConfig};
use charliebot_store::Store;
use charliebot_store::StoreError;
use charliebot_store::metadata::{SessionMetadata, TaskType};

/// Backend-resolution input errors that the improve route answers as bad requests.
#[derive(Debug)]
pub struct BackendResolutionValueError(String);

impl Display for BackendResolutionValueError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for BackendResolutionValueError {}

fn value_error(message: impl Into<String>) -> anyhow::Error {
  anyhow::Error::new(BackendResolutionValueError(message.into()))
}

/// A resolver role is a persisted task type or the improve loop's literal role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentRole {
  Task(TaskType),
  Improve,
}

impl From<TaskType> for SubagentRole {
  fn from(task_type: TaskType) -> Self {
    Self::Task(task_type)
  }
}

impl SubagentRole {
  fn pool_caller(self) -> Option<&'static str> {
    match self {
      Self::Task(TaskType::Implement) => Some("implement"),
      Self::Task(TaskType::QuickEdit) => Some("quick-edit"),
      Self::Task(TaskType::ScriptRun) => Some("script-run"),
      Self::Task(TaskType::Verify | TaskType::ColdRead) => None,
      Self::Improve => Some("improve"),
    }
  }
}

/// Return the refusal core when a session pins a backend id absent from `config.yaml`.
pub fn unknown_backend_pin_refusal(backend_id: &str, fallback_id: &str) -> String {
  format!("'{backend_id}' is not in config.yaml backends.options — refusing to substitute '{fallback_id}'")
}

/// Resolve the exact backend and recorded model on one run.
pub fn resolve_backend_option(
  cfg: &CharlieBotConfig,
  backend_id: &str,
  model: Option<&str>,
) -> anyhow::Result<BackendOption> {
  if backend_id.is_empty() {
    return Err(value_error("resolved backend is required"));
  }
  let mut option = cfg
    .get_backend_option(backend_id)
    .cloned()
    .ok_or_else(|| value_error(format!("resolved backend '{backend_id}' is not in backends.options")))?;
  let model = model.filter(|model| !model.is_empty()).ok_or_else(|| value_error("resolved model is required"))?;
  option.model = model.to_string();
  Ok(option)
}

/// Resolve a session's pinned backend or its configured first backend. A session on the `auto` backend resolves to the
/// member turn start recorded in `auto_member`; a missing `backends.auto` or member is a refusal that names it.
pub fn resolve_session_default_backend_model(
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
) -> anyhow::Result<(String, Option<String>)> {
  let option = if session_meta.backend == AUTO_BACKEND_ID {
    let member = charliebot_agents::auto_backend::current_member_id(cfg, session_meta)
      .map_err(|error| value_error(error.to_string()))?;
    cfg.get_backend_option(&member).ok_or_else(|| {
      value_error(format!("'{member}' is not in config.yaml backends.options — the auto member cannot run"))
    })?
  } else if !session_meta.backend.is_empty() {
    let backend_id = session_meta.backend.as_str();
    match cfg.get_backend_option(backend_id) {
      Some(option) => option,
      None if cfg.backends.options.is_empty() => {
        return Err(value_error("session backend resolution requires a configured backends.options entry"));
      }
      None => {
        let fallback = &cfg.backends.options[0].id;
        tracing::error!(
          target: "charliebot_tasks",
          session_id = %session_meta.id,
          stored = backend_id,
          refused_substitute = fallback,
          "session_backend_unresolved"
        );
        return Err(value_error(format!("session backend {}", unknown_backend_pin_refusal(backend_id, fallback))));
      }
    }
  } else {
    cfg
      .backends
      .options
      .first()
      .ok_or_else(|| value_error("session backend resolution requires a configured backends.options entry"))?
  };
  let model = option.required_model("session backend").map_err(value_error)?;
  Ok((option.id.clone(), Some(model)))
}

/// Resolve the backend id the session's own defaults use for verify's excluded candidate.
pub fn session_backend_id(cfg: &CharlieBotConfig, session_meta: &SessionMetadata) -> anyhow::Result<String> {
  resolve_session_default_backend_model(cfg, session_meta).map(|(backend_id, _)| backend_id)
}

/// Which rule chose a subagent's backend. A pool pick and an Auto member pick fail over through different owners, so
/// the caller records which one ran the child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendSource {
  /// The request named the backend, or the session's own backend runs it.
  Direct,
  /// `backends.pool` chose the backend.
  Pool,
  /// The session is on `auto` and its current member runs it.
  AutoMember,
}

/// Resolve a delegation's backend and the rule that chose it (`spawner_backends.resolve_requested_subagent_backend_model`).
pub async fn resolve_requested_subagent_backend_model(
  cfg: &CharlieBotConfig,
  store: &Store,
  session_id: &str,
  requested_backend: Option<&str>,
  role: SubagentRole,
  pool: &BackendPool,
) -> anyhow::Result<(String, Option<String>, BackendSource)> {
  let meta = store.require_session(session_id).await.map_err(|error| match error {
    StoreError::NotFound(_) => value_error(format!("session '{session_id}' not found")),
    error => anyhow::Error::new(error),
  })?;
  if let Some(backend_id) = requested_backend {
    if backend_id.is_empty() {
      return Err(value_error("requested backend is required"));
    }
    let option = cfg
      .get_backend_option(backend_id)
      .ok_or_else(|| value_error(format!("requested backend '{backend_id}' is not in backends.options")))?;
    let model = option.required_model("requested backend").map_err(value_error)?;
    return Ok((option.id.clone(), Some(model), BackendSource::Direct));
  }
  if !cfg.backends.pool.is_empty()
    && let Some(caller) = role.pool_caller()
  {
    let option = pool.select(cfg, caller, &[])?.ok_or_else(|| value_error(pool.no_member_message(cfg)))?;
    let model = option.required_model("pool member").map_err(value_error)?;
    return Ok((option.id.clone(), Some(model), BackendSource::Pool));
  }
  let (backend_id, model) = resolve_session_default_backend_model(cfg, &meta)?;
  let source = if meta.backend == AUTO_BACKEND_ID { BackendSource::AutoMember } else { BackendSource::Direct };
  Ok((backend_id, model, source))
}

/// Resolve a verify or cold-read checker, excluding the requesting task's backend (`spawner_backends.select_verify_backend`).
pub async fn select_verify_backend(
  cfg: &CharlieBotConfig,
  store: &Store,
  session_id: &str,
  caller: &str,
  tried_backends: &[String],
  pool: &BackendPool,
) -> anyhow::Result<Option<(String, Option<String>, Vec<String>)>> {
  let meta = store.require_session(session_id).await?;
  let (session_backend, session_model) = resolve_session_default_backend_model(cfg, &meta)?;
  crate::review::select_reviewer_backend(
    cfg,
    pool,
    caller,
    &session_backend,
    session_model.as_deref(),
    tried_backends,
    &HashSet::new(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn refusal_names_the_missing_pin_and_rejected_substitute() {
    assert_eq!(
      unknown_backend_pin_refusal("synthetic-missing", "synthetic-default"),
      "'synthetic-missing' is not in config.yaml backends.options — refusing to substitute 'synthetic-default'"
    );
  }

  #[test]
  fn resolved_backend_requires_an_explicit_backend_and_model() {
    let cfg = test_config();
    assert_eq!(resolve_backend_option(&cfg, "synthetic", Some("model-2")).unwrap().model, "model-2");
    assert_eq!(
      resolve_backend_option(&cfg, "", Some("model-2")).unwrap_err().to_string(),
      "resolved backend is required"
    );
    assert_eq!(resolve_backend_option(&cfg, "synthetic", None).unwrap_err().to_string(), "resolved model is required");
  }

  #[test]
  fn an_auto_session_resolves_to_the_recorded_member() {
    let cfg = auto_config();
    let meta = auto_session();

    assert_eq!(
      resolve_session_default_backend_model(&cfg, &meta).unwrap(),
      ("synthetic".to_string(), Some("model-1".to_string()))
    );
    assert_eq!(session_backend_id(&cfg, &meta).unwrap(), "synthetic", "verify excludes the member the turn runs on");
  }

  #[test]
  fn a_turn_without_backends_auto_fails_naming_it() {
    let error = resolve_session_default_backend_model(&test_config(), &auto_session()).unwrap_err().to_string();
    assert!(error.contains("backends.auto"), "{error}");
  }

  #[test]
  fn a_recorded_member_the_config_drops_fails_naming_backends_auto() {
    let mut cfg = auto_config();
    cfg.backends.auto.as_mut().expect("the auto set").members[0].backend = "retired".to_string();

    let error = resolve_session_default_backend_model(&cfg, &auto_session()).unwrap_err().to_string();
    assert!(error.contains("backends.auto") && error.contains("no longer lists"), "{error}");
  }

  #[tokio::test]
  async fn a_delegation_request_naming_auto_is_refused() {
    use std::sync::Arc;

    use charliebot_agents::backend_availability::BackendAvailability;
    use charliebot_registry::Registry;
    use charliebot_registry::store_hooks::store_hooks;
    use charliebot_store::metadata::TaskProfile;
    use charliebot_store::{CreateTask, Store, TaskCaller};
    use serde_json::Map;

    let home = tempfile::tempdir().unwrap();
    let mut cfg = auto_config();
    cfg.charliebot_home = home.path().to_path_buf();
    let registry = Arc::new(Registry::new());
    let store = Store::new(cfg.clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
    let pool = BackendPool::new(Arc::new(BackendAvailability::load(home.path()).unwrap()));
    let session = store
      .create_task(
        CreateTask {
          request_id: "auto-delegation".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Auto delegation".to_string()),
          backend: Some(AUTO_BACKEND_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();

    let error = resolve_requested_subagent_backend_model(
      &cfg,
      &store,
      &session.id,
      Some(AUTO_BACKEND_ID),
      SubagentRole::Improve,
      &pool,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("requested backend 'auto' is not in backends.options"), "{error}");
  }

  fn auto_config() -> CharlieBotConfig {
    let mut cfg = test_config();
    cfg.backends.auto = Some(charliebot_core::config::AutoConfig {
      label: "Auto".to_string(),
      members: vec![charliebot_core::config::AutoMember {
        backend: "synthetic".to_string(),
        billing: charliebot_core::config::AutoBilling::Subscription,
      }],
    });
    cfg
  }

  fn auto_session() -> SessionMetadata {
    let mut meta =
      SessionMetadata::new("auto-session", "Auto session", charliebot_store::metadata::TaskProfile::Manager);
    meta.backend = AUTO_BACKEND_ID.to_string();
    meta.set_extra(charliebot_agents::auto_backend::AUTO_MEMBER_FIELD, serde_json::json!("synthetic"));
    meta
  }

  fn test_config() -> CharlieBotConfig {
    use charliebot_core::config::{BackendsConfig, PathsConfig, ServerConfig, TelegramConfig, UiConfig};
    use std::path::PathBuf;

    CharlieBotConfig {
      charliebot_home: PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "chromium".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18600,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 1,
        session_swap_max_mb: 0,
        public_url: None,
        session_tasks_max: 1,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig {
        pool: Vec::new(),
        routing: None,
        options: vec![BackendOption {
          id: "synthetic".to_string(),
          label: "synthetic".to_string(),
          model: "model-1".to_string(),
          prompt_overlay: Some("none".to_string()),
          backend_type: "synthetic".to_string(),
          type_fields: Default::default(),
        }],
        auto: None,
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Default::default(),
    }
  }
}
