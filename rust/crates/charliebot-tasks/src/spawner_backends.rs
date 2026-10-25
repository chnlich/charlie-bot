//! Backend-resolution refusals used by the manager spawner.

use std::collections::HashSet;
use std::fmt::{Display, Formatter};

use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_store::Store;
use charliebot_store::StoreError;
use charliebot_store::metadata::{SessionMetadata, TaskType};

/// Backend-resolution input errors that the improve route exposes as Python ValueError.
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

/// Resolve a session's pinned backend or its configured first backend.
pub fn resolve_session_default_backend_model(
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
) -> anyhow::Result<(String, Option<String>)> {
  let option = if !session_meta.backend.is_empty() {
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
  let model = if option.model.is_empty() {
    return Err(value_error(format!("session backend '{}' has no default model", option.id)));
  } else {
    option.model.clone()
  };
  Ok((option.id.clone(), Some(model)))
}

/// Resolve the backend id the session's own defaults use for verify's excluded candidate.
pub fn session_backend_id(cfg: &CharlieBotConfig, session_meta: &SessionMetadata) -> anyhow::Result<String> {
  resolve_session_default_backend_model(cfg, session_meta).map(|(backend_id, _)| backend_id)
}

/// Resolve a delegation's backend and whether `backends.pool` selected it (`spawner_backends.resolve_requested_subagent_backend_model`).
pub async fn resolve_requested_subagent_backend_model(
  cfg: &CharlieBotConfig,
  store: &Store,
  session_id: &str,
  requested_backend: Option<&str>,
  role: SubagentRole,
  pool: &BackendPool,
) -> anyhow::Result<(String, Option<String>, bool)> {
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
    return Ok((option.id.clone(), Some(required_model(&option.model, "requested backend ", &option.id)?), false));
  }
  if !cfg.backends.pool.is_empty()
    && let Some(caller) = role.pool_caller()
  {
    let option = pool.select(cfg, caller, &[])?.ok_or_else(|| value_error(pool.no_member_message(cfg)))?;
    return Ok((option.id.clone(), Some(required_model(&option.model, "pool member ", &option.id)?), true));
  }
  let (backend_id, model) = resolve_session_default_backend_model(cfg, &meta)?;
  Ok((backend_id, model, false))
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

fn required_model(model: &str, subject: &str, backend_id: &str) -> anyhow::Result<String> {
  if model.is_empty() {
    return Err(value_error(format!("{subject}'{backend_id}' has no default model")));
  }
  Ok(model.to_string())
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
        session_tasks_max: 1,
        min_free_disk_gib: 0,
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
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Default::default(),
    }
  }
}
