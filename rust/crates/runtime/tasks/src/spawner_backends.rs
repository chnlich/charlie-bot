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

/// Resolve a session's current backend and model. A pool session resolves to its recorded member; a model session
/// resolves to its own id.
pub fn resolve_session_default_backend_model(
  cfg: &CharlieBotConfig,
  session_meta: &SessionMetadata,
) -> anyhow::Result<(String, Option<String>)> {
  let backend_id = charliebot_agents::model_pool::current_member_id(cfg, session_meta)
    .map_err(|error| value_error(error.to_string()))?;
  let option = cfg
    .get_backend_option(&backend_id)
    .ok_or_else(|| value_error(format!("session backend '{backend_id}' resolves to no backends.options entry")))?;
  let model = option.required_model("session backend").map_err(value_error)?;
  Ok((option.id.clone(), Some(model)))
}

/// The backend id a child inherits from its parent when no call target or explicit id applies.
fn inherited_backend_id(cfg: &CharlieBotConfig, session_meta: &SessionMetadata) -> anyhow::Result<String> {
  let backend_id = if session_meta.backend.is_empty() {
    cfg
      .backends
      .new_session_backend()
      .ok_or_else(|| value_error("session backend resolution requires a configured backends.options entry"))?
  } else {
    session_meta.backend.clone()
  };
  cfg.backends.check_backend_id(&backend_id).map_err(value_error)?;
  Ok(backend_id)
}

/// Resolve the backend id the session's own defaults use for verify's excluded candidate.
pub fn session_backend_id(cfg: &CharlieBotConfig, session_meta: &SessionMetadata) -> anyhow::Result<String> {
  resolve_session_default_backend_model(cfg, session_meta).map(|(backend_id, _)| backend_id)
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
  SubagentBackend {
    session_backend: pool_id.map_or_else(|| backend.clone(), str::to_string),
    run_backend: backend,
    model,
  }
}

/// The ids a verify or cold-read selection leaves out: the member the initiating session runs on now, which is its own
/// backend when that backend is a concrete model.
fn verify_exclusion(cfg: &CharlieBotConfig, meta: &SessionMetadata, role: SubagentRole) -> anyhow::Result<Vec<String>> {
  match role.call_class() {
    CallClass::Verify | CallClass::ColdRead => Ok(vec![session_current_member(cfg, meta)?]),
    _ => Ok(Vec::new()),
  }
}

/// The ids a verify or cold-read child leaves out at every member selection, its quota switches included: the member
/// the initiating session (the session its `task_parent_id` names) runs on now. Empty for every other task type.
pub async fn switch_exclusions(
  cfg: &CharlieBotConfig,
  store: &Store,
  meta: &SessionMetadata,
) -> anyhow::Result<Vec<String>> {
  let verify_or_cold_read = meta
    .task
    .as_ref()
    .and_then(|task| task.task_type)
    .is_some_and(|kind| matches!(kind, TaskType::Verify | TaskType::ColdRead));
  if !verify_or_cold_read {
    return Ok(Vec::new());
  }
  let Some(parent_id) = meta.task_parent_id.as_deref() else { return Ok(Vec::new()) };
  let parent = store.require_session(parent_id).await?;
  Ok(vec![session_current_member(cfg, &parent)?])
}

/// The backend id the session runs on now: its `pool_member` for a pool-id session, else its own backend.
pub(crate) fn session_current_member(cfg: &CharlieBotConfig, meta: &SessionMetadata) -> anyhow::Result<String> {
  resolve_session_default_backend_model(cfg, meta).map(|(backend_id, _)| backend_id)
}

/// Resolve a delegation's backend: the requested id, its call class's target, or the session's own default.
///
/// A `backends.pools` id, whether requested or named by `backends.calls`, selects its member now and keeps the pool id on
/// the child session. A verify or cold read leaves the initiating session's current member out of every selection.
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
  let excluded = verify_exclusion(cfg, &meta, role)?;
  let selector = pool.selector(cfg).with_store(store);
  let backend_id = match requested_backend {
    Some("") => return Err(value_error("requested backend is required")),
    Some(backend_id) => backend_id.to_string(),
    None => match cfg.backends.calls.get(role.call_class()) {
      Some(CallTarget::Backend(target)) => target.clone(),
      None | Some(CallTarget::Session) => inherited_backend_id(cfg, &meta)?,
    },
  };
  let Some(member) = selector.select(&backend_id, &excluded)? else {
    return Err(value_error(selector.exhausted_message(&backend_id)?));
  };
  let model = member.required_model("backend").map_err(value_error)?;
  let session_backend = if cfg.backends.pool(&backend_id).is_some() { backend_id } else { member.id.clone() };
  Ok(SubagentBackend { session_backend, run_backend: member.id, model: Some(model) })
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
  let selector = pool.selector(cfg).with_store(store);
  crate::review::select_checker_backend(
    cfg,
    &selector,
    class,
    crate::review::ReviewScope {
      route: None,
      worker_backend: &session_backend,
      worker_model: session_model.as_deref(),
      tried_backends,
      used_backends: &HashSet::new(),
    },
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_missing_session_backend_uses_the_core_validity_error() {
    let cfg = test_config();
    let mut meta = SessionMetadata::new("unknown", "Unknown", charliebot_store::metadata::TaskProfile::Manager);
    meta.backend = "synthetic-missing".to_string();

    assert_eq!(
      resolve_session_default_backend_model(&cfg, &meta).unwrap_err().to_string(),
      "backend 'synthetic-missing' is not a backends.options or backends.pools id; valid ids: ['synthetic']"
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

    let model_subagent = resolve_requested_subagent_backend_model(
      &cfg,
      &store,
      &session.id,
      Some("synthetic"),
      SubagentRole::Improve,
      &pool,
    )
    .await
    .unwrap();
    assert_eq!(model_subagent.session_backend, "synthetic");
    assert_eq!(model_subagent.run_backend, subagent.run_backend);

    pool
      .availability()
      .record("synthetic", None, charliebot_agents::backend_availability::REASON_QUOTA_ERROR, "spent")
      .unwrap();
    for backend_id in ["synthetic", POOL_ID] {
      let error = resolve_requested_subagent_backend_model(
        &cfg,
        &store,
        &session.id,
        Some(backend_id),
        SubagentRole::Improve,
        &pool,
      )
      .await
      .unwrap_err()
      .to_string();
      assert!(error.contains("no selectable member") && error.contains("quota_error"), "{backend_id}: {error}");
    }
  }

  fn pool_config() -> CharlieBotConfig {
    let mut cfg = test_config();
    cfg.backends.pools.push(charliebot_core::config::PoolConfig {
      id: POOL_ID.to_string(),
      label: "Auto".to_string(),
      rule: charliebot_core::config::PoolRule::Priority,
      members: vec![charliebot_core::config::PoolMember { backend: "synthetic".to_string() }],
      visible: true,
    });
    cfg.backends.options.iter_mut().for_each(|option| {
      option.billing = Some(charliebot_core::config::PoolBilling::Subscription);
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
          billing: None,
          backend_type: "synthetic".to_string(),
          type_fields: Default::default(),
          visible: true,
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
