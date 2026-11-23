//! The scheduled-task service and its croniter-compatible five-field scan.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_backend_check::{Hit, Roots, Vocabulary, gate};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::SequenceExecutor;
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use charliebot_store::control_events::ACTOR_SYSTEM;
use charliebot_store::metadata::{
  LAST_RUN_STATUS, LAST_SCHEDULED_CRON, LAST_SCHEDULED_RUN, SCHEDULED_TASK, SessionMetadata, SessionStatus,
  TASK_SCHEDULE_SLOTS, TaskApprover, TaskProfile, TaskSpec, UtcDateTime,
};
use charliebot_store::run_records::{RunKind, RunRecord, SequenceRef};
use charliebot_store::{CreateTask, TaskCaller};
use chrono::{Datelike, LocalResult, Offset};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use crate::config::ScheduledTaskConfig;
use crate::firing::{firing_ref, round_request_id};
use crate::{host_gate, loader, rounds, session_input, steps};

const TICK_SECONDS: u64 = 60;
const AUTO_BIND_PREFIX: &str = "scheduled-node:";
/// The skip reason of a fire whose bound session is archived.
const ARCHIVED_BINDING_SKIP: &str = "binding unavailable: archived";

/// The shared scheduler runtime installed after recovery finishes. It keeps no per-round state: whether a round is in
/// flight is read from the persisted runs (`round_in_flight`).
#[derive(Clone)]
pub struct CronScheduler {
  features: Arc<FeatureContext>,
}

impl CronScheduler {
  pub(super) fn new(features: Arc<FeatureContext>) -> Self {
    Self { features }
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

  pub(crate) async fn tick_at(&self, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
    let cfg = self.features.config.get()?;
    let snapshot = loader::load_snapshot(&self.features, &cfg)?;
    if snapshot.tasks.is_empty() {
      return Ok(());
    }
    let old_sessions = self.features.store.list_sessions(Some(SessionStatus::Active), None, Some(true))?.rows;
    let mut old_by_task: HashMap<String, Vec<Arc<SessionMetadata>>> = HashMap::new();
    for session in old_sessions {
      if let Some(name) = session.extra.get(SCHEDULED_TASK).and_then(Value::as_str) {
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
          backend: None,
          group: None,
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
    write_cron_key(cfg, &task.name, "session_id", Value::String(node.id.clone()))?;
    task.session_id = Some(node.id.clone());
    self.archive_legacy(&task.name, old_by_task).await?;
    tracing::info!(task = %task.name, session = %node.id, "scheduled_task_auto_bound");
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

  /// The reason the bound session's backend cannot take a fire of `task`, or `None`. A task that takes no session backend
  /// (a handler) is never refused here.
  fn fire_backend_refusal(
    task: &ScheduledTaskConfig,
    cfg: &CharlieBotConfig,
    session: &SessionMetadata,
  ) -> anyhow::Result<Option<String>> {
    if !task.uses_session_backend() {
      return Ok(None);
    }
    rounds::session_backend_refusal(cfg, &session.backend, task.improve_host.is_some())
  }

  /// The reason a fire of `task` is refused because the profile's machinery, or the task's own prompt files, hold a
  /// hardcoded backend choice; `None` when the fire may run. Every cron task is checked, a handler's fire included.
  fn literal_refusal(&self, task: &ScheduledTaskConfig, cfg: &CharlieBotConfig) -> anyhow::Result<Option<String>> {
    let repo = self.features.startup.require::<RepoRoot>()?;
    let prompt_pointers = task
      .prompt_file
      .iter()
      .chain(task.steps.iter().flatten().filter_map(|step| step.prompt_file.as_ref()))
      .filter(|pointer| !pointer.is_empty());
    let mut prompts = Vec::new();
    for pointer in prompt_pointers {
      prompts.push(loader::resolve_path(pointer, repo.path()).map_err(anyhow::Error::msg)?);
    }
    let (hits, roots) = self.literal_sites(cfg, &prompts)?;
    Ok((!hits.is_empty()).then(|| {
      format!("a hardcoded backend choice, so the fire does not run:\n{}", gate::report(&hits, roots.repo.as_deref()))
    }))
  }

  /// The open sites in the profile's machinery and in `prompts`, read under the server's own configuration, and the
  /// roots that the sites are reported under.
  fn literal_sites(&self, cfg: &CharlieBotConfig, prompts: &[PathBuf]) -> anyhow::Result<(Vec<Hit>, Roots)> {
    let repo = self.features.startup.require::<RepoRoot>()?;
    let mut paths = charliebot_backend_check::default_paths(None, &cfg.charliebot_home);
    paths.extend_from_slice(prompts);
    let roots = gate::roots(Some(repo.path()), &cfg.charliebot_home)?;
    let vocab = Vocabulary::of_config(&self.features.registry, cfg)?;
    let hits = gate::open_hits(&vocab, &roots, &paths)?;
    Ok((hits, roots))
  }

  /// The server-start check: the profile's machinery is read once, when the server starts. Each site is logged, and the
  /// report goes to the bound session of each enabled task, because every fire of such a task is refused until the site
  /// is removed. A check that cannot run is logged and reported the same way. The start goes on either way.
  async fn report_machinery_literals(&self) {
    let message = match self.machinery_report() {
      Ok(Some(message)) => message,
      Ok(None) => return,
      Err(error) => {
        tracing::error!(error = %format!("{error:#}"), "scheduler_machinery_check_error");
        format!("the backend check of the profile's machinery could not run at server start: {error:#}")
      }
    };
    self.post_to_bound_sessions(&message).await;
  }

  /// The startup report of the machinery's open sites, one log line per site, or `None` when there are none.
  fn machinery_report(&self) -> anyhow::Result<Option<String>> {
    let cfg = self.features.config.get()?;
    let (hits, roots) = self.literal_sites(&cfg, &[])?;
    if hits.is_empty() {
      return Ok(None);
    }
    for hit in &hits {
      tracing::warn!(
        path = %hit.path.display(),
        line = hit.line,
        class = hit.class.label(),
        value = %hit.value,
        "scheduler_machinery_literal"
      );
    }
    Ok(Some(format!(
      "the server start found hardcoded backend choices in the profile's machinery, so each scheduled fire is refused until \
       they are removed:\n{}",
      gate::report(&hits, roots.repo.as_deref())
    )))
  }

  /// Posts `message` to the bound session of each enabled task. A session that cannot be read or written is logged, and
  /// the other sessions still get the message.
  async fn post_to_bound_sessions(&self, message: &str) {
    let sessions = match self.enabled_bound_sessions() {
      Ok(sessions) => sessions,
      Err(error) => {
        tracing::error!(error = %format!("{error:#}"), "scheduler_startup_report_error");
        return;
      }
    };
    for session_id in sessions {
      let posted = match self.features.store.get_session(&session_id).await {
        Ok(Some(session)) => self.post_fire_error(&session, message, false).await,
        Ok(None) => Err(anyhow::anyhow!("bound session {session_id} does not exist")),
        Err(error) => Err(error.into()),
      };
      if let Err(error) = posted {
        tracing::error!(session = %session_id, error = %format!("{error:#}"), "scheduler_startup_report_error");
      }
    }
  }

  /// The distinct session ids that the enabled cron tasks are bound to.
  fn enabled_bound_sessions(&self) -> anyhow::Result<BTreeSet<String>> {
    let cfg = self.features.config.get()?;
    let snapshot = loader::load_snapshot(&self.features, &cfg)?;
    Ok(snapshot.tasks.into_iter().filter(|task| task.enabled).filter_map(|task| task.session_id).collect())
  }

  /// Refuse one fire of `task` whose bound session's backend cannot take it. The reason is posted to the session, the
  /// fire counts as used so the next tick does not retry it, and the task's last run records as failed. No trigger
  /// input is queued and no worker starts.
  async fn refuse_fire(
    &self,
    session: &SessionMetadata,
    task: &ScheduledTaskConfig,
    now: chrono::DateTime<chrono::Utc>,
    reason: &str,
  ) -> anyhow::Result<()> {
    self.post_fire_error(session, &format!("scheduled task '{}' did not run: {reason}", task.name), false).await?;
    self.record_bound_fire(session, task, now).await?;
    self.update_status(&session.id, &task.name, "failed").await
  }

  async fn maybe_run(
    &self,
    task: &ScheduledTaskConfig,
    cfg: &CharlieBotConfig,
    now: chrono::DateTime<chrono::Utc>,
  ) -> anyhow::Result<()> {
    let session = self.bound_session(task).await?;
    let timezone = task.timezone.parse::<chrono_tz::Tz>()?;
    let now = now.with_timezone(&timezone);
    let last_cron = task_slot_string(&session, &task.name, LAST_SCHEDULED_CRON);
    if last_cron.as_deref().is_some_and(|previous| previous != task.cron) {
      update_task_slot(
        &self.features,
        &session.id,
        &task.name,
        json!({LAST_SCHEDULED_RUN: isoformat_text(&now)?, LAST_SCHEDULED_CRON: task.cron}),
      )
      .await?;
      tracing::info!(task = %task.name, old = ?last_cron, new = %task.cron, "scheduler_cron_changed");
      return Ok(());
    }
    let base = match task_slot_string(&session, &task.name, LAST_SCHEDULED_RUN) {
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
    if let Some(reason) = Self::fire_backend_refusal(task, cfg, &session)? {
      self.refuse_fire(&session, task, now.with_timezone(&chrono::Utc), &reason).await?;
      return Ok(());
    }
    if let Some(reason) = self.skip_reason(task, cfg, &session).await? {
      self.record_skip(&session.id, &task.name, isoformat_text(&now)?, reason).await?;
      return Ok(());
    }
    self
      .execute_task(task.clone(), Some(isoformat_text(&due)?), true, cfg.clone(), now.with_timezone(&chrono::Utc))
      .await?;
    Ok(())
  }

  /// The reason a due fire of `task` does not run, or `None` when it runs. Before this check, `maybe_run` checks the
  /// backend of the bound session for a task that takes one (a handler does not), and fails such a fire rather than
  /// skipping it. An archived bound session skips it next, since its inputs are no longer read (S2 b). Then the task's
  /// own work holds it: a prompt task's trigger input and its round (S2 a). A host that awaits login approval holds it
  /// last (S2 c).
  async fn skip_reason(
    &self,
    task: &ScheduledTaskConfig,
    cfg: &CharlieBotConfig,
    session: &SessionMetadata,
  ) -> anyhow::Result<Option<String>> {
    if self.features.store.task_state(&session.id)? != "open" {
      return Ok(Some(ARCHIVED_BINDING_SKIP.to_string()));
    }
    if task.is_prompt()
      && let Some(input) = rounds::trigger_input_in_flight(&self.features, task, &session.id)?
    {
      return Ok(Some(format!("previous round still running ({input})")));
    }
    if let Some(round) = rounds::round_in_flight(&self.features, task, &session.id, cfg)? {
      return Ok(Some(format!("previous round still running ({round})")));
    }
    host_gate::hold_reason(&self.features, task).await
  }

  /// One due fire of `task_name` used up on its bound session, with the reason it did not run recorded.
  /// The slots take the fire as used, so the next due fire is the next occurrence. No worker starts and no session
  /// is woken.
  async fn record_skip(
    &self,
    session_id: &str,
    task_name: &str,
    skipped_at: String,
    reason: String,
  ) -> anyhow::Result<()> {
    update_task_slot(
      &self.features,
      session_id,
      task_name,
      json!({LAST_SCHEDULED_RUN: skipped_at, LAST_RUN_STATUS: "skipped"}),
    )
    .await?;
    self
      .features
      .store
      .persist_and_broadcast(
        session_id,
        json!({
          "type": event_types::SCHEDULED_RUN_SKIPPED,
          "task": task_name,
          "skipped_at": skipped_at,
          "reason": reason,
        }),
      )
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
        if let Some(name) = session.extra.get(SCHEDULED_TASK).and_then(Value::as_str) {
          old_by_task.entry(name.to_string()).or_default().push(session);
        }
      }
      self.auto_bind(&mut task, &cfg, &old_by_task).await?;
    }
    let session = self.fireable_binding(&task).await?;
    let firing = match firing {
      Some(value) => value,
      None => UtcDateTime::now().to_isoformat_text(),
    };
    // A due fire was checked in `maybe_run`; a run started now is checked here.
    if !scheduled && let Some(reason) = Self::fire_backend_refusal(&task, &cfg, &session)? {
      self.refuse_fire(&session, &task, now, &reason).await?;
      return Ok(TaskRunResult { session_id: session.id, thread_id: None });
    }
    let refusal = match self.literal_refusal(&task, &cfg) {
      Ok(refusal) => refusal,
      Err(error) => {
        tracing::error!(task = %task.name, error = %format!("{error:#}"), "scheduler_backend_check_error");
        Some(format!("the backend check could not run: {error:#}"))
      }
    };
    if let Some(reason) = refusal {
      self.refuse_fire(&session, &task, now, &reason).await?;
      return Ok(TaskRunResult { session_id: session.id, thread_id: None });
    }
    if task.is_prompt() {
      return self.admit_prompt_fire(&task, &session, &firing, now).await;
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
      self.update_status(&session.id, &task.name, status).await?;
      self.features.store.persist_and_broadcast(&session.id, event).await?;
      return Ok(TaskRunResult { session_id: session.id, thread_id: None });
    }
    let adapter = self.features.startup.require::<charliebot_tasks::execution::TaskExecutionAdapter>()?;
    let executor: Arc<dyn SequenceExecutor> = adapter.clone();
    let first_step =
      task.steps.as_ref().and_then(|steps| steps.first()).expect("a scheduled-step run has a first step");
    let configured_backend = steps::step_backend_id(first_step.backend.as_deref(), &session.backend);
    // A pool fire selects its member now, and the fire's child keeps the pool id as its backend while its run runs on
    // that member. A pool with no selectable member ends the fire here, with the error posted to the bound session.
    let session_backend = cfg.backends.pool(&configured_backend).is_some().then_some(configured_backend.as_str());
    let option = match steps::select_step_member(&self.features, &cfg, &configured_backend, &[])? {
      steps::StepMember::Selected(option) => option,
      steps::StepMember::Exhausted(message) => {
        self.post_fire_error(&session, &message, true).await?;
        self.record_bound_fire(&session, &task, now).await?;
        return Ok(TaskRunResult { session_id: session.id, thread_id: None });
      }
    };
    let backend = option.id.clone();
    let model = steps::option_model(&option);
    let (leaf, run, first_prompt) =
      self.register_steps_work(&task, &session, &firing, session_backend, &backend, model.as_deref()).await?;
    self.record_bound_fire(&session, &task, now).await?;
    if scheduled {
      let executor = Arc::clone(&executor);
      let leaf_id = leaf.id.clone();
      let run_id = run.id.clone();
      let task_name = task.name.clone();
      let owner_ref = firing_ref(&task, &firing);
      let boundary = crate::boundary::subscribe_boundary(&leaf_id, &owner_ref);
      tokio::spawn(async move {
        match executor.launch_and_settle(&leaf_id, &run_id, Some(first_prompt.as_str())).await {
          Ok(settlement) if settlement.withheld.is_none() => {
            // A steps fire keeps its task alive until the step boundary reports the chain settled.
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
    } else {
      executor.launch(&leaf.id, &run.id, Some(first_prompt.as_str())).await?;
    }
    Ok(TaskRunResult { session_id: session.id, thread_id: None })
  }

  /// A prompt task's fire: its trigger input goes to the bound session, which decides whether and when the round runs
  /// (S1). A template that cannot be read posts an error to the session and records a skip, and no worker starts.
  async fn admit_prompt_fire(
    &self,
    task: &ScheduledTaskConfig,
    session: &SessionMetadata,
    firing: &str,
    now: chrono::DateTime<chrono::Utc>,
  ) -> anyhow::Result<TaskRunResult> {
    let repo_root = self.features.startup.require::<RepoRoot>()?;
    match session_input::trigger_text(repo_root.path(), task) {
      Ok(text) => {
        let dispatcher = charliebot_tasks::dispatch::dispatcher(&self.features)?;
        dispatcher
          .admit_input(
            &session.id,
            event_types::SCHEDULED_TRIGGER,
            &text,
            ACTOR_SYSTEM,
            json!({(event_types::INPUT_ID): firing_ref(task, firing)}),
          )
          .await?;
        dispatcher.dispatch_pending(&session.id).await?;
        self.record_bound_fire(session, task, now).await?;
      }
      Err(unreadable) => {
        self
          .post_fire_error(
            session,
            &format!("scheduled task '{}' did not run: {}", task.name, unreadable.message()),
            false,
          )
          .await?;
        let timezone = task.timezone.parse::<chrono_tz::Tz>()?;
        let skipped_at = isoformat_text(&now.with_timezone(&timezone))?;
        self.record_skip(&session.id, &task.name, skipped_at, unreadable.reason()).await?;
      }
    }
    Ok(TaskRunResult { session_id: session.id.clone(), thread_id: None })
  }

  /// Post a fire's refusal to the bound session as an error, so the user reads why nothing ran. A quota refusal also
  /// carries the quota flag.
  async fn post_fire_error(
    &self,
    session: &SessionMetadata,
    message: &str,
    quota_exhausted: bool,
  ) -> anyhow::Result<()> {
    let mut event = json!({"type": event_types::ASSISTANT_ERROR, "content": message});
    if quota_exhausted {
      event[event_types::QUOTA_EXHAUSTED] = json!(true);
    }
    self.features.store.persist_and_broadcast(&session.id, event).await?;
    Ok(())
  }

  /// Register the worker leaf and its run of one steps fire, whose chain starts with the first step's prompt.
  #[allow(clippy::too_many_arguments)]
  async fn register_steps_work(
    &self,
    task: &ScheduledTaskConfig,
    parent: &SessionMetadata,
    firing: &str,
    session_backend: Option<&str>,
    backend: &str,
    model: Option<&str>,
  ) -> anyhow::Result<(SessionMetadata, RunRecord, String)> {
    if parent.profile != TaskProfile::Manager {
      anyhow::bail!(
        "scheduled task '{}' cannot create a worker leaf under {}: the parent is not a manager task",
        task.name,
        parent.id
      );
    }
    let first_step =
      task.steps.as_ref().and_then(|steps| steps.first()).expect("a scheduled-step run has a first step");
    let prompt = first_step.prompt.clone().expect("validated scheduled steps have prompt text");
    let leaf = self
      .features
      .store
      .create_task(
        CreateTask {
          request_id: round_request_id(task, firing),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Worker,
          task: Some(TaskSpec {
            goal: format!("{} steps", task.name),
            repo_path: task.repo.clone(),
            base_branch: None,
            task_type: None,
            keep_worktree: false,
            cold_read: None,
            approver: TaskApprover::User,
            improve_host: None,
          }),
          name: Some(format!("{} · {}", task.name, firing)),
          backend: session_backend.map(str::to_string),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await?;
    if session_backend.is_some() {
      let guard = self.features.store.control_lock().await;
      self
        .features
        .store
        .update_task_slot_fields_locked(
          &guard,
          &leaf.id,
          charliebot_agents::model_pool::OWNER,
          Map::from_iter([(charliebot_agents::model_pool::POOL_MEMBER_FIELD.to_string(), json!(backend))]),
        )
        .await?;
    }
    let run_request = format!("{}:step:0", firing_ref(task, firing));
    let run_id = charliebot_store::control_events::stable_run_id(&leaf.id, &run_request);
    let record = RunRecord {
      id: run_id,
      session_id: leaf.id.clone(),
      kind: RunKind::ScheduledStep,
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
      sequence_ref: Some(SequenceRef {
        kind: "cron_steps".to_string(),
        owner_ref: firing_ref(task, firing),
        position: 0,
      }),
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let record = {
      let guard = self.features.store.control_lock().await;
      self.features.store.register_run_locked(&guard, &leaf.id, record, Some(prompt.as_str())).await?
    };
    Ok((leaf, record, prompt))
  }

  /// The session a task's file binds, which must exist. A prompt task's session must be a manager.
  async fn bound_session(&self, task: &ScheduledTaskConfig) -> anyhow::Result<SessionMetadata> {
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
    if task.is_prompt() && meta.profile != TaskProfile::Manager {
      anyhow::bail!("scheduled task '{}' binds task {session_id}, which is not a manager", task.name);
    }
    Ok(meta)
  }

  /// The bound session of a fire that runs: it exists, is a manager when the task is a prompt task, and is open.
  async fn fireable_binding(&self, task: &ScheduledTaskConfig) -> anyhow::Result<SessionMetadata> {
    let meta = self.bound_session(task).await?;
    if self.features.store.task_state(&meta.id)? != "open" {
      anyhow::bail!("scheduled task '{}' bound task {} is archived", task.name, meta.id);
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
    let now = isoformat_text(&now.with_timezone(&timezone))?;
    update_task_slot(
      &self.features,
      &session.id,
      &task.name,
      json!({LAST_SCHEDULED_RUN: now, LAST_SCHEDULED_CRON: task.cron}),
    )
    .await?;
    Ok(())
  }

  async fn update_status(&self, session_id: &str, task_name: &str, status: &str) -> anyhow::Result<()> {
    update_task_slot(&self.features, session_id, task_name, json!({LAST_RUN_STATUS: status})).await
  }

  async fn run_now(&self, name: &str) -> Result<TaskRunResult, RunError> {
    let cfg = self.features.config.get().map_err(|error| RunError::Failed(error.to_string()))?;
    let snapshot = loader::load_snapshot(&self.features, &cfg).map_err(|error| RunError::Failed(error.to_string()))?;
    let task =
      snapshot.tasks.into_iter().find(|task| task.name == name).ok_or_else(|| RunError::Missing(name.to_string()))?;
    // A prompt task's input queues behind the turn that runs now, so only a steps task refuses a round in flight.
    if let Some(session_id) = task.session_id.as_deref()
      && !task.is_prompt()
      && let Some(round) = rounds::round_in_flight(&self.features, &task, session_id, &cfg)
        .map_err(|error| RunError::Failed(format!("{error:#}")))?
    {
      return Err(RunError::RoundInFlight { task: name.to_string(), round });
    }
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
  /// A round of the task is still in flight; `round` names its worker session.
  RoundInFlight { task: String, round: String },
  /// The task fire failed visibly.
  Failed(String),
}

/// One task fired now through the scheduler registered at startup.
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
      scheduler.report_machinery_literals().await;
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

/// A string key of one task's entry in the session's schedule slots.
fn task_slot_string(session: &SessionMetadata, task_name: &str, key: &str) -> Option<String> {
  session.extra.get(TASK_SCHEDULE_SLOTS)?.get(task_name)?.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Merge `values` into one task's entry of the session's schedule slots. The other tasks bound to the session keep
/// their entries.
async fn update_task_slot(
  features: &FeatureContext,
  session_id: &str,
  task_name: &str,
  values: Value,
) -> anyhow::Result<()> {
  let values = values.as_object().cloned().ok_or_else(|| anyhow::anyhow!("task slot updates must be an object"))?;
  features.store.update_slot_entry_fields(session_id, "cron", TASK_SCHEDULE_SLOTS, task_name, values).await?;
  Ok(())
}

async fn wait_for_steps_boundary(mut boundary: tokio::sync::watch::Receiver<crate::boundary::BoundaryNotice>) {
  loop {
    let notice = *boundary.borrow_and_update();
    if notice.state.is_settled() {
      return;
    }
    if boundary.changed().await.is_err() {
      return;
    }
  }
}

/// The next cron occurrence in the configured time zone.
pub(super) fn next_fire_iso(expression: &str, timezone: &str, now_utc: UtcDateTime) -> anyhow::Result<String> {
  let timezone = timezone.parse::<chrono_tz::Tz>()?;
  let inner = now_utc.into_inner();
  let now = chrono::DateTime::<chrono::Utc>::from_timestamp(inner.unix_timestamp(), inner.nanosecond())
    .ok_or_else(|| anyhow::anyhow!("UTC timestamp is outside the supported range"))?;
  isoformat_text(&CronExpression::parse(expression)?.next_after(now.with_timezone(&timezone))?)
}

/// The open sessions that an enabled task binds and that hold a next fire at `now`. The task files load once per call,
/// and a file that fails to load is logged as the tick logs it. A task whose bound session is archived holds no fire,
/// since the tick skips every such fire; a task whose binding or cron fails is logged and holds none.
pub(crate) fn sessions_with_next_fire(
  features: &Arc<FeatureContext>,
  now: UtcDateTime,
) -> anyhow::Result<HashSet<String>> {
  let cfg = features.config.get()?;
  let snapshot = loader::load_snapshot(features, &cfg)?;
  for error in &snapshot.errors {
    tracing::warn!(task = %error.name, error = %error.error, "scheduler_invalid_task_skipped");
  }
  let mut held = HashSet::new();
  for task in snapshot.tasks.iter().filter(|task| task.enabled) {
    let Some(session_id) = task.session_id.as_deref() else { continue };
    match bound_fire_holds(features, task, session_id, now) {
      Ok(true) => {
        held.insert(session_id.to_string());
      }
      Ok(false) => {}
      Err(error) => tracing::error!(task = %task.name, error = %error, "scheduler_task_error"),
    }
  }
  Ok(held)
}

/// Whether `task`, bound to the open session `session_id`, has a next fire after `now`.
fn bound_fire_holds(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  session_id: &str,
  now: UtcDateTime,
) -> anyhow::Result<bool> {
  if features.store.task_state(session_id)? != "open" {
    return Ok(false);
  }
  next_fire_iso(&task.cron, &task.timezone, now)?;
  Ok(true)
}

fn isoformat_text<Tz>(moment: &chrono::DateTime<Tz>) -> anyhow::Result<String>
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
  let mut data = loader::read_task_file(&path).map_err(|error| anyhow::anyhow!(error.message_without_path()))?;
  data.insert(key.to_string(), value);
  loader::write_task_file(&path, &Value::Object(data))?;
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

/// The next-fire search horizon in calendar days: eight years of 366 days.
const NEXT_FIRE_HORIZON_DAYS: i64 = 8 * 366;

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
    // The next fire is the earliest matching local time on the UTC minute grid. The scan stops at the eight-year
    // horizon (`NEXT_FIRE_HORIZON_DAYS`); an expression with no fire inside it is an error.
    self.next_after_with_work(base).0
  }

  /// The next fire and the work its search did, reported on the error path too.
  fn next_after_with_work<Tz: chrono::TimeZone + Clone>(
    &self,
    base: chrono::DateTime<Tz>,
  ) -> (anyhow::Result<chrono::DateTime<Tz>>, CronSearchWork) {
    let base_utc = base.with_timezone(&chrono::Utc);
    let timezone = base.timezone().clone();
    let start = base_utc.timestamp().div_euclid(60) * 60 + 60;
    let end = start + NEXT_FIRE_HORIZON_DAYS * 24 * 60 * 60;
    let first_date = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone)
      .date_naive();
    let start_local = chrono::DateTime::from_timestamp(start, 0)
      .expect("a bounded Unix minute is representable")
      .with_timezone(&timezone);
    let mut work = CronSearchWork::default();
    let mut best: Option<chrono::DateTime<Tz>> = None;

    for day_offset in 0..=(NEXT_FIRE_HORIZON_DAYS + 3) {
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
        return (Ok(next.clone()), work);
      }
      if !self.date_matches(date) {
        continue;
      }
      if date == start_local.date_naive() && self.minute.values.len() == 60 && self.hour.values.len() == 24 {
        let first = chrono::DateTime::from_timestamp(start, 0)
          .expect("a bounded Unix minute is representable")
          .with_timezone(&timezone);
        return (Ok(first), work);
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
    let next = best.ok_or_else(|| anyhow::anyhow!("cron expression has no fire within eight years"));
    (next, work)
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
  use charliebot_core::event_types;
  use charliebot_registry::context::FeatureContext;
  use charliebot_store::compact::CompactValue;
  use charliebot_store::metadata::{
    LAST_RUN_STATUS, LAST_SCHEDULED_CRON, LAST_SCHEDULED_RUN, SCHEDULED_TASK, SessionMetadata, SessionStatus,
    TASK_SCHEDULE_SLOTS, TaskProfile, UtcDateTime,
  };
  use charliebot_store::run_records::{HostBoot, RunDisplayState, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
  use charliebot_store::{CreateTask, TaskCaller};
  use charliebot_tasks::git::git_worktree_dir_name;
  use chrono::{TimeZone, Timelike, Utc};
  use serde_json::{Map, Value, json};
  use std::path::Path;
  use std::sync::Arc;

  use super::{CronExpression, CronScheduler, isoformat_text};
  use crate::config::{ScheduledTaskConfig, StepConfig, TaskMode};
  use crate::test_support::{
    AUTO_POOL_ID, FakeGit, GIT_PROGRAM_LOCK, auto_manager, bound_round_leaf, feature_context,
    feature_context_with_auto, feature_context_with_login_hosts, host_probe_count, install_execution,
    keyed_child_manager, keyed_manager, manager, set_host_login, write_prompted_task, write_task,
  };
  use charliebot_registry::registry::HostLogin;
  use charliebot_tasks::test_support::process_guard::ProcessGroupGuard;

  /// One task's value under `key` in the session's schedule slots.
  fn task_slot(meta: &SessionMetadata, task_name: &str, key: &str) -> Option<Value> {
    meta.extra.get(TASK_SCHEDULE_SLOTS)?.get(task_name)?.get(key).cloned()
  }

  /// Seed one task's entry in a session's schedule slots, through the write the scheduler itself makes.
  async fn seed_task_slot(features: &FeatureContext, session_id: &str, task_name: &str, values: Value) {
    let values = values.as_object().cloned().expect("a seeded task slot is an object");
    features.store.update_slot_entry_fields(session_id, "cron", TASK_SCHEDULE_SLOTS, task_name, values).await.unwrap();
  }

  /// Write session-level keys that the store does not register into a session's metadata file. The scheduler must
  /// ignore them, so the tests seed them this way.
  fn write_legacy_session_slots(features: &FeatureContext, session_id: &str, legacy: &[(&str, Value)]) {
    let path = features.store.metadata_path(session_id);
    let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for (key, value) in legacy {
      meta[*key] = value.clone();
    }
    std::fs::write(&path, serde_json::to_string(&meta).unwrap()).unwrap();
  }

  #[test]
  fn cron_expression_uses_minute_grid_and_croniter_day_union() {
    let expression = CronExpression::parse("0 8 1 * mon").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2026-06-08T08:00:00+00:00");
  }

  #[test]
  fn sparse_cron_checks_calendar_dates_within_the_eight_year_horizon() {
    let expression = CronExpression::parse("0 0 1 * *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

    let (next, work) = expression.next_after_with_work(base);
    let next = next.unwrap();

    assert_eq!(next.to_rfc3339(), "2026-02-01T00:00:00+00:00");
    assert!(work.calendar_days <= 33, "{work:?}");
    assert_eq!(work.local_candidates, 1);

    // Jan 1 is 364 days out, inside the eight-year horizon.
    let yearly = CronExpression::parse("0 0 1 1 *").unwrap();
    assert_eq!(yearly.next_after(base).unwrap().to_rfc3339(), "2027-01-01T00:00:00+00:00");

    // Feb 30 never occurs: the search stops at the horizon, within its calendar-day cap.
    let never = CronExpression::parse("0 0 30 2 *").unwrap();
    let (result, work) = never.next_after_with_work(base);
    assert!(result.unwrap_err().to_string().contains("no fire within eight years"));
    assert!(work.calendar_days <= 8 * 366 + 4, "{work:?}");
  }

  #[test]
  fn next_fire_more_than_49_days_out_resolves_in_the_task_timezone() {
    let timezone = "America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap();

    let expression = CronExpression::parse("0 3 1 1 *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 11, 8, 3, 6, 0).unwrap().with_timezone(&timezone);
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2027-01-01T03:00:00-08:00");

    let expression = CronExpression::parse("0 17 15 12 *").unwrap();
    let base = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap().with_timezone(&timezone);
    assert_eq!(expression.next_after(base).unwrap().to_rfc3339(), "2026-12-15T17:00:00-08:00");
  }

  #[test]
  fn cron_stamps_use_microseconds_and_keep_the_task_timezone_offset() {
    let timezone = "America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap();
    let whole_seconds = timezone.with_ymd_and_hms(2026, 10, 20, 2, 26, 6).single().unwrap();
    let fractional = whole_seconds.with_nanosecond(685_829_123).unwrap();

    assert_eq!(isoformat_text(&whole_seconds).unwrap(), "2026-10-20T02:26:06-07:00");
    assert_eq!(isoformat_text(&fractional).unwrap(), "2026-10-20T02:26:06.685829-07:00");
  }

  #[tokio::test]
  async fn tick_binds_disabled_tasks_without_projects_once_and_reuses_the_stable_node() {
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
    assert_eq!(first_node.extra.get(SCHEDULED_TASK), Some(&Value::Null));
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

  /// A file-based task in `mode: master` with no `session_id` loads, and one
  /// tick auto-binds it: the scheduler creates the manager session on the ordinary new-session backend, leaves it
  /// ungrouped, and writes its id back into the host file.
  #[tokio::test]
  async fn tick_binds_task_with_mode_and_no_session_id_after_one_tick() {
    let (home, features) = feature_context();
    let path = write_prompted_task(home.path(), "proactive-agent", &json!({"cron":"0 7 * * *","mode":"master"}));
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick().await.unwrap();

    let bound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let session_id = bound["session_id"].as_str().expect("the task has a session_id after one tick");
    assert!(!session_id.is_empty());
    let node = features.store.get_session(session_id).await.unwrap().expect("the bound session exists");
    assert_eq!(node.profile, TaskProfile::Manager);
    let expected_backend = features.config.get().unwrap().backends.new_session_backend().unwrap();
    assert_eq!(node.backend, expected_backend);
    assert_eq!(node.group.as_deref(), None);
    assert_eq!(features.store.task_state(session_id).unwrap(), "open");

    // A second tick reuses the written binding instead of creating another node.
    scheduler.tick().await.unwrap();
    let rebound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(rebound["session_id"].as_str(), Some(session_id));
  }

  /// A file-based task carrying `session_id: ""` loads as unbound, and one tick
  /// auto-binds it: the empty value is dropped so the node is created and the
  /// written binding replaces it in the host file.
  #[tokio::test]
  async fn tick_binds_task_with_empty_session_id_after_one_tick() {
    let (home, features) = feature_context();
    let path =
      write_prompted_task(home.path(), "proactive-agent", &json!({"cron":"0 7 * * *","mode":"master","session_id":""}));
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick().await.unwrap();

    let bound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let session_id = bound["session_id"].as_str().expect("the task has a session_id after one tick");
    assert!(!session_id.is_empty());
    let node = features.store.get_session(session_id).await.unwrap().expect("the bound session exists");
    assert_eq!(node.profile, TaskProfile::Manager);
    assert_eq!(node.group.as_deref(), None);
    assert_eq!(features.store.task_state(session_id).unwrap(), "open");

    // A second tick reuses the written binding instead of creating another node.
    scheduler.tick().await.unwrap();
    let rebound: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(rebound["session_id"].as_str(), Some(session_id));
  }

  #[tokio::test]
  async fn scheduler_tick_runs_handlers_and_updates_the_shared_session_views() {
    let (home, features) = feature_context();
    let path = write_task(
      home.path(),
      "synthetic-handler",
      &json!({"cron":"0 8 * * *","handler":"synthetic_handler","enabled":false}),
    );
    let scheduler = CronScheduler::new(Arc::clone(&features));

    // Both ticks run at 08:00:30 of the task's default timezone on a fixed date. No wall clock then decides
    // whether the first tick fires the 0 8 * * * occurrence.
    let clock = due_clock();
    scheduler.tick_at(clock).await.unwrap();
    let mut binding: Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    binding["enabled"] = json!(true);
    std::fs::write(&path, crate::yaml::dump(&binding).unwrap()).unwrap();
    let session_id = binding["session_id"].as_str().unwrap();
    seed_task_slot(
      &features,
      session_id,
      "synthetic-handler",
      json!({LAST_SCHEDULED_RUN: "2026-01-01T03:00:00+00:00"}),
    )
    .await;

    scheduler.tick_at(clock).await.unwrap();

    let meta = features.store.get_session(session_id).await.unwrap().unwrap();
    assert_eq!(task_slot(&meta, "synthetic-handler", LAST_RUN_STATUS), Some(json!("success")));
    assert!(task_slot(&meta, "synthetic-handler", LAST_SCHEDULED_RUN).is_some());
    let events = features.store.load_chat_events_sync(session_id).unwrap();
    assert!(events.iter().any(|event| event["type"].as_str() == Some(charliebot_core::event_types::HANDLER_RESULT)));
    let projection = features.store.get_message_projection(session_id).unwrap().unwrap();
    assert_eq!(projection.history().len(), 1);
    assert!(features.store.child_ids(session_id).unwrap().is_empty());
  }

  /// 08:00:30 of a fixed date in the task's default timezone: the minute of a `0 8 * * *` task, once the task is due.
  fn due_clock() -> chrono::DateTime<Utc> {
    let clock_timezone = crate::config::DEFAULT_TIMEZONE.parse::<chrono_tz::Tz>().unwrap();
    chrono::NaiveDate::from_ymd_opt(2026, 1, 5)
      .unwrap()
      .and_hms_opt(8, 0, 30)
      .unwrap()
      .and_local_timezone(clock_timezone)
      .single()
      .unwrap()
      .with_timezone(&Utc)
  }

  /// Writes a master-mode task that sends `prompt` at 08:00, binds it with one tick, then ticks at its due minute with
  /// the task enabled. Returns the bound session's id.
  async fn fire_prompt_task_at_due_minute(
    home: &Path,
    features: &Arc<FeatureContext>,
    name: &str,
    prompt: &Path,
  ) -> String {
    install_execution(features).await;
    let body =
      json!({"cron": "0 8 * * *", "mode": "master", "prompt_file": prompt.display().to_string(), "enabled": false});
    let path = write_task(home, name, &body);
    let scheduler = CronScheduler::new(Arc::clone(features));
    scheduler.tick_at(due_clock()).await.unwrap();
    let mut binding: Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    binding["enabled"] = json!(true);
    std::fs::write(&path, crate::yaml::dump(&binding).unwrap()).unwrap();
    let session_id = binding["session_id"].as_str().unwrap().to_string();
    seed_task_slot(features, &session_id, name, json!({LAST_SCHEDULED_RUN: "2026-01-01T03:00:00+00:00"})).await;
    scheduler.tick_at(due_clock()).await.unwrap();
    session_id
  }

  /// The content of each error the bound session has been posted, in order.
  fn posted_errors(features: &FeatureContext, session_id: &str) -> Vec<String> {
    features
      .store
      .load_chat_events_sync(session_id)
      .unwrap()
      .iter()
      .filter(|event| event["type"].as_str() == Some(event_types::ASSISTANT_ERROR))
      .filter_map(|event| event["content"].as_str().map(str::to_string))
      .collect()
  }

  /// A due fire whose prompt file holds a hardcoded backend choice is refused: the task records failed, the bound session
  /// is told the file and line, and no trigger input is queued.
  #[tokio::test]
  async fn a_due_fire_whose_prompt_file_holds_a_backend_literal_is_refused_and_queues_no_input() {
    let (home, features) = feature_context();
    let prompt = home.path().join("literal-prompt.md");
    std::fs::write(&prompt, "Review the diff on claude-opus-5-5.\n").unwrap();

    let session_id = fire_prompt_task_at_due_minute(home.path(), &features, "literal-task", &prompt).await;

    let meta = features.store.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(task_slot(&meta, "literal-task", LAST_RUN_STATUS), Some(json!("failed")));
    let errors = posted_errors(&features, &session_id);
    let expected = format!("{}:1 model-id claude-opus-5-5", prompt.display());
    assert!(errors.iter().any(|error| error.contains(&expected)), "{errors:?}");
    let events = features.store.load_chat_events_sync(&session_id).unwrap();
    assert!(!events.iter().any(|event| event["type"].as_str() == Some(event_types::SCHEDULED_TRIGGER)));
  }

  /// A due fire whose prompt file is clean runs: its trigger input is queued to the bound session and nothing is refused.
  #[tokio::test]
  async fn a_due_fire_whose_prompt_file_is_clean_queues_its_trigger() {
    let (home, features) = feature_context();
    let prompt = home.path().join("clean-prompt.md");
    std::fs::write(&prompt, "Review the diff.\n").unwrap();

    let session_id = fire_prompt_task_at_due_minute(home.path(), &features, "clean-task", &prompt).await;

    assert!(posted_errors(&features, &session_id).iter().all(|error| !error.contains("hardcoded")));
    let events = features.store.load_chat_events_sync(&session_id).unwrap();
    assert!(events.iter().any(|event| event["type"].as_str() == Some(event_types::SCHEDULED_TRIGGER)));
  }

  /// A hardcoded backend choice in the profile's machinery refuses every due fire, whatever the task's own prompt says.
  #[tokio::test]
  async fn a_due_fire_is_refused_by_a_backend_literal_in_the_profile_machinery() {
    let (home, features) = feature_context();
    std::fs::create_dir_all(home.path().join("scripts")).unwrap();
    std::fs::write(home.path().join("scripts/run.sh"), "echo start\nmodel=claude-opus-5-5\n").unwrap();
    let prompt = home.path().join("clean-prompt.md");
    std::fs::write(&prompt, "Review the diff.\n").unwrap();

    let session_id = fire_prompt_task_at_due_minute(home.path(), &features, "machinery-task", &prompt).await;

    let meta = features.store.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(task_slot(&meta, "machinery-task", LAST_RUN_STATUS), Some(json!("failed")));
    let errors = posted_errors(&features, &session_id);
    let expected = format!("{}:2 model-id claude-opus-5-5", home.path().join("scripts/run.sh").display());
    assert!(errors.iter().any(|error| error.contains(&expected)), "{errors:?}");
  }

  /// The server start reports a backend literal in the profile's machinery to the bound session of each enabled task,
  /// and posts nothing when the machinery is clean.
  #[tokio::test]
  async fn the_server_start_reports_a_machinery_literal_to_each_enabled_bound_session() {
    let (home, features) = feature_context();
    write_task(home.path(), "startup-task", &json!({"cron": "0 9 * * *", "handler": "synthetic_handler"}));
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick_at(due_clock()).await.unwrap();
    let session_id = {
      let binding: Value =
        serde_yaml::from_str(&std::fs::read_to_string(home.path().join("config.d/cron.d/startup-task.yaml")).unwrap())
          .unwrap();
      binding["session_id"].as_str().unwrap().to_string()
    };

    scheduler.report_machinery_literals().await;
    assert!(posted_errors(&features, &session_id).is_empty(), "a clean machinery posts nothing");

    std::fs::create_dir_all(home.path().join("skills")).unwrap();
    std::fs::write(home.path().join("skills/guide.md"), "Use claude-opus-5-5 for the review.\n").unwrap();
    scheduler.report_machinery_literals().await;
    let errors = posted_errors(&features, &session_id);
    let expected = format!("{}:1 model-id claude-opus-5-5", home.path().join("skills/guide.md").display());
    assert!(errors.iter().any(|error| error.contains(&expected)), "{errors:?}");
  }

  /// The task names of a session's handler results, in the order the scheduler wrote them.
  fn handler_result_tasks(features: &FeatureContext, session_id: &str) -> Vec<String> {
    features
      .store
      .load_chat_events_sync(session_id)
      .unwrap()
      .iter()
      .filter(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(charliebot_core::event_types::HANDLER_RESULT)
      })
      .filter_map(|event| event.get("task").and_then(CompactValue::as_str).map(str::to_string))
      .collect()
  }

  /// Tick `ticks` times, one minute apart from `first`, and return each handler fire as its task and tick time.
  async fn tick_each_minute(
    scheduler: &CronScheduler,
    features: &FeatureContext,
    session_id: &str,
    first: chrono::DateTime<Utc>,
    ticks: i64,
  ) -> Vec<(String, chrono::DateTime<Utc>)> {
    let mut fires = Vec::new();
    let mut seen = 0;
    for minute in 0..ticks {
      let now = first + chrono::Duration::minutes(minute);
      scheduler.tick_at(now).await.unwrap();
      let tasks = handler_result_tasks(features, session_id);
      fires.extend(tasks[seen..].iter().map(|task| (task.clone(), now)));
      seen = tasks.len();
    }
    fires
  }

  /// Two enabled tasks bound to one session each keep their own schedule slot: each fires at its own due minutes, and
  /// the ticks never read one task's cron as a change to the other's.
  #[tokio::test]
  async fn two_tasks_bound_to_one_session_each_fire_at_their_own_due_minutes() {
    let (home, features) = feature_context();
    let node = manager(&features, "synthetic-shared-node").await.unwrap();
    for (name, cron) in [("synthetic-morning", "0 8 * * *"), ("synthetic-afternoon", "30 9 * * *")] {
      write_task(
        home.path(),
        name,
        &json!({"cron": cron, "handler": "synthetic_handler", "session_id": node.id, "timezone": "UTC", "enabled": true}),
      );
    }
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let day = |hour, minute, second| Utc.with_ymd_and_hms(2026, 1, 5, hour, minute, second).unwrap();

    // 07:59:30 to 10:29:30, one tick a minute: each task's due minute is the tick at :30 of that minute.
    let fires = tick_each_minute(&scheduler, &features, &node.id, day(7, 59, 30), 150).await;

    assert_eq!(
      fires,
      vec![("synthetic-morning".to_string(), day(8, 0, 30)), ("synthetic-afternoon".to_string(), day(9, 30, 30))]
    );
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(task_slot(&meta, "synthetic-morning", LAST_SCHEDULED_CRON), Some(json!("0 8 * * *")));
    assert_eq!(task_slot(&meta, "synthetic-afternoon", LAST_SCHEDULED_CRON), Some(json!("30 9 * * *")));
  }

  /// A session that holds only session-level slot keys gives its task no entry of its own. The task's first tick
  /// neither replays the runs those keys show as missed nor skips its next due minute.
  #[tokio::test]
  async fn a_task_with_only_the_old_session_slot_neither_replays_nor_skips_its_next_due_run() {
    let (home, features) = feature_context();
    let node = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-old-slot-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("synthetic-old-slot".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    write_legacy_session_slots(
      &features,
      &node.id,
      &[
        (LAST_SCHEDULED_RUN, json!("2026-01-01T08:00:00+00:00")),
        (LAST_SCHEDULED_CRON, json!("0 8 * * *")),
        (LAST_RUN_STATUS, json!("success")),
      ],
    );
    write_task(
      home.path(),
      "synthetic-old-slot",
      &json!({"cron": "0 8 * * *", "handler": "synthetic_handler", "session_id": node.id, "timezone": "UTC", "enabled": true}),
    );
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let day = |hour, minute, second| Utc.with_ymd_and_hms(2026, 1, 5, hour, minute, second).unwrap();

    let fires = tick_each_minute(&scheduler, &features, &node.id, day(7, 59, 30), 60).await;

    assert_eq!(fires, vec![("synthetic-old-slot".to_string(), day(8, 0, 30))]);
  }

  #[tokio::test]
  async fn tick_archives_the_old_active_cron_session_when_it_binds_a_new_node() {
    let (home, features) = feature_context();
    let mut legacy_extra = Map::new();
    legacy_extra.insert(SCHEDULED_TASK.to_string(), json!("legacy-task"));
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
    write_legacy_session_slots(&features, &legacy.id, &[(LAST_SCHEDULED_RUN, json!("2026-10-01T08:00:00-07:00"))]);
    assert_eq!(legacy.extra.get(SCHEDULED_TASK).and_then(Value::as_str), Some("legacy-task"));
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
    assert_eq!(bound.extra.get(SCHEDULED_TASK), Some(&Value::Null));
    assert_eq!(task_slot(&bound, "legacy-task", LAST_SCHEDULED_RUN), None, "the bound node adopts no old slot");
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
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      allow_failure: false,
      session_id: Some(parent.id.clone()),
      mode: Some(TaskMode::Worker),
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
      improve_host: None,
    };
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let (leaf, run, prompt) = scheduler
      .register_steps_work(&task, &parent, "2026-10-21T03:00:00-07:00", None, "synthetic", None)
      .await
      .unwrap();

    assert_eq!(run.kind, RunKind::ScheduledStep);
    assert_eq!(prompt, "Select candidates.");
    assert_eq!(std::fs::read_to_string(run.task_spec_ref.as_deref().unwrap()).unwrap(), "Select candidates.");
    let expected_hash = charliebot_store::control_events::sha256_hex("Select candidates.");
    assert_eq!(run.task_spec_hash.as_deref(), Some(expected_hash.as_str()));
    assert_eq!(features.store.read_run_sync(&leaf.id, &run.id).unwrap().unwrap().task_spec_ref, run.task_spec_ref);
  }

  /// A fire without `mode`, `handler` or `steps` is a work run on a type-less worker leaf, and no reviewer run follows
  /// it. The leaf's harness then must not promise one: it carries the type-less contract instead of the implement
  /// STOP line (`spawner_prompt::reviewer_run_follows` is the one decision both sides read).
  #[tokio::test]
  async fn auto_bind_replays_each_durable_migration_boundary_to_one_node_and_binding() {
    for fail_after in ["create", "write"] {
      let (home, features) = feature_context();
      let task_name = format!("synthetic-replay-{fail_after}");
      let mut extra = Map::new();
      extra.insert(SCHEDULED_TASK.to_string(), json!(task_name));
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
      write_legacy_session_slots(&features, &legacy.id, &[(LAST_SCHEDULED_RUN, json!("2026-10-19T09:00:00+00:00"))]);
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
      if fail_after == "write" {
        let mut body = serde_yaml::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        body.as_object_mut().unwrap().insert("session_id".to_string(), json!(node.id));
        std::fs::write(&path, crate::yaml::dump(&body).unwrap()).unwrap();
      }

      CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

      let replayed: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
      assert_eq!(replayed["session_id"].as_str(), Some(node.id.as_str()), "after {fail_after}");
      let stored_node = features.store.get_session(&node.id).await.unwrap().unwrap();
      assert_eq!(task_slot(&stored_node, &task_name, LAST_SCHEDULED_RUN), None, "after {fail_after}");
      assert_eq!(features.store.get_session(&legacy.id).await.unwrap().unwrap().status, SessionStatus::Archived);
      let active = features.store.list_sessions(Some(SessionStatus::Active), None, None).unwrap();
      assert_eq!(active.rows.iter().filter(|session| session.name == task_name).count(), 1, "after {fail_after}");
    }
  }

  #[tokio::test]
  async fn scheduler_sweep_archives_a_busy_legacy_cron_session() {
    let (home, features) = feature_context();
    let task_name = "synthetic-sweep-task";
    let node = crate::test_support::manager(&features, "synthetic-sweep-node").await.unwrap();
    let mut extra = Map::new();
    extra.insert(SCHEDULED_TASK.to_string(), json!(task_name));
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
  async fn an_upcoming_occurrence_preserves_last_run_and_does_not_catch_up() {
    let (home, features) = feature_context();
    let node = crate::test_support::manager(&features, "synthetic-no-catchup-node").await.unwrap();
    let last_run = UtcDateTime::now().to_isoformat_text();
    seed_task_slot(
      &features,
      &node.id,
      "synthetic-no-catchup",
      json!({LAST_SCHEDULED_RUN: last_run, LAST_SCHEDULED_CRON: "0 0 1 1 *"}),
    )
    .await;
    write_task(
      home.path(),
      "synthetic-no-catchup",
      &json!({"cron":"0 0 1 1 *","handler":"synthetic_handler","session_id":node.id,"enabled":true}),
    );

    CronScheduler::new(Arc::clone(&features)).tick().await.unwrap();

    let fresh = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(task_slot(&fresh, "synthetic-no-catchup", LAST_SCHEDULED_RUN), Some(json!(last_run)));
    assert_eq!(task_slot(&fresh, "synthetic-no-catchup", LAST_RUN_STATUS), None);
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
    std::fs::write(&path, crate::yaml::dump(&unbound).unwrap()).unwrap();
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

  fn worker_task(name: &str, node_id: &str, repo: Option<&Path>) -> ScheduledTaskConfig {
    ScheduledTaskConfig {
      name: name.to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: repo.map(|path| path.display().to_string()),
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      allow_failure: false,
      session_id: Some(node_id.to_string()),
      mode: Some(TaskMode::Worker),
      prompt: Some("Synthetic scheduled work".to_string()),
      handler: None,
      steps: None,
      improve_host: None,
    }
  }

  /// One task fired through the scheduler's launch path, returned with its worker leaf and the run once `done` holds.
  /// A steps task fires through `execute_task`, scheduled or run now as `scheduled` says. A prompt task's round starts
  /// through the bound delegation, which has no scheduled form, so `scheduled` does not apply to it.
  async fn fire_until(
    features: &Arc<FeatureContext>,
    task: ScheduledTaskConfig,
    scheduled: bool,
    done: fn(&FeatureContext, &str, &RunRecord) -> bool,
  ) -> (String, RunRecord) {
    let leaf_id = if task.steps.is_some() {
      let node_id = task.session_id.clone().expect("the synthetic task is bound to a node");
      let scheduler = CronScheduler::new(Arc::clone(features));
      let cfg = (*features.config.get().unwrap()).clone();
      scheduler
        .execute_task(task, Some("2026-10-21T03:00:00-07:00".to_string()), scheduled, cfg, Utc::now())
        .await
        .unwrap();
      features.store.child_ids(&node_id).unwrap().first().cloned().expect("the fire created a worker leaf")
    } else {
      bound_round_leaf(features, &task, "").await.0.id
    };
    let guard = ProcessGroupGuard::new();
    let mut tracked = false;
    let run = tokio::time::timeout(std::time::Duration::from_secs(15), async {
      loop {
        if let Some(run) = features.store.list_run_records_sync(&leaf_id).unwrap().first() {
          if !tracked && let Some(pid) = run.pid {
            guard.track(pid);
            tracked = true;
          }
          if done(features, &leaf_id, run) {
            break run.clone();
          }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("the fired run reaches the awaited state");
    (leaf_id, run)
  }

  /// One task fired, returned with its worker leaf and the run once the run reaches a terminal fact.
  async fn fire_and_settle(
    features: &Arc<FeatureContext>,
    task: ScheduledTaskConfig,
    scheduled: bool,
  ) -> (String, RunRecord) {
    fire_until(features, task, scheduled, |features, leaf_id, run| {
      let facts = features.store.load_all_facts(leaf_id).unwrap();
      charliebot_store::run_records::terminal_outcome(&facts, &run.id).unwrap().is_some()
    })
    .await
  }

  /// One task fired, returned with its worker leaf and the run once the launch text is written.
  /// The run keeps going after this returns, so a caller asserts only what the launch records.
  async fn fire_and_launch(
    features: &Arc<FeatureContext>,
    task: ScheduledTaskConfig,
    scheduled: bool,
  ) -> (String, RunRecord) {
    fire_until(features, task, scheduled, |features, leaf_id, run| {
      features.store.run_dir(leaf_id, &run.id).join(charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME).is_file()
    })
    .await
  }

  fn launch_text(features: &FeatureContext, session_id: &str, run: &RunRecord) -> String {
    std::fs::read_to_string(
      features.store.run_dir(session_id, &run.id).join(charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME),
    )
    .unwrap()
  }

  async fn assert_repo_fire_prepares_a_worktree() {
    let _git_lock = GIT_PROGRAM_LOCK.lock().await;
    let git = FakeGit::install();
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let checkout_home = tempfile::tempdir().unwrap();
    let repo = checkout_home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let node = manager(&features, "synthetic-worktree-node").await.unwrap();
    let task = worker_task("synthetic-worktree-task", &node.id, Some(&repo));

    let (leaf_id, run) = fire_and_settle(&features, task, false).await;

    assert_eq!(run.kind, RunKind::Work);
    let worktree_path = run.worktree_path.clone().expect("a repo-backed work run records its worktree");
    let configured_worktree_dir = features.config.get().unwrap().paths.worktree_dir.clone();
    let worktree_dir = std::fs::canonicalize(&configured_worktree_dir).unwrap();
    assert!(Path::new(&worktree_path).starts_with(&worktree_dir), "{worktree_path}");
    let branch_name = run.branch_name.as_deref().expect("a repo-backed work run records its branch");
    assert!(branch_name.starts_with("charliebot/task-"), "{branch_name}");
    let launch = launch_text(&features, &leaf_id, &run);
    assert!(launch.contains(&worktree_path), "{launch}");
    assert!(launch.contains("Synthetic scheduled work"), "{launch}");
    let requested_worktree = Path::new(&configured_worktree_dir).join(git_worktree_dir_name(branch_name));
    let launch_calls = [
      "-c protocol.version=0 ls-remote --symref origin".to_string(),
      "rev-parse --verify --quiet refs/remotes/origin/main".to_string(),
      format!("worktree add -b {branch_name} {} origin/main", requested_worktree.display()),
    ];
    // Delivery removes the worktree after the terminal fact, so the snapshot may stop at any call of that cleanup.
    let cleanup_calls = [format!("worktree remove --force {worktree_path}"), "worktree prune".to_string()];
    let calls = git.calls();
    assert!(calls.starts_with(&launch_calls), "{calls:?}");
    assert!(cleanup_calls.starts_with(&calls[launch_calls.len()..]), "{calls:?}");
  }

  /// Test ①: a fire on a pool runs on the member the pool selects for the child, not the member the bound session
  /// recorded. The child keeps the pool id as its backend and records the selected member.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pool_fire_runs_on_the_member_the_pool_selects() {
    let (_home, features) = feature_context_with_auto(&["member-a", "member-b"]);
    install_execution(&features).await;
    let node = auto_manager(&features, "synthetic-auto-fire-node", "member-b").await.unwrap();
    let task = ScheduledTaskConfig {
      name: "synthetic-auto-fire".to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: None,
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      allow_failure: false,
      session_id: Some(node.id.clone()),
      mode: Some(TaskMode::Worker),
      prompt: Some("Summarize the queue.".to_string()),
      handler: None,
      steps: None,
      improve_host: None,
    };

    let (leaf_id, run) = fire_and_launch(&features, task, false).await;

    assert_eq!(run.backend.as_deref(), Some("member-a"), "the priority pool picks its first selectable member");
    let leaf = features.store.get_session(&leaf_id).await.unwrap().unwrap();
    assert_eq!(leaf.backend, AUTO_POOL_ID, "the child keeps the pool id");
    assert_eq!(leaf.extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD), Some(&json!("member-a")));
  }

  /// Test ②: a cron child on an Auto member that runs out of quota retries on the next member as one retry run. The
  /// child keeps the pool id and moves its own member, so the switch record sits in its own chat. The retry's input
  /// names the failed run's directory as an absolute path, and carries the summary from the run's events log.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_cron_child_quota_end_retries_on_the_next_auto_member() {
    let (_home, features) = feature_context_with_auto(&["member-a", "member-b"]);
    install_execution(&features).await;
    // The bound session holds member-b. The pool's subscription position starts at the list head, so the child takes
    // member-a.
    let node = auto_manager(&features, "synthetic-auto-child-quota-node", "member-b").await.unwrap();
    let task = ScheduledTaskConfig {
      name: "synthetic-auto-child-quota".to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: None,
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      allow_failure: false,
      session_id: Some(node.id.clone()),
      mode: Some(TaskMode::Worker),
      prompt: Some("Summarize the queue.".to_string()),
      handler: None,
      steps: None,
      improve_host: None,
    };
    let (leaf, run) = bound_round_leaf(&features, &task, "").await;
    assert_eq!(run.backend.as_deref(), Some("member-a"), "the round runs on the member the pool selects");
    // The child's run read a file before its quota error, so its events log holds the four summary parts.
    let events = [
      json!({"type":"assistant","message":{"content":[
        {"type":"text","text":"I read the entry point."},
        {"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"/synthetic/src/main.rs"}}
      ]}}),
      json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"fn main() {}"}]}}),
      json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"tool-2","name":"Bash","input":{"command":"cargo test -p cron"}}
      ]}}),
    ];
    let failed_dir = features.store.run_dir(&leaf.id, &run.id);
    std::fs::create_dir_all(&failed_dir).unwrap();
    let lines = events.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    std::fs::write(failed_dir.join(charliebot_store::run_records::RUN_EVENTS_NAME), format!("{lines}\n")).unwrap();
    let failed = RunOutcome { exit_code: Some(1), ..RunOutcome::new(RunOutcomeKind::Failed) };
    charliebot_tasks::dispatch::dispatcher(&features).unwrap().finish_run(&leaf.id, &run.id, failed).await.unwrap();
    let adapter = features.startup.require::<charliebot_tasks::execution::TaskExecutionAdapter>().unwrap();
    adapter.record_quota_failure_for_test(&leaf.id, &run.id, None);
    adapter.after_worker_run_for_test(&leaf.id, &run.id, "failed").await.unwrap();

    let runs = features.store.list_run_records_sync(&leaf.id).unwrap();
    let retries =
      runs.iter().filter(|candidate| candidate.retry_of_run_id.as_deref() == Some(run.id.as_str())).collect::<Vec<_>>();
    assert_eq!(retries.len(), 1, "one retry run: {runs:?}");
    let retry = retries[0];
    assert_eq!(retry.backend.as_deref(), Some("member-b"), "the next Auto member runs the retry");
    let retry_id = retry.id.clone();

    let switches = features
      .store
      .load_chat_events_sync(&leaf.id)
      .unwrap()
      .iter()
      .map(|event| event.to_value())
      .filter(|event| event["type"] == event_types::QUOTA_FAILOVER)
      .collect::<Vec<_>>();
    assert_eq!(switches.len(), 1, "one switch record in the child's chat: {switches:?}");
    assert_eq!(switches[0]["from"], "member-a");
    assert_eq!(switches[0]["to"], "member-b");
    assert_eq!(switches[0]["set"], AUTO_POOL_ID);
    assert_eq!(switches[0]["summary"], charliebot_tasks::handoff::SUMMARY_OK);
    let leaf_meta = features.store.get_session(&leaf.id).await.unwrap().unwrap();
    assert_eq!(leaf_meta.backend, AUTO_POOL_ID, "the child keeps the pool id");
    assert_eq!(leaf_meta.extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD), Some(&json!("member-b")));
    let node_meta = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(
      node_meta.extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD),
      Some(&json!("member-b")),
      "the child's switch leaves the bound session's member alone"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
      let facts = features.store.load_all_facts(&leaf.id).unwrap();
      if charliebot_store::run_records::terminal_outcome(&facts, &retry_id).unwrap().is_some() {
        break;
      }
      assert!(std::time::Instant::now() < deadline, "the retry run settles");
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let prompt = std::fs::read_to_string(
      features.store.run_dir(&leaf.id, &retry_id).join(charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME),
    )
    .unwrap();
    assert!(prompt.contains(&failed_dir.display().to_string()), "{prompt}");
    assert!(prompt.contains("Handoff summary:"), "{prompt}");
    assert!(prompt.contains("in progress: $ Bash cargo test -p cron (no result yet)"), "{prompt}");
    assert!(prompt.contains("files: /synthetic/src/main.rs"), "{prompt}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_bound_round_without_steps_prepares_a_worktree_for_its_run() {
    assert_repo_fire_prepares_a_worktree().await;
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_steps_task_launches_its_first_step_prompt_in_either_launch_path() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    for scheduled in [true, false] {
      let node = manager(&features, &format!("synthetic-step-launch-node-{scheduled}")).await.unwrap();
      let mut task = worker_task("synthetic-step-launch-task", &node.id, None);
      task.prompt = None;
      task.steps = Some(vec![
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
      ]);

      let (leaf_id, run) = fire_and_launch(&features, task, scheduled).await;

      assert_eq!(run.kind, RunKind::ScheduledStep, "scheduled = {scheduled}");
      assert_eq!(run.sequence_ref.as_ref().map(|sequence| sequence.position), Some(0));
      assert_eq!(run.worktree_path, None);
      let launch = launch_text(&features, &leaf_id, &run);
      assert!(launch.contains("Select candidates."), "{launch}");
      assert!(!launch.contains("Review the diff."), "{launch}");
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_task_without_a_repo_runs_without_a_worktree() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let node = manager(&features, "synthetic-repo-less-node").await.unwrap();
    let task = worker_task("synthetic-repo-less-task", &node.id, None);

    let (leaf_id, run) = fire_and_settle(&features, task, false).await;

    assert_eq!(run.kind, RunKind::Work);
    assert_eq!(run.worktree_path, None);
    assert_eq!(run.branch_name, None);
    let worktree_dir = features.config.get().unwrap().paths.worktree_dir.clone();
    let launch = launch_text(&features, &leaf_id, &run);
    assert!(launch.contains("Synthetic scheduled work"), "{launch}");
    assert!(!launch.contains(&worktree_dir), "{launch}");
  }

  const LAUNCH_HOST: &str = "synthetic-launch-host";

  /// The verdict of `LAUNCH_HOST` that lets a round start: the host answers ok.
  fn launch_host_verdict() -> HostLogin {
    HostLogin {
      alias: LAUNCH_HOST.to_string(),
      status: "ok".to_string(),
      detail: "Synthetic login check".to_string(),
      last_change_at: "2026-06-01T00:00:00+00:00".to_string(),
      awaits_approval: false,
    }
  }

  /// A repo round of a task that names an `improve_host` runs where the launcher decides. The server prepares no
  /// worktree for it and its brief names none: a worktree path here would be a place that is absent where the run
  /// executes, and the round's protocol on the launcher's host owns the working place.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_repo_round_with_an_improve_host_prepares_no_worktree_and_names_none() {
    let _git_lock = GIT_PROGRAM_LOCK.lock().await;
    let git = FakeGit::install();
    let (_home, features) = feature_context_with_login_hosts(&[LAUNCH_HOST]);
    let home = features.config.get().unwrap().charliebot_home.clone();
    set_host_login(&home, launch_host_verdict());
    install_execution(&features).await;
    let checkout_home = tempfile::tempdir().unwrap();
    let repo = checkout_home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let node = keyed_manager(&features, "synthetic-launcher-node").await.unwrap();
    let task = ScheduledTaskConfig {
      improve_host: Some(LAUNCH_HOST.to_string()),
      ..worker_task("synthetic-launcher-task", &node.id, Some(&repo))
    };

    let (leaf_id, run) = fire_and_settle(&features, task, true).await;

    let leaf = features.store.get_session(&leaf_id).await.unwrap().unwrap();
    assert_eq!(leaf.task.as_ref().and_then(|spec| spec.improve_host.as_deref()), Some(LAUNCH_HOST));
    assert_eq!(run.kind, RunKind::Work);
    assert_eq!(run.worktree_path, None, "an improve-host round prepares no worktree");
    assert_eq!(run.branch_name, None, "an improve-host round starts no branch");
    let worktree_dir = features.config.get().unwrap().paths.worktree_dir.clone();
    let launch = launch_text(&features, &leaf_id, &run);
    assert!(launch.contains("Synthetic scheduled work"), "{launch}");
    assert!(!launch.contains(&worktree_dir), "{launch}");
    assert!(!launch.contains("charliebot/task-"), "{launch}");
    assert!(git.calls().is_empty(), "the launch prepares no worktree: {:?}", git.calls());
  }

  /// A handler task bound to a fresh manager node, its yaml rewritten with the binding.
  async fn bound_handler_task(features: &Arc<FeatureContext>, name: &str) -> (std::path::PathBuf, SessionMetadata) {
    let node = manager(features, &format!("{name}-node")).await.unwrap();
    let home = features.config.get().unwrap().charliebot_home.clone();
    let path = write_task(
      &home,
      name,
      &json!({"cron": "* * * * *", "handler": "synthetic_handler", "enabled": true, "timezone": "UTC", "session_id": node.id}),
    );
    (path, node)
  }

  fn utc_at(minute: u32, second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, minute, second).unwrap()
  }

  /// A prompt task bound to a fresh manager node, ticking every minute. Its round starts through the bound delegation,
  /// and that round's process stays open.
  async fn held_worker_task(features: &Arc<FeatureContext>, name: &str) -> (ScheduledTaskConfig, SessionMetadata) {
    let node = manager(features, &format!("{name}-node")).await.unwrap();
    let home = features.config.get().unwrap().charliebot_home.clone();
    write_prompted_task(
      &home,
      name,
      &json!({
        "cron": "* * * * *", "mode": "worker", "enabled": true,
        "timezone": "UTC", "session_id": node.id,
      }),
    );
    let task = crate::loader::read_snapshot(features, &features.config.get().unwrap())
      .unwrap()
      .0
      .tasks
      .into_iter()
      .find(|task| task.name == name)
      .expect("the written task loads");
    (task, node)
  }

  /// The pending inputs of `session_id`, which a prompt fire queues there.
  fn pending_inputs(features: &FeatureContext, session_id: &str) -> Vec<CompactValue> {
    charliebot_tasks::dispatch::dispatcher(features).unwrap().pending_inputs(session_id).unwrap()
  }

  /// The number of rounds a node has started, each one a worker session bound under it.
  fn round_count(features: &FeatureContext, node_id: &str) -> usize {
    features.store.child_ids(node_id).unwrap().len()
  }

  /// The session and run of the first round under `node_id` to reach a running run, waited for up to 15 seconds.
  /// The run's process joins `processes`, so the test ends even when it fails before `end_round`.
  async fn wait_for_running_round(
    features: &Arc<FeatureContext>,
    processes: &ProcessGroupGuard,
    node_id: &str,
  ) -> (String, RunRecord) {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), async {
      loop {
        for leaf_id in features.store.child_ids(node_id).unwrap() {
          for run in features.store.list_run_records_sync(&leaf_id).unwrap() {
            let node = features.store.node_state(&leaf_id).unwrap();
            if matches!(node.display_state(&run, &mut HostBoot::default()).unwrap(), RunDisplayState::Running) {
              processes.track(run.pid.expect("a running run has a process"));
              return (leaf_id, run);
            }
          }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    })
    .await;
    match outcome {
      Ok(round) => round,
      Err(_) => panic!("a round reaches a running run; rounds: {}", round_states(features, node_id)),
    }
  }

  /// Each run under `node_id`'s rounds with its display state and process, for a failure message.
  fn round_states(features: &Arc<FeatureContext>, node_id: &str) -> String {
    let mut states = Vec::new();
    for leaf_id in features.store.child_ids(node_id).unwrap() {
      let node = features.store.node_state(&leaf_id).unwrap();
      for run in features.store.list_run_records_sync(&leaf_id).unwrap() {
        let state = node.display_state(&run, &mut HostBoot::default());
        states.push(format!("{leaf_id}/{} pid={:?} state={state:?}", run.id, run.pid));
      }
    }
    format!("{states:?}")
  }

  /// The end of a held round: its process killed, and its run waited for until a terminal fact lands.
  async fn end_round(features: &Arc<FeatureContext>, leaf_id: &str, run: &RunRecord) {
    let pid = run.pid.expect("a running round has a process");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
      loop {
        let facts = features.store.load_all_facts(leaf_id).unwrap();
        if charliebot_store::run_records::terminal_outcome(&facts, &run.id).unwrap().is_some() {
          return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("the ended round reaches a terminal fact");
  }

  fn skipped_events(features: &FeatureContext, session_id: &str) -> Vec<CompactValue> {
    features
      .store
      .load_chat_events_sync(session_id)
      .unwrap()
      .into_iter()
      .filter(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(charliebot_core::event_types::SCHEDULED_RUN_SKIPPED)
      })
      .collect()
  }

  fn handler_results(features: &FeatureContext, session_id: &str) -> usize {
    features
      .store
      .load_chat_events_sync(session_id)
      .unwrap()
      .iter()
      .filter(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(charliebot_core::event_types::HANDLER_RESULT)
      })
      .count()
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_due_tick_skips_while_the_previous_round_is_in_flight_and_consumes_the_delayed_occurrences() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let (task, node) = held_worker_task(&features, "code-health").await;
    let processes = ProcessGroupGuard::new();
    let scheduler = CronScheduler::new(Arc::clone(&features));
    // The session starts the task's round itself; the due fires below all meet that round in flight.
    bound_round_leaf(&features, &task, "").await;
    let (first_round, first_run) = wait_for_running_round(&features, &processes, &node.id).await;

    // Several due ticks arrived while the first round was still running: one judgment at 00:04 consumes the
    // occurrences 00:01-00:04, no second round joins the running one.
    scheduler.tick_at(utc_at(4, 0)).await.unwrap();
    let skips = skipped_events(&features, &node.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0].get("task").and_then(CompactValue::as_str), Some("code-health"));
    let reason = skips[0].get("reason").and_then(CompactValue::as_str).unwrap();
    assert!(reason.starts_with("previous round still running"), "{reason}");
    assert_eq!(round_count(&features, &node.id), 1, "the running round was never joined by a second one");
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(task_slot(&meta, "code-health", LAST_RUN_STATUS).as_ref().and_then(Value::as_str), Some("skipped"));
    assert_eq!(
      task_slot(&meta, "code-health", LAST_SCHEDULED_RUN).as_ref().and_then(Value::as_str),
      Some("2026-06-01T00:04:00+00:00")
    );

    // The running round ends; a mid-minute tick is not on the cron grid, so nothing fires there and the anchor stays
    // where the skip left it. The next fire lands on 00:05, the next occurrence from the anchor.
    end_round(&features, &first_round, &first_run).await;
    scheduler.tick_at(utc_at(2, 30)).await.unwrap();
    assert_eq!(round_count(&features, &node.id), 1, "no fire at the completion moment");
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(
      task_slot(&meta, "code-health", LAST_SCHEDULED_RUN).as_ref().and_then(Value::as_str),
      Some("2026-06-01T00:04:00+00:00")
    );
    // The 00:05 fire starts no worker: a prompt fire queues its trigger input in the session, which decides the round.
    scheduler.tick_at(utc_at(5, 0)).await.unwrap();
    assert_eq!(round_count(&features, &node.id), 1, "a prompt fire starts no worker");
    let triggers: Vec<CompactValue> = pending_inputs(&features, &node.id)
      .into_iter()
      .filter(|event| {
        event.get("id").and_then(CompactValue::as_str).is_some_and(|id| id.starts_with("cron:code-health:"))
      })
      .collect();
    assert_eq!(triggers.len(), 1, "the fire queues one trigger input");
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(
      task_slot(&meta, "code-health", LAST_SCHEDULED_RUN).as_ref().and_then(Value::as_str),
      Some("2026-06-01T00:05:00+00:00")
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_round_in_flight_in_persisted_state_skips_a_due_tick_on_a_fresh_scheduler() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let (task, node) = held_worker_task(&features, "code-health").await;
    let processes = ProcessGroupGuard::new();
    bound_round_leaf(&features, &task, "").await;
    let (round, run) = wait_for_running_round(&features, &processes, &node.id).await;

    // A restart gives the new scheduler no state of its own: the running round is still in flight in its runs.
    let restarted = CronScheduler::new(Arc::clone(&features));
    restarted.tick_at(utc_at(4, 0)).await.unwrap();
    let skips = skipped_events(&features, &node.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(round_count(&features, &node.id), 1, "the restarted scheduler started no second round");
    end_round(&features, &round, &run).await;
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_steps_round_refuses_a_manual_fire_and_starts_no_run() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let node = manager(&features, "synthetic-steps-refuse-node").await.unwrap();
    let home = features.config.get().unwrap().charliebot_home.clone();
    let step_prompt = home.join("code-health-step.md");
    std::fs::write(&step_prompt, "Run the scheduled step.\n").unwrap();
    write_task(
      &home,
      "code-health",
      &json!({
        "cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node.id,
        "steps": [{"name": "work", "prompt_file": step_prompt.display().to_string()}],
      }),
    );
    let task = crate::loader::read_snapshot(&features, &features.config.get().unwrap())
      .unwrap()
      .0
      .tasks
      .into_iter()
      .find(|task| task.name == "code-health")
      .expect("the written task loads");
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let (round, _, _) =
      scheduler.register_steps_work(&task, &node, "2026-06-01T00:00:00+00:00", None, "synthetic", None).await.unwrap();

    let Err(super::RunError::RoundInFlight { task: task_name, round: refused }) =
      scheduler.run_now("code-health").await
    else {
      panic!("a manual fire with a steps round in flight is refused");
    };
    assert_eq!(task_name, "code-health");
    assert_eq!(refused, round.id);
    assert_eq!(round_count(&features, &node.id), 1, "the refused fire started no round");
    assert_eq!(features.store.list_run_records_sync(&round.id).unwrap().len(), 1);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_manual_fire_queues_its_trigger_input_and_keeps_the_due_tick_from_starting_a_round() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let (_task, node) = held_worker_task(&features, "code-health").await;
    let scheduler = CronScheduler::new(Arc::clone(&features));
    match scheduler.run_now("code-health").await {
      Ok(_) => {}
      Err(super::RunError::Failed(message)) => panic!("the manual fire failed: {message}"),
      Err(_) => panic!("the manual fire was refused"),
    }
    let inputs = pending_inputs(&features, &node.id);
    assert_eq!(inputs.len(), 1, "a manual prompt fire queues its trigger input: {inputs:?}");
    assert!(
      inputs[0].get("id").and_then(CompactValue::as_str).is_some_and(|id| id.starts_with("cron:code-health:")),
      "{inputs:?}"
    );

    // The next occurrence after the manual fire is due two minutes later; the queued input of the task holds it.
    scheduler.tick_at(chrono::Utc::now() + chrono::Duration::minutes(2)).await.unwrap();
    let skips = skipped_events(&features, &node.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    let reason = skips[0].get("reason").and_then(CompactValue::as_str).unwrap();
    assert!(reason.starts_with("previous round still running (input "), "{reason}");
    assert_eq!(round_count(&features, &node.id), 0, "neither the manual fire nor the due tick started a worker");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_steps_round_stays_in_flight_until_its_chain_settles() {
    let (_home, features) = feature_context();
    install_execution(&features).await;
    let cfg = (*features.config.get().unwrap()).clone();
    let scheduler = CronScheduler::new(Arc::clone(&features));
    let firing = "2026-06-01T00:00:00+00:00";
    let steps_task = |node_id: &str| {
      let mut task = worker_task("synthetic-steps-in-flight", node_id, None);
      task.prompt = None;
      task.steps = Some(vec![
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
      ]);
      task
    };
    // Step 0 is registered and not yet launched: a queued run keeps the round in flight.
    let node = manager(&features, "synthetic-steps-passed-node").await.unwrap();
    let task = steps_task(&node.id);
    let (leaf, step0, _) = scheduler.register_steps_work(&task, &node, firing, None, "synthetic", None).await.unwrap();
    assert_eq!(crate::rounds::round_in_flight(&features, &task, &node.id, &cfg).unwrap(), Some(leaf.id.clone()));

    // Step 0 passed with exit 0 and step 1 is not registered yet: the chain awaits its next step. The round is
    // still in flight in the gap between the two steps.
    let guard = features.store.control_lock().await;
    let passed = RunOutcome { exit_code: Some(0), ..RunOutcome::new(RunOutcomeKind::Success) };
    features.store.record_finish_locked(&guard, &leaf.id, &step0.id, passed).await.unwrap();
    drop(guard);
    assert_eq!(crate::rounds::round_in_flight(&features, &task, &node.id, &cfg).unwrap(), Some(leaf.id.clone()));

    // A failed step 0 settles the chain with a report, so its round is no longer in flight.
    let failed_node = manager(&features, "synthetic-steps-failed-node").await.unwrap();
    let failed_task = steps_task(&failed_node.id);
    let (failed_leaf, failed_step0, _) =
      scheduler.register_steps_work(&failed_task, &failed_node, firing, None, "synthetic", None).await.unwrap();
    let guard = features.store.control_lock().await;
    let failed = RunOutcome { exit_code: Some(1), ..RunOutcome::new(RunOutcomeKind::Failed) };
    features.store.record_finish_locked(&guard, &failed_leaf.id, &failed_step0.id, failed).await.unwrap();
    drop(guard);
    assert_eq!(crate::rounds::round_in_flight(&features, &failed_task, &failed_node.id, &cfg).unwrap(), None);
  }

  #[tokio::test]
  async fn the_due_fire_ignores_stuck_disk_state_and_a_manual_run_starts_with_no_round_in_flight() {
    let (_home, features) = feature_context();
    let (_path, node) = bound_handler_task(&features, "code-health").await;
    let scheduler = CronScheduler::new(Arc::clone(&features));
    scheduler.tick_at(utc_at(0, 30)).await.unwrap();

    // Stuck on-disk signals: a thread record whose status says running with a pid that no longer exists, and
    // node bookkeeping stuck at running. No round is in flight, so the due fire proceeds.
    let thread_path = features.store.thread_metadata_path(&node.id, "stuck-thread");
    std::fs::create_dir_all(thread_path.parent().unwrap()).unwrap();
    std::fs::write(
      &thread_path,
      crate::yaml::dump(&json!({
        "id": "stuck-thread", "session_id": node.id, "description": "stuck round",
        "status": "running", "pid": 999999,
      }))
      .unwrap(),
    )
    .unwrap();
    seed_task_slot(
      &features,
      &node.id,
      "code-health",
      json!({LAST_SCHEDULED_RUN: "2026-06-01T00:00:00+00:00", LAST_RUN_STATUS: "running"}),
    )
    .await;
    scheduler.tick_at(utc_at(1, 0)).await.unwrap();
    assert_eq!(handler_results(&features, &node.id), 2, "the due fire proceeds past the running disk state");

    // A manual run with no round in flight starts and records its handler result.
    match scheduler.run_now("code-health").await {
      Ok(_) => {}
      Err(super::RunError::Failed(message)) => panic!("the manual run failed: {message}"),
      Err(_) => panic!("the manual run failed"),
    }
    assert_eq!(handler_results(&features, &node.id), 3);
  }

  const GATED_ALIAS: &str = "synthetic-login-host";
  const GATED_DETAIL: &str = "Synthetic login check\nhttps://login.example.internal/check";

  /// The verdict the stub host reports for `GATED_ALIAS`, changed at `since`.
  fn gate_verdict(status: &str, since: &str) -> HostLogin {
    HostLogin {
      alias: GATED_ALIAS.to_string(),
      status: status.to_string(),
      detail: GATED_DETAIL.to_string(),
      last_change_at: since.to_string(),
      awaits_approval: matches!(status, "needs-interactive-auth" | "needs-okta" | "router-rejected"),
    }
  }

  /// A feature context whose config names `GATED_ALIAS` as a browser-login host.
  fn gated_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    feature_context_with_login_hosts(&[GATED_ALIAS])
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_host_awaiting_approval_holds_each_due_fire_as_one_skip_and_the_first_fire_after_approval_runs() {
    let (_home, features) = gated_context();
    install_execution(&features).await;
    let home = features.config.get().unwrap().charliebot_home.clone();
    set_host_login(&home, gate_verdict("needs-interactive-auth", "2026-06-01T00:00:00+00:00"));
    let root = manager(&features, "gate-root").await.unwrap();
    let node = keyed_child_manager(&features, &root.id, "gate-node").await.unwrap();
    write_prompted_task(
      &home,
      "gated",
      &json!({
        "cron": "* * * * *", "mode": "worker", "enabled": true,
        "timezone": "UTC", "session_id": node.id, "improve_host": GATED_ALIAS,
      }),
    );
    let root_facts = features.store.load_all_facts(&root.id).unwrap().len();
    let scheduler = CronScheduler::new(Arc::clone(&features));

    scheduler.tick_at(utc_at(0, 30)).await.unwrap();
    let skips = skipped_events(&features, &node.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    let reason = skips[0].get("reason").and_then(CompactValue::as_str).unwrap();
    assert_eq!(reason, format!("host {GATED_ALIAS} is needs-interactive-auth: {GATED_DETAIL}"));
    assert_eq!(round_count(&features, &node.id), 0, "a held fire starts no worker");
    assert_eq!(host_probe_count(&home, GATED_ALIAS), 1);
    assert_eq!(features.store.load_all_facts(&root.id).unwrap().len(), root_facts, "the holder is not woken");

    // The next due minute is held again: one more skip, still no worker.
    scheduler.tick_at(utc_at(1, 30)).await.unwrap();
    assert_eq!(skipped_events(&features, &node.id).len(), 2);
    assert_eq!(round_count(&features, &node.id), 0);

    // Approval lands: the next due minute probes ok, reaches the bound session and records no skip.
    set_host_login(&home, gate_verdict("ok", "2026-06-01T00:02:10+00:00"));
    scheduler.tick_at(utc_at(2, 30)).await.unwrap();
    assert_eq!(skipped_events(&features, &node.id).len(), 2, "the fire that approval unblocks records no skip");
    assert_eq!(round_count(&features, &node.id), 0, "a prompt fire starts no worker");
    let inputs = pending_inputs(&features, &node.id);
    assert_eq!(inputs.len(), 1, "the unblocked fire reaches the bound session: {inputs:?}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn an_unreachable_host_runs_the_fire_so_its_failure_reports_as_before() {
    let (_home, features) = gated_context();
    install_execution(&features).await;
    let home = features.config.get().unwrap().charliebot_home.clone();
    set_host_login(&home, gate_verdict("unreachable", "2026-06-01T00:00:00+00:00"));
    let node = keyed_manager(&features, "gate-unreachable-node").await.unwrap();
    write_prompted_task(
      &home,
      "unreachable",
      &json!({
        "cron": "* * * * *", "mode": "worker", "enabled": true,
        "timezone": "UTC", "session_id": node.id, "improve_host": GATED_ALIAS,
      }),
    );
    CronScheduler::new(Arc::clone(&features)).tick_at(utc_at(0, 30)).await.unwrap();
    assert!(skipped_events(&features, &node.id).is_empty());
    assert_eq!(round_count(&features, &node.id), 0, "a prompt fire starts no worker");
    assert_eq!(pending_inputs(&features, &node.id).len(), 1, "the fire reaches the bound session");
  }

  #[tokio::test]
  async fn a_probe_that_errors_records_no_skip_and_starts_no_worker() {
    let (_home, features) = gated_context();
    install_execution(&features).await;
    let home = features.config.get().unwrap().charliebot_home.clone();
    let node = keyed_manager(&features, "gate-error-node").await.unwrap();
    write_prompted_task(
      &home,
      "probe-error",
      &json!({
        "cron": "* * * * *", "mode": "worker", "enabled": true,
        "timezone": "UTC", "session_id": node.id, "improve_host": GATED_ALIAS,
      }),
    );
    // No verdict is set, so the stub's probe errors, as the real probe does for an alias the ssh config lacks.
    CronScheduler::new(Arc::clone(&features)).tick_at(utc_at(0, 30)).await.unwrap();
    assert_eq!(host_probe_count(&home, GATED_ALIAS), 1);
    assert!(skipped_events(&features, &node.id).is_empty(), "an errored probe records no skip");
    assert_eq!(round_count(&features, &node.id), 0, "an errored probe starts no worker");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_task_without_an_improve_host_and_a_handler_task_probe_no_host() {
    let (_home, features) = gated_context();
    install_execution(&features).await;
    let home = features.config.get().unwrap().charliebot_home.clone();
    set_host_login(&home, gate_verdict("needs-okta", "2026-06-01T00:00:00+00:00"));
    let (_task, plain) = held_worker_task(&features, "plain").await;
    let handled = manager(&features, "handled-node").await.unwrap();
    write_task(
      &home,
      "handled",
      &json!({
        "cron": "* * * * *", "handler": "synthetic_handler", "enabled": true,
        "timezone": "UTC", "session_id": handled.id,
      }),
    );
    CronScheduler::new(Arc::clone(&features)).tick_at(utc_at(0, 30)).await.unwrap();
    assert_eq!(host_probe_count(&home, GATED_ALIAS), 0, "no task of these names the host");
    assert!(skipped_events(&features, &handled.id).is_empty());
    assert_eq!(handler_results(&features, &handled.id), 1, "the handler runs despite the host's wait");
    assert_eq!(pending_inputs(&features, &plain.id).len(), 1, "the plain fire reaches its bound session");
  }
}
