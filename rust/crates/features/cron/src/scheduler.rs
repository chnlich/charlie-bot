//! The scheduled-task service and its croniter-compatible five-field scan.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::SequenceExecutor;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use charliebot_store::control_events::ACTOR_SYSTEM;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, TaskSpec, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunRecord, SequenceRef};
use charliebot_store::{CreateTask, TaskCaller};
use chrono::{Datelike, LocalResult, Offset};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use crate::config::ScheduledTaskConfig;
use crate::loader;

const TICK_SECONDS: u64 = 60;
const AUTO_BIND_PREFIX: &str = "scheduled-node:";

/// The shared scheduler runtime installed after recovery finishes.
#[derive(Clone)]
pub struct CronScheduler {
  features: Arc<FeatureContext>,
  handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl CronScheduler {
  pub(super) fn new(features: Arc<FeatureContext>) -> Self {
    Self { features, handles: Arc::new(Mutex::new(HashMap::new())) }
  }

  async fn loop_forever(&self) {
    loop {
      tokio::time::sleep(Duration::from_secs(TICK_SECONDS)).await;
      if let Err(error) = self.tick().await {
        tracing::error!(error = %error, "scheduler_tick_error");
      }
    }
  }

  async fn tick(&self) -> anyhow::Result<()> {
    self.tick_at(chrono::Utc::now()).await
  }

  async fn tick_at(&self, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
    let cfg = self.features.config.get()?;
    let snapshot = loader::load_snapshot(&self.features, &cfg)?;
    if snapshot.tasks.is_empty() {
      return Ok(());
    }
    let old_sessions = self.features.store.list_sessions(Some(SessionStatus::Active), None, Some(true))?.rows;
    let mut old_by_task: HashMap<String, Vec<Arc<SessionMetadata>>> = HashMap::new();
    for session in old_sessions {
      if let Some(name) = session.extra.get("scheduled_task").and_then(Value::as_str) {
        old_by_task.entry(name.to_string()).or_default().push(session);
      }
    }
    let mut tasks = snapshot.tasks;
    for task in &mut tasks {
      if task.session_id.is_none() {
        if let Err(error) = self.auto_bind(task, &cfg, &old_by_task).await {
          tracing::error!(task = %task.name, error = %error, "scheduler_bind_error");
        }
      } else if let Err(error) = self.archive_legacy(&task.name, &old_by_task).await {
        tracing::error!(task = %task.name, error = %error, "scheduler_bind_error");
      }
    }
    for task in &tasks {
      if task.session_id.is_some()
        && let Err(error) = self.align_backend(task, &cfg).await
      {
        tracing::error!(task = %task.name, error = %error, "scheduler_backend_align_error");
      }
    }
    for task in &tasks {
      if !task.enabled || task.session_id.is_none() {
        continue;
      }
      if let Err(error) = self.maybe_run(task, &cfg, now).await {
        tracing::error!(task = %task.name, error = %error, "scheduler_task_error");
      }
    }
    for error in snapshot.errors {
      tracing::warn!(task = %error.name, error = %error.error, "scheduler_invalid_task_skipped");
    }
    Ok(())
  }

  async fn auto_bind(
    &self,
    task: &mut ScheduledTaskConfig,
    cfg: &CharlieBotConfig,
    old_by_task: &HashMap<String, Vec<Arc<SessionMetadata>>>,
  ) -> anyhow::Result<()> {
    let backend = effective_backend(task, cfg)?;
    let node = self
      .features
      .store
      .create_task(
        CreateTask {
          request_id: format!("{AUTO_BIND_PREFIX}{}", task.name),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some(task.name.clone()),
          backend: Some(backend.clone()),
          group: task.project.clone(),
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await?;
    let mut node = node;
    if node.status == SessionStatus::Archived {
      node =
        self.features.store.unarchive_session(&node.id).await?.ok_or_else(|| {
          anyhow::anyhow!("scheduled task '{}' node {} vanished during unarchive", task.name, node.id)
        })?;
    }
    if self.features.store.task_state(&node.id)? != "open" {
      let completion = self.features.startup.require::<charliebot_tasks::completion::TaskCompletion>()?;
      let request_id = format!("{AUTO_BIND_PREFIX}{}:restore", task.name);
      let restored = completion.restore_chain(&node.id, &request_id, "cron auto-bind").await?;
      if restored.is_empty() {
        anyhow::bail!("scheduled task '{}' node {} stayed archived during auto-bind", task.name, node.id);
      }
      node = self
        .features
        .store
        .load_meta(&node.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' node {} vanished after restore", task.name, node.id))?;
    }
    if let Some(old) = old_by_task.get(&task.name).and_then(|sessions| newest_session(sessions)) {
      let guard = self.features.store.control_lock().await;
      self
        .features
        .store
        .adopt_metadata_slot_locked(
          &guard,
          &node.id,
          old,
          "cron",
          Some(&["last_scheduled_run", "last_scheduled_cron", "last_run_status"]),
        )
        .await?;
    }
    write_cron_key(cfg, &task.name, "session_id", Value::String(node.id.clone()))?;
    task.session_id = Some(node.id.clone());
    self.archive_legacy(&task.name, old_by_task).await?;
    tracing::info!(task = %task.name, session = %node.id, backend, "scheduled_task_auto_bound");
    Ok(())
  }

  async fn archive_legacy(
    &self,
    task_name: &str,
    old_by_task: &HashMap<String, Vec<Arc<SessionMetadata>>>,
  ) -> anyhow::Result<()> {
    if let Some(sessions) = old_by_task.get(task_name) {
      for session in sessions {
        if session.status == SessionStatus::Active {
          self.features.store.archive_session(&session.id).await?;
        }
      }
    }
    Ok(())
  }

  async fn align_backend(&self, task: &ScheduledTaskConfig, cfg: &CharlieBotConfig) -> anyhow::Result<()> {
    let session_id = task.session_id.as_deref().expect("the task binding pass supplied a session id");
    let backend = effective_backend(task, cfg)?;
    let Some(node) = self.features.store.get_session(session_id).await? else {
      anyhow::bail!("scheduled task '{}' bound node {session_id} not found", task.name);
    };
    if node.backend == backend {
      return Ok(());
    }
    if self.features.store.busy_since(session_id).is_some() {
      anyhow::bail!("scheduled node {session_id} is busy while aligning backend to '{backend}'");
    }
    self.features.store.switch_backend(session_id, &backend).await?;
    tracing::info!(task = %task.name, session = %session_id, backend, "scheduled_node_backend_aligned");
    Ok(())
  }

  async fn maybe_run(
    &self,
    task: &ScheduledTaskConfig,
    cfg: &CharlieBotConfig,
    now: chrono::DateTime<chrono::Utc>,
  ) -> anyhow::Result<()> {
    let session = self.fireable_binding(task).await?;
    let timezone = task.timezone.parse::<chrono_tz::Tz>()?;
    let now = now.with_timezone(&timezone);
    let last_cron = metadata_string(&session, "last_scheduled_cron");
    if last_cron.as_deref().is_some_and(|previous| previous != task.cron) {
      update_cron_slots(
        &self.features,
        &session.id,
        json!({"last_scheduled_run": python_isoformat(&now)?, "last_scheduled_cron": task.cron}),
      )
      .await?;
      tracing::info!(task = %task.name, old = ?last_cron, new = %task.cron, "scheduler_cron_changed");
      return Ok(());
    }
    let base = match metadata_string(&session, "last_scheduled_run") {
      Some(value) => match chrono::DateTime::parse_from_rfc3339(&value) {
        Ok(value) => value.with_timezone(&timezone),
        Err(error) => {
          tracing::warn!(task = %task.name, value, error = %error, "scheduler_bad_last_run");
          now - chrono::Duration::seconds(TICK_SECONDS as i64)
        }
      },
      None => now - chrono::Duration::seconds(TICK_SECONDS as i64),
    };
    let due = CronExpression::parse(&task.cron)?.next_after(base)?;
    if due > now {
      return Ok(());
    }
    let running_handle = {
      self
        .handles
        .lock()
        .expect("cron handle mutex is not poisoned")
        .get(&task.name)
        .filter(|handle| !handle.is_finished())
        .map(|handle| handle.id().to_string())
    };
    if let Some(handle_id) = running_handle {
      let now_text = python_isoformat(&now)?;
      update_cron_slots(
        &self.features,
        &session.id,
        json!({"last_scheduled_run": now_text, "last_run_status": "skipped"}),
      )
      .await?;
      self
        .features
        .store
        .persist_and_broadcast(
          &session.id,
          json!({
            "type": event_types::SCHEDULED_RUN_SKIPPED,
            "task": task.name,
            "skipped_at": now_text,
            "reason": format!("previous round still running ({handle_id})"),
          }),
        )
        .await?;
      return Ok(());
    }
    self
      .execute_task(task.clone(), Some(python_isoformat(&due)?), true, cfg.clone(), now.with_timezone(&chrono::Utc))
      .await?;
    Ok(())
  }

  async fn execute_task(
    &self,
    mut task: ScheduledTaskConfig,
    firing: Option<String>,
    scheduled: bool,
    cfg: CharlieBotConfig,
    now: chrono::DateTime<chrono::Utc>,
  ) -> anyhow::Result<TaskRunResult> {
    if task.session_id.is_none() {
      let sessions = self.features.store.list_sessions(Some(SessionStatus::Active), None, Some(true))?.rows;
      let mut old_by_task: HashMap<String, Vec<Arc<SessionMetadata>>> = HashMap::new();
      for session in sessions {
        if let Some(name) = session.extra.get("scheduled_task").and_then(Value::as_str) {
          old_by_task.entry(name.to_string()).or_default().push(session);
        }
      }
      self.auto_bind(&mut task, &cfg, &old_by_task).await?;
    }
    let session = self.fireable_binding(&task).await?;
    let firing = match firing {
      Some(value) => value,
      None => UtcDateTime::now().to_python_isoformat(),
    };
    if task.mode.as_deref() == Some("master") {
      let dispatcher = charliebot_tasks::dispatch::dispatcher(&self.features)?;
      let input_id = firing_ref(&task, &firing);
      dispatcher
        .admit_input(
          &session.id,
          event_types::SCHEDULED_TRIGGER,
          task.prompt.as_deref().unwrap_or_default(),
          ACTOR_SYSTEM,
          json!({"input_id": input_id}),
        )
        .await?;
      dispatcher.dispatch_pending(&session.id).await?;
      self.record_bound_fire(&session, &task, now).await?;
      return Ok(TaskRunResult { session_id: session.id, thread_id: None });
    }
    if let Some(handler) = task.handler.as_deref() {
      self.record_bound_fire(&session, &task, now).await?;
      let handler = self
        .features
        .registry
        .scheduled_handler_named(handler)
        .ok_or_else(|| anyhow::anyhow!("Unknown handler: {handler:?}"))?;
      let event = match handler(Arc::clone(&self.features)).await {
        Ok(message) => {
          json!({"type": event_types::HANDLER_RESULT, "task": task.name, "status": event_types::HANDLER_STATUS_OK, "message": message})
        }
        Err(error) => {
          tracing::warn!(task = %task.name, error = %error, "handler_task_error");
          json!({"type": event_types::HANDLER_RESULT, "task": task.name, "status": event_types::HANDLER_STATUS_ERROR, "message": error.to_string()})
        }
      };
      let status = if event.get("status").and_then(Value::as_str) == Some(event_types::HANDLER_STATUS_OK) {
        "success"
      } else {
        "failed"
      };
      self.update_status(&session.id, status).await?;
      self.features.store.persist_and_broadcast(&session.id, event).await?;
      return Ok(TaskRunResult { session_id: session.id, thread_id: None });
    }
    let executor: Arc<dyn SequenceExecutor> =
      self.features.startup.require::<charliebot_tasks::execution::TaskExecutionAdapter>()?;
    let backend = effective_backend(&task, &cfg)?;
    let option = cfg
      .get_backend_option(&backend)
      .ok_or_else(|| anyhow::anyhow!("scheduled task backend '{backend}' is not configured"))?;
    let model = (!option.model.is_empty()).then(|| option.model.clone());
    let (leaf, run, prompt, _steps) =
      self.register_fire_work(&task, &session, &firing, &backend, model.as_deref(), &cfg).await?;
    self.record_bound_fire(&session, &task, now).await?;
    if scheduled {
      let executor = Arc::clone(&executor);
      let leaf_id = leaf.id.clone();
      let run_id = run.id.clone();
      let task_name = task.name.clone();
      let owner_ref = firing_ref(&task, &firing);
      let step_count = task.steps.as_ref().map_or(0, Vec::len);
      let boundary = crate::boundary::subscribe_boundary(&leaf_id, &owner_ref);
      let prompt = prompt.clone();
      let handle = tokio::spawn(async move {
        match executor.launch_and_settle(&leaf_id, &run_id, Some(&prompt)).await {
          Ok(settlement) if settlement.withheld.is_none() && step_count > 0 => {
            // Python keeps the firing task alive through the sequence chain (`src/features/cron/scheduler.py:413-434`).
            wait_for_steps_boundary(boundary).await;
            crate::boundary::remove_boundary(&leaf_id, &owner_ref);
          }
          Ok(_) => crate::boundary::remove_boundary(&leaf_id, &owner_ref),
          Err(error) => {
            crate::boundary::remove_boundary(&leaf_id, &owner_ref);
            tracing::error!(task = %task_name, error = %error, "scheduled_round_failed");
          }
        }
      });
      self.handles.lock().expect("cron handle mutex is not poisoned").insert(task.name.clone(), handle);
    } else {
      executor.launch(&leaf.id, &run.id, Some(&prompt)).await?;
    }
    Ok(TaskRunResult { session_id: session.id, thread_id: None })
  }

  async fn register_fire_work(
    &self,
    task: &ScheduledTaskConfig,
    parent: &SessionMetadata,
    firing: &str,
    backend: &str,
    model: Option<&str>,
    _cfg: &CharlieBotConfig,
  ) -> anyhow::Result<(SessionMetadata, RunRecord, String, bool)> {
    if parent.profile != TaskProfile::Manager {
      anyhow::bail!(
        "scheduled task '{}' cannot create a worker leaf under {}: the parent is not a manager task",
        task.name,
        parent.id
      );
    }
    let steps = task.steps.as_ref().is_some_and(|steps| !steps.is_empty());
    let task_spec_text = if steps {
      Some(
        task
          .steps
          .as_ref()
          .expect("a scheduled-step run has step configuration")
          .first()
          .expect("a scheduled-step run has a first step")
          .prompt
          .as_deref()
          .expect("validated scheduled steps have prompt text"),
      )
    } else {
      task.prompt.as_deref()
    };
    let goal = if steps { format!("{} steps", task.name) } else { task.prompt.clone().unwrap_or_default() };
    let request_id = format!("{}:leaf", firing_ref(task, firing));
    let leaf = self
      .features
      .store
      .create_task(
        CreateTask {
          request_id,
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Worker,
          task: Some(TaskSpec { goal, repo_path: task.repo.clone(), ..TaskSpec::default() }),
          name: Some(format!("{} · {}", task.name, firing)),
          backend: None,
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await?;
    let run_request =
      if steps { format!("{}:step:0", firing_ref(task, firing)) } else { format!("{}:work", firing_ref(task, firing)) };
    let run_id = charliebot_store::control_events::stable_run_id(&leaf.id, &run_request);
    let sequence_ref = if steps {
      Some(SequenceRef { kind: "cron_steps".to_string(), owner_ref: firing_ref(task, firing), position: 0 })
    } else {
      None
    };
    let record = RunRecord {
      id: run_id,
      session_id: leaf.id.clone(),
      kind: if steps { RunKind::ScheduledStep } else { RunKind::Work },
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some(backend.to_string()),
      model: model.map(str::to_string),
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: task.repo.clone(),
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let record = {
      let guard = self.features.store.control_lock().await;
      self.features.store.register_run_locked(&guard, &leaf.id, record, task_spec_text).await?
    };
    let first_prompt = task
      .steps
      .as_ref()
      .and_then(|steps| steps.first())
      .and_then(|step| step.prompt.clone())
      .or_else(|| task.prompt.clone())
      .unwrap_or_default();
    Ok((leaf, record, first_prompt, steps))
  }

  async fn fireable_binding(&self, task: &ScheduledTaskConfig) -> anyhow::Result<SessionMetadata> {
    let session_id = task
      .session_id
      .as_deref()
      .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' has no session binding", task.name))?;
    let meta = self
      .features
      .store
      .load_meta(session_id)
      .await?
      .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' bound session {session_id} does not exist", task.name))?;
    if self.features.store.task_state(session_id)? != "open" {
      anyhow::bail!("scheduled task '{}' bound task {session_id} is archived", task.name);
    }
    if task.mode.as_deref() == Some("master") && meta.profile != TaskProfile::Manager {
      anyhow::bail!("scheduled task '{}' (mode master) binds task {session_id}, which is not a manager", task.name);
    }
    Ok(meta)
  }

  async fn record_bound_fire(
    &self,
    session: &SessionMetadata,
    task: &ScheduledTaskConfig,
    now: chrono::DateTime<chrono::Utc>,
  ) -> anyhow::Result<()> {
    let timezone = task.timezone.parse::<chrono_tz::Tz>()?;
    let now = python_isoformat(&now.with_timezone(&timezone))?;
    update_cron_slots(
      &self.features,
      &session.id,
      json!({"last_scheduled_run": now, "last_scheduled_cron": task.cron}),
    )
    .await?;
    Ok(())
  }

  async fn update_status(&self, session_id: &str, status: &str) -> anyhow::Result<()> {
    update_cron_slots(&self.features, session_id, json!({"last_run_status": status})).await
  }

  async fn run_now(&self, name: &str) -> Result<TaskRunResult, RunError> {
    let cfg = self.features.config.get().map_err(|error| RunError::Failed(error.to_string()))?;
    let snapshot = loader::load_snapshot(&self.features, &cfg).map_err(|error| RunError::Failed(error.to_string()))?;
    let task =
      snapshot.tasks.into_iter().find(|task| task.name == name).ok_or_else(|| RunError::Missing(name.to_string()))?;
    self
      .execute_task(task, None, false, cfg.as_ref().clone(), chrono::Utc::now())
      .await
      .map_err(|error| RunError::Failed(format!("{error:#}")))
  }
}

/// The HTTP result of one manual fire.
pub struct TaskRunResult {
  /// The bound node that received the fire.
  pub session_id: String,
  /// The worker run id, absent for master and handler modes.
  pub thread_id: Option<String>,
}

/// Whether the manual fire could resolve its service and task.
pub enum RunError {
  /// Scheduler service has not started.
  Unavailable,
  /// No valid task by this name exists.
  Missing(String),
  /// The task fire failed visibly.
  Failed(String),
}

/// Run one task through the scheduler instance registered at startup.
pub async fn run_task_now(features: &Arc<FeatureContext>, name: &str) -> Result<TaskRunResult, RunError> {
  let scheduler = features.startup.get::<CronScheduler>().ok_or(RunError::Unavailable)?;
  scheduler.run_now(name).await
}

/// The scheduler lifecycle registration (`service` phase `Ready`).
#[derive(Default)]
pub struct SchedulerService {
  task: Mutex<Option<JoinHandle<()>>>,
}

impl Service for SchedulerService {
  fn name(&self) -> &'static str {
    "scheduler"
  }

  fn phase(&self) -> Phase {
    Phase::Ready
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      ctx.recovery_done.await;
      let scheduler = CronScheduler::new(Arc::clone(&ctx.features));
      ctx.features.startup.insert(scheduler.clone())?;
      let run_scheduler = scheduler.clone();
      let task = tokio::spawn(async move { run_scheduler.loop_forever().await });
      *self.task.lock().expect("scheduler task mutex is not poisoned") = Some(task);
      tracing::info!("scheduler_started");
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let task = { self.task.lock().expect("scheduler task mutex is not poisoned").take() };
      if let Some(task) = task {
        task.abort();
        if let Err(error) = task.await
          && !error.is_cancelled()
        {
          return Err(error.into());
        }
      }
      tracing::info!("scheduler_stopped");
      Ok(())
    })
  }
}

fn newest_session(sessions: &[Arc<SessionMetadata>]) -> Option<&Arc<SessionMetadata>> {
  sessions.iter().max_by_key(|session| session.created_at)
}

fn metadata_string(session: &SessionMetadata, key: &str) -> Option<String> {
  session.extra.get(key).and_then(Value::as_str).map(str::to_string)
}

async fn update_cron_slots(features: &FeatureContext, session_id: &str, values: Value) -> anyhow::Result<()> {
  let values = values.as_object().cloned().ok_or_else(|| anyhow::anyhow!("cron slot updates must be an object"))?;
  features.store.update_slot_fields(session_id, "cron", values).await?;
  Ok(())
}

fn effective_backend(task: &ScheduledTaskConfig, cfg: &CharlieBotConfig) -> anyhow::Result<String> {
  if let Some(backend) = task.backend.as_deref().filter(|backend| !backend.is_empty()) {
    if cfg.get_backend_option(backend).is_none() {
      anyhow::bail!("scheduled task backend '{backend}' is not in backends.options");
    }
    return Ok(backend.to_string());
  }
  cfg
    .backends
    .options
    .first()
    .map(|option| option.id.clone())
    .ok_or_else(|| anyhow::anyhow!("scheduled task backend resolution requires a configured backends.options entry"))
}

fn firing_ref(task: &ScheduledTaskConfig, firing: &str) -> String {
  format!("cron:{}:{firing}", task.name)
}

async fn wait_for_steps_boundary(mut boundary: tokio::sync::watch::Receiver<crate::boundary::BoundaryNotice>) {
  loop {
    let notice = *boundary.borrow_and_update();
    if notice.done || notice.failed {
      return;
    }
    if boundary.changed().await.is_err() {
      return;
    }
  }
}

/// Return the next cron occurrence in the configured time zone.
pub(super) fn next_fire_iso(expression: &str, timezone: &str, now_utc: UtcDateTime) -> anyhow::Result<String> {
  let timezone = timezone.parse::<chrono_tz::Tz>()?;
  let inner = now_utc.into_inner();
  let now = chrono::DateTime::<chrono::Utc>::from_timestamp(inner.unix_timestamp(), inner.nanosecond())
    .ok_or_else(|| anyhow::anyhow!("UTC timestamp is outside the supported range"))?;
  python_isoformat(&CronExpression::parse(expression)?.next_after(now.with_timezone(&timezone))?)
}

fn python_isoformat<Tz>(moment: &chrono::DateTime<Tz>) -> anyhow::Result<String>
where
  Tz: chrono::TimeZone,
  Tz::Offset: Offset,
{
  let instant = time::OffsetDateTime::from_unix_timestamp(moment.timestamp())?
    .replace_microsecond(moment.timestamp_subsec_micros())?;
  let offset = time::UtcOffset::from_whole_seconds(moment.offset().fix().local_minus_utc())?;
  Ok(charliebot_core::timefmt::isoformat(instant.to_offset(offset)))
}

fn write_cron_key(cfg: &CharlieBotConfig, name: &str, key: &str, value: Value) -> anyhow::Result<()> {
  let path = loader::cron_path(cfg, name);
  let raw = std::fs::read_to_string(&path)?;
  let yaml = serde_yaml::from_str::<serde_yaml::Value>(&raw)?;
  let mut value_json = serde_json::to_value(yaml)?;
  let object = value_json.as_object_mut().ok_or_else(|| anyhow::anyhow!("cron config must be a mapping"))?;
  object.insert(key.to_string(), value);
  std::fs::write(&path, charliebot_core::yaml::dump(&value_json)?)?;
  Ok(())
}

#[derive(Clone)]
struct CronField {
  values: HashSet<u32>,
  ordered: Vec<u32>,
  wildcard: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CronSearchWork {
  calendar_days: usize,
  local_candidates: usize,
}

struct CronExpression {
  minute: CronField,
  hour: CronField,
  day: CronField,
  month: CronField,
  weekday: CronField,
}

impl CronExpression {
  fn parse(expression: &str) -> anyhow::Result<Self> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
      anyhow::bail!("cron expression must have five fields: {expression:?}");
    }
    Ok(Self {
      minute: parse_field(fields[0], 0, 59, &[])?,
      hour: parse_field(fields[1], 0, 23, &[])?,
      day: parse_field(fields[2], 1, 31, &[])?,
      month: parse_field(fields[3], 1, 12, &MONTH_NAMES)?,
      weekday: parse_field(fields[4], 0, 7, &WEEKDAY_NAMES)?,
    })
  }

  fn next_after<Tz: chrono::TimeZone + Clone>(
    &self,
    base: chrono::DateTime<Tz>,
  ) -> anyhow::Result<chrono::DateTime<Tz>> {
    // Match Python's `croniter` field jumps (`src/features/cron/scheduler.py:323-326`) while preserving the Rust port's UTC minute grid.
    self.next_after_with_work(base).map(|(next, _)| next)
  }

  fn next_after_with_work<Tz: chrono::TimeZone + Clone>(
    &self,
    base: chrono::DateTime<Tz>,
  ) -> anyhow::Result<(chrono::DateTime<Tz>, CronSearchWork)> {
    let base_utc = base.with_timezone(&chrono::Utc);
    let timezone = base.timezone().clone();
    let start = base_utc.timestamp().div_euclid(60) * 60 + 60;
    // Preserve the existing timestamp horizon: the old `8 * 366 * 24 * 60` value was seconds, not minutes.
    let end = start + 8 * 366 * 24 * 60;
    let first_date = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone)
      .date_naive();
    let start_local = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone);
    let mut work = CronSearchWork::default();
    let mut best: Option<chrono::DateTime<Tz>> = None;

    for day_offset in 0..=(8 * 366 + 3) {
      let date = first_date + chrono::Duration::days(day_offset);
      work.calendar_days += 1;
      // IANA offsets stay within 24 hours, giving a conservative UTC floor for local candidates on this date.
      let date_floor =
        date.and_hms_opt(0, 0, 0).expect("midnight is a valid naive datetime").and_utc().timestamp() - 24 * 60 * 60;
      if date_floor >= end {
        break;
      }
      if let Some(next) = best.as_ref()
        && date_floor > next.with_timezone(&chrono::Utc).timestamp()
      {
        return Ok((next.clone(), work));
      }
      if !self.date_matches(date) {
        continue;
      }
      if date == start_local.date_naive() && self.minute.values.len() == 60 && self.hour.values.len() == 24 {
        return Ok((
          chrono::DateTime::from_timestamp(start, 0)
            .expect("a bounded Unix minute is representable")
            .with_timezone(&timezone),
          work,
        ));
      }
      for hour in &self.hour.ordered {
        for minute in &self.minute.ordered {
          work.local_candidates += 1;
          let Some(local_time) = date.and_hms_opt(*hour, *minute, 0) else { continue };
          let mut consider = |candidate: chrono::DateTime<Tz>| {
            let timestamp = candidate.with_timezone(&chrono::Utc).timestamp();
            if timestamp >= start
              && timestamp < end
              && timestamp.rem_euclid(60) == 0
              && best.as_ref().is_none_or(|current| timestamp < current.with_timezone(&chrono::Utc).timestamp())
            {
              best = Some(candidate);
            }
          };
          match timezone.from_local_datetime(&local_time) {
            LocalResult::Single(candidate) => consider(candidate),
            LocalResult::Ambiguous(first, second) => {
              consider(first);
              consider(second);
            }
            LocalResult::None => {}
          }
        }
      }
    }
    best.map(|next| (next, work)).ok_or_else(|| anyhow::anyhow!("cron expression has no fire within eight years"))
  }

  fn date_matches(&self, date: chrono::NaiveDate) -> bool {
    let day_match = self.day.values.contains(&date.day());
    let weekday = date.weekday().num_days_from_sunday();
    let weekday_match = self.weekday.values.contains(&weekday) || (weekday == 0 && self.weekday.values.contains(&7));
    let date_matches = match (self.day.wildcard, self.weekday.wildcard) {
      (true, true) => true,
      (true, false) => weekday_match,
      (false, true) => day_match,
      (false, false) => day_match || weekday_match,
    };
    self.month.values.contains(&date.month()) && date_matches
  }
}

const MONTH_NAMES: [(&str, u32); 12] = [
  ("jan", 1),
  ("feb", 2),
  ("mar", 3),
  ("apr", 4),
  ("may", 5),
  ("jun", 6),
  ("jul", 7),
  ("aug", 8),
  ("sep", 9),
  ("oct", 10),
  ("nov", 11),
  ("dec", 12),
];
const WEEKDAY_NAMES: [(&str, u32); 7] =
  [("sun", 0), ("mon", 1), ("tue", 2), ("wed", 3), ("thu", 4), ("fri", 5), ("sat", 6)];

fn parse_field(field: &str, min: u32, max: u32, names: &[(&str, u32)]) -> anyhow::Result<CronField> {
  let wildcard = field == "*" || field == "?";
  let mut values = HashSet::new();
  for member in field.split(',') {
    let (range, step) =
      member.split_once('/').map_or((member, 1), |(range, step)| (range, step.parse::<u32>().unwrap_or(0)));
    if step == 0 {
      anyhow::bail!("invalid cron step in {field:?}");
    }
    let (start, end) = if range == "*" || range == "?" {
      (min, max)
    } else if let Some((start, end)) = range.split_once('-') {
      (parse_cron_value(start, names)?, parse_cron_value(end, names)?)
    } else {
      let value = parse_cron_value(range, names)?;
      (value, if member.contains('/') { max } else { value })
    };
    if start < min || end > max || start > end {
      anyhow::bail!("cron field {field:?} is outside {min}..={max}");
    }
    values.extend((start..=end).step_by(step as usize));
  }
  if values.is_empty() {
    anyhow::bail!("cron field {field:?} has no values");
  }
  let mut ordered = values.iter().copied().collect::<Vec<_>>();
  ordered.sort_unstable();
  Ok(CronField { values, ordered, wildcard })
}

fn parse_cron_value(value: &str, names: &[(&str, u32)]) -> anyhow::Result<u32> {
  if let Some((_, number)) = names.iter().find(|(name, _)| value.eq_ignore_ascii_case(name)) {
    return Ok(*number);
  }
  value.parse().map_err(|_| anyhow::anyhow!("invalid cron field value {value:?}"))
}

#[cfg(test)]
mod tests {
  use charliebot_store::metadata::{SessionStatus, TaskProfile, UtcDateTime};
  use charliebot_store::run_records::RunKind;
  use charliebot_store::{CreateTask, TaskCaller};
  use chrono::{TimeZone, Timelike, Utc};
  use serde_json::{Map, Value, json};
  use std::collections::HashMap;
  use std::path::Path;
  use std::sync::Arc;

  use super::{CronExpression, CronScheduler, python_isoformat};
  use crate::config::{ScheduledTaskConfig, StepConfig};
  use crate::test_support::{feature_context, manager};

  fn write_task(home: &Path, name: &str, body: &Value) -> std::path::PathBuf {
    let path = home.join("config.d/cron.d").join(format!("{name}.yaml"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, charliebot_core::yaml::dump(body).unwrap()).unwrap();
    path
  }

  #[test]
  fn cron_expression_uses_minute_grid_and_croniter_day_union() {
    let expression = CronExpression::parse("0 8 1 * mon").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2026-06-08T08:00:00+00:00");
  }

  #[test]
  fn sparse_cron_checks_calendar_dates_within_the_existing_horizon() {
    let expression = CronExpression::parse("0 0 1 * *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

    let (next, work) = expression.next_after_with_work(base).unwrap();

    assert_eq!(next.to_rfc3339(), "2026-02-01T00:00:00+00:00");
    assert!(work.calendar_days <= 33, "{work:?}");
    assert_eq!(work.local_candidates, 1);

    let yearly = CronExpression::parse("0 0 1 1 *").unwrap();
    assert!(yearly.next_after(base).is_err());
  }

  #[test]
  fn cron_stamps_use_python_microseconds_and_keep_the_task_timezone_offset() {
    let timezone = "America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap();
    let whole_seconds = timezone.with_ymd_and_hms(2026, 10, 20, 2, 26, 6).single().unwrap();
    let fractional = whole_seconds.with_nanosecond(685_829_123).unwrap();

    assert_eq!(python_isoformat(&whole_seconds).unwrap(), "2026-10-20T02:26:06-07:00");
    assert_eq!(python_isoformat(&fractional).unwrap(), "2026-10-20T02:26:06.685829-07:00");
  }

  #[tokio::test]
  async fn tick_binds_disabled_tasks_without_projects_once_and_reuses_the_stable_node() {
    // Python: tests/test_cron_auto_bind.py::test_task_without_project_lands_ungrouped
    // Python: tests/test_cron_auto_bind.py::test_disabled_task_and_task_without_prior_cron_session_are_bound_too
    // Python: tests/test_cron_auto_bind.py::test_recreated_task_reattaches_to_its_original_node
    let (home, features) = feature_context();
    let path = write_task(
      home.path(),
      "synthetic-task",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}),
    );
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let enabled_path = write_task(
      home.path(),
      "synthetic-enabled-task",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":true}),
    );

    scheduler.tick().await.unwrap();
    let first: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let first_id = first["session_id"].as_str().unwrap().to_string();
    let first_node = features.store.get_session(&first_id).await.unwrap().unwrap();
    assert_eq!(first_node.extra.get("scheduled_task"), Some(&Value::Null));
    assert_eq!(first_node.group, None);
    assert_eq!(features.store.task_state(&first_id).unwrap(), "open");
    let enabled: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&enabled_path).unwrap()).unwrap();
    let enabled_id = enabled["session_id"].as_str().unwrap();
    let enabled_node = features.store.get_session(enabled_id).await.unwrap().unwrap();
    assert_eq!(enabled_node.name, "synthetic-enabled-task");
    assert_eq!(enabled_node.group, None);

    scheduler.tick().await.unwrap();
    let bound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(bound["session_id"].as_str(), Some(first_id.as_str()));
    let scheduled = features.store.list_sessions(None, None, Some(true)).unwrap();
    assert!(scheduled.rows.is_empty());

    std::fs::remove_file(&path).unwrap();
    let recreated = write_task(
      home.path(),
      "synthetic-task",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}),
    );
    scheduler.tick().await.unwrap();
    let rebound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(recreated).unwrap()).unwrap();
    assert_eq!(rebound["session_id"].as_str(), Some(first_id.as_str()));
  }

  #[tokio::test]
  async fn scheduler_tick_runs_handlers_and_updates_the_shared_session_views() {
    // Python: tests/test_cron_auto_bind.py::test_handler_task_binds_and_records_its_result_on_the_node
    // Python: tests/test_scheduler_shared_session_blocks.py::test_scheduled_fire_bookkeeping_writes_the_injected_session_blocks
    // Python: tests/test_scheduler_shared_session_blocks.py::test_scheduled_round_events_reach_shared_read_cache
    let (home, features) = feature_context();
    let path = write_task(
      home.path(),
      "synthetic-handler",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}),
    );
    let scheduler = CronScheduler::new(Arc::clone(&features));

    // Both ticks run at 08:00:30 of the task's default timezone on a fixed date, so no wall clock decides whether
    // the first tick fires the 0 8 * * * occurrence.
    let clock_timezone = crate::config::DEFAULT_TIMEZONE.parse::<chrono_tz::Tz>().unwrap();
    let clock = chrono::NaiveDate::from_ymd_opt(2026, 1, 5)
      .unwrap()
      .and_hms_opt(8, 0, 30)
      .unwrap()
      .and_local_timezone(clock_timezone)
      .single()
      .unwrap()
      .with_timezone(&Utc);
    scheduler.tick_at(clock).await.unwrap();
    let mut binding: Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    binding["enabled"] = json!(true);
    std::fs::write(&path, charliebot_core::yaml::dump(&binding).unwrap()).unwrap();
    let session_id = binding["session_id"].as_str().unwrap();
    let guard = features.store.control_lock().await;
    features
      .store
      .update_slot_fields_locked(
        &guard,
        session_id,
        "cron",
        Map::from_iter([("last_scheduled_run".to_string(), json!("2026-01-01T03:00:00+00:00"))]),
      )
      .await
      .unwrap();
    drop(guard);

    scheduler.tick_at(clock).await.unwrap();

    let meta = features.store.get_session(session_id).await.unwrap().unwrap();
    assert_eq!(meta.extra.get("last_run_status").and_then(Value::as_str), Some("success"));
    assert!(meta.extra.get("last_scheduled_run").and_then(Value::as_str).is_some());
    let events = features.store.load_chat_events_sync(session_id).unwrap();
    assert!(events.iter().any(|event| event["type"].as_str() == Some(charliebot_core::event_types::HANDLER_RESULT)));
    let projection = features.store.get_message_projection(session_id).unwrap().unwrap();
    assert_eq!(projection.history().len(), 1);
    assert!(features.store.child_ids(session_id).unwrap().is_empty());
  }

  #[tokio::test]
  async fn tick_archives_the_old_active_cron_session_when_it_binds_a_new_node() {
    // Python: tests/test_cron_auto_bind.py::test_tick_auto_binds_unbound_task_with_active_legacy_cron_session
    let (home, features) = feature_context();
    let mut legacy_extra = Map::new();
    legacy_extra.insert("scheduled_task".to_string(), json!("legacy-task"));
    legacy_extra.insert("last_scheduled_run".to_string(), json!("2026-10-01T08:00:00-07:00"));
    let legacy = features
      .store
      .create_task(
        CreateTask {
          request_id: "legacy-cron-session".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("legacy-task".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        legacy_extra,
      )
      .await
      .unwrap();
    assert_eq!(legacy.status, SessionStatus::Active);
    assert_eq!(legacy.extra.get("scheduled_task").and_then(Value::as_str), Some("legacy-task"));
    assert!(
      features
        .store
        .list_sessions(Some(SessionStatus::Active), None, Some(true))
        .unwrap()
        .rows
        .iter()
        .any(|session| session.id == legacy.id)
    );
    let path = write_task(
      home.path(),
      "legacy-task",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}),
    );

    CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

    let task: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let bound_id = task["session_id"].as_str().unwrap();
    assert_ne!(bound_id, legacy.id);
    let archived_legacy = features.store.get_session(&legacy.id).await.unwrap().unwrap();
    assert_eq!(archived_legacy.status, SessionStatus::Archived);
    let bound = features.store.get_session(bound_id).await.unwrap().unwrap();
    assert_eq!(bound.extra.get("scheduled_task"), Some(&Value::Null));
    assert_eq!(bound.extra.get("last_scheduled_run"), Some(&json!("2026-10-01T08:00:00-07:00")));
  }

  #[tokio::test]
  async fn auto_bind_migration_registers_the_due_fire_leaf_under_its_node() {
    // Python: tests/test_cron_auto_bind.py::test_due_fire_after_migration_creates_its_leaf_under_the_node
    let (home, features) = feature_context();
    let repo_home = tempfile::tempdir().unwrap();
    let repo = crate::test_support::init_repo_with_origin(repo_home.path()).unwrap();
    let task_name = "synthetic-migrated-task";
    let mut legacy_extra = Map::new();
    legacy_extra.insert("scheduled_task".to_string(), json!(task_name));
    legacy_extra.insert("last_scheduled_run".to_string(), json!("2026-06-07T03:00:00-07:00"));
    legacy_extra.insert("last_scheduled_cron".to_string(), json!("0 3 * * *"));
    let legacy = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-legacy-cron-session".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some(task_name.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        legacy_extra,
      )
      .await
      .unwrap();
    write_task(
      home.path(),
      task_name,
      &json!({"cron":"0 3 * * *","prompt":"Synthetic migration work","repo":repo,"backend":"synthetic","enabled":true}),
    );
    let mut task = ScheduledTaskConfig {
      name: task_name.to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: Some(repo.display().to_string()),
      backend: Some("synthetic".to_string()),
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      project: None,
      allow_failure: false,
      session_id: None,
      mode: None,
      prompt: Some("Synthetic migration work".to_string()),
      handler: None,
      steps: None,
    };
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let old_by_task = HashMap::from([(task_name.to_string(), vec![Arc::new(legacy.clone())])]);
    scheduler.auto_bind(&mut task, &features.config.get().unwrap(), &old_by_task).await.unwrap();

    let node_id = task.session_id.as_deref().unwrap();
    let node = features.store.get_session(node_id).await.unwrap().unwrap();
    let (leaf, run, prompt, _) = scheduler
      .register_fire_work(
        &task,
        &node,
        "2026-06-08T03:00:00-07:00",
        "synthetic",
        Some("synthetic-model"),
        &features.config.get().unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(leaf.task_parent_id.as_deref(), Some(node_id));
    assert_eq!(leaf.task.as_ref().unwrap().task_type, None);
    assert_eq!(leaf.task.as_ref().unwrap().repo_path.as_deref(), Some(repo.to_str().unwrap()));
    assert_eq!(leaf.task.as_ref().unwrap().base_branch, None);
    assert_eq!(run.backend.as_deref(), Some("synthetic"));
    assert_eq!(run.model.as_deref(), Some("synthetic-model"));
    let expected_hash = charliebot_store::control_events::sha256_hex("Synthetic migration work");
    assert_eq!(run.task_spec_hash.as_deref(), Some(expected_hash.as_str()));
    let task_spec_path = Path::new(run.task_spec_ref.as_deref().unwrap());
    assert_eq!(task_spec_path.file_name().and_then(|name| name.to_str()), Some("task_spec.md"));
    assert_eq!(std::fs::read_to_string(task_spec_path).unwrap(), "Synthetic migration work");
    assert_eq!(prompt, "Synthetic migration work");
    assert_eq!(features.store.get_session(&legacy.id).await.unwrap().unwrap().status, SessionStatus::Archived);
    assert!(features.store.child_ids(&legacy.id).unwrap().is_empty());

    std::fs::write(
      features.config.get().unwrap().credentials_file(),
      "charliebot:\n  access_key: synthetic-access-key\n",
    )
    .unwrap();
    charliebot_tasks::execution::pre_recovery_step(Arc::clone(&features)).await.unwrap();
    let dispatcher = features.startup.require::<charliebot_tasks::dispatch::Dispatcher>().unwrap();
    dispatcher.set_executor(None);
    let adapter = features.startup.require::<charliebot_tasks::execution::TaskExecutionAdapter>().unwrap();
    let mut process_guard = crate::test_support::ProcessGroupGuard::new();
    adapter.launch(&leaf.id, &run.id).unwrap();

    let (finished_run, outcome) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
      loop {
        let current = features.store.read_run_sync(&leaf.id, &run.id).unwrap().unwrap();
        if let Some(pid) = current.pid {
          process_guard.track(pid);
        }
        if let Some(outcome) =
          charliebot_store::run_records::terminal_outcome(&features.store.load_all_facts(&leaf.id).unwrap(), &run.id)
            .unwrap()
        {
          break (current, outcome);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("migrated repo-bound work run reaches a terminal fact");
    assert_eq!(outcome, charliebot_store::run_records::RunOutcomeKind::Success);
    assert_eq!(finished_run.kind, charliebot_store::run_records::RunKind::Work);
    assert!(finished_run.pid.is_some());
    assert!(finished_run.pid_start.is_some());
    assert_eq!(finished_run.backend.as_deref(), Some("synthetic"));
    assert_eq!(finished_run.model.as_deref(), Some("synthetic-model"));
  }

  #[tokio::test]
  async fn scheduled_step_run_pins_its_configured_prompt() {
    let (_home, features) = feature_context();
    let parent = manager(&features, "synthetic-step-task-spec-parent").await.unwrap();
    let task = ScheduledTaskConfig {
      name: "synthetic-step-task".to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: None,
      backend: Some("synthetic".to_string()),
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      project: None,
      allow_failure: false,
      session_id: Some(parent.id.clone()),
      mode: Some("worker".to_string()),
      prompt: None,
      handler: None,
      steps: Some(vec![
        StepConfig {
          name: "select".to_string(),
          prompt_file: None,
          prompt: Some("Select candidates.".to_string()),
          backend: None,
          distinct_backend_from: None,
        },
        StepConfig {
          name: "review".to_string(),
          prompt_file: None,
          prompt: Some("Review the diff.".to_string()),
          backend: None,
          distinct_backend_from: None,
        },
      ]),
    };
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let cfg = features.config.get().unwrap();
    let (leaf, run, prompt, _) =
      scheduler.register_fire_work(&task, &parent, "2026-10-21T03:00:00-07:00", "synthetic", None, &cfg).await.unwrap();

    assert_eq!(run.kind, RunKind::ScheduledStep);
    assert_eq!(prompt, "Select candidates.");
    assert_eq!(std::fs::read_to_string(run.task_spec_ref.as_deref().unwrap()).unwrap(), "Select candidates.");
    let expected_hash = charliebot_store::control_events::sha256_hex("Select candidates.");
    assert_eq!(run.task_spec_hash.as_deref(), Some(expected_hash.as_str()));
    assert_eq!(features.store.read_run_sync(&leaf.id, &run.id).unwrap().unwrap().task_spec_ref, run.task_spec_ref);
  }

  #[tokio::test]
  async fn auto_bind_replays_each_durable_migration_boundary_to_one_node_and_binding() {
    // Python: tests/test_cron_auto_bind.py::test_crash_replay_after_each_step_ends_in_one_node_one_binding_no_active_cron_session
    for fail_after in ["create", "copy", "write"] {
      let (home, features) = feature_context();
      let task_name = format!("synthetic-replay-{fail_after}");
      let mut extra = Map::new();
      extra.insert("scheduled_task".to_string(), json!(task_name));
      extra.insert("last_scheduled_run".to_string(), json!("2026-10-19T09:00:00+00:00"));
      let legacy = features
        .store
        .create_task(
          CreateTask {
            request_id: format!("synthetic-replay-legacy-{fail_after}"),
            task_parent_id: None,
            profile: TaskProfile::Manager,
            task: None,
            name: Some(task_name.clone()),
            backend: Some("synthetic".to_string()),
            group: None,
            session_id: None,
          },
          TaskCaller::System,
          extra,
        )
        .await
        .unwrap();
      let path =
        write_task(home.path(), &task_name, &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}));
      let node = features
        .store
        .create_task(
          CreateTask {
            request_id: format!("scheduled-node:{task_name}"),
            task_parent_id: None,
            profile: TaskProfile::Manager,
            task: None,
            name: Some(task_name.clone()),
            backend: Some("synthetic".to_string()),
            group: None,
            session_id: None,
          },
          TaskCaller::System,
          Map::new(),
        )
        .await
        .unwrap();
      if fail_after != "create" {
        let guard = features.store.control_lock().await;
        features
          .store
          .adopt_metadata_slot_locked(
            &guard,
            &node.id,
            &legacy,
            "cron",
            Some(&["last_scheduled_run", "last_scheduled_cron", "last_run_status"]),
          )
          .await
          .unwrap();
      }
      if fail_after == "write" {
        let mut body = serde_yaml::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        body.as_object_mut().unwrap().insert("session_id".to_string(), json!(node.id));
        std::fs::write(&path, charliebot_core::yaml::dump(&body).unwrap()).unwrap();
      }

      CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

      let replayed: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
      assert_eq!(replayed["session_id"].as_str(), Some(node.id.as_str()), "after {fail_after}");
      let stored_node = features.store.get_session(&node.id).await.unwrap().unwrap();
      assert_eq!(stored_node.extra.get("last_scheduled_run"), Some(&json!("2026-10-19T09:00:00+00:00")));
      assert_eq!(features.store.get_session(&legacy.id).await.unwrap().unwrap().status, SessionStatus::Archived);
      let active = features.store.list_sessions(Some(SessionStatus::Active), None, None).unwrap();
      assert_eq!(active.rows.iter().filter(|session| session.name == task_name).count(), 1, "after {fail_after}");
    }
  }

  #[tokio::test]
  async fn scheduler_sweep_archives_a_busy_legacy_cron_session() {
    // Python: tests/test_cron_auto_bind.py::test_sweep_archives_active_cron_session_of_bound_task_despite_stuck_running_thread
    let (home, features) = feature_context();
    let task_name = "synthetic-sweep-task";
    let node = crate::test_support::manager(&features, "synthetic-sweep-node").await.unwrap();
    let mut extra = Map::new();
    extra.insert("scheduled_task".to_string(), json!(task_name));
    let legacy = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-stuck-cron-session".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some(task_name.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        extra,
      )
      .await
      .unwrap();
    features.store.mark_busy(&legacy.id, UtcDateTime::now()).unwrap();
    write_task(
      home.path(),
      task_name,
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","session_id":node.id,"enabled":false}),
    );

    CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

    assert_eq!(features.store.get_session(&legacy.id).await.unwrap().unwrap().status, SessionStatus::Archived);
    assert!(features.store.busy_since(&legacy.id).is_some());
    assert_eq!(features.store.get_session(&node.id).await.unwrap().unwrap().status, SessionStatus::Active);
  }

  #[tokio::test]
  async fn edited_yaml_backend_is_aligned_on_the_next_tick_without_rebinding() {
    // Python: tests/test_cron_auto_bind.py::test_hand_edited_yaml_backend_is_followed_on_the_next_tick
    let (home, features) = crate::test_support::feature_context_with_extra_backends(&["synthetic-alt"]);
    let node = crate::test_support::manager(&features, "synthetic-edit-backend-node").await.unwrap();
    let path = write_task(
      home.path(),
      "synthetic-edit-backend",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","session_id":node.id,"backend":"synthetic","enabled":false}),
    );
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick().await.unwrap();
    let mut body = serde_yaml::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
    body.as_object_mut().unwrap().insert("backend".to_string(), json!("synthetic-alt"));
    std::fs::write(&path, charliebot_core::yaml::dump(&body).unwrap()).unwrap();

    scheduler.tick().await.unwrap();

    assert_eq!(features.store.get_session(&node.id).await.unwrap().unwrap().backend, "synthetic-alt");
    assert_eq!(features.store.list_sessions(None, None, None).unwrap().rows.len(), 1);
    let persisted: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(persisted["session_id"].as_str(), Some(node.id.as_str()));
  }

  #[tokio::test]
  async fn scheduler_aligns_a_bound_node_backend_in_place() {
    // Python: tests/test_cron_backend.py::test_scheduler_aligns_bound_node_backend_in_place
    let (home, features) = crate::test_support::feature_context_with_extra_backends(&["synthetic-alt"]);
    let node = crate::test_support::manager(&features, "synthetic-align-backend-node").await.unwrap();
    write_task(
      home.path(),
      "synthetic-align-backend",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","session_id":node.id,"backend":"synthetic-alt","enabled":false}),
    );

    CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

    assert_eq!(features.store.get_session(&node.id).await.unwrap().unwrap().backend, "synthetic-alt");
    assert_eq!(features.store.list_sessions(None, None, None).unwrap().rows.len(), 1);
  }

  #[tokio::test]
  async fn an_upcoming_occurrence_preserves_last_run_and_does_not_catch_up() {
    // Python: tests/test_cron_backend.py::test_backend_alignment_preserves_last_run_to_avoid_catchup_fire
    let (home, features) = feature_context();
    let node = crate::test_support::manager(&features, "synthetic-no-catchup-node").await.unwrap();
    let last_run = UtcDateTime::now().to_python_isoformat();
    let guard = features.store.control_lock().await;
    features
      .store
      .update_slot_fields_locked(
        &guard,
        &node.id,
        "cron",
        Map::from_iter([
          ("last_scheduled_run".to_string(), json!(last_run)),
          ("last_scheduled_cron".to_string(), json!("0 0 1 1 *")),
        ]),
      )
      .await
      .unwrap();
    drop(guard);
    write_task(
      home.path(),
      "synthetic-no-catchup",
      &json!({"cron":"0 0 1 1 *","handler":"synthetic_handler","session_id":node.id,"enabled":true}),
    );

    CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

    let fresh = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(fresh.extra.get("last_scheduled_run").and_then(Value::as_str), Some(last_run.as_str()));
    assert_eq!(fresh.extra.get("last_run_status"), Some(&Value::Null));
    assert!(
      !features
        .store
        .load_chat_events_sync(&node.id)
        .unwrap()
        .iter()
        .any(|event| event["type"].as_str() == Some(charliebot_core::event_types::HANDLER_RESULT))
    );
  }

  #[tokio::test]
  async fn auto_bind_restores_a_status_archived_node_without_a_task_reopened_fact() {
    // Python: tests/test_archive_cron_restore.py::test_auto_bind_restore_leaves_the_legacy_status_branch_alone
    let (home, features) = feature_context();
    let task_name = "synthetic-legacy-restore";
    let path =
      write_task(home.path(), task_name, &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}));
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick().await.unwrap();
    let first: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let node_id = first["session_id"].as_str().unwrap().to_string();
    let mut unbound = serde_yaml::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
    unbound.as_object_mut().unwrap().remove("session_id");
    std::fs::write(&path, charliebot_core::yaml::dump(&unbound).unwrap()).unwrap();
    features.store.archive_session(&node_id).await.unwrap();
    assert_eq!(features.store.task_state(&node_id).unwrap(), "open");

    scheduler.tick().await.unwrap();

    let restored = features.store.get_session(&node_id).await.unwrap().unwrap();
    assert_eq!(restored.status, SessionStatus::Active);
    assert_eq!(features.store.task_state(&node_id).unwrap(), "open");
    assert!(
      !features
        .store
        .load_all_facts(&node_id)
        .unwrap()
        .iter()
        .any(|event| event["type"].as_str() == Some(charliebot_core::event_types::TASK_REOPENED))
    );
    let rebound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(rebound["session_id"].as_str(), Some(node_id.as_str()));
  }
}
