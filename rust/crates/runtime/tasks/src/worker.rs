//! The task-run process manager (`src/runtime/worker.py`). A task run's process is placed through the backend lifecycle,
//! relayed when the lifecycle asks, and each of its events is persisted to the run's events log and broadcast on the
//! thread's channel. The terminal events close the run.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::{self, QUOTA_ERROR_PATTERNS};
use charliebot_agents::backend_types::{self, BuiltBackend};
use charliebot_agents::event_builders;
use charliebot_agents::launch_loop::{self, ProcessResult, RunProcess};
use charliebot_agents::runner::{
  AgentRunner, DEFAULT_CLEANUP_TIMEOUT, DEFAULT_POST_RESULT_TIMEOUT, OnSpawnFn, RunRequest, capture_process_diagnostics,
};
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::cursor;
use charliebot_agents::transport::follow::{self, SilenceCallback};
use charliebot_agents::transport::identity::ProcessIdentity;
use charliebot_agents::transport::kill;
use charliebot_agents::transport::recovery;
use charliebot_core::RepoRoot;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types as ET;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_core::timefmt;
use charliebot_registry::backend::{BackendLifecycle, Env, Launch, LaunchArgs, LaunchContext, LaunchKind, LaunchWatch};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata};
use charliebot_store::run_records::{CURSOR_NAME, RAW_LOG_NAME, RunKind, STDERR_LOG_NAME};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use charliebot_registry::context::FeatureContext;

use crate::run_support::{
  StreamEvent, extra_flags_of, read_stderr_tail, route_resume_session_style, stream_event_channel,
  subprocess_buffer_bytes,
};

/// The stash-guard `git` wrapper directory, under the checkout. It rides first on every child's PATH.
const GIT_STASH_GUARD_DIR: &str = "src/runtime/git_stash_guard";
/// The instructions file of a task run, kept beside its events log (`src/runtime/runs.py:52`).
pub const HARNESS_INSTRUCTIONS_NAME: &str = "harness_instructions.md";
/// The diagnostics file of a run whose process outlives its result (`worker.py` `_emit_terminal_events`).
pub const HANG_DIAGNOSTICS_NAME: &str = "hang_diagnostics.json";
/// The silence after which the follow reports a run with no output (`src/infra/timeouts.py:167`).
const NO_OUTPUT_REPORT_THRESHOLD: Duration = Duration::from_secs(2 * 60 * 60);
/// How many characters of a stderr tail the warning log carries (`worker.py` `_emit_terminal_events`).
const STDERR_LOG_CHARS: usize = 500;
/// The quota message of an error event that carries none (`worker.py` `_process_event`).
const QUOTA_MESSAGE_FALLBACK: &str = "Quota exhausted";
/// The failover message of an error event that carries none (`worker.py` `_process_event`).
const TEMPORARY_MESSAGE_FALLBACK: &str = "Backend temporarily unavailable";

/// Receives the session-level notices of a run, such as the login-required event (`Worker.on_session_event`).
pub type SessionEventFn = Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Receives the task node after its process spawns, with `pid` and `pid_start` set (`Worker._on_spawn`).
pub type SpawnedFn = Arc<dyn Fn(ThreadMetadata) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// The run's backend reported a spent quota. `resets_at` is when the quota returns, `None` when the backend names no
/// time.
#[derive(Debug)]
pub struct QuotaExhaustedError {
  message: String,
  pub resets_at: Option<OffsetDateTime>,
}

impl QuotaExhaustedError {
  pub fn new(message: impl Into<String>, resets_at: Option<OffsetDateTime>) -> Self {
    Self { message: message.into(), resets_at }
  }
}

impl std::fmt::Display for QuotaExhaustedError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl std::error::Error for QuotaExhaustedError {}

/// One persisted event as its log line: compact UTF-8 JSON and a newline (`worker.py` `_event_line`).
pub fn event_line(event: &Value) -> Vec<u8> {
  let mut line = serde_json::to_vec(event).expect("a JSON value serializes");
  line.push(b'\n');
  line
}

/// The inputs of one task run's worker (`worker.py` `Worker.__init__`).
pub struct WorkerSpec {
  /// The task node. The run writes its `pid` and `pid_start` when the process spawns.
  pub thread: ThreadMetadata,
  pub working_dir: PathBuf,
  pub events_log: PathBuf,
  pub task_description: String,
  pub cfg: Arc<CharlieBotConfig>,
  pub backend_option: Option<BackendOption>,
  /// The launch's own environment: the child's session id, its run token and the selected home. It applies after the
  /// agent base environment.
  pub extra_env: Env,
  pub on_spawned: Option<SpawnedFn>,
  pub instructions_content: Option<String>,
  pub session_meta: Option<SessionMetadata>,
  pub held_native_id: Option<String>,
  pub run_kind: RunKind,
}

/// The process manager of one task run. A run is one process, or a chain of processes when the backend lifecycle relays
/// it; every process of the run appends to the same events log, and the terminal events close the last one.
pub struct Worker {
  features: Arc<FeatureContext>,
  cfg: Arc<CharlieBotConfig>,
  thread: Arc<Mutex<ThreadMetadata>>,
  thread_id: String,
  session_id: String,
  working_dir: PathBuf,
  events_log: PathBuf,
  data_dir: PathBuf,
  task_description: String,
  backend_option: Option<BackendOption>,
  extra_env: Env,
  on_spawned: Option<SpawnedFn>,
  instructions_content: Option<String>,
  session_meta: Option<Arc<Mutex<SessionMetadata>>>,
  held_native_id: Option<String>,
  run_kind: RunKind,
  on_session_event: Option<SessionEventFn>,
  post_result_timeout: Duration,
  cleanup_timeout: Duration,
  relays: AtomicUsize,
  active: Mutex<Option<Arc<AgentRunner>>>,
  last: Mutex<Option<LastProcess>>,
}

/// What the last process of a run left behind, which the terminal events read.
struct LastProcess {
  exit_code: i32,
  stderr_text: String,
  hang_diagnostics: Option<Value>,
  cgroup_exit_report: Option<String>,
}

impl Worker {
  pub fn new(features: Arc<FeatureContext>, spec: WorkerSpec) -> anyhow::Result<Self> {
    let data_dir = spec.events_log.parent().context("the events log path has no directory")?.to_path_buf();
    Ok(Self {
      features,
      cfg: spec.cfg,
      thread_id: spec.thread.id.clone(),
      session_id: spec.thread.session_id.clone(),
      thread: Arc::new(Mutex::new(spec.thread)),
      working_dir: spec.working_dir,
      events_log: spec.events_log,
      data_dir,
      task_description: spec.task_description,
      backend_option: spec.backend_option,
      extra_env: spec.extra_env,
      on_spawned: spec.on_spawned,
      instructions_content: spec.instructions_content,
      session_meta: spec.session_meta.map(|meta| Arc::new(Mutex::new(meta))),
      held_native_id: spec.held_native_id,
      run_kind: spec.run_kind,
      on_session_event: None,
      post_result_timeout: DEFAULT_POST_RESULT_TIMEOUT,
      cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
      relays: AtomicUsize::new(0),
      active: Mutex::new(None),
      last: Mutex::new(None),
    })
  }

  /// Binds the session-level notice hook (`Worker.on_session_event`).
  pub fn set_on_session_event(&mut self, hook: SessionEventFn) {
    self.on_session_event = Some(hook);
  }

  /// Shortens the follow after a result and the cleanup that follows it, so a run whose process outlives its result
  /// reaches the hang path within a test.
  #[cfg(feature = "test-hooks")]
  pub fn with_hang_timeouts(mut self, post_result_timeout: Duration, cleanup_timeout: Duration) -> Self {
    self.post_result_timeout = post_result_timeout;
    self.cleanup_timeout = cleanup_timeout;
    self
  }

  /// The number of account relays the run has made (`Worker.account_relays`).
  pub fn account_relays(&self) -> usize {
    self.relays.load(Ordering::Acquire)
  }

  /// Runs the task's process, relaying it as the lifecycle asks, and emits the terminal events. Returns the exit code of
  /// the last process (`Worker.run`).
  pub async fn run(&self) -> anyhow::Result<i32> {
    let sink = self.open_sink()?;
    let mut process = WorkerProcess { worker: self, sink: Arc::clone(&sink), env: self.process_env()? };
    match self.backend_option.as_ref() {
      None => {
        let launch =
          Launch { backend_kwargs: Map::new(), resume_id: None, prompt: None, account_label: None, private: None };
        process.run(&launch, None, 0).await?;
      }
      Some(option) => {
        let lifecycle = self.features.registry.lifecycle_for(&option.backend_type)?;
        let context = self.launch_context(option, lifecycle.as_ref(), &sink)?;
        launch_loop::run_launches(
          &context,
          lifecycle.as_ref(),
          &mut process,
          || self.thread_native_id(lifecycle.as_ref()),
          |relays| self.relays.store(relays, Ordering::Release),
        )
        .await?;
      }
    }
    let last = self
      .last
      .lock()
      .expect("last process mutex is not poisoned")
      .take()
      .context("the run ended without a process")?;
    let completion = recovery::raw_completion_time(&self.raw_log_path());
    self
      .emit_terminal_events(
        &sink,
        last.exit_code,
        &last.stderr_text,
        last.hang_diagnostics.as_ref(),
        last.cgroup_exit_report.as_deref(),
        completion,
      )
      .await?;
    tracing::info!(target: "charliebot_tasks", thread = %self.thread_id, exit_code = last.exit_code, "worker_finished");
    Ok(last.exit_code)
  }

  /// Re-attaches to an interrupted run and streams its remaining output, then emits the terminal events (`Worker.resume`).
  /// `is_alive` judges the process from its pinned identity; `on_silence` is the follow's silence recheck. A process
  /// still alive after the follow gets its diagnostics captured and its group killed before the terminal events.
  pub async fn resume(
    &self,
    is_alive: Arc<dyn Fn() -> bool + Send + Sync>,
    on_silence: Option<SilenceCallback>,
  ) -> anyhow::Result<i32> {
    let raw_path = self.raw_log_path();
    charliebot_agents::transport::final_stdout::recover(
      Arc::clone(&self.features.registry),
      self.backend_option.as_ref(),
      &self.cfg,
      &self.data_dir,
      || is_alive(),
      || false,
      follow::TAIL_POLL_INTERVAL,
    )
    .await?;
    let stderr_path = self.data_dir.join(STDERR_LOG_NAME);
    let cursor_path = self.data_dir.join(CURSOR_NAME);
    let sink = self.open_sink()?;
    // Replay the delivered prefix through a dedicated translator to restore stateful backend accumulators.
    let mut translator = self.build_backend(None, false)?.backend.translator();
    let cursor_offset = cursor::read_raw_cursor(&cursor_path);
    let replay_path = raw_path.clone();
    let prefix = tokio::task::spawn_blocking(move || {
      charliebot_agents::transport::recovery::raw_events_before_cursor(&replay_path, cursor_offset)
    })
    .await
    .context("joining OpenCode translator prefix replay")??;
    for raw in prefix {
      translator.translate(raw)?;
    }
    let failure: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let buffer_limit = charliebot_agents::transport::final_stdout::event_buffer_limit(
      &self.data_dir,
      subprocess_buffer_bytes(&self.cfg)?,
    );
    let follow_sink = Arc::clone(&sink);
    let follow_failure = Arc::clone(&failure);
    let alive = Arc::clone(&is_alive);
    let is_opencode = self.backend_option.as_ref().is_some_and(|option| option.backend_type == "opencode");
    let opencode_silence =
      if is_opencode { charliebot_agents::backends::opencode::relay::recovery_silence(&self.data_dir)? } else { None };
    let (on_silence, silence_threshold) = match opencode_silence {
      Some((callback, threshold)) => (Some(callback), threshold),
      None => (on_silence, NO_OUTPUT_REPORT_THRESHOLD),
    };
    let followed = follow::tail_follow_events(
      &raw_path,
      move |raw| {
        if is_opencode {
          charliebot_agents::backends::opencode::reject_permission(&raw)?;
        }
        translator.translate(raw)
      },
      move |event| {
        let sink = Arc::clone(&follow_sink);
        let failure = Arc::clone(&follow_failure);
        async move {
          handle_event(&sink, event, None::<&mut dyn LaunchWatch>, None).await.map(|_| ()).map_err(|error| {
            let message = format!("{error:#}");
            *failure.lock().expect("resume failure mutex is not poisoned") = Some(error);
            std::io::Error::other(message)
          })
        }
      },
      move || alive(),
      Some(&cursor_path),
      cursor_offset,
      self.post_result_timeout,
      buffer_limit,
      follow::TAIL_POLL_INTERVAL,
      on_silence,
      silence_threshold,
    )
    .await;
    // The event's own error outranks the follow's wrapper of it, so a quota error keeps its type.
    let stored = failure.lock().expect("resume failure mutex is not poisoned").take();
    if let Some(error) = stored {
      return Err(error);
    }
    let follow_failed = match followed {
      Ok(_) => false,
      Err(error) if is_opencode => {
        let message = format!("OpenCode backend failed: {error}");
        handle_event(&sink, event_builders::make_error_event(&message), None::<&mut dyn LaunchWatch>, None).await?;
        let pid = self.thread.lock().expect("thread metadata mutex is not poisoned").pid;
        let pid = pid.context("a failed resumed run has no recorded pid")?;
        if is_alive() {
          kill::kill_group_escalating(pid, || is_alive()).await;
        }
        true
      }
      Err(error) => return Err(error),
    };

    if let Some(outcome) = charliebot_agents::transport::final_stdout::read_outcome(&self.data_dir)?
      && let Some(error) = outcome.guard_error
    {
      return Err(charliebot_agents::transport::final_stdout::GuardError(error).into());
    }

    let exit_code = if follow_failed {
      -1
    } else {
      let mut scanned = self.build_backend(None, false)?.backend.translator();
      let (_, _, exit_code) = recovery::scan_result_exit(&raw_path, move |raw| scanned.translate(raw))?;
      exit_code
    };
    // The follow ends with a live process only on the post-result timeout, so a live process here outlived its result.
    // Its diagnostics are captured before its group is killed (`worker.py` `resume`).
    let hang_diagnostics = if is_alive() {
      let pid = self.thread.lock().expect("thread metadata mutex is not poisoned").pid;
      let pid = pid.context("a live resumed run has no recorded pid")?;
      let diagnostics = capture_process_diagnostics(pid).await;
      kill::kill_group_escalating(pid, || is_alive()).await;
      Some(diagnostics)
    } else {
      None
    };
    let completion = recovery::raw_completion_time(&raw_path);
    let stderr_text = read_stderr_tail(&stderr_path).await?;
    self.emit_terminal_events(&sink, exit_code, &stderr_text, hang_diagnostics.as_ref(), None, completion).await?;
    tracing::info!(target: "charliebot_tasks", thread = %self.thread_id, exit_code, "worker_resume_finished");
    Ok(exit_code)
  }

  /// Terminates the running process, if one runs (`Worker.terminate`).
  pub async fn terminate(&self) -> anyhow::Result<()> {
    let active = self.active.lock().expect("active runner mutex is not poisoned").clone();
    match active {
      Some(runner) => runner.terminate().await,
      None => Ok(()),
    }
  }

  /// Drops the running process's handle without a signal, for the shutdown let-go (`Worker.detach`).
  pub fn detach(&self) {
    let active = self.active.lock().expect("active runner mutex is not poisoned").clone();
    if let Some(runner) = active {
      runner.detach();
    }
  }

  fn raw_log_path(&self) -> PathBuf {
    self.data_dir.join(RAW_LOG_NAME)
  }

  fn open_sink(&self) -> anyhow::Result<Arc<RunSink>> {
    fs::create_dir_all(&self.data_dir)
      .with_context(|| format!("creating run directory {}", self.data_dir.display()))?;
    let sink = RunSink::open(
      &self.events_log,
      Arc::clone(&self.features.store),
      self.thread_id.clone(),
      self.on_session_event.clone(),
    )?;
    Ok(Arc::new(sink))
  }

  /// The child environment overlay of every process: the launch's own variables, with the stash-guard directory first
  /// on PATH (`Worker.run`).
  fn process_env(&self) -> anyhow::Result<Env> {
    let base_env = self.features.startup.require::<AgentBaseEnvironment>()?;
    let checkout = self.features.startup.require::<RepoRoot>()?;
    let guard_dir = checkout.path().join(GIT_STASH_GUARD_DIR);
    let mut env = self.extra_env.clone();
    let inherited = env.get("PATH").or_else(|| base_env.values.get("PATH")).cloned();
    let path = match inherited {
      Some(old_path) if !old_path.is_empty() => format!("{}:{old_path}", guard_dir.display()),
      _ => guard_dir.display().to_string(),
    };
    env.insert("PATH".to_string(), path);
    Ok(env)
  }

  fn thread_native_id(&self, lifecycle: &dyn BackendLifecycle) -> Option<String> {
    let thread = self.thread.lock().expect("thread metadata mutex is not poisoned");
    lifecycle.thread_native_id(&thread)
  }

  /// The launch context of the run: its events go to the events log and the session hook (`Worker._launch_context`).
  fn launch_context(
    &self,
    option: &BackendOption,
    lifecycle: &dyn BackendLifecycle,
    sink: &Arc<RunSink>,
  ) -> anyhow::Result<LaunchContext> {
    let session_meta =
      self.session_meta.clone().context("a task run with a backend option needs its session metadata")?;
    let base_env = self.features.startup.require::<AgentBaseEnvironment>()?;
    let emit_sink = Arc::clone(sink);
    let context_sink = Arc::clone(sink);
    Ok(LaunchContext {
      cfg: Arc::clone(&self.cfg),
      option: option.clone(),
      session_meta,
      kind: LaunchKind::Task,
      run_kind: self.run_kind,
      cwd: self.working_dir.clone(),
      base_env: Arc::new(base_env.values.clone()),
      held_native_id: self.held_native_id.clone(),
      preassigned_native_id: self.thread_native_id(lifecycle),
      emit: Arc::new(move |event| {
        let sink = Arc::clone(&emit_sink);
        Box::pin(async move { sink.emit(event).await })
      }),
      // A task keeps no account label: the label is a manager turn's session field.
      record_account: Arc::new(|_label: String| Box::pin(async { Ok(()) })),
      context_state: Arc::new(move || {
        let tokens = context_sink.context_tokens();
        Box::pin(async move { Ok((tokens, None)) })
      }),
      running_turns: None,
      run_id: None,
    })
  }

  /// The backend of one process (`Worker._build_backend`). A launcher build fails loudly. A translate-only build falls
  /// back to the binary-free translate backend when the backend cannot be constructed.
  fn build_backend(&self, launch: Option<&Launch>, launcher: bool) -> anyhow::Result<BuiltBackend> {
    let registry = Arc::clone(&self.features.registry);
    let base = LaunchArgs {
      buffer_limit: Some(subprocess_buffer_bytes(&self.cfg)?),
      instructions_content: self.instructions_content.clone(),
      instructions_file: Some(self.data_dir.join(HARNESS_INSTRUCTIONS_NAME)),
      log_dir: Some(self.data_dir.clone()),
      cgroup_session_id: Some(self.session_id.clone()),
      ..LaunchArgs::default()
    };
    if let Some(option) = &self.backend_option {
      let mut backend_kwargs = launch.map(|launch| launch.backend_kwargs.clone()).unwrap_or_default();
      let mut args = base.clone();
      let mut extra_flags = extra_flags_of(&backend_kwargs)?;
      backend_kwargs.remove("extra_flags");
      if let Some(resume_id) = launch.and_then(|launch| launch.resume_id.as_deref()).filter(|id| !id.is_empty()) {
        let style = self
          .features
          .registry
          .backend_type_of(&option.backend_type)
          .with_context(|| format!("unknown backend type {}", option.backend_type))?
          .traits
          .resume;
        let (resume_flags, native_id) = route_resume_session_style(style, Some(resume_id))?;
        extra_flags = [resume_flags, extra_flags].concat();
        args.resume_session_id = native_id;
      }
      args.extra_flags = extra_flags;
      args.backend_kwargs = backend_kwargs;
      match backend_types::build_backend(Arc::clone(&registry), option, &self.cfg, args) {
        Ok(built) => return Ok(built),
        Err(error) if launcher => return Err(error),
        Err(error) => tracing::warn!(
          target: "charliebot_tasks",
          thread_id = %self.thread_id,
          backend = %option.id,
          backend_type = %option.backend_type,
          error = %error,
          "translate_backend_unresolved"
        ),
      }
    }
    backend_types::build_translate_fallback(registry, &self.cfg, base)
  }

  fn spawn_hook(&self) -> OnSpawnFn {
    let thread = Arc::clone(&self.thread);
    let on_spawned = self.on_spawned.clone();
    Arc::new(move |identity: ProcessIdentity| {
      let thread = Arc::clone(&thread);
      let on_spawned = on_spawned.clone();
      Box::pin(async move {
        let snapshot = {
          let mut thread = thread.lock().expect("thread metadata mutex is not poisoned");
          thread.pid = Some(identity.pid);
          thread.pid_start = identity.pid_start.clone();
          thread.clone()
        };
        tracing::info!(target: "charliebot_tasks", thread = %snapshot.id, pid = identity.pid, "worker_spawned");
        if let Some(on_spawned) = on_spawned {
          on_spawned(snapshot).await?;
        }
        Ok(())
      })
    })
  }

  /// Runs one planned process and streams its events through the sink until it exits (`Worker._run_process`).
  async fn run_process(
    &self,
    sink: &Arc<RunSink>,
    env: &Env,
    launch: &Launch,
    mut watch: Option<&mut dyn LaunchWatch>,
    relays: usize,
  ) -> anyhow::Result<ProcessResult> {
    self.relays.store(relays, Ordering::Release);
    let built = self.build_backend(Some(launch), true)?;
    let runner = Arc::new(AgentRunner::new(&built.backend_type, built.backend, built.launch));
    tracing::info!(
      target: "charliebot_tasks",
      thread = %self.thread_id,
      cwd = %self.working_dir.display(),
      account = launch.account_label.as_deref(),
      relays,
      "worker_starting"
    );
    let prompt = launch.prompt.clone().unwrap_or_else(|| self.task_description.clone());
    let (on_event, mut receiver) = stream_event_channel("event consumer stopped", "event acknowledgement stopped");
    let base_env = self.features.startup.require::<AgentBaseEnvironment>()?;
    let mut request = RunRequest::new(
      prompt,
      self.working_dir.clone(),
      base_env.as_ref().clone(),
      SessionCgroupConfig::from_config(&self.cfg),
      Arc::clone(&self.features.registry),
    );
    request.environment_overlay = env.clone();
    request.session_id = self.extra_env.get(launch_loop::SESSION_ID_ENV).cloned();
    request.post_result_timeout = self.post_result_timeout;
    request.cleanup_timeout = self.cleanup_timeout;
    request.on_event = on_event;
    request.gate_child_until_on_spawn();
    request.on_spawn = Some(self.spawn_hook());
    *self.active.lock().expect("active runner mutex is not poisoned") = Some(Arc::clone(&runner));

    let run_future = runner.run(request);
    tokio::pin!(run_future);
    let outcome = loop {
      tokio::select! {
        result = &mut run_future => {
          match result {
            Ok(outcome) => break outcome,
            Err(error) => return Err(self.stop_after(&runner, error).await),
          }
        }
        Some(stream_event) = receiver.recv() => {
          let StreamEvent { event, acknowledged } = stream_event;
          let result = async {
            let terminate_now =
              handle_event(sink, event, watch.as_deref_mut(), launch.account_label.as_deref()).await?;
            if terminate_now {
              runner.terminate().await?;
            }
            Ok::<(), anyhow::Error>(())
          }
          .await;
          let acknowledgement = result.as_ref().map(|_| ()).map_err(|error| format!("{error:#}"));
          // The send fails only when the runner's follow is gone, and the error below ends this process either way.
          let _ = acknowledged.send(acknowledgement);
          if let Err(error) = result {
            return Err(self.stop_after(&runner, error).await);
          }
        }
      }
    };
    self.clear_active();
    let exit_code = outcome.exit_code.unwrap_or(-1);
    let stderr_text = outcome.stderr_text.clone();
    *self.last.lock().expect("last process mutex is not poisoned") = Some(LastProcess {
      exit_code,
      stderr_text: stderr_text.clone(),
      hang_diagnostics: outcome.hang_diagnostics,
      cgroup_exit_report: outcome.cgroup_exit_report,
    });
    Ok((exit_code, stderr_text))
  }

  fn clear_active(&self) {
    *self.active.lock().expect("active runner mutex is not poisoned") = None;
  }

  /// Stops the process of a run that an error ends, and returns that error. A stop that fails replaces it, carrying the
  /// error as its context.
  async fn stop_after(&self, runner: &AgentRunner, error: anyhow::Error) -> anyhow::Error {
    self.clear_active();
    match runner.terminate().await {
      Ok(()) => error,
      Err(stop_error) => stop_error.context(format!("stopping the agent after: {error:#}")),
    }
  }

  /// Persists and broadcasts the synthesized events that close a run (`Worker._emit_terminal_events`).
  async fn emit_terminal_events(
    &self,
    sink: &RunSink,
    exit_code: i32,
    stderr_text: &str,
    hang_diagnostics: Option<&Value>,
    cgroup_report: Option<&str>,
    clamp_to: Option<OffsetDateTime>,
  ) -> anyhow::Result<()> {
    if let Some(diagnostics) = hang_diagnostics {
      let diag_path = self.data_dir.join(HANG_DIAGNOSTICS_NAME);
      let written = hang_diagnostics_json(diagnostics);
      match tokio::fs::write(&diag_path, written).await {
        Ok(()) => tracing::warn!(
          target: "charliebot_tasks",
          thread = %self.thread_id,
          path = %diag_path.display(),
          "worker_wrote_hang_diagnostics"
        ),
        Err(error) => tracing::error!(
          target: "charliebot_tasks",
          thread = %self.thread_id,
          error = %error,
          "worker_write_hang_diagnostics_failed"
        ),
      }
      let diag_event = json!({
        "type": ET::SYSTEM,
        "subtype": "hang_diagnostics",
        "diagnostics_path": diag_path.display().to_string(),
        "exit_code": exit_code,
        "timestamp": clamp_ts(clamp_to),
      });
      sink.append_durable(&diag_event)?;
      sink.broadcast(diag_event).await?;
    }

    if !stderr_text.is_empty() {
      let mut stderr_event = json!({
        "type": if exit_code == 0 { ET::SYSTEM } else { ET::ERROR },
        "content": stderr_text,
        "timestamp": clamp_ts(clamp_to),
      });
      if exit_code == 0 {
        stderr_event["subtype"] = json!("stderr");
      }
      sink.append_durable(&stderr_event)?;
      sink.broadcast(stderr_event).await?;
      tracing::warn!(
        target: "charliebot_tasks",
        thread = %self.thread_id,
        stderr = %stderr_text.chars().take(STDERR_LOG_CHARS).collect::<String>(),
        "worker_stderr"
      );
    }

    if let Some(report) = cgroup_report {
      // The session memory-cap or host-OOM attribution, on the failure channel the finalize path reads.
      let cap_event = json!({"type": ET::ERROR, "content": report, "timestamp": clamp_ts(clamp_to)});
      sink.append_durable(&cap_event)?;
      sink.broadcast(cap_event).await?;
      tracing::warn!(target: "charliebot_tasks", thread = %self.thread_id, report, "worker_cgroup_exit_report");
    }

    let final_event = json!({
      "type": if exit_code == 0 { ET::COMPLETE } else { ET::ERROR },
      "status": if exit_code == 0 { "success" } else { "failed" },
      "exit_code": exit_code,
      "timestamp": clamp_ts(clamp_to),
    });
    sink.broadcast(final_event).await
  }
}

/// The process runner of a task run: each `run` plans one process, which streams through the run's sink.
struct WorkerProcess<'a> {
  worker: &'a Worker,
  sink: Arc<RunSink>,
  env: Env,
}

impl RunProcess for WorkerProcess<'_> {
  fn run<'a>(
    &'a mut self,
    launch: &'a Launch,
    watch: Option<&'a mut dyn LaunchWatch>,
    relays: usize,
  ) -> BoxFuture<'a, anyhow::Result<ProcessResult>> {
    Box::pin(self.worker.run_process(&self.sink, &self.env, launch, watch, relays))
  }
}

/// The events log of one run, held open for the whole run, and the broadcast channel of its thread.
struct RunSink {
  path: PathBuf,
  file: Mutex<File>,
  store: Arc<Store>,
  channel: String,
  on_session_event: Option<SessionEventFn>,
  context_tokens: Mutex<Option<i64>>,
}

impl RunSink {
  fn open(
    path: &Path,
    store: Arc<Store>,
    channel: String,
    on_session_event: Option<SessionEventFn>,
  ) -> anyhow::Result<Self> {
    let file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(path)
      .with_context(|| format!("opening events log {}", path.display()))?;
    Ok(Self {
      path: path.to_path_buf(),
      file: Mutex::new(file),
      store,
      channel,
      on_session_event,
      context_tokens: Mutex::new(None),
    })
  }

  /// Appends one event line (`Worker._append_event_line`).
  fn append(&self, event: &Value) -> anyhow::Result<()> {
    let mut file = self.file.lock().expect("events log mutex is not poisoned");
    file.write_all(&event_line(event)).with_context(|| format!("appending to events log {}", self.path.display()))
  }

  /// Appends one event line and syncs its data before returning, as the durable chat append does.
  fn append_durable(&self, event: &Value) -> anyhow::Result<()> {
    self.append(event)?;
    let file = self.file.lock().expect("events log mutex is not poisoned");
    file.sync_data().with_context(|| format!("syncing events log {}", self.path.display()))
  }

  async fn broadcast(&self, event: Value) -> anyhow::Result<()> {
    self.store.broadcast(&self.channel, event).await?;
    Ok(())
  }

  /// Fills the event's timestamp, appends it and broadcasts it (`Worker._persist_and_broadcast`).
  async fn persist_and_broadcast(&self, mut event: Value) -> anyhow::Result<()> {
    fill_timestamp(&mut event);
    self.append(&event)?;
    self.broadcast(event).await
  }

  /// The lifecycle's `emit`: persist and broadcast the event, then hand it to the session hook.
  async fn emit(&self, mut event: Value) -> anyhow::Result<()> {
    fill_timestamp(&mut event);
    self.persist_and_broadcast(event.clone()).await?;
    if let Some(hook) = &self.on_session_event {
      hook(event).await?;
    }
    Ok(())
  }

  fn context_tokens(&self) -> Option<i64> {
    *self.context_tokens.lock().expect("context tokens mutex is not poisoned")
  }
}

/// Processes one event of a run's process. The watch folds the event, a quota or rate-limit rejection ends the run,
/// and the event is persisted, broadcast and followed by its compaction events. Returns whether the watch asks the
/// process to stop now (`Worker._process_event`).
async fn handle_event<W: LaunchWatch + ?Sized>(
  sink: &RunSink,
  mut event: Value,
  watch: Option<&mut W>,
  account: Option<&str>,
) -> anyhow::Result<bool> {
  let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
  // The run-start adoption signal carries no renderable content, so it is persisted and not broadcast.
  if event_type == ET::SESSION_ATTACHED {
    fill_timestamp(&mut event);
    sink.append(&event)?;
    return Ok(false);
  }
  let (message, content) = if event_type == ET::ERROR {
    (lowered_text(event.get("message")), lowered_text(event.get("content")))
  } else {
    (String::new(), String::new())
  };

  fill_timestamp(&mut event);
  let watched = watch.is_some();
  let terminate_now = match watch {
    Some(watch) => watch.observe(&event),
    None => false,
  };

  if event_type == ET::RATE_LIMIT_EVENT {
    let info = event.get(ET::RATE_LIMIT_INFO).cloned().unwrap_or(Value::Null);
    if info.get("status").and_then(Value::as_str) == Some("rejected") {
      let rate_type = info.get("rateLimitType").and_then(Value::as_str).unwrap_or("unknown");
      let resets_at_epoch = info.get("resetsAt");
      let resets_at = resets_at_of(resets_at_epoch)?;
      let epoch_text = pyfmt::python_str(resets_at_epoch.unwrap_or(&Value::Null));
      tracing::warn!(
        target: "charliebot_tasks",
        thread = %sink.channel,
        rate_type,
        resets_at = %epoch_text,
        account,
        "worker_rate_limited"
      );
      if !watched {
        sink.append(&event)?;
        let message = format!("Rate limited ({rate_type}), resets at {epoch_text}");
        return Err(QuotaExhaustedError::new(message, resets_at).into());
      }
    }
  }

  if event_type == ET::ASSISTANT
    && let Some(usage) = event.pointer("/message/usage").and_then(Value::as_object)
  {
    let tokens = prompt_token_sum(usage);
    if tokens > 0 {
      *sink.context_tokens.lock().expect("context tokens mutex is not poisoned") = Some(tokens);
    }
  }

  if event_type == ET::ERROR
    && QUOTA_ERROR_PATTERNS.iter().any(|pattern| message.contains(pattern) || content.contains(pattern))
  {
    sink.append(&event)?;
    let text = event.get("message").and_then(Value::as_str).unwrap_or(QUOTA_MESSAGE_FALLBACK);
    return Err(QuotaExhaustedError::new(text, None).into());
  }

  if event_type == ET::ERROR && backend_pool::is_temporary_unavailable_error(&message) {
    sink.append(&event)?;
    let text = event.get("message").and_then(Value::as_str).unwrap_or(TEMPORARY_MESSAGE_FALLBACK);
    return Err(QuotaExhaustedError::new(text, None).into());
  }

  // Only an errored Claude Code result is classified here, by its status and then by its text.
  if event_type == ET::RESULT
    && event.get("is_error") == Some(&Value::Bool(true))
    && backend_pool::is_temporary_unavailable_result(&event)
  {
    let text = pyfmt::python_str(event.get("result").context("a temporary result error carries its result text")?);
    sink.append(&event)?;
    return Err(QuotaExhaustedError::new(text, None).into());
  }

  sink.append(&event)?;
  sink.broadcast(event.clone()).await?;
  handle_compaction_events(sink, &event).await?;
  if terminate_now {
    tracing::warn!(
      target: "charliebot_tasks",
      thread = %sink.channel,
      account,
      "worker_account_relay_safe_point"
    );
  }
  Ok(terminate_now)
}

/// Synthesizes the context event that a compaction notice of the process implies, and persists it
/// (`streaming.py` `handle_compaction_events`).
async fn handle_compaction_events(sink: &RunSink, event: &Value) -> anyhow::Result<()> {
  if event.get("type").and_then(Value::as_str) != Some(ET::SYSTEM) {
    return Ok(());
  }
  match event.get("subtype").and_then(Value::as_str) {
    Some(ET::COMPACT_BOUNDARY) => {
      let metadata = event.get(ET::COMPACT_METADATA).cloned().unwrap_or_else(|| json!({}));
      let trigger = metadata.get("trigger").and_then(Value::as_str).unwrap_or("unknown").to_string();
      tracing::info!(
        target: "charliebot_tasks",
        thread = %sink.channel,
        trigger = %trigger,
        pre_tokens = ?metadata.get(ET::COMPACT_PRE_TOKENS),
        post_tokens = ?metadata.get(ET::COMPACT_POST_TOKENS),
        "cc_context_compacted"
      );
      sink.persist_and_broadcast(event_builders::make_context_compacted_event(&trigger, Some(metadata), None)).await
    }
    Some("status") if event.get("compact_result").and_then(Value::as_str) == Some("failed") => {
      let error = event.get("compact_error").and_then(Value::as_str);
      tracing::info!(target: "charliebot_tasks", thread = %sink.channel, error = ?error, "cc_context_compact_failed");
      sink.persist_and_broadcast(event_builders::make_context_compact_failed_event(error, None, None)).await
    }
    _ => Ok(()),
  }
}

/// Input, cache-creation and cache-read tokens of a Claude Code usage block (`session_usage._prompt_token_sum`).
fn prompt_token_sum(usage: &Map<String, Value>) -> i64 {
  [ET::USAGE_INPUT_TOKENS, ET::USAGE_CACHE_CREATION_INPUT_TOKENS, ET::USAGE_CACHE_READ_INPUT_TOKENS]
    .iter()
    .map(|key| usage.get(*key).and_then(Value::as_i64).unwrap_or(0))
    .sum()
}

/// The lowercase text of an event field, for the quota pattern check; empty when the field is absent.
fn lowered_text(value: Option<&Value>) -> String {
  value.map(pyfmt::python_str).unwrap_or_default().to_lowercase()
}

/// The reset moment of a rate-limit rejection: a number of Unix seconds, `None` for anything else.
fn resets_at_of(epoch: Option<&Value>) -> anyhow::Result<Option<OffsetDateTime>> {
  let Some(seconds) = epoch.and_then(Value::as_f64) else { return Ok(None) };
  let moment = OffsetDateTime::from_unix_timestamp_nanos((seconds * 1e9).round() as i128)
    .with_context(|| format!("rate-limit reset {seconds} is outside the calendar"))?;
  Ok(Some(moment))
}

/// Sets the event's timestamp when it has none (`Worker._process_event`).
fn fill_timestamp(event: &mut Value) {
  let missing = match event.get("timestamp") {
    None | Some(Value::Null) => true,
    Some(Value::String(text)) => text.is_empty(),
    Some(_) => false,
  };
  if missing && let Some(object) = event.as_object_mut() {
    object.insert("timestamp".to_string(), json!(timefmt::utc_now_iso()));
  }
}

/// The timestamp of a synthesized event: now, capped at the run's end so every event stays at or before the raw log's
/// final mtime (`worker.py` `_clamp_ts`).
fn clamp_ts(clamp_to: Option<OffsetDateTime>) -> String {
  let now = OffsetDateTime::now_utc();
  match clamp_to {
    Some(clamp_to) if clamp_to < now => timefmt::iso_from_system_time(SystemTime::from(clamp_to)),
    _ => timefmt::iso_from_system_time(SystemTime::from(now)),
  }
}

/// `json.dumps(diagnostics, indent=2)`: the file the hang diagnostics are written as.
fn hang_diagnostics_json(diagnostics: &Value) -> String {
  let style =
    JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ",", key_separator: ": ", indent: Some(2) };
  pyfmt::json_dumps(diagnostics, &style)
}

#[cfg(feature = "test-hooks")]
impl Worker {
  /// Builds the backend of one process, as a run's launch does (`Worker._build_backend`).
  pub fn build_process_backend_for_test(
    &self,
    launch: Option<&Launch>,
    launcher: bool,
  ) -> anyhow::Result<BuiltBackend> {
    self.build_backend(launch, launcher)
  }

  /// Persists and broadcasts one event of a run's process (`Worker._process_event`). Returns whether the watch asks the
  /// process to stop now.
  pub async fn process_event_for_test(
    &self,
    event: Value,
    watch: Option<&mut dyn LaunchWatch>,
  ) -> anyhow::Result<bool> {
    let sink = self.open_sink()?;
    handle_event(&sink, event, watch, None).await
  }
}
