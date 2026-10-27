//! Manager turn process execution and recovery.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_types;
use charliebot_agents::launch_loop::{self, ProcessResult, RunProcess};
use charliebot_agents::runner::{AgentRunner, RunRequest};
use charliebot_agents::transport::{cursor, follow, identity, recovery};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::{
  ContextStateFn as TurnContextStateFn, Env, Launch, LaunchArgs, LaunchContext, LaunchKind, LaunchWatch,
};
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::run_records::{CURSOR_NAME, RAW_LOG_NAME, RunKind, STDERR_LOG_NAME};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::master_state::{ContextStateFn, RunTracking, SessionCallbacks, WorkItem};
use crate::run_support::{
  extra_flags_of, read_stderr_tail, route_resume_session, stream_event_channel, subprocess_buffer_bytes,
};

const NOTICE: &str = "[模型未输出正文，以下为其思考内容]";
const POST_RESULT_TIMEOUT: Duration = Duration::from_secs(90);

/// The per-round result consumed by the session queue.
pub(crate) struct MasterRunOutcome {
  pub cc_session_id: Option<String>,
  pub exit_code: i32,
  pub error_msg: Option<String>,
  pub finish_extras: Map<String, Value>,
  pub cancelled: bool,
  /// The cancellation was a covered let-go: the process keeps running in its detached transport.
  pub let_go: bool,
}

struct RunTimingTracker {
  session_id: String,
  backend_type: String,
  model: Option<String>,
  started: Instant,
  spawned: Option<Instant>,
  first_event: Option<Instant>,
  first_assistant: Option<Instant>,
  thinking_text: Vec<String>,
  saw_result: bool,
  saw_zero_usage: bool,
  saw_assistant_text: bool,
  saw_thinking: bool,
  saw_tool_use: bool,
  saw_manual_compact: bool,
}

impl RunTimingTracker {
  fn new(session_id: &str, backend_type: &str, model: Option<&str>) -> Self {
    Self {
      session_id: session_id.to_string(),
      backend_type: backend_type.to_string(),
      model: model.map(str::to_string),
      started: Instant::now(),
      spawned: None,
      first_event: None,
      first_assistant: None,
      thinking_text: Vec::new(),
      saw_result: false,
      saw_zero_usage: false,
      saw_assistant_text: false,
      saw_thinking: false,
      saw_tool_use: false,
      saw_manual_compact: false,
    }
  }

  fn on_spawn(&mut self, pid: u32) {
    self.spawned = Some(Instant::now());
    tracing::info!(
      target: "charliebot_tasks",
      session = %self.session_id,
      pid,
      backend = %self.backend_type,
      model = self.model.as_deref(),
      "master_cc_spawned"
    );
  }

  fn on_event(&mut self, event: &Value) {
    let now = Instant::now();
    if self.first_event.is_none() {
      self.first_event = Some(now);
      let reference = self.spawned.unwrap_or(self.started);
      let millis = elapsed_ms(reference, now);
      tracing::info!(
        target: "charliebot_tasks",
        session = %self.session_id,
        event_type = event.get("type").and_then(|value| value.as_str()),
        spawn_to_first_event_ms = millis,
        "master_cc_first_event"
      );
      if millis > 10_000 {
        tracing::warn!(target: "charliebot_tasks", session = %self.session_id, spawn_to_first_event_ms = millis, "master_cc_slow_first_event");
      }
    }

    if event.get("type").and_then(Value::as_str) == Some(event_types::RESULT) {
      self.saw_result = true;
      if let Some(usage) = event.get("usage").and_then(Value::as_object) {
        self.saw_zero_usage = [
          event_types::USAGE_INPUT_TOKENS,
          event_types::USAGE_OUTPUT_TOKENS,
          event_types::USAGE_CACHE_READ_INPUT_TOKENS,
          event_types::USAGE_CACHE_CREATION_INPUT_TOKENS,
        ]
        .iter()
        .all(|key| usage.get(*key).is_none_or(is_zero));
      }
    }
    self.saw_manual_compact |= is_manual_compact_boundary(event);
    if event.get("type").and_then(Value::as_str) == Some(event_types::TOOL_USE) {
      self.saw_tool_use = true;
    }
    if event.get("type").and_then(Value::as_str) == Some(event_types::THINKING)
      && let Some(content) = event.get("content").and_then(Value::as_str).filter(|value| !value.is_empty())
    {
      self.thinking_text.push(content.to_string());
      self.saw_thinking = true;
    }
    if self.saw_assistant_text {
      return;
    }
    let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array) else {
      return;
    };
    if event.get("type").and_then(Value::as_str) != Some(event_types::ASSISTANT) {
      return;
    }
    for block in blocks {
      if block.get("type").and_then(Value::as_str) == Some("thinking")
        && let Some(text) = block.get("thinking").and_then(Value::as_str).filter(|value| !value.is_empty())
      {
        self.thinking_text.push(text.to_string());
        self.saw_thinking = true;
      }
      if block.get("type").and_then(Value::as_str) == Some(event_types::TOOL_USE) {
        self.saw_tool_use = true;
      }
      if block.get("type").and_then(Value::as_str) == Some("text")
        && block.get("text").and_then(Value::as_str).is_some_and(|value| !value.is_empty())
      {
        self.saw_assistant_text = true;
        self.first_assistant = Some(Instant::now());
        tracing::info!(
          target: "charliebot_tasks",
          session = %self.session_id,
          first_assistant_ms = elapsed_ms(self.started, self.first_assistant.expect("assistant time was recorded")),
          "master_cc_first_assistant_text"
        );
        break;
      }
    }
  }

  fn zero_output_guard(&self) -> bool {
    self.saw_result
      && self.saw_zero_usage
      && !self.saw_assistant_text
      && !self.saw_thinking
      && !self.saw_tool_use
      && !self.saw_manual_compact
  }

  fn finish_extras(&self) -> Map<String, Value> {
    let total_ms = elapsed_ms(self.started, Instant::now());
    let mut extras = Map::from_iter([
      ("backend".to_string(), json!(self.backend_type)),
      ("model".to_string(), json!(self.model)),
      ("total_ms".to_string(), json!(total_ms)),
    ]);
    if self.zero_output_guard() {
      extras.insert("zero_output".to_string(), json!(true));
    }
    if let Some(first_event) = self.first_event {
      extras.insert(
        "spawn_to_first_event_ms".to_string(),
        json!(elapsed_ms(self.spawned.unwrap_or(self.started), first_event)),
      );
    }
    if let Some(first_assistant) = self.first_assistant {
      extras.insert("first_assistant_ms".to_string(), json!(elapsed_ms(self.started, first_assistant)));
    }
    if total_ms > 120_000 {
      tracing::warn!(target: "charliebot_tasks", session = %self.session_id, total_ms, "master_cc_slow_total");
    }
    extras
  }

  fn salvage_thinking_text(&self) -> Option<String> {
    if self.saw_result && !self.saw_assistant_text {
      let thinking = self.thinking_text.concat();
      if !thinking.trim().is_empty() {
        return Some(thinking);
      }
    }
    None
  }
}

fn elapsed_ms(start: Instant, end: Instant) -> u128 {
  end.saturating_duration_since(start).as_millis()
}

fn is_zero(value: &Value) -> bool {
  value.as_f64().is_some_and(|number| number == 0.0)
}

fn is_manual_compact_boundary(event: &Value) -> bool {
  event.get("type").and_then(Value::as_str) == Some(event_types::SYSTEM)
    && event.get("subtype").and_then(Value::as_str) == Some(event_types::COMPACT_BOUNDARY)
    && event.pointer("/compact_metadata/trigger").and_then(Value::as_str) == Some("manual")
}

async fn salvage_silent_turn(
  tracker: &Arc<Mutex<RunTimingTracker>>,
  error_msg: Option<&str>,
  callbacks: &SessionCallbacks,
  session_id: &str,
) -> anyhow::Result<()> {
  if error_msg.is_some() {
    return Ok(());
  }
  let thinking = tracker.lock().expect("run timing mutex is not poisoned").salvage_thinking_text();
  let Some(thinking) = thinking else { return Ok(()) };
  (callbacks.persist_and_broadcast)(
    session_id.to_string(),
    charliebot_agents::event_builders::make_text_event(&format!("{NOTICE}\n\n{thinking}")),
  )
  .await?;
  tracing::info!(target: "charliebot_tasks", session = session_id, "master_cc_silent_turn_salvaged");
  Ok(())
}

fn build_extra_flags(
  features: &FeatureContext,
  option: &BackendOption,
  launch: &Launch,
  item: &WorkItem,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
  let (mut flags, resume_id) = route_resume_session(features, option, launch.resume_id.as_deref())?;
  flags.extend(extra_flags_of(&launch.backend_kwargs)?);
  if let Some(extra_flags) = &item.extra_claude_flags {
    flags.extend(extra_flags.iter().cloned());
  }
  Ok((flags, resume_id))
}

fn build_master_env(item: &WorkItem) -> Env {
  let mut env =
    Env::from([("GIT_CEILING_DIRECTORIES".to_string(), item.cfg.charliebot_home.to_string_lossy().into_owned())]);
  if let Some(extra) = &item.extra_env {
    env.extend(extra.iter().map(|(key, value)| (key.clone(), value.clone())));
  }
  env
}

async fn handle_event(
  features: &FeatureContext,
  callbacks: &SessionCallbacks,
  session_id: &str,
  event: Value,
  cc_session_id: &mut Option<String>,
) -> anyhow::Result<()> {
  if cc_session_id.is_none()
    && let Some(session_id) = event.get("session_id").and_then(Value::as_str).filter(|id| !id.is_empty())
  {
    *cc_session_id = Some(session_id.to_string());
  }
  (callbacks.persist_and_broadcast)(session_id.to_string(), event.clone()).await?;
  features.store.handle_compaction_event(session_id, &event).await?;
  Ok(())
}

fn agent_error_event(message: &str, quota_exhausted: Option<bool>) -> Value {
  let mut event = Map::from_iter([
    ("type".to_string(), json!(event_types::ASSISTANT_ERROR)),
    ("content".to_string(), json!(format!("Agent error: {message}"))),
  ]);
  if let Some(quota_exhausted) = quota_exhausted {
    event.insert(event_types::QUOTA_EXHAUSTED.to_string(), json!(quota_exhausted));
  }
  Value::Object(event)
}

async fn report_turn_error_and_salvage(
  tracker: &Arc<Mutex<RunTimingTracker>>,
  item: &WorkItem,
  error_msg: Option<&str>,
  quota_exhausted: Option<bool>,
) -> anyhow::Result<()> {
  let metadata = item.session_metadata();
  if let Some(message) = error_msg {
    (item.callbacks.persist_and_broadcast)(metadata.id.clone(), agent_error_event(message, quota_exhausted)).await?;
  }
  salvage_silent_turn(tracker, error_msg, &item.callbacks, &metadata.id).await
}

async fn refuse_turn(
  item: &WorkItem,
  message: &str,
  quota_exhausted: Option<bool>,
) -> anyhow::Result<MasterRunOutcome> {
  let session_id = item.session_metadata().id;
  (item.callbacks.persist_and_broadcast)(session_id.clone(), agent_error_event(message, quota_exhausted)).await?;
  (item.callbacks.mark_unread)(session_id).await?;
  Ok(MasterRunOutcome {
    cc_session_id: None,
    exit_code: 1,
    error_msg: Some(message.to_string()),
    finish_extras: Map::new(),
    cancelled: false,
    let_go: false,
  })
}

async fn mark_turn_end_unread(
  item: &WorkItem,
  error_msg: Option<&str>,
  exit_code: i32,
  finish_extras: &Map<String, Value>,
) -> anyhow::Result<()> {
  let clean = error_msg.is_none()
    && exit_code == 0
    && !finish_extras.get("zero_output").and_then(Value::as_bool).unwrap_or(false);
  let metadata = item.session_metadata();
  if clean && item.callbacks.features.registry.turn_end_unread_claimed(&metadata) {
    return Ok(());
  }
  (item.callbacks.mark_unread)(metadata.id).await
}

fn resolve_turn_option(item: &WorkItem) -> Option<BackendOption> {
  if let Some(option) = &item.backend_option {
    return Some(option.clone());
  }
  let metadata = item.session_metadata();
  if !metadata.backend.is_empty() {
    return item.cfg.get_backend_option(&metadata.backend).cloned();
  }
  None
}

fn turn_launch_context(
  tracking: Arc<dyn RunTracking>,
  item: &WorkItem,
  option: BackendOption,
  cwd: PathBuf,
  held_native_id: Option<String>,
) -> anyhow::Result<LaunchContext> {
  let callbacks = item.callbacks.clone();
  let base_env = callbacks.features.startup.require::<AgentBaseEnvironment>()?;
  let session_id = item.session_metadata().id;
  let running_session_id = session_id.clone();
  let shared_metadata = Arc::clone(&item.session_meta);
  let running_tracking = Arc::clone(&tracking);
  let record_account = Arc::new(move |label: String| {
    let callbacks = callbacks.clone();
    let session_id = session_id.clone();
    Box::pin(async move {
      if let Some(persist) = &callbacks.persist_account_label {
        persist(session_id, label).await?;
      }
      Ok(())
    }) as BoxFuture<'static, anyhow::Result<()>>
  });
  let context_state = turn_context_state(
    item.callbacks.context_state.clone(),
    item.session_metadata().id,
    Arc::clone(&item.session_meta),
  );
  Ok(LaunchContext {
    cfg: Arc::clone(&item.cfg),
    option,
    session_meta: shared_metadata,
    kind: LaunchKind::Turn,
    run_kind: RunKind::ManagerTurn,
    cwd,
    base_env: Arc::new(base_env.values.clone()),
    held_native_id,
    preassigned_native_id: None,
    emit: Arc::new({
      let callbacks = item.callbacks.clone();
      let session_id = item.session_metadata().id;
      move |event| {
        let callbacks = callbacks.clone();
        let session_id = session_id.clone();
        Box::pin(async move { (callbacks.persist_and_broadcast)(session_id, event).await })
      }
    }),
    record_account,
    context_state,
    running_turns: Some(Arc::new(move || running_tracking.running_turns_except(&running_session_id))),
    run_id: Some(item.task_run.run_id.clone()),
  })
}

fn turn_context_state(
  read: Option<ContextStateFn>,
  session_id: String,
  session_meta: Arc<Mutex<SessionMetadata>>,
) -> TurnContextStateFn {
  Arc::new(move || {
    let read = read.clone();
    let session_id = session_id.clone();
    let session_meta = Arc::clone(&session_meta);
    Box::pin(async move {
      let session_meta = { session_meta.lock().expect("work item metadata mutex is not poisoned").clone() };
      match read {
        Some(read) => read(session_id, session_meta).await,
        None => Ok((None, None)),
      }
    }) as BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>>
  })
}

struct TurnProcess<'a> {
  tracking: Arc<dyn RunTracking>,
  item: &'a mut WorkItem,
  features: Arc<FeatureContext>,
  option: BackendOption,
  cwd: PathBuf,
  env: Env,
  tracker: Arc<Mutex<RunTimingTracker>>,
  cc_session_id: Arc<Mutex<Option<String>>>,
  error_event_messages: Vec<String>,
  log_dir: PathBuf,
  record_persisted: Arc<AtomicBool>,
  cancelled: Arc<Mutex<Option<bool>>>,
  last_exit_code: Option<i32>,
  relay_count: Arc<std::sync::atomic::AtomicUsize>,
  was_built: bool,
}

impl RunProcess for TurnProcess<'_> {
  fn run<'a>(
    &'a mut self,
    launch: &'a Launch,
    watch: Option<&'a mut dyn LaunchWatch>,
    relays: usize,
  ) -> BoxFuture<'a, anyhow::Result<ProcessResult>> {
    Box::pin(async move {
      let metadata = self.item.session_metadata();
      self.relay_count.store(relays, Ordering::Release);
      if *self.item.cancel.borrow() {
        *self.cancelled.lock().expect("cancel state mutex is not poisoned") = Some(false);
        return Err(TurnCancelled.into());
      }
      if self.was_built {
        self.record_persisted.store(false, Ordering::Release);
      }
      let process_prompt = launch.prompt.as_deref().unwrap_or(&self.item.user_content);
      if relays == 0 {
        let resume_id = launch.resume_id.as_deref();
        if resume_id.is_none() && !self.item.task_run.fresh_native_context && metadata.cc_session_id.is_some() {
          tracing::error!(
            target: "charliebot_tasks",
            session = %metadata.id,
            backend = %self.option.backend_type,
            reason = event_types::RESUME_REASON_TRANSCRIPT_MISSING,
            "master_cc_resume_anchor_missing"
          );
          (self.item.callbacks.persist_and_broadcast)(
            metadata.id.clone(),
            json!({
              "type": event_types::RESUME_CONTEXT_DROPPED,
              "reason": event_types::RESUME_REASON_TRANSCRIPT_MISSING
            }),
          )
          .await?;
        }
        tracing::info!(
          target: "charliebot_tasks",
          session = %metadata.id,
          backend = %self.option.backend_type,
          model = %self.option.model,
          prompt_chars = process_prompt.chars().count(),
          resume_session = resume_id.is_some_and(|id| !id.is_empty()),
          cwd = %self.cwd.display(),
          account = launch.account_label.as_deref(),
          "master_cc_starting"
        );
      }
      let (extra_flags, resume_session_id) = build_extra_flags(&self.features, &self.option, launch, self.item)?;
      let mut launch_args = LaunchArgs {
        extra_flags,
        buffer_limit: Some(subprocess_buffer_bytes(&self.item.cfg)?),
        instructions_content: self.item.task_instructions.clone(),
        resume_session_id,
        log_dir: Some(self.log_dir.clone()),
        cgroup_session_id: Some(metadata.id.clone()),
        ..LaunchArgs::default()
      };
      launch_args.backend_kwargs = launch
        .backend_kwargs
        .iter()
        .filter(|(key, _)| key.as_str() != "extra_flags")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
      let built =
        backend_types::build_backend(Arc::clone(&self.features.registry), &self.option, &self.item.cfg, launch_args)?;
      if launch.backend_kwargs.get("midturn_delivery").and_then(Value::as_bool).unwrap_or(false) {
        charliebot_registry::midturn_delivery::bind_process(&self.item.task_run.run_id, built.backend.midturn_run_id())
          .await?;
      }
      let runner = Arc::new(AgentRunner::new(built.backend, built.launch.clone()));
      self.was_built = true;
      let session_id = metadata.id.clone();
      let (on_event, mut receiver) =
        stream_event_channel("manager event consumer stopped", "manager event acknowledgement stopped");
      let task_session_id = self
        .item
        .extra_env
        .as_ref()
        .and_then(|env| env.get(launch_loop::SESSION_ID_ENV))
        .cloned()
        .unwrap_or_else(|| session_id.clone());
      let mut request = RunRequest::new(
        process_prompt.to_string(),
        self.cwd.clone(),
        self.features.startup.require::<AgentBaseEnvironment>()?.as_ref().clone(),
        charliebot_agents::transport::cgroup::SessionCgroupConfig::from_config(&self.item.cfg),
        Arc::clone(&self.features.registry),
      );
      request.environment_overlay = self.env.clone();
      request.uploaded_files = self.item.uploaded_files.clone().unwrap_or_default();
      request.session_id = Some(task_session_id);
      request.on_event = on_event;
      request.gate_child_until_on_spawn();
      request.on_spawn = Some({
        let tracker = Arc::clone(&self.tracker);
        let on_task_spawn = Arc::clone(&self.item.on_task_spawn);
        let record_persisted = Arc::clone(&self.record_persisted);
        Arc::new(move |identity| {
          let tracker = Arc::clone(&tracker);
          let on_task_spawn = Arc::clone(&on_task_spawn);
          let record_persisted = Arc::clone(&record_persisted);
          Box::pin(async move {
            tracker.lock().expect("run timing mutex is not poisoned").on_spawn(identity.pid);
            on_task_spawn(identity.pid, identity.pid_start).await?;
            record_persisted.store(true, Ordering::Release);
            Ok(())
          })
        })
      });
      self.tracking.set_active_runner(&session_id, Arc::clone(&runner));
      let run_future = runner.run(request);
      tokio::pin!(run_future);
      let mut watch = watch;
      let outcome = loop {
        tokio::select! {
          result = &mut run_future => {
            match result {
              Ok(outcome) => break outcome,
              Err(error) => {
                let _ = runner.terminate().await;
                self.tracking.clear_active_runner(&session_id);
                return Err(error);
              }
            }
          }
          Some(stream_event) = receiver.recv() => {
            let result = async {
              self.tracker.lock().expect("run timing mutex is not poisoned").on_event(&stream_event.event);
              if stream_event.event.get("type").and_then(Value::as_str) == Some(event_types::ERROR) {
                self.error_event_messages.push(stream_event.event.get("message").and_then(Value::as_str).unwrap_or_default().to_string());
              }
              let tracking = Arc::clone(&self.tracking);
              crate::midturn_delivery::record_ack(
                tracking.as_ref(),
                &session_id,
                &mut self.item.user_event_ids,
                &self.item.task_run.run_id,
                &stream_event.event,
              )?;
              let mut cc_session_id = self.cc_session_id.lock().expect("native session id mutex is not poisoned").clone();
              handle_event(
                &self.features,
                &self.item.callbacks,
                &session_id,
                stream_event.event.clone(),
                &mut cc_session_id,
              )
              .await?;
              *self.cc_session_id.lock().expect("native session id mutex is not poisoned") = cc_session_id;
              if let Some(watch) = watch.as_deref_mut()
                && watch.observe(&stream_event.event)
              {
                runner.terminate().await?;
              }
              Ok::<(), anyhow::Error>(())
            }.await;
            let ack_result = match &result {
              Ok(()) => Ok(()),
              Err(error) => Err(error.to_string()),
            };
            let _ = stream_event.acknowledged.send(ack_result);
            if let Err(error) = result {
              runner.terminate().await?;
              self.tracking.clear_active_runner(&session_id);
              return Err(error);
            }
          }
          changed = self.item.cancel.changed() => {
            if changed.is_err() || *self.item.cancel.borrow() {
              let covered = self
                .features
                .registry
                .backend_type_of(&self.option.backend_type)
                .context("manager backend type is not registered")?
                .traits
                .restart_reattach
                && self.record_persisted.load(Ordering::Acquire);
              if covered {
                runner.detach();
              } else {
                runner.terminate().await?;
              }
              self.tracking.clear_active_runner(&session_id);
              *self.cancelled.lock().expect("cancel state mutex is not poisoned") = Some(covered);
              return Err(TurnCancelled.into());
            }
          }
        }
      };
      self.tracking.clear_active_runner(&session_id);
      if let Some(covered) = *self.cancelled.lock().expect("cancel state mutex is not poisoned") {
        let _ = covered;
        return Err(TurnCancelled.into());
      }
      let exit_code = outcome.exit_code.unwrap_or(-1);
      self.last_exit_code = Some(exit_code);
      if !outcome.stderr_text.is_empty() {
        tracing::warn!(target: "charliebot_tasks", session = %session_id, stderr = %outcome.stderr_text, "master_cc_stderr");
      }
      let lifecycle = self.features.registry.lifecycle_for(&self.option.backend_type)?;
      if launch_loop::has_round_notices(lifecycle.as_ref()) {
        let raw_path = self.log_dir.join(RAW_LOG_NAME);
        if !raw_path.is_file() {
          tracing::warn!(target: "charliebot_tasks", session = %session_id, raw_log = %raw_path.display(), "master_cc_fallback_notice_raw_log_missing");
        } else {
          let metadata = self.item.session_metadata();
          let events = project_fresh_raw(
            &self.features,
            &self.option,
            &self.item.cfg,
            &metadata,
            &self.item.task_run.run_id,
            &raw_path,
            true,
          )?;
          emit_round_notices(self.item, &self.option, &self.features, &events).await?;
        }
      }
      Ok((exit_code, outcome.stderr_text))
    })
  }
}

#[derive(Debug)]
pub(crate) struct TurnCancelled;

impl std::fmt::Display for TurnCancelled {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("manager turn cancelled")
  }
}

impl std::error::Error for TurnCancelled {}

/// The error a covered cancellation leaves on its turn. The process keeps running in its detached transport, and the
/// next boot's reconcile settles the run, so the launch path records no failure for it (`master_cc_run.py:648-691`).
#[derive(Debug)]
pub(crate) struct TurnLetGo;

impl std::fmt::Display for TurnLetGo {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("manager turn let go at shutdown; its process keeps running")
  }
}

impl std::error::Error for TurnLetGo {}

fn project_fresh_raw(
  features: &FeatureContext,
  option: &BackendOption,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
  run_id: &str,
  raw_path: &Path,
  notices_only: bool,
) -> anyhow::Result<Vec<Value>> {
  let translator = crate::midturn_delivery::fresh_translate(features, cfg, Some(option), meta, run_id)?;
  let filter = notices_only.then(|| recovery::TypeLineFilter::new([event_types::ASSISTANT]));
  recovery::project_raw_file(raw_path, translator, filter.as_ref())
}

async fn emit_round_notices(
  item: &WorkItem,
  option: &BackendOption,
  features: &FeatureContext,
  events: &[Value],
) -> anyhow::Result<()> {
  let lifecycle = features.registry.lifecycle_for(&option.backend_type)?;
  let session_id = item.session_metadata().id;
  for notice in lifecycle.round_notices(option, events) {
    (item.callbacks.persist_and_broadcast)(session_id.clone(), notice).await?;
  }
  Ok(())
}

pub(crate) async fn run_cc(tracking: Arc<dyn RunTracking>, item: &mut WorkItem) -> anyhow::Result<MasterRunOutcome> {
  let metadata = item.session_metadata();
  tokio::fs::create_dir_all(item.cfg.sessions_dir().join(&metadata.id)).await?;
  let option = match resolve_turn_option(item) {
    Some(option) => option,
    None => {
      let message = if !metadata.backend.is_empty() {
        let fallback = item.cfg.backends.options.first().map(|option| option.id.as_str()).unwrap_or("(none)");
        tracing::error!(
          target: "charliebot_tasks",
          session = %metadata.id,
          requested = %metadata.backend,
          fallback,
          "master_cc_backend_unresolved"
        );
        format!(
          "backend {}; this run did not execute.",
          crate::spawner_backends::unknown_backend_pin_refusal(&metadata.backend, fallback)
        )
      } else {
        tracing::error!(target: "charliebot_tasks", session = %metadata.id, requested = "(none)", "master_cc_backend_unresolved");
        "no backend option was given and this session pins none — refusing to fall back to backends.options[0]; this run did not execute.".to_string()
      };
      return refuse_turn(item, &message, None).await;
    }
  };
  let features = Arc::clone(&item.callbacks.features);
  features
    .registry
    .backend_type_of(&option.backend_type)
    .with_context(|| format!("unknown backend type {}", option.backend_type))?;
  let lifecycle = features.registry.lifecycle_for(&option.backend_type)?;
  let fresh_native = item.task_run.fresh_native_context;
  if fresh_native {
    item.session_meta.lock().expect("work item metadata mutex is not poisoned").cc_session_id = None;
  }
  let metadata = item.session_metadata();
  let held_native_id = (!fresh_native).then(|| metadata.cc_session_id.clone()).flatten();
  let cwd = item.cfg.sessions_dir().join(&metadata.id);
  let context = turn_launch_context(Arc::clone(&tracking), item, option.clone(), cwd.clone(), held_native_id.clone())?;
  let env = build_master_env(item);
  let tracker = Arc::new(Mutex::new(RunTimingTracker::new(&metadata.id, &option.backend_type, Some(&option.model))));
  let log_dir = item.task_run.transport_dir.clone();
  let cc_session_id = Arc::new(Mutex::new(held_native_id));
  let record_persisted = Arc::new(AtomicBool::new(false));
  let cancelled = Arc::new(Mutex::new(None));
  let relay_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let session_id = metadata.id.clone();
  let mut process = TurnProcess {
    tracking: Arc::clone(&tracking),
    item,
    features: Arc::clone(&features),
    option: option.clone(),
    cwd,
    env,
    tracker: Arc::clone(&tracker),
    cc_session_id: Arc::clone(&cc_session_id),
    error_event_messages: Vec::new(),
    log_dir: log_dir.clone(),
    record_persisted: Arc::clone(&record_persisted),
    cancelled: Arc::clone(&cancelled),
    last_exit_code: None,
    relay_count: Arc::clone(&relay_count),
    was_built: false,
  };
  let _on_relay = {
    let relay_count = Arc::clone(&relay_count);
    move |count| relay_count.store(count, Ordering::Release)
  };
  let launches = launch_loop::run_launches(
    &context,
    lifecycle.as_ref(),
    &mut process,
    {
      let cc_session_id = Arc::clone(&cc_session_id);
      move || cc_session_id.lock().expect("session id mutex is not poisoned").clone()
    },
    _on_relay,
  )
  .await;
  let mut exit_code = 1;
  let mut error_msg = None;
  let mut quota_exhausted = None;
  if let Err(error) = launches {
    if error.downcast_ref::<TurnCancelled>().is_some() {
      let covered = cancelled.lock().expect("cancel state mutex is not poisoned").unwrap_or(false);
      tracing::warn!(target: "charliebot_tasks", session = %session_id, action = if covered { "let_go" } else { "terminate" }, "master_cc_cancelled");
    } else if let Some(refusal) = error.downcast_ref::<charliebot_registry::backend::LaunchRefused>() {
      tracing::error!(target: "charliebot_tasks", session = %session_id, error = %refusal, quota_exhausted = refusal.quota_exhausted, "master_cc_launch_refused");
      error_msg = Some(refusal.to_string());
      quota_exhausted = Some(refusal.quota_exhausted);
    } else {
      tracing::error!(target: "charliebot_tasks", session = %session_id, error = %error, "master_cc_crashed");
      error_msg = Some(error.to_string());
    }
  } else {
    exit_code = process.last_exit_code.unwrap_or(1);
    if exit_code != 0 {
      let stderr = read_stderr_tail(&log_dir.join(STDERR_LOG_NAME)).await?;
      error_msg = charliebot_agents::runs::select_error_hint(&process.error_event_messages, &stderr);
    }
  }
  let mut finish_extras = tracker.lock().expect("run timing mutex is not poisoned").finish_extras();
  if process.was_built {
    finish_extras.insert("native_backend".to_string(), json!(option.id));
  }
  let relays = relay_count.load(Ordering::Acquire);
  if relays > 0 {
    finish_extras.insert("account_relays".to_string(), json!(relays));
  }
  report_turn_error_and_salvage(&tracker, process.item, error_msg.as_deref(), quota_exhausted).await?;
  let was_cancelled = cancelled.lock().expect("cancel state mutex is not poisoned").is_some();
  let covered = cancelled.lock().expect("cancel state mutex is not poisoned").unwrap_or(false);
  if !was_cancelled || !covered {
    mark_turn_end_unread(process.item, error_msg.as_deref(), exit_code, &finish_extras).await?;
    tracing::info!(target: "charliebot_tasks", session = %session_id, exit_code, finish_extras = ?finish_extras, "master_cc_finished");
  }
  let cc_session_id = cc_session_id.lock().expect("session id mutex is not poisoned").clone();
  Ok(MasterRunOutcome {
    cc_session_id,
    exit_code,
    error_msg,
    finish_extras,
    cancelled: was_cancelled,
    let_go: was_cancelled && covered,
  })
}

fn resolve_resume_option(item: &WorkItem) -> Option<BackendOption> {
  if let Some(option) = &item.backend_option {
    return Some(option.clone());
  }
  let metadata = item.session_metadata();
  if !metadata.backend.is_empty() {
    if let Some(option) = item.cfg.get_backend_option(&metadata.backend) {
      return Some(option.clone());
    }
    tracing::warn!(target: "charliebot_tasks", session = %metadata.id, backend = %metadata.backend, "master_cc_resume_backend_unresolved");
  }
  item.cfg.backends.options.first().cloned()
}

pub(crate) fn build_fresh_translate(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  option: Option<&BackendOption>,
  meta: &SessionMetadata,
  run_id: &str,
) -> anyhow::Result<crate::midturn_delivery::FreshTranslate> {
  crate::midturn_delivery::fresh_translate(features, cfg, option, meta, run_id)
}

pub(crate) async fn resume_cc(tracking: Arc<dyn RunTracking>, item: &mut WorkItem) -> anyhow::Result<MasterRunOutcome> {
  let metadata = item.session_metadata();
  let record = item.resume_record.as_ref().context("resume item has no recorded run")?.clone();
  let is_alive = item.resume_is_alive.clone().context("resume item has no liveness probe")?;
  let raw_path = record.raw_log.clone();
  let log_dir = raw_path.parent().context("resume raw log has no parent directory")?.to_path_buf();
  let cursor_path = log_dir.join(CURSOR_NAME);
  let stderr_path = log_dir.join(STDERR_LOG_NAME);
  let option = resolve_resume_option(item);
  tracing::info!(target: "charliebot_tasks", session = %metadata.id, pid = record.pid, raw_log = %raw_path.display(), "master_cc_resuming");
  let backend_type = option.as_ref().map(|option| option.backend_type.as_str()).unwrap_or("unknown");
  let model = option.as_ref().map(|option| option.model.as_str());
  let tracker = Arc::new(Mutex::new(RunTimingTracker::new(&metadata.id, backend_type, model)));
  let callback_cc_session_id = Arc::new(Mutex::new(metadata.cc_session_id.clone()));
  let mut exit_code = -1;
  let mut error_msg = None;
  let features = Arc::clone(&item.callbacks.features);
  let callbacks = item.callbacks.clone();
  let cfg = Arc::clone(&item.cfg);
  let session_id = metadata.id.clone();
  let buffer_limit = subprocess_buffer_bytes(&cfg)?;
  let resume_meta = metadata.clone();
  let resume_run_id = item.task_run.run_id.clone();
  let translate = build_fresh_translate(&features, &cfg, option.as_ref(), &resume_meta, &resume_run_id)?;
  let translate_stream = translate;
  let cursor_read = cursor::read_raw_cursor(&cursor_path);
  let cancel = item.cancel.clone();
  // The resume follow's acknowledgements join the item's web-reply binding like a fresh turn's do. The item keeps its
  // own ids so a failing follow leaves the binding as Python's in-place mutation does.
  let ack_run_id = item.task_run.run_id.clone();
  let ack_ids = Arc::new(std::sync::Mutex::new(item.user_event_ids.clone()));
  let stream_tracker = Arc::clone(&tracker);
  let stream_callbacks = callbacks.clone();
  let stream_features = Arc::clone(&features);
  let stream_cc_session_id = Arc::clone(&callback_cc_session_id);
  let stream_events = Arc::new(Mutex::new(Vec::<Value>::new()));
  let persisted_events = Arc::clone(&stream_events);
  let stream_ack_ids = Arc::clone(&ack_ids);
  let follow_result = follow::tail_follow_events(
    &raw_path,
    translate_stream,
    {
      move |event| {
        let tracker = Arc::clone(&stream_tracker);
        let callbacks = stream_callbacks.clone();
        let features = Arc::clone(&stream_features);
        let session_id = session_id.clone();
        let cc_session_id = Arc::clone(&stream_cc_session_id);
        let events = Arc::clone(&persisted_events);
        let ack_ids = Arc::clone(&stream_ack_ids);
        let ack_tracking = Arc::clone(&tracking);
        let ack_run_id = ack_run_id.clone();
        async move {
          tracker.lock().expect("run timing mutex is not poisoned").on_event(&event);
          crate::midturn_delivery::record_ack(
            ack_tracking.as_ref(),
            &session_id,
            &mut ack_ids.lock().expect("resume acknowledgement mutex is not poisoned"),
            &ack_run_id,
            &event,
          )
          .map_err(|error| std::io::Error::other(error.to_string()))?;
          let mut cc_session_id_value = cc_session_id.lock().expect("native session id mutex is not poisoned").clone();
          handle_event(&features, &callbacks, &session_id, event.clone(), &mut cc_session_id_value)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
          *cc_session_id.lock().expect("native session id mutex is not poisoned") = cc_session_id_value;
          events.lock().expect("resume event mutex is not poisoned").push(event);
          Ok(())
        }
      }
    },
    move || is_alive() && !*cancel.borrow(),
    Some(&cursor_path),
    cursor_read,
    POST_RESULT_TIMEOUT,
    buffer_limit,
    follow::TAIL_POLL_INTERVAL,
    None,
    Duration::ZERO,
  )
  .await;
  if let Err(error) = follow_result {
    error_msg = Some(error.to_string());
  }
  let cancelled = *item.cancel.borrow();
  let restart_reattach = option
    .as_ref()
    .and_then(|option| features.registry.backend_type_of(&option.backend_type))
    .is_some_and(|registration| registration.traits.restart_reattach);
  let has_process_identity = record.pid.is_some() && record.pid_start.is_some();
  let let_go = cancelled && restart_reattach && has_process_identity;
  if cancelled {
    // A resume follow owns no process handle. A covered run only drops this follower; an uncovered process is
    // terminated using the persisted identity, as the fresh-turn cancellation path does.
    error_msg = None;
    exit_code = 1;
    if !let_go {
      let pid = record.pid.context("resumed run cancellation has no process id")?;
      let host_boot = identity::read_host_boot_time().context("reading host boot time for resumed cancellation")?;
      let alive = charliebot_agents::runs::run_alive_probe(
        Some(pid),
        record.pid_start.clone(),
        Some(record.started_at.into_inner()),
        host_boot,
      );
      if alive() {
        charliebot_agents::transport::kill::kill_group_escalating(pid, alive).await;
      }
    }
    tracing::warn!(
      target: "charliebot_tasks",
      session = %metadata.id,
      action = if let_go { "let_go" } else { "terminate" },
      "master_cc_resume_cancelled"
    );
  } else {
    let mut events = stream_events.lock().expect("resume event mutex is not poisoned").clone();
    // Replay from the start of the stored stream to preserve completion and manual-compaction evidence.
    let translate_scan = build_fresh_translate(&features, &cfg, option.as_ref(), &resume_meta, &resume_run_id)?;
    let scanned = recovery::scan_result_exit(&raw_path, translate_scan);
    match scanned {
      Ok((projected, _, code)) => {
        events = projected;
        exit_code = code;
        if events.iter().any(is_manual_compact_boundary) {
          tracker.lock().expect("run timing mutex is not poisoned").saw_manual_compact = true;
        }
      }
      Err(error) => error_msg = Some(error.to_string()),
    }
    let stderr = read_stderr_tail(&stderr_path).await?;
    if !stderr.is_empty() {
      tracing::warn!(target: "charliebot_tasks", session = %metadata.id, stderr, "master_cc_stderr");
    }
    if exit_code != 0 && error_msg.is_none() {
      let messages = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some(event_types::ERROR))
        .map(|event| event.get("message").and_then(Value::as_str).unwrap_or_default().to_string())
        .collect::<Vec<_>>();
      error_msg = charliebot_agents::runs::select_error_hint(&messages, &stderr);
    }
    if let Some(option) = option.as_ref() {
      emit_round_notices(item, option, &features, &events).await?;
    }
    if let Some(pid) = record.pid {
      let host_boot = identity::read_host_boot_time().context("reading host boot time for resumed run")?;
      let alive = charliebot_agents::runs::run_alive_probe(
        Some(pid),
        record.pid_start.clone(),
        Some(record.started_at.into_inner()),
        host_boot,
      );
      if alive() {
        tracing::warn!(target: "charliebot_tasks", session = %metadata.id, pid, "master_cc_resumed_run_hung_after_result");
        charliebot_agents::transport::kill::kill_group_escalating(pid, alive).await;
      }
    }
  }
  let finish_extras = tracker.lock().expect("run timing mutex is not poisoned").finish_extras();
  item.user_event_ids = std::mem::take(&mut *ack_ids.lock().expect("resume acknowledgement mutex is not poisoned"));
  report_turn_error_and_salvage(&tracker, item, error_msg.as_deref(), None).await?;
  if !let_go {
    mark_turn_end_unread(item, error_msg.as_deref(), exit_code, &finish_extras).await?;
  }
  tracing::info!(target: "charliebot_tasks", session = %metadata.id, exit_code, finish_extras = ?finish_extras, "master_cc_resume_finished");
  Ok(MasterRunOutcome {
    cc_session_id: callback_cc_session_id.lock().expect("native session id mutex is not poisoned").clone(),
    exit_code,
    error_msg,
    finish_extras,
    cancelled,
    let_go,
  })
}

#[cfg(test)]
mod tests {
  use charliebot_agents::backends::claude_code::claude_accounts;
  use charliebot_agents::backends::claude_code::claude_idle_compaction::{self, IdleProcessIdentity};
  use charliebot_agents::backends::claude_code::claude_lifecycle::ClaudeCodeLifecycle;
  use charliebot_registry::backend::{BackendLifecycle, ResumeStyle};
  use charliebot_store::metadata::TaskProfile;

  use super::*;
  use crate::run_support::route_resume_session_style;
  use crate::test_fixtures;

  const CC_SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

  #[test]
  fn resume_session_routing_matches_backend_style() {
    assert_eq!(
      route_resume_session_style(ResumeStyle::CliFlag, Some("synthetic-native-id")).unwrap(),
      (vec!["--resume".to_string(), "synthetic-native-id".to_string()], None)
    );
    assert_eq!(
      route_resume_session_style(ResumeStyle::NativeId, Some("synthetic-native-id")).unwrap(),
      (Vec::new(), Some("synthetic-native-id".to_string()))
    );
    assert_eq!(route_resume_session_style(ResumeStyle::NativeId, Some("")).unwrap(), (Vec::new(), None));
  }

  #[tokio::test]
  async fn turn_context_state_reads_the_current_shared_metadata() {
    let session_meta =
      Arc::new(Mutex::new(SessionMetadata::new("synthetic-session", "Synthetic", TaskProfile::Manager)));
    let read: crate::master_queue::ContextStateFn = Arc::new(|_, meta| {
      Box::pin(async move {
        let tokens = (meta.backend == "synthetic-updated-backend").then_some(17);
        Ok((tokens, None))
      })
    });
    let context_state = turn_context_state(Some(read), "synthetic-session".to_string(), Arc::clone(&session_meta));

    session_meta.lock().unwrap().backend = "synthetic-updated-backend".to_string();

    assert_eq!(context_state().await.unwrap(), (Some(17), None));
  }

  // Python: tests/test_claude_idle_compaction.py:186.
  #[tokio::test]
  async fn idle_argv_equals_the_master_turn_argv_for_the_same_option_and_account() {
    let root = tempfile::tempdir().unwrap();
    let account_dir = root.path().join("account-a");
    test_fixtures::write_login(&account_dir);
    test_fixtures::write_transcript(&account_dir, CC_SESSION_ID);
    let features = test_fixtures::claude_features(
      &root.path().join("home"),
      &test_fixtures::claude_config(Some(account_dir.as_path())),
    );
    let queue = Arc::new(crate::master_queue::MasterQueue::new(Arc::new(
      charliebot_agents::backend_pool::BackendPool::default(),
    )));
    let meta = test_fixtures::manager_session(
      &features,
      "idle_argv_equals_the_master_turn_argv_for_the_same_option_and_account",
      "fable",
      Some("acct-a"),
      CC_SESSION_ID,
    )
    .await;
    let cfg = features.config.get().unwrap();
    let option = cfg.get_backend_option("fable").unwrap().clone();
    let item = test_fixtures::work_item(&queue, &features, meta.clone());
    let ctx = turn_launch_context(
      Arc::clone(&queue) as Arc<dyn RunTracking>,
      &item,
      option.clone(),
      root.path().join("session"),
      Some(CC_SESSION_ID.to_string()),
    )
    .unwrap();

    // The master turn path, as TurnProcess::run builds one process of a turn.
    let launch = ClaudeCodeLifecycle::new().place(&ctx).await.unwrap();
    let (extra_flags, resume_session_id) = build_extra_flags(&features, &option, &launch, &item).unwrap();
    let launch_args = LaunchArgs {
      extra_flags,
      buffer_limit: Some(subprocess_buffer_bytes(&cfg).unwrap()),
      instructions_content: item.task_instructions.clone(),
      resume_session_id,
      log_dir: Some(root.path().join("run")),
      cgroup_session_id: Some(meta.id.clone()),
      backend_kwargs: launch
        .backend_kwargs
        .iter()
        .filter(|(key, _)| key.as_str() != "extra_flags")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect(),
      ..LaunchArgs::default()
    };
    let turn = backend_types::build_backend(Arc::clone(&features.registry), &option, &cfg, launch_args).unwrap();
    let expected = turn.backend.build_command(&item.user_content).unwrap();

    // The idle path, as the trigger path builds its process for the same account.
    let account = claude_accounts::account_by_label(&cfg, launch.account_label.as_deref()).unwrap().unwrap();
    let identity = IdleProcessIdentity {
      session_id: meta.id.clone(),
      cc_session_id: CC_SESSION_ID.to_string(),
      run_kind: RunKind::ManagerTurn,
    };
    let (argv, _env) = claude_idle_compaction::build_idle_process(
      Arc::clone(&features.registry),
      &cfg,
      &option,
      &account,
      identity,
      &Env::new(),
    )
    .unwrap();

    let format_index = expected.iter().position(|value| value == "--input-format").unwrap();
    assert_eq!(&expected[format_index..format_index + 3], &["--input-format", "stream-json", "--replay-user-messages"]);
    assert!(!argv.iter().any(|value| value == "--input-format"));
    assert_eq!(argv, [&expected[..format_index], &expected[format_index + 3..]].concat());
  }
}
