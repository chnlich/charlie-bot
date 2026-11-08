//! Backend-resolution refusals used by the manager spawner.

use std::collections::HashSet;
use std::fmt::{Display, Formatter};

use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{BackendOption, CallClass, CallTarget, CharlieBotConfig};
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
  /// The `backends.calls` class whose target picks this role's backend.
  fn call_class(self) -> CallClass {
    match self {
      Self::Task(TaskType::Implement | TaskType::QuickEdit | TaskType::ScriptRun) => CallClass::Worker,
      Self::Task(TaskType::Verify) => CallClass::Verify,
      Self::Task(TaskType::ColdRead) => CallClass::ColdRead,
      Self::Improve => CallClass::Improve,
    }
  }

  /// The caller name a log line gives this role's pool selection.
  fn caller(self) -> &'static str {
    match self {
      Self::Task(TaskType::Implement) => "implement",
      Self::Task(TaskType::QuickEdit) => "quick-edit",
      Self::Task(TaskType::ScriptRun) => "script-run",
      Self::Task(TaskType::Verify) => "verify",
      Self::Task(TaskType::ColdRead) => "cold_read",
      Self::Improve => "improve",
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

/// The `backends.pools` id that `backends.calls` names for `class`, or `None` when the key is unset, names a session or
/// a model.
pub fn class_pool_id(cfg: &CharlieBotConfig, class: CallClass) -> Option<&str> {
  match cfg.backends.calls.get(class) {
    Some(CallTarget::Backend(target)) if cfg.backends.pool(target).is_some() => Some(target.as_str()),
    _ => None,
  }
}

/// The pool a worker run's member comes from: the session's own backend when it is a pool id, else the worker class's
/// pool.
pub fn worker_pool_id<'a>(cfg: &'a CharlieBotConfig, session_meta: &SessionMetadata) -> Option<&'a str> {
  cfg
    .backends
    .pool(&session_meta.backend)
    .map(|pool| pool.id.as_str())
    .or_else(|| class_pool_id(cfg, CallClass::Worker))
}

/// Resolve a session's pinned backend or its configured first backend. A session whose backend is a pool id resolves to
/// the member turn start recorded in `pool_member`; a missing pool or member is a refusal that names it.
pub fn resolve_session_default_backend_model(
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
) -> anyhow::Result<(String, Option<String>)> {
  let option = if cfg.backends.pool(&session_meta.backend).is_some() {
    let member = charliebot_agents::model_pool::current_member_id(cfg, session_meta)
      .map_err(|error| value_error(error.to_string()))?;
    cfg.get_backend_option(&member).ok_or_else(|| {
      value_error(format!("'{member}' is not in config.yaml backends.options — the pool member cannot run"))
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
    let backend = cfg
      .backends
      .new_session_backend()
      .ok_or_else(|| value_error("session backend resolution requires a configured backends.options entry"))?;
    cfg.get_backend_option(&backend).ok_or_else(|| {
      value_error(format!("backends.calls.new_session names '{backend}', which is not a backends.options id"))
    })?
  };
  let model = option.required_model("session backend").map_err(value_error)?;
  Ok((option.id.clone(), Some(model)))
}

/// Resolve the backend id the session's own defaults use for verify's excluded candidate.
pub fn session_backend_id(cfg: &CharlieBotConfig, session_meta: &SessionMetadata) -> anyhow::Result<String> {
  resolve_session_default_backend_model(cfg, session_meta).map(|(backend_id, _)| backend_id)
}

/// Which rule chose a subagent's backend. A pool pick and an inherited pool member fail over through different owners,
/// so the caller records which one ran the child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendSource {
  /// The request named the backend, or the session's own backend runs it.
  Direct,
  /// The `backends.calls` target of the role named a pool, and the pool chose the backend.
  Pool,
  /// The session's backend is a pool and its current member runs the child.
  PoolMember,
}

/// The backends of one delegation: the child session's backend and the backend its work run launches on. A pool keeps
/// its id on the child session, and the work run names the member the pool selected, so the run never names a pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentBackend {
  /// The backend the child session names: the pool id for a pool-chosen run, else the run's backend.
  pub session_backend: String,
  /// The backend the work run launches on: an options id.
  pub run_backend: String,
  /// The model the work run records.
  pub model: Option<String>,
  /// The rule that chose the backend.
  pub source: BackendSource,
}

/// The backend of a run that the pool `pool_id` chose: the child keeps `pool_id`, and the run names `member`.
pub fn pool_chosen_backend(pool_id: &str, member: &BackendOption) -> anyhow::Result<SubagentBackend> {
  let model = member.required_model("pool member").map_err(value_error)?;
  Ok(SubagentBackend {
    session_backend: pool_id.to_string(),
    run_backend: member.id.clone(),
    model: Some(model),
    source: BackendSource::Pool,
  })
}

/// The backend of a verify or cold-read run that `backends.calls` selected: `backend` is the member the class's pool chose,
/// or the worker's own backend when no member was selectable. A chosen member keeps its pool's id on the child.
pub fn class_selected_backend(
  cfg: &CharlieBotConfig,
  class_pool: Option<&str>,
  backend: String,
  model: Option<String>,
) -> SubagentBackend {
  let pool_id = class_pool.filter(|pool_id| {
    cfg.backends.pool(pool_id).is_some_and(|pool| pool.members.iter().any(|member| member.backend == backend))
  });
  let source = if class_pool.is_some() { BackendSource::Pool } else { BackendSource::Direct };
  SubagentBackend {
    session_backend: pool_id.map_or_else(|| backend.clone(), str::to_string),
    run_backend: backend,
    model,
    source,
  }
}

/// Resolve a delegation's backend and the rule that chose it (`spawner_backends.resolve_requested_subagent_backend_model`).
pub async fn resolve_requested_subagent_backend_model(
  cfg: &CharlieBotConfig,
  store: &Store,
  session_id: &str,
  requested_backend: Option<&str>,
  role: SubagentRole,
  pool: &BackendPool,
) -> anyhow::Result<SubagentBackend> {
  let meta = store.require_session(session_id).await.map_err(|error| match error {
    StoreError::NotFound(_) => value_error(format!("session '{session_id}' not found")),
    error => anyhow::Error::new(error),
  })?;
  if let Some(backend_id) = requested_backend {
    if backend_id.is_empty() {
      return Err(value_error("requested backend is required"));
    }
    if cfg.backends.pool(backend_id).is_some() {
      let Some(member) = pool.select(cfg, role.caller(), backend_id, &[])? else {
        return Err(value_error(pool.exhausted_message(cfg, backend_id)?));
      };
      return pool_chosen_backend(backend_id, &member);
    }
    let option = cfg
      .get_backend_option(backend_id)
      .ok_or_else(|| value_error(format!("requested backend '{backend_id}' is not in backends.options")))?;
    let model = option.required_model("requested backend").map_err(value_error)?;
    return Ok(SubagentBackend {
      session_backend: option.id.clone(),
      run_backend: option.id.clone(),
      model: Some(model),
      source: BackendSource::Direct,
    });
  }
  match cfg.backends.calls.get(role.call_class()) {
    None | Some(CallTarget::Session) => {}
    Some(CallTarget::Backend(target)) if cfg.backends.pool(target).is_some() => {
      let Some(member) = pool.select(cfg, role.caller(), target, &[])? else {
        return Err(value_error(pool.exhausted_message(cfg, target)?));
      };
      return pool_chosen_backend(target, &member);
    }
    Some(CallTarget::Backend(target)) => {
      let option = cfg
        .get_backend_option(target)
        .ok_or_else(|| value_error(format!("calls target '{target}' is not a backends.options id")))?;
      let model = option.required_model("calls target").map_err(value_error)?;
      return Ok(SubagentBackend {
        session_backend: option.id.clone(),
        run_backend: option.id.clone(),
        model: Some(model),
        source: BackendSource::Direct,
      });
    }
  }
  let (backend_id, model) = resolve_session_default_backend_model(cfg, &meta)?;
  let source =
    if cfg.backends.pool(&meta.backend).is_some() { BackendSource::PoolMember } else { BackendSource::Direct };
  Ok(SubagentBackend { session_backend: backend_id.clone(), run_backend: backend_id, model, source })
}

/// Resolve a verify or cold-read checker for the call class `class`, excluding the session's current backend.
pub async fn select_verify_backend(
  cfg: &CharlieBotConfig,
  store: &Store,
  session_id: &str,
  class: CallClass,
  tried_backends: &[String],
  pool: &BackendPool,
) -> anyhow::Result<Option<(String, Option<String>, Vec<String>)>> {
  let meta = store.require_session(session_id).await?;
  let (session_backend, session_model) = resolve_session_default_backend_model(cfg, &meta)?;
  crate::review::select_reviewer_backend(
    cfg,
    pool,
    class,
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

  /// The id of the priority pool that the pool-session tests name.
  const POOL_ID: &str = "auto";

  #[test]
  fn a_pool_session_resolves_to_the_recorded_member() {
    let cfg = pool_config();
    let meta = pool_session();

    assert_eq!(
      resolve_session_default_backend_model(&cfg, &meta).unwrap(),
      ("synthetic".to_string(), Some("model-1".to_string()))
    );
    assert_eq!(session_backend_id(&cfg, &meta).unwrap(), "synthetic", "verify excludes the member the turn runs on");
  }

  #[test]
  fn a_pool_session_on_a_config_without_its_pool_fails_naming_the_backend() {
    let error = resolve_session_default_backend_model(&test_config(), &pool_session()).unwrap_err().to_string();
    assert!(error.contains("'auto'"), "{error}");
  }

  #[test]
  fn a_recorded_member_the_pool_drops_fails_naming_backends_pools() {
    let mut cfg = pool_config();
    cfg.backends.pools[0].members[0].backend = "retired".to_string();

    let error = resolve_session_default_backend_model(&cfg, &pool_session()).unwrap_err().to_string();
    assert!(error.contains("backends.pools") && error.contains("no longer lists"), "{error}");
  }

  #[tokio::test]
  async fn a_delegation_request_naming_a_pool_selects_a_member_and_keeps_the_pool_id() {
    use std::sync::Arc;

    use charliebot_agents::backend_availability::BackendAvailability;
    use charliebot_registry::Registry;
    use charliebot_registry::store_hooks::store_hooks;
    use charliebot_store::metadata::TaskProfile;
    use charliebot_store::{CreateTask, Store, TaskCaller};
    use serde_json::Map;

    let home = tempfile::tempdir().unwrap();
    let mut cfg = pool_config();
    cfg.charliebot_home = home.path().to_path_buf();
    let registry = Arc::new(Registry::new());
    let store = Store::new(cfg.clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
    let pool = BackendPool::new(
      Arc::new(BackendAvailability::load(home.path()).unwrap()),
      Arc::clone(&registry),
      Arc::new(charliebot_agents::model_pool::RotationPositions::default()),
    );
    let session = store
      .create_task(
        CreateTask {
          request_id: "pool-delegation".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Pool delegation".to_string()),
          backend: Some(POOL_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();

    let subagent =
      resolve_requested_subagent_backend_model(&cfg, &store, &session.id, Some(POOL_ID), SubagentRole::Improve, &pool)
        .await
        .unwrap();

    assert_eq!(subagent.session_backend, POOL_ID, "the child keeps the pool id");
    assert_eq!(subagent.run_backend, "synthetic", "the run names the member the pool selected");
    assert_eq!(subagent.model.as_deref(), Some("model-1"));
    assert_eq!(subagent.source, BackendSource::Pool);
  }

  fn pool_config() -> CharlieBotConfig {
    let mut cfg = test_config();
    cfg.backends.pools.push(charliebot_core::config::PoolConfig {
      id: POOL_ID.to_string(),
      label: "Auto".to_string(),
      rule: charliebot_core::config::PoolRule::Priority,
      members: vec![charliebot_core::config::PoolMember {
        backend: "synthetic".to_string(),
        billing: Some(charliebot_core::config::PoolBilling::Subscription),
      }],
    });
    cfg
  }

  fn pool_session() -> SessionMetadata {
    let mut meta =
      SessionMetadata::new("pool-session", "Pool session", charliebot_store::metadata::TaskProfile::Manager);
    meta.backend = POOL_ID.to_string();
    meta.set_extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD, serde_json::json!("synthetic"));
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
        pools: Vec::new(),
        calls: charliebot_core::config::CallsConfig::default(),
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
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Default::default(),
    }
  }
}
