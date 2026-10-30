//! The raw-log `AgentBackend.run` template method.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::Registry;
use charliebot_registry::backend::{
  AgentBackend, Env, ExitedProbe, LaunchArgs, ServeTransport, ServeTurnArgs, TransportKind,
};
use charliebot_store::run_records::{RAW_LOG_NAME, STDERR_LOG_NAME};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::agent_environment::AgentBaseEnvironment;
use crate::launch_loop::{self, ChildEnvPolicy};
use crate::transport::cgroup::{SessionCgroup, SessionCgroupConfig};
use crate::transport::final_stdout;
use crate::transport::follow;
use crate::transport::identity::{self, ProcessIdentity};
use crate::transport::kill::{self, exit_code};
use crate::transport::rotation::rotate_stale_transport;
use crate::transport::spawn::{self, AgentChild, RawStdin, SpawnSetup, SpawnSpec, read_stderr_tail};

pub const DEFAULT_POST_RESULT_TIMEOUT: Duration = Duration::from_secs(90);
pub const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const FINAL_STDOUT_LOG_NAME: &str = final_stdout::STDOUT_LOG_NAME;
const DIAGNOSTIC_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// Callback invoked after the child identity is pinned and before tail-follow begins.
pub type OnSpawnFn = Arc<dyn Fn(ProcessIdentity) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Callback invoked once for each translated event.
pub type EventFn = Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Inputs owned by the runtime for one backend process.
pub struct RunRequest {
  pub prompt: String,
  /// The attachment refs of the user message, as the message's `uploaded_files` lists them.
  pub uploaded_files: Vec<Value>,
  pub cwd: PathBuf,
  pub agent_environment: AgentBaseEnvironment,
  pub child_env_policy: ChildEnvPolicy,
  pub environment_overlay: Env,
  pub registry: Arc<Registry>,
  pub session_id: Option<String>,
  pub cgroup_config: SessionCgroupConfig,
  pub on_spawn: Option<OnSpawnFn>,
  /// Executable that implements the capture supervisor entry point; None uses the running server executable.
  pub final_stdout_supervisor: Option<PathBuf>,
  child_waits_for_spawn_callback: bool,
  pub on_event: EventFn,
  pub post_result_timeout: Duration,
  pub cleanup_timeout: Duration,
}

impl RunRequest {
  /// Construct a request with Python's post-result and cleanup timeouts.
  pub fn new(
    prompt: String,
    cwd: PathBuf,
    agent_environment: AgentBaseEnvironment,
    cgroup_config: SessionCgroupConfig,
    registry: Arc<Registry>,
  ) -> Self {
    Self {
      prompt,
      uploaded_files: Vec::new(),
      cwd,
      agent_environment,
      child_env_policy: ChildEnvPolicy::RegisteredEdits,
      environment_overlay: Env::new(),
      registry,
      session_id: None,
      cgroup_config,
      on_spawn: None,
      final_stdout_supervisor: None,
      child_waits_for_spawn_callback: false,
      on_event: Arc::new(|_| Box::pin(async { Ok(()) })),
      post_result_timeout: DEFAULT_POST_RESULT_TIMEOUT,
      cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
    }
  }

  /// Keep the spawned child stopped until `on_spawn` has recorded its process identity.
  ///
  /// Python has no such gate: `base.py:919-933` runs `on_spawn` while the child runs. Without the gate, a crash in
  /// that window leaves an agent that acts with no recorded identity, and the recovery relaunch then starts a second
  /// one beside it. With the gate the child cannot act before its identity is durable. The test
  /// `gated_child_waits_until_spawn_callback_finishes` fails without the gate.
  pub fn gate_child_until_on_spawn(&mut self) {
    self.child_waits_for_spawn_callback = true;
  }
}

/// Process status and artifacts recorded after the run finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
  pub exit_code: Option<i32>,
  pub stderr_text: String,
  pub cgroup_exit_report: Option<String>,
  pub hang_diagnostics: Option<Value>,
}

impl RunOutcome {
  /// The outcome of a run that ended before any process started: no exit code, as `AgentRunner` never spawned.
  fn without_process() -> Self {
    Self { exit_code: None, stderr_text: String::new(), cgroup_exit_report: None, hang_diagnostics: None }
  }
}

struct ActiveProcess {
  pid: u32,
  exit: watch::Receiver<Option<Result<i32, String>>>,
  detached: AtomicBool,
  /// The serve transport of a serve run, which finishes the turn on terminate. `None` for the other transports.
  serve: Option<Arc<dyn ServeTransport>>,
}

/// One registered backend instance and its single raw-log launch.
pub struct AgentRunner {
  backend_type: String,
  backend: tokio::sync::Mutex<Box<dyn AgentBackend>>,
  launch: LaunchArgs,
  active: Mutex<Option<Arc<ActiveProcess>>>,
  started: AtomicBool,
  terminated: AtomicBool,
  exit_code: Mutex<Option<i32>>,
}

impl AgentRunner {
  /// Create a runner from the backend built by `Registry`, of the registered backend type `backend_type`.
  pub fn new(backend_type: &str, backend: Box<dyn AgentBackend>, launch: LaunchArgs) -> Self {
    Self {
      backend_type: backend_type.to_string(),
      backend: tokio::sync::Mutex::new(backend),
      launch,
      active: Mutex::new(None),
      started: AtomicBool::new(false),
      terminated: AtomicBool::new(false),
      exit_code: Mutex::new(None),
    }
  }

  /// The exit code the runner's process left with (`backend.exit_code`), or `None` before a process exits. A
  /// final-stdout run records the code its backend chose, which can differ from the process's.
  pub fn exit_code(&self) -> Option<i32> {
    *self.exit_code.lock().expect("exit code lock poisoned")
  }

  /// Prepare, spawn and follow one registered raw-log backend.
  pub async fn run(&self, request: RunRequest) -> anyhow::Result<RunOutcome> {
    if self.started.swap(true, Ordering::AcqRel) {
      bail!("an AgentRunner can run only once");
    }
    let accepted = self.backend.lock().await.accept_uploads(&request.uploaded_files);
    if let Err(message) = accepted {
      (request.on_event)(crate::event_builders::make_error_event(&message)).await?;
      return Ok(RunOutcome::without_process());
    }
    prepare_cwd(&self.launch, &request.cwd, self.backend.lock().await.as_ref().instructions_target()).await?;

    let _temporary_log_dir;
    let log_dir = match self.launch.log_dir.as_deref() {
      Some(path) => path.to_path_buf(),
      None => {
        _temporary_log_dir = charliebot_core::tempdir::mkdtemp("charliebot-run-")?;
        _temporary_log_dir.path().to_path_buf()
      }
    };
    tokio::fs::create_dir_all(&log_dir).await?;
    let mut backend = self.backend.lock().await;
    let transport = backend.transport();
    backend.prepare_transport(&log_dir)?;
    let command = backend.build_command(&request.prompt)?;
    let stdin_prompt = backend.stdin_prompt(&request.prompt);
    let child_environment = launch_loop::build_child_env(
      &request.agent_environment.values,
      &request.registry,
      request.child_env_policy,
      request.session_id.as_deref(),
      &request.environment_overlay,
    );
    let final_env = backend.prepare_env(child_environment)?;
    // A Serve type with no registered transport takes the raw-log path below.
    if transport == TransportKind::Serve
      && let Some(serve) = request.registry.serve_transport(&self.backend_type)
    {
      drop(backend);
      return self.run_serve(request, serve, &command, final_env, &log_dir).await;
    }
    let raw_path = log_dir.join(RAW_LOG_NAME);
    let stderr_path = log_dir.join(STDERR_LOG_NAME);
    let cursor_path = log_dir.join(charliebot_store::run_records::CURSOR_NAME);
    rotate_stale_transport(&log_dir, &raw_path, &stderr_path, &cursor_path)?;
    let buffer_limit = self.launch.buffer_limit.unwrap_or(follow::DEFAULT_BUFFER_LIMIT);
    let original_program = command.first().context("backend command is empty")?.clone();
    let (program, args): (std::ffi::OsString, Vec<std::ffi::OsString>) =
      if request.child_waits_for_spawn_callback && transport != TransportKind::FinalStdout {
        let mut args =
          vec!["-c".into(), "kill -STOP $$; exec \"$@\"".into(), "charliebot-run-gate".into(), original_program.into()];
        args.extend(command.iter().skip(1).cloned().map(Into::into));
        ("/bin/sh".into(), args)
      } else {
        (original_program.into(), command.iter().skip(1).cloned().map(Into::into).collect())
      };
    let spec = SpawnSpec::new(
      program,
      args,
      Some(request.cwd.clone()),
      final_env.into_iter().map(|(key, value)| (key.into(), value.into())).collect::<HashMap<_, _>>(),
      self.launch.cgroup_session_id.clone(),
      request.cgroup_config.clone(),
    );
    if transport == TransportKind::FinalStdout {
      if stdin_prompt.is_some() {
        bail!("a final-stdout backend takes its prompt in argv, not on stdin");
      }
      drop(backend);
      return self.run_final_stdout(request, &spec, &log_dir).await;
    }
    let gated = request.child_waits_for_spawn_callback;
    let (mut spawned, gate) = spawn::spawn_raw_log(
      &spec,
      &raw_path,
      &stderr_path,
      if stdin_prompt.is_some() { RawStdin::Piped } else { RawStdin::Null },
      move |child| gated.then(|| SpawnGate::arm(child)),
    )
    .await?;
    if gate.is_some() {
      wait_until_stopped_or_terminate(&mut spawned.child).await?;
    }
    let mut stdin_slot = spawned.child.stdin.take();
    let midturn = backend.midturn_run_id().is_some();
    // The mid-turn writer runs after the spawn gate opens: a stopped child cannot drain its pipe.
    let (plain_prompt, midturn_prompt) = match stdin_prompt {
      Some(prompt) if midturn => (None, Some(prompt)),
      Some(prompt) => (Some(prompt), None),
      None => (None, None),
    };
    let mut stdin_task: Option<tokio::task::JoinHandle<std::io::Result<()>>> = match plain_prompt {
      Some(prompt) => {
        let mut stdin = stdin_slot.take().context("agent stdin was not piped")?;
        Some(tokio::spawn(async move {
          stdin.write_all(prompt.as_bytes()).await?;
          stdin.shutdown().await
        }))
      }
      None => None,
    };
    let on_spawn = request.on_spawn.clone();
    let (mut spawned, identity) = spawn::pin_identity_and_on_spawn(spawned, move |identity| async move {
      match on_spawn {
        Some(callback) => callback(identity).await,
        None => Ok(()),
      }
    })
    .await?;
    if let Some(gate) = gate {
      gate.open()?;
    }

    let cgroup = spawned.cgroup.take();
    let child = spawned.child;
    let pid = identity.pid;
    let (sender, receiver) = watch::channel(None);
    tokio::spawn(reap_child(child, sender));
    let active = Arc::new(ActiveProcess { pid, exit: receiver, detached: AtomicBool::new(false), serve: None });
    if self.register_active(Arc::clone(&active)) {
      self.graceful_shutdown(&active, kill::KILL_ESCALATION_GRACE, "backend_terminate_timeout").await?;
    }

    if let Some(prompt) = midturn_prompt {
      let stdin = stdin_slot.take().context("agent stdin was not piped")?;
      let exited: ExitedProbe = {
        let exit = active.exit.clone();
        Arc::new(move || exit.borrow().is_some())
      };
      stdin_task = Some(match backend.write_stdin_prompt(Box::new(stdin), exited, &prompt) {
        Some(write) => {
          tokio::spawn(async move { write.await.map_err(|error| std::io::Error::other(error.to_string())) })
        }
        None => bail!("a mid-turn backend writes its own stdin prompt"),
      });
    }

    drop(backend);

    let mut translator = self.backend.lock().await.translator();
    let translate = move |raw| translator.translate(raw);
    let exit_receiver = active.exit.clone();
    let follow_active = Arc::clone(&active);
    let is_alive = move || !follow_active.detached.load(Ordering::Acquire) && exit_receiver.borrow().is_none();
    let handler = request.on_event.clone();
    let hook_backend = &self.backend;
    follow::tail_follow_events(
      &raw_path,
      translate,
      move |event| {
        let handler = handler.clone();
        async move {
          {
            // The backend hook runs before the consumer sees the event, like `tail_follow_events(on_event=...)`.
            let backend = hook_backend.lock().await;
            if let Some(hook) = backend.on_event(&event) {
              hook.await.map_err(|error| std::io::Error::other(error.to_string()))?;
            }
          }
          handler(event).await.map_err(|error| std::io::Error::other(error.to_string()))
        }
      },
      is_alive,
      Some(&cursor_path),
      0,
      request.post_result_timeout,
      buffer_limit,
      follow::TAIL_POLL_INTERVAL,
      None,
      Duration::ZERO,
    )
    .await?;

    if active.detached.load(Ordering::Acquire) {
      self.clear_active(&active);
      return Ok(RunOutcome {
        exit_code: None,
        stderr_text: String::new(),
        cgroup_exit_report: None,
        hang_diagnostics: None,
      });
    }
    let stdin_error = finish_stdin(stdin_task, request.cleanup_timeout, pid).await.err();
    let mut hang_diagnostics = None;
    let exit_code = match wait_for_exit(&active, request.cleanup_timeout).await? {
      Some(code) => Some(code),
      None => {
        tracing::warn!(target: "charliebot_agents", pid, "backend_wait_timeout_after_result");
        hang_diagnostics = Some(capture_process_diagnostics(pid).await);
        self.graceful_shutdown(&active, request.cleanup_timeout, "backend_sigkill_after_result").await?;
        wait_for_exit(&active, request.cleanup_timeout).await?
      }
    };
    if let Some(code) = exit_code {
      self.record_exit_code(code);
    }
    let stderr_text = read_stderr_tail(&stderr_path)?;
    let cgroup_exit_report = if self.terminated.load(Ordering::Acquire) {
      None
    } else {
      cgroup.and_then(|state: SessionCgroup| state.classify_exit(exit_code))
    };
    self.clear_active(&active);
    if let Some(error) = stdin_error {
      return Err(error);
    }
    Ok(RunOutcome { exit_code, stderr_text, cgroup_exit_report, hang_diagnostics })
  }

  /// The final-stdout transport (`antigravity_cli.py` `run`): stdout is piped and teed to `stdout.log`, stderr streams
  /// to `stderr.log`, and once the process exits the backend turns the whole output into events.
  async fn run_serve(
    &self,
    request: RunRequest,
    serve: Arc<dyn ServeTransport>,
    command: &[String],
    mut env: Env,
    run_directory: &Path,
  ) -> anyhow::Result<RunOutcome> {
    let serve_turn = serve.take_turn(command)?;
    let raw_path = run_directory.join(RAW_LOG_NAME);
    let stderr_path = run_directory.join(STDERR_LOG_NAME);
    let cursor_path = run_directory.join(charliebot_store::run_records::CURSOR_NAME);
    rotate_stale_transport(run_directory, &raw_path, &stderr_path, &cursor_path)?;
    fs::write(&raw_path, [])?;
    fs::write(run_directory.join("agent.stdout.log"), [])?;
    let custom_process = serve_turn.identity_process(command, run_directory)?;
    let (mut program, mut args) = match custom_process {
      Some(process) => {
        env.extend(process.env);
        (process.program, process.args)
      }
      None => {
        let program = command.first().context("serve backend command is empty")?.clone();
        (program.into(), command.iter().skip(1).cloned().map(Into::into).collect())
      }
    };
    if request.child_waits_for_spawn_callback {
      let mut gated = vec!["-c".into(), "kill -STOP $$; exec \"$@\"".into(), "charliebot-run-gate".into(), program];
      gated.extend(args);
      program = "/bin/sh".into();
      args = gated;
    }
    let spec = SpawnSpec::new(
      program,
      args,
      Some(request.cwd.clone()),
      env.clone().into_iter().map(|(key, value)| (key.into(), value.into())).collect::<HashMap<_, _>>(),
      self.launch.cgroup_session_id.clone(),
      request.cgroup_config.clone(),
    );
    let mut spawned = spawn::spawn_detached_files(
      &spec,
      &run_directory.join("agent.stdout.log"),
      &stderr_path,
      SpawnSetup::ParentSideTurnTree,
    )
    .await?;
    let gate = request.child_waits_for_spawn_callback.then(|| SpawnGate::arm(&spawned.child));
    if gate.is_some() {
      wait_until_stopped_or_terminate(&mut spawned.child).await?;
    }
    let on_spawn = request.on_spawn.clone();
    let (mut spawned, identity) = spawn::pin_identity_and_on_spawn(spawned, move |identity| async move {
      match on_spawn {
        Some(callback) => callback(identity).await,
        None => Ok(()),
      }
    })
    .await?;
    if let Some(gate) = gate {
      gate.open()?;
    }

    let cgroup = spawned.cgroup.take();
    let (sender, receiver) = watch::channel(None);
    tokio::spawn(reap_child(spawned.child, sender));
    let active = Arc::new(ActiveProcess {
      pid: identity.pid,
      exit: receiver,
      detached: AtomicBool::new(false),
      serve: Some(serve),
    });
    if self.register_active(Arc::clone(&active)) {
      self.graceful_shutdown(&active, kill::KILL_ESCALATION_GRACE, "backend_terminate_timeout").await?;
    }

    let run_result = serve_turn
      .run(ServeTurnArgs {
        prompt: request.prompt,
        cwd: request.cwd,
        run_directory: run_directory.to_path_buf(),
        raw_log: raw_path,
        raw_cursor: cursor_path,
        buffer_limit: self.launch.buffer_limit.unwrap_or(follow::DEFAULT_BUFFER_LIMIT),
        post_result_timeout: request.post_result_timeout,
        env,
        uploaded_files: request.uploaded_files,
        process_id: identity.pid,
        cgroup_path: cgroup.as_ref().map(|state| state.path.clone()),
        exit: active.exit.clone(),
        on_event: request.on_event.clone(),
      })
      .await;

    if wait_for_exit(&active, Duration::ZERO).await?.is_none() {
      self.graceful_shutdown(&active, request.cleanup_timeout, "backend_terminate_timeout").await?;
    }
    let process_exit =
      wait_for_exit(&active, request.cleanup_timeout).await?.context("serve turn process did not exit")?;
    let stderr_text = String::new();
    let custom_exit = run_result.as_ref().ok().and_then(|result| result.exit_code);
    let exit_code = custom_exit.unwrap_or(process_exit);
    self.record_exit_code(exit_code);
    let cgroup_exit_report = if self.terminated.load(Ordering::Acquire) {
      None
    } else {
      cgroup.and_then(|state| state.classify_exit(Some(process_exit)))
    };
    self.clear_active(&active);
    run_result?;
    Ok(RunOutcome { exit_code: Some(exit_code), stderr_text, cgroup_exit_report, hang_diagnostics: None })
  }

  async fn run_final_stdout(
    &self,
    request: RunRequest,
    spec: &SpawnSpec,
    log_dir: &Path,
  ) -> anyhow::Result<RunOutcome> {
    let mut spec = final_stdout::prepare(spec, log_dir, &self.launch, request.final_stdout_supervisor.as_deref())?;
    if request.child_waits_for_spawn_callback {
      let mut args = vec!["-c".into(), "kill -STOP $$; exec \"$@\"".into(), "charliebot-run-gate".into(), spec.program];
      args.extend(spec.args);
      spec.program = "/bin/sh".into();
      spec.args = args;
    }
    let gated = request.child_waits_for_spawn_callback;
    let (mut spawned, gate) = spawn::spawn_raw_log(
      &spec,
      Path::new("/dev/null"),
      &log_dir.join(STDERR_LOG_NAME),
      RawStdin::Null,
      move |child| gated.then(|| SpawnGate::arm(child)),
    )
    .await?;
    if gate.is_some() {
      wait_until_stopped_or_terminate(&mut spawned.child).await?;
    }
    let on_spawn = request.on_spawn.clone();
    let (mut spawned, identity) = spawn::pin_identity_and_on_spawn(spawned, move |identity| async move {
      match on_spawn {
        Some(callback) => callback(identity).await,
        None => Ok(()),
      }
    })
    .await?;
    if let Some(gate) = gate {
      gate.open()?;
    }
    let cgroup = spawned.cgroup.take();
    let (sender, receiver) = watch::channel(None);
    tokio::spawn(reap_child(spawned.child, sender));
    let active =
      Arc::new(ActiveProcess { pid: identity.pid, exit: receiver, detached: AtomicBool::new(false), serve: None });
    if self.register_active(Arc::clone(&active)) {
      self.graceful_shutdown(&active, kill::KILL_ESCALATION_GRACE, "backend_terminate_timeout").await?;
    }
    while active.exit.borrow().is_none() && !active.detached.load(Ordering::Acquire) {
      tokio::time::sleep(follow::TAIL_POLL_INTERVAL).await;
    }
    if active.detached.load(Ordering::Acquire) {
      self.clear_active(&active);
      return Ok(RunOutcome {
        exit_code: None,
        stderr_text: String::new(),
        cgroup_exit_report: None,
        hang_diagnostics: None,
      });
    }
    let observed_exit = wait_for_exit(&active, request.cleanup_timeout).await?;
    let outcome = final_stdout::materialize(self.backend.lock().await.as_mut(), log_dir, observed_exit)?;
    let process_exit_code = final_stdout::process_exit_code(log_dir, observed_exit)?;
    self.record_exit_code(outcome.exit_code);
    let raw_path = log_dir.join(RAW_LOG_NAME);
    let cursor_path = log_dir.join(charliebot_store::run_records::CURSOR_NAME);
    let handler = request.on_event;
    follow::tail_follow_events(
      &raw_path,
      |event| Ok(vec![event]),
      move |event| {
        let handler = handler.clone();
        async move { handler(event).await.map_err(|error| std::io::Error::other(error.to_string())) }
      },
      || false,
      Some(&cursor_path),
      0,
      request.post_result_timeout,
      usize::MAX,
      follow::TAIL_POLL_INTERVAL,
      None,
      Duration::ZERO,
    )
    .await?;
    let stderr_text = read_stderr_tail(&log_dir.join(STDERR_LOG_NAME))?;
    let cgroup_exit_report = if self.terminated.load(Ordering::Acquire) {
      None
    } else {
      cgroup.and_then(|state| state.classify_exit(Some(process_exit_code)))
    };
    self.clear_active(&active);
    if let Some(guard_error) = outcome.guard_error {
      bail!(guard_error);
    }
    Ok(RunOutcome { exit_code: Some(outcome.exit_code), stderr_text, cgroup_exit_report, hang_diagnostics: None })
  }

  /// Send SIGTERM to the process group and escalate to SIGKILL after five seconds.
  pub async fn terminate(&self) -> anyhow::Result<()> {
    let active = {
      let active = self.active.lock().expect("active process lock poisoned");
      self.terminated.store(true, Ordering::Release);
      active.clone()
    };
    if let Some(active) = active {
      let finish = match &active.serve {
        Some(serve) => serve.finish_active_turn(active.pid).await,
        None => Ok(()),
      };
      let shutdown = self.graceful_shutdown(&active, kill::KILL_ESCALATION_GRACE, "backend_terminate_timeout").await;
      finish?;
      shutdown?;
    }
    Ok(())
  }

  /// Drop the runner's process handle without sending a signal.
  pub fn detach(&self) {
    if let Some(active) = self.active.lock().expect("active process lock poisoned").take() {
      active.detached.store(true, Ordering::Release);
    }
  }

  async fn graceful_shutdown(
    &self,
    active: &ActiveProcess,
    timeout: Duration,
    timeout_event: &'static str,
  ) -> anyhow::Result<()> {
    if !kill::kill_process_group(active.pid, nix::sys::signal::Signal::SIGTERM) {
      return Ok(());
    }
    if wait_for_exit(active, timeout).await?.is_none() {
      log_timeout(timeout_event, active.pid);
      kill::kill_process_group(active.pid, nix::sys::signal::Signal::SIGKILL);
      wait_for_exit(active, Duration::from_secs(30)).await?;
    }
    Ok(())
  }

  fn record_exit_code(&self, exit_code: i32) {
    *self.exit_code.lock().expect("exit code lock poisoned") = Some(exit_code);
  }

  fn register_active(&self, active: Arc<ActiveProcess>) -> bool {
    let mut active_slot = self.active.lock().expect("active process lock poisoned");
    *active_slot = Some(active);
    self.terminated.load(Ordering::Acquire)
  }

  fn clear_active(&self, active: &Arc<ActiveProcess>) {
    let mut slot = self.active.lock().expect("active process lock poisoned");
    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, active)) {
      slot.take();
    }
  }
}

/// The gated child of a launch until its gate opens. A launch dropped before then, such as one the server's let-go
/// cancels in its spawn callback, leaves the gate shell stopped before `exec` with no recorded owner, so the guard's drop
/// kills it: the agent never starts, and the next boot launches the run. The start tick pinned at spawn keeps a reaped
/// child's reused pid from being signalled. A raw-log launch arms its guard on the spawning thread, so a launch dropped
/// while the spawn is still in flight drops an armed guard too.
struct SpawnGate {
  child: ProcessIdentity,
  opened: bool,
}

impl SpawnGate {
  fn arm(child: &AgentChild) -> Self {
    Self { child: identity::pin_process_identity(child.id().expect("spawned child has a process id")), opened: false }
  }

  /// Continue the stopped child and disarm the guard.
  fn open(mut self) -> std::io::Result<()> {
    spawn::continue_stopped_child(self.child.pid)?;
    self.opened = true;
    Ok(())
  }
}

impl Drop for SpawnGate {
  fn drop(&mut self) {
    let ProcessIdentity { pid, pid_start } = &self.child;
    if self.opened || pid_start.is_none() {
      return;
    }
    if identity::read_pid_stat(*pid).is_some_and(|stat| Some(&stat.pid_start) == pid_start.as_ref()) {
      let process = nix::unistd::Pid::from_raw(i32::try_from(*pid).expect("process id fits pid_t"));
      if let Err(error) = nix::sys::signal::kill(process, nix::sys::signal::Signal::SIGKILL) {
        tracing::warn!(target: "charliebot_agents", pid, error = %error, "spawn_gate_kill_failed");
      }
    }
  }
}

async fn wait_until_stopped_or_terminate(child: &mut AgentChild) -> anyhow::Result<()> {
  if let Err(wait_error) = spawn::wait_until_stopped(child).await {
    kill::terminate_child(child)
      .await
      .with_context(|| format!("terminating and reaping the child after the spawn gate failed: {wait_error}"))?;
    return Err(wait_error.into());
  }
  Ok(())
}

async fn reap_child(mut child: AgentChild, sender: watch::Sender<Option<Result<i32, String>>>) {
  let result = child.wait().await.map(exit_code).map_err(|error| error.to_string());
  sender.send_replace(Some(result));
}

fn log_timeout(event: &str, pid: u32) {
  match event {
    "backend_terminate_timeout" => tracing::warn!(target: "charliebot_agents", pid, "backend_terminate_timeout"),
    "backend_sigkill_after_result" => tracing::warn!(target: "charliebot_agents", pid, "backend_sigkill_after_result"),
    _ => unreachable!("runner only uses named timeout events"),
  }
}

async fn wait_for_exit(active: &ActiveProcess, timeout: Duration) -> anyhow::Result<Option<i32>> {
  let mut receiver = active.exit.clone();
  if let Some(result) = receiver.borrow().clone() {
    return result.map(Some).map_err(anyhow::Error::msg);
  }
  match tokio::time::timeout(timeout, async {
    while receiver.borrow().is_none() {
      receiver.changed().await.context("agent process exit watcher stopped")?;
    }
    anyhow::Ok(())
  })
  .await
  {
    Err(_) => return Ok(None),
    Ok(Err(error)) => return Err(error),
    Ok(Ok(())) => {}
  }
  receiver.borrow().clone().expect("exit watcher completed").map(Some).map_err(anyhow::Error::msg)
}

async fn finish_stdin(
  task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
  timeout: Duration,
  pid: u32,
) -> anyhow::Result<()> {
  let Some(mut task) = task else { return Ok(()) };
  match tokio::time::timeout(timeout, &mut task).await {
    Ok(joined) => Ok(joined.context("waiting for agent stdin writer failed")??),
    Err(error) => {
      tracing::warn!(target: "charliebot_agents", pid, timeout = timeout.as_secs_f64(), "backend_stdin_write_timeout");
      task.abort();
      if let Err(error) = task.await
        && !error.is_cancelled()
      {
        tracing::debug!(target: "charliebot_agents", pid, error = %error, "backend_stdin_write_cancelled");
      }
      Err(error.into())
    }
  }
}

/// A best-effort snapshot of a process that outlives its result: its pgid, `ps` process tree, `/proc` status, open fds
/// and children. A probe that fails is recorded in its own field, so the snapshot always comes back.
pub async fn capture_process_diagnostics(pid: u32) -> Value {
  let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
  let pgid_field = if pgid < 0 {
    let error = std::io::Error::last_os_error();
    tracing::warn!(target: "charliebot_agents", pid, error = %error, "hang_diagnostics_pgid_failed");
    Value::String(format!("<error: {error}>"))
  } else {
    Value::from(pgid)
  };
  let pgid_for_command = pgid_field.as_i64().unwrap_or(i64::from(pid));
  let mut diagnostics = serde_json::Map::from_iter([
    ("captured_at".to_string(), Value::String(charliebot_core::timefmt::utc_now_iso())),
    ("pid".to_string(), Value::from(pid)),
    ("pgid".to_string(), pgid_field),
  ]);
  for (key, command) in diagnostic_commands(pid, pgid_for_command) {
    let args = ["-c".to_string(), command];
    diagnostics.insert(key.to_string(), capture_command(pid, key, "bash", &args).await);
  }
  Value::Object(diagnostics)
}

/// The `bash -c` command of each field. The strings match `_capture_proc_diagnostics` (`base.py:107-110`) byte for
/// byte, and each one's `2>&1` is part of the string.
fn diagnostic_commands(pid: u32, pgid: i64) -> [(&'static str, String); 4] {
  [
    ("process_tree", format!("ps --forest -o pid,pgid,stat,etime,wchan,cmd -g {pgid} 2>&1")),
    ("fds", format!("ls -l /proc/{pid}/fd 2>&1 | head -200")),
    ("status", format!("cat /proc/{pid}/status 2>&1")),
    ("children", format!("ps -o pid,stat,etime,cmd --ppid {pid} 2>&1")),
  ]
}

/// Runs `program args` under the diagnostic timeout and returns its output, or `<capture failed: <error>>`.
async fn capture_command(pid: u32, key: &str, program: &str, args: &[String]) -> Value {
  let error = match tokio::time::timeout(DIAGNOSTIC_CAPTURE_TIMEOUT, read_merged_output(program, args)).await {
    Ok(Ok(bytes)) => return Value::String(String::from_utf8_lossy(&bytes).into_owned()),
    Ok(Err(error)) => error.to_string(),
    // Python's `asyncio.wait_for` raises a `TimeoutError` with no text, and the field records that empty text.
    Err(_) => String::new(),
  };
  tracing::warn!(target: "charliebot_agents", pid, key, error = %error, "hang_diagnostics_capture_failed");
  Value::String(format!("<capture failed: {error}>"))
}

/// Reads the child's stdout and stderr from one pipe, so the bytes come back in write order as Python's
/// `stderr=STDOUT` returns them. The exit status is not part of the field.
async fn read_merged_output(program: &str, args: &[String]) -> std::io::Result<Vec<u8>> {
  let (reader, writer) = std::io::pipe()?;
  let stderr = writer.try_clone()?;
  // The command holds the write ends until this statement ends, so the reader sees EOF when the child exits.
  let mut child = tokio::process::Command::new(program)
    .args(args)
    .stdin(Stdio::null())
    .stdout(writer)
    .stderr(stderr)
    .kill_on_drop(true)
    .spawn()?;
  let mut output = tokio::net::unix::pipe::Receiver::from_owned_fd(reader.into())?;
  let mut bytes = Vec::new();
  output.read_to_end(&mut bytes).await?;
  child.wait().await?;
  Ok(bytes)
}

async fn prepare_cwd(
  launch: &LaunchArgs,
  cwd: &Path,
  target: Option<(&'static str, &'static str)>,
) -> anyhow::Result<()> {
  let (path, event) = match launch.instructions_file.as_deref() {
    Some(path) => (Some(path.to_path_buf()), Some("run_instructions_written")),
    None => target.map(|(name, event)| (Some(cwd.join(name)), Some(event))).unwrap_or((None, None)),
  };
  if let (Some(content), Some(path), Some(event)) = (&launch.instructions_content, path, event)
    && !content.is_empty()
  {
    tokio::fs::write(&path, content).await?;
    tracing::debug!(target: "charliebot_agents", path = %path.display(), "{event}");
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use charliebot_registry::backend::FinalStdoutOutcome;
  use serde_json::json;

  use super::*;

  #[tokio::test]
  async fn raw_log_runner_writes_instructions_follows_events_and_records_exit() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    fs::create_dir(&cwd).unwrap();
    let logs = root.path().join("logs");
    let launch = LaunchArgs {
      instructions_content: Some("synthetic instructions".to_string()),
      log_dir: Some(logs.clone()),
      ..LaunchArgs::default()
    };
    // This test uses a tiny AgentBackend so the child is an explicit /bin/sh process.
    let backend: Box<dyn AgentBackend> =
      Box::new(ShellBackend::new("printf '%s\\n' '{\"type\":\"result\"}'; printf 'synthetic stderr' >&2"));
    let runner = AgentRunner::new("shell", backend, launch);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let mut request = test_request(
      "prompt".to_string(),
      cwd.clone(),
      Env::from([("PATH".to_string(), "/bin".to_string())]),
      root.path().join("cgroups"),
    );
    request.cgroup_config.memory_max_mb = 0;
    request.on_event = Arc::new(move |event| {
      let events = Arc::clone(&events);
      Box::pin(async move {
        events.lock().unwrap().push(event);
        Ok(())
      })
    });
    let outcome = runner.run(request).await.unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stderr_text, "synthetic stderr");
    assert_eq!(fs::read_to_string(cwd.join("HARNESS.md")).unwrap(), "synthetic instructions");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(logs.join(RAW_LOG_NAME).exists());
  }

  #[tokio::test]
  async fn gated_child_waits_until_spawn_callback_finishes() {
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("backend-started");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new("touch \"$MARKER\"; printf '%s\\n' '{\"type\":\"result\"}'")),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    );
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([
        ("PATH".to_string(), "/bin".to_string()),
        ("MARKER".to_string(), marker.to_string_lossy().into_owned()),
      ]),
      root.path().join("cgroups"),
    );
    request.gate_child_until_on_spawn();
    let (callback_entered, callback_entered_rx) = oneshot::channel();
    let (callback_release, callback_release_rx) = oneshot::channel();
    let callback_entered = Arc::new(Mutex::new(Some(callback_entered)));
    let callback_release_rx = Arc::new(Mutex::new(Some(callback_release_rx)));
    let callback_saw_stopped_child = Arc::new(Mutex::new(None));
    request.on_spawn = Some(Arc::new({
      let marker = marker.clone();
      let callback_entered = Arc::clone(&callback_entered);
      let callback_release_rx = Arc::clone(&callback_release_rx);
      let callback_saw_stopped_child = Arc::clone(&callback_saw_stopped_child);
      move |_| {
        let marker = marker.clone();
        let callback_entered = callback_entered
          .lock()
          .expect("spawn callback channel state is not poisoned")
          .take()
          .expect("spawn callback runs once");
        let callback_release_rx = callback_release_rx
          .lock()
          .expect("spawn callback channel state is not poisoned")
          .take()
          .expect("spawn callback runs once");
        let callback_saw_stopped_child = Arc::clone(&callback_saw_stopped_child);
        Box::pin(async move {
          callback_entered.send(()).expect("test waits for spawn callback");
          callback_release_rx.await.expect("test releases spawn callback");
          *callback_saw_stopped_child.lock().expect("spawn callback state is not poisoned") = Some(!marker.exists());
          Ok(())
        })
      }
    }));

    let run = tokio::spawn(async move { runner.run(request).await });
    callback_entered_rx.await.expect("spawn callback starts");
    assert!(!marker.exists());
    callback_release.send(()).expect("spawn callback is waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(30), run).await.unwrap().unwrap().unwrap();

    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(*callback_saw_stopped_child.lock().expect("spawn callback state is not poisoned"), Some(true));
    assert!(marker.exists());
  }

  /// Kills the child a test pinned when the test ends, so a failed assertion leaves no stopped process behind.
  struct PinnedChildCleanup(Arc<Mutex<Option<ProcessIdentity>>>);

  impl Drop for PinnedChildCleanup {
    fn drop(&mut self) {
      let pinned = self.0.lock().expect("pinned child state is not poisoned").take();
      if let Some(ProcessIdentity { pid, pid_start: Some(pid_start) }) = pinned
        && identity::read_pid_stat(pid).is_some_and(|stat| stat.pid_start == pid_start)
      {
        unsafe { libc::kill(i32::try_from(pid).expect("process id fits pid_t"), libc::SIGKILL) };
      }
    }
  }

  #[tokio::test]
  async fn a_launch_dropped_in_its_spawn_callback_kills_the_gated_child() {
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("backend-started");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new("touch \"$MARKER\"; printf '%s\\n' '{\"type\":\"result\"}'")),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    );
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([
        ("PATH".to_string(), "/bin".to_string()),
        ("MARKER".to_string(), marker.to_string_lossy().into_owned()),
      ]),
      root.path().join("cgroups"),
    );
    request.gate_child_until_on_spawn();
    let pinned = Arc::new(Mutex::new(None));
    let _cleanup = PinnedChildCleanup(Arc::clone(&pinned));
    let (callback_entered, callback_entered_rx) = oneshot::channel();
    let callback_entered = Mutex::new(Some(callback_entered));
    request.on_spawn = Some(Arc::new({
      let pinned = Arc::clone(&pinned);
      move |identity| {
        *pinned.lock().expect("pinned child state is not poisoned") = Some(identity);
        let entered = callback_entered
          .lock()
          .expect("spawn callback channel state is not poisoned")
          .take()
          .expect("spawn callback runs once");
        Box::pin(async move {
          entered.send(()).expect("test waits for spawn callback");
          std::future::pending::<anyhow::Result<()>>().await
        })
      }
    }));

    // The owner's callback never returns, as a launch the let-go cancels while it waits for the control lock.
    let run = tokio::spawn(async move { runner.run(request).await });
    callback_entered_rx.await.expect("spawn callback starts");
    run.abort();
    assert!(run.await.expect_err("the launch was cancelled").is_cancelled());

    let child =
      pinned.lock().expect("pinned child state is not poisoned").clone().expect("the callback pinned the child");
    assert_gated_child_killed(&child, &marker).await;
  }

  /// A launch dropped while the spawn of its gated child is still in flight, as a let-go cancel can drop it, never
  /// receives the child. The guard armed on the spawning thread kills it.
  #[test]
  fn a_launch_dropped_while_its_child_spawns_kills_the_gated_child() {
    use tokio::sync::oneshot;

    // One blocking thread, which the backend fills, so the launch's spawn waits in the pool's queue.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().unwrap();
    runtime.block_on(async {
      let root = tempfile::tempdir().unwrap();
      let marker = root.path().join("backend-started");
      let log_dir = root.path().join("logs");
      let (pool_filled, pool_filled_rx) = oneshot::channel();
      let (release_pool, pool_released) = std::sync::mpsc::channel();
      let runner = AgentRunner::new(
        "shell",
        Box::new(ShellBackend::filling_the_blocking_pool("touch \"$MARKER\"", pool_filled, pool_released)),
        LaunchArgs { log_dir: Some(log_dir.clone()), ..LaunchArgs::default() },
      );
      let mut request = test_request(
        "prompt".to_string(),
        root.path().to_path_buf(),
        Env::from([
          ("PATH".to_string(), "/bin".to_string()),
          ("MARKER".to_string(), marker.to_string_lossy().into_owned()),
        ]),
        root.path().join("cgroups"),
      );
      request.gate_child_until_on_spawn();
      let pinned = Arc::new(Mutex::new(None));
      let _cleanup = PinnedChildCleanup(Arc::clone(&pinned));

      let run = tokio::spawn(async move { runner.run(request).await });
      pool_filled_rx.await.expect("the launch fills the blocking pool");
      // On this one-thread runtime the launch ran on from `prepare_env` to its spawn, which waits in the pool's queue.
      run.abort();
      assert!(run.await.expect_err("the launch was cancelled").is_cancelled());
      release_pool.send(()).expect("the pool filler waits for its release");
      // The pool runs its queue in order, so this closure runs after the dropped launch's spawn has returned.
      tokio::task::spawn_blocking(|| ()).await.unwrap();

      let raw_path = log_dir.join(RAW_LOG_NAME);
      let child = spawn::TEST_RAW_LOG_CHILDREN
        .lock()
        .expect("test raw-log child list is not poisoned")
        .iter()
        .find_map(|(path, child)| (path == &raw_path).then(|| child.clone()))
        .expect("the dropped launch spawned its gated child");
      *pinned.lock().expect("pinned child state is not poisoned") = Some(child.clone());
      assert_gated_child_killed(&child, &marker).await;
    });
  }

  /// Wait up to a second for a gated child to die. The killed child stays a zombie until the runtime reaps it; a stopped
  /// one keeps state `T`.
  async fn assert_gated_child_killed(child: &ProcessIdentity, marker: &Path) {
    let pid_start = child.pid_start.as_deref().expect("the gated child has a stat line");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Some(stat) =
      identity::read_pid_stat(child.pid).filter(|stat| stat.pid_start == pid_start && stat.state != "Z")
    {
      assert!(tokio::time::Instant::now() < deadline, "the gated child is still in state {}", stat.state);
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!marker.exists(), "the agent command never ran");
  }

  #[tokio::test]
  async fn gated_final_stdout_child_waits_until_spawn_callback_finishes() {
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("final-stdout-started");
    let runner = AgentRunner::new(
      "shell",
      Box::new(FinalStdoutShell::new("touch \"$MARKER\"; printf 'answer'", None, None)),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    );
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([
        ("PATH".to_string(), "/bin".to_string()),
        ("MARKER".to_string(), marker.to_string_lossy().into_owned()),
      ]),
      root.path().join("cgroups"),
    );
    request.gate_child_until_on_spawn();
    let (callback_entered, callback_entered_rx) = oneshot::channel();
    let (callback_release, callback_release_rx) = oneshot::channel();
    let callback_entered = Arc::new(Mutex::new(Some(callback_entered)));
    let callback_release_rx = Arc::new(Mutex::new(Some(callback_release_rx)));
    request.on_spawn = Some(Arc::new({
      let callback_entered = Arc::clone(&callback_entered);
      let callback_release_rx = Arc::clone(&callback_release_rx);
      move |_| {
        let callback_entered = callback_entered
          .lock()
          .expect("spawn callback channel state is not poisoned")
          .take()
          .expect("spawn callback runs once");
        let callback_release_rx = callback_release_rx
          .lock()
          .expect("spawn callback channel state is not poisoned")
          .take()
          .expect("spawn callback runs once");
        Box::pin(async move {
          callback_entered.send(()).expect("test waits for spawn callback");
          callback_release_rx.await.expect("test releases spawn callback");
          Ok(())
        })
      }
    }));

    let run = tokio::spawn(async move { runner.run(request).await });
    callback_entered_rx.await.expect("spawn callback starts");
    assert!(!marker.exists());
    callback_release.send(()).expect("spawn callback is waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(30), run).await.unwrap().unwrap().unwrap();

    assert!(marker.exists());
    assert_eq!(outcome.exit_code, Some(0));
  }

  #[tokio::test]
  async fn run_instructions_are_written_in_the_log_dir_without_touching_the_worktree() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("worktree");
    fs::create_dir(&cwd).unwrap();
    let logs = root.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let instructions = logs.join("harness_instructions.md");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new("printf '%s\\n' '{\"type\":\"result\"}'")),
      LaunchArgs {
        instructions_content: Some("synthetic harness rules".to_string()),
        instructions_file: Some(instructions.clone()),
        log_dir: Some(logs),
        ..LaunchArgs::default()
      },
    );

    runner
      .run(test_request(
        "prompt".to_string(),
        cwd.clone(),
        Env::from([("PATH".to_string(), "/bin".to_string())]),
        root.path().join("cgroups"),
      ))
      .await
      .unwrap();

    assert_eq!(fs::read_to_string(instructions).unwrap(), "synthetic harness rules");
    assert!(!cwd.join("HARNESS.md").exists());
  }

  #[tokio::test]
  async fn stderr_outcome_is_the_last_64_kilobytes() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new(r#"head -c 70000 /dev/zero | tr '\000' x >&2; printf '%s\n' '{"type":"result"}'"#)),
      LaunchArgs { log_dir: Some(logs.clone()), ..LaunchArgs::default() },
    );
    let outcome = runner
      .run(test_request(
        "prompt".to_string(),
        root.path().to_path_buf(),
        Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
        root.path().join("cgroups"),
      ))
      .await
      .unwrap();
    assert_eq!(outcome.stderr_text.len(), 64 * 1024);
    assert!(outcome.stderr_text.bytes().all(|byte| byte == b'x'));
    assert_eq!(fs::metadata(logs.join(STDERR_LOG_NAME)).unwrap().len(), 70_000);
  }

  #[tokio::test]
  async fn a_child_that_hangs_after_result_returns_process_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new(r#"printf '%s\n' '{"type":"result"}'; kill -STOP $$"#)),
      LaunchArgs { log_dir: Some(logs.clone()), ..LaunchArgs::default() },
    );
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
      root.path().join("cgroups"),
    );
    request.post_result_timeout = Duration::from_millis(40);
    request.cleanup_timeout = Duration::from_millis(40);

    let outcome = runner.run(request).await.unwrap();

    let diagnostics = outcome.hang_diagnostics.unwrap();
    assert!(diagnostics["captured_at"].is_string());
    assert!(diagnostics["process_tree"].is_string());
    assert!(diagnostics["status"].is_string());
    assert!(diagnostics["fds"].is_string());
    assert!(diagnostics["children"].is_string());
    assert!(outcome.exit_code.is_some_and(|code| code != 0));
    assert_eq!(fs::read_to_string(logs.join(RAW_LOG_NAME)).unwrap(), "{\"type\":\"result\"}\n");
  }

  #[test]
  fn diagnostic_commands_are_the_probe_strings() {
    assert_eq!(
      diagnostic_commands(4321, 4000),
      [
        ("process_tree", "ps --forest -o pid,pgid,stat,etime,wchan,cmd -g 4000 2>&1".to_string()),
        ("fds", "ls -l /proc/4321/fd 2>&1 | head -200".to_string()),
        ("status", "cat /proc/4321/status 2>&1".to_string()),
        ("children", "ps -o pid,stat,etime,cmd --ppid 4321 2>&1".to_string()),
      ]
    );
  }

  #[tokio::test]
  async fn capture_keeps_stdout_and_stderr_in_write_order() {
    let args = ["-c".to_string(), "echo one; echo two >&2; echo three".to_string()];
    let captured = capture_command(std::process::id(), "probe", "bash", &args).await;
    assert_eq!(captured, json!("one\ntwo\nthree\n"));
  }

  #[tokio::test]
  async fn fds_field_keeps_the_first_200_lines() {
    // This test process holds 250 descriptors, so `ls` lists more entries than the field keeps.
    let held: Vec<fs::File> = (0..250).map(|_| fs::File::open("/dev/null").unwrap()).collect();
    let diagnostics = capture_process_diagnostics(std::process::id()).await;
    assert_eq!(diagnostics["fds"].as_str().unwrap().lines().count(), 200);
    drop(held);
  }

  #[tokio::test]
  async fn a_probe_that_outlives_the_timeout_records_an_empty_error() {
    // The diagnostic timeout drops the probe, and `kill_on_drop` kills the `sleep`.
    let args = ["30".to_string()];
    let captured = capture_command(std::process::id(), "probe", "sleep", &args).await;
    assert_eq!(captured, json!("<capture failed: >"));
  }

  #[tokio::test]
  async fn outcome_reports_a_session_cgroup_memory_kill_from_the_injected_base() {
    let root = tempfile::tempdir().unwrap();
    let cgroup_base = root.path().join("user@1001.service");
    let agents_slice = charliebot_core::cgroup::agents_slice_dir(&cgroup_base);
    fs::create_dir_all(&agents_slice).unwrap();
    fs::write(agents_slice.join("cgroup.subtree_control"), "").unwrap();
    let cgroup_dir = agents_slice.join("charliebot-sess-test1234");
    fs::create_dir(&cgroup_dir).unwrap();
    // The control files the kernel publishes for a cgroup, which the fake tree must carry.
    for name in
      ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control", "memory.max", "memory.swap.max", "pids.max"]
    {
      fs::write(cgroup_dir.join(name), "").unwrap();
    }
    fs::write(cgroup_dir.join("memory.events"), "max 0\noom_kill 0\n").unwrap();
    fs::write(cgroup_dir.join("cgroup.procs"), "").unwrap();
    let script =
      format!("printf 'max 1\\noom_kill 0\\n' > {}; kill -KILL $$", cgroup_dir.join("memory.events").display());
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new(script)),
      LaunchArgs {
        log_dir: Some(root.path().join("logs")),
        cgroup_session_id: Some("test1234-session".to_string()),
        ..LaunchArgs::default()
      },
    );
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([("PATH".to_string(), "/bin".to_string())]),
      cgroup_base,
    );
    request.cgroup_config.memory_max_mb = 64;
    let outcome = runner.run(request).await.unwrap();
    assert_eq!(outcome.exit_code, Some(-libc::SIGKILL));
    assert!(outcome.cgroup_exit_report.as_deref().unwrap().contains("session 内存上限触发"));
  }

  /// Waits until the runner holds the spawned child, failing after 30 s.
  async fn wait_for_active_child(runner: &AgentRunner) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while runner.active.lock().expect("active process lock poisoned").is_none() {
      assert!(tokio::time::Instant::now() < deadline, "runner never acquired the child within 30 s");
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  }

  #[tokio::test]
  async fn terminate_signals_the_child_group_and_waits_for_exit() {
    let root = tempfile::tempdir().unwrap();
    let runner = Arc::new(AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new("exec /bin/sleep 30")),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    ));
    let run_runner = Arc::clone(&runner);
    let run_root = root.path().to_path_buf();
    let task = tokio::spawn(async move {
      run_runner
        .run(test_request(
          "prompt".to_string(),
          run_root.clone(),
          Env::from([("PATH".to_string(), "/bin".to_string())]),
          run_root.join("cgroups"),
        ))
        .await
    });
    wait_for_active_child(&runner).await;
    runner.terminate().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(30), task).await.unwrap().unwrap().unwrap();
    assert!(outcome.exit_code.is_some_and(|code| code != 0));
  }

  #[tokio::test]
  async fn refused_uploads_end_the_run_with_one_error_event_and_no_process() {
    let root = tempfile::tempdir().unwrap();
    let spawned = root.path().join("spawned");
    let runner = AgentRunner::new(
      "shell",
      Box::new(ShellBackend::refusing_uploads(format!("touch {}", spawned.display()), "image refused")),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([("PATH".to_string(), "/bin".to_string())]),
      root.path().join("cgroups"),
    );
    request.uploaded_files = vec![json!({"filename": "shot.png", "path": "/synthetic/shot.png"})];
    request.on_event = Arc::new(move |event| {
      let events = Arc::clone(&events);
      Box::pin(async move {
        events.lock().unwrap().push(event);
        Ok(())
      })
    });

    let outcome = runner.run(request).await.unwrap();

    assert_eq!(outcome.exit_code, None);
    assert_eq!(*seen.lock().unwrap(), vec![crate::event_builders::make_error_event("image refused")]);
    assert!(!spawned.exists());
  }

  #[tokio::test]
  async fn detach_stops_following_without_signalling_the_child() {
    let root = tempfile::tempdir().unwrap();
    let finished = root.path().join("finished");
    let release = root.path().join("release-child");
    let script = format!(
      "while [ ! -e '{}' ]; do /bin/sleep 0.01; done; touch '{}'; printf '%s\\n' '{{\"type\":\"result\"}}'",
      release.display(),
      finished.display()
    );
    let runner = Arc::new(AgentRunner::new(
      "shell",
      Box::new(ShellBackend::new(script)),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    ));
    let run_runner = Arc::clone(&runner);
    let run_root = root.path().to_path_buf();
    let task = tokio::spawn(async move {
      run_runner
        .run(test_request(
          "prompt".to_string(),
          run_root.clone(),
          Env::from([("PATH".to_string(), "/bin".to_string())]),
          run_root.join("cgroups"),
        ))
        .await
    });
    wait_for_active_child(&runner).await;
    runner.detach();
    let outcome = tokio::time::timeout(Duration::from_secs(30), task).await.unwrap().unwrap().unwrap();
    assert_eq!(outcome.exit_code, None);
    assert!(!finished.exists());
    fs::write(&release, "continue").unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
      while !finished.exists() {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("detached child reaches its finished marker");
    assert!(finished.exists());
  }

  #[tokio::test]
  async fn final_stdout_run_emits_the_backend_events_before_its_guard_error() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    fs::create_dir(&logs).unwrap();
    fs::write(logs.join(FINAL_STDOUT_LOG_NAME), "stale").unwrap();
    let runner = AgentRunner::new(
      "shell",
      Box::new(FinalStdoutShell::new("printf 'answer'; printf 'warned' >&2; exit 0", Some("envelope guard"), None)),
      LaunchArgs { log_dir: Some(logs.clone()), ..LaunchArgs::default() },
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let mut request = test_request(
      "prompt".to_string(),
      root.path().to_path_buf(),
      Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
      root.path().join("cgroups"),
    );
    request.on_event = Arc::new(move |event| {
      let events = Arc::clone(&events);
      Box::pin(async move {
        events.lock().unwrap().push(event);
        Ok(())
      })
    });

    let error = runner.run(request).await.unwrap_err();

    assert_eq!(error.to_string(), "envelope guard");
    assert_eq!(
      *seen.lock().unwrap(),
      vec![json!({"type": "seen", "stdout": "answer", "stderr": "warned", "exit_code": 0})]
    );
    assert_eq!(fs::read_to_string(logs.join(FINAL_STDOUT_LOG_NAME)).unwrap(), "answer");
    assert_eq!(fs::read_to_string(logs.join(STDERR_LOG_NAME)).unwrap(), "warned");
    assert_eq!(runner.exit_code(), Some(0));
  }

  #[tokio::test]
  async fn final_stdout_run_records_the_exit_code_its_backend_chose() {
    let root = tempfile::tempdir().unwrap();
    let runner = AgentRunner::new(
      "shell",
      Box::new(FinalStdoutShell::new("exit 3", None, Some(1))),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    );

    let outcome = runner
      .run(test_request(
        "prompt".to_string(),
        root.path().to_path_buf(),
        Env::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
        root.path().join("cgroups"),
      ))
      .await
      .unwrap();

    assert_eq!(outcome.exit_code, Some(1));
    assert_eq!(runner.exit_code(), Some(1));
  }

  #[tokio::test]
  async fn final_stdout_capture_preserves_normal_and_signal_exit_codes() {
    for (script, code) in [("exit 3", 3), ("exit 137", 137), ("kill -TERM $$", -15)] {
      let root = tempfile::tempdir().unwrap();
      let runner = AgentRunner::new(
        "shell",
        Box::new(FinalStdoutShell::new(script, None, None)),
        LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
      );
      let outcome = runner
        .run(test_request("test".into(), root.path().to_path_buf(), Env::new(), root.path().join("cgroups")))
        .await
        .unwrap();
      assert_eq!(outcome.exit_code, Some(code));
    }
  }

  #[tokio::test]
  async fn final_stdout_stop_preserves_the_signal_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let runner = Arc::new(AgentRunner::new(
      "shell",
      Box::new(FinalStdoutShell::new("exec sleep 300", None, None)),
      LaunchArgs { log_dir: Some(root.path().join("logs")), ..LaunchArgs::default() },
    ));
    let mut request = test_request(
      "test".into(),
      root.path().to_path_buf(),
      Env::from([("PATH".into(), "/bin:/usr/bin".into())]),
      root.path().join("cgroups"),
    );
    let callback_runner = Arc::clone(&runner);
    request.on_spawn = Some(Arc::new(move |_| {
      let callback_runner = Arc::clone(&callback_runner);
      Box::pin(async move { callback_runner.terminate().await })
    }));
    let task_runner = Arc::clone(&runner);
    let mut run = tokio::spawn(async move { task_runner.run(request).await });
    let outcome = tokio::time::timeout(Duration::from_secs(5), &mut run).await;
    if outcome.is_err() {
      runner.terminate().await.unwrap();
      run.await.unwrap().unwrap();
      panic!("stop requested during spawn did not stop the child");
    }
    assert_eq!(outcome.unwrap().unwrap().unwrap().exit_code, Some(-15));
  }

  fn test_request(prompt: String, cwd: PathBuf, values: Env, cgroup_base: PathBuf) -> RunRequest {
    let cgroup_config = SessionCgroupConfig { base_dir: cgroup_base, ..SessionCgroupConfig::disabled() };
    RunRequest::new(prompt, cwd, AgentBaseEnvironment { values }, cgroup_config, Arc::new(Registry::new()))
  }

  /// The signal that the blocking pool is full, and the release that empties it.
  type PoolFill = (tokio::sync::oneshot::Sender<()>, std::sync::mpsc::Receiver<()>);

  struct ShellBackend {
    script: String,
    refusal: Option<String>,
    /// Taken by `prepare_env`, the launch's last step before its spawn, to hold one blocking-pool thread until release.
    pool_fill: Mutex<Option<PoolFill>>,
  }

  impl ShellBackend {
    fn new(script: impl Into<String>) -> Self {
      Self { script: script.into(), refusal: None, pool_fill: Mutex::new(None) }
    }

    fn refusing_uploads(script: impl Into<String>, refusal: &str) -> Self {
      Self { script: script.into(), refusal: Some(refusal.to_string()), pool_fill: Mutex::new(None) }
    }

    fn filling_the_blocking_pool(
      script: impl Into<String>,
      filled: tokio::sync::oneshot::Sender<()>,
      release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
      Self { script: script.into(), refusal: None, pool_fill: Mutex::new(Some((filled, release))) }
    }
  }

  /// A final-stdout backend whose finish reports the output and exit code it was given, so a test sees the runner's
  /// inputs. `exit_override` replaces the exit code it records, and `guard_error` ends the run after its events.
  struct FinalStdoutShell {
    script: String,
    guard_error: Option<String>,
    exit_override: Option<i32>,
  }

  impl FinalStdoutShell {
    fn new(script: &str, guard_error: Option<&str>, exit_override: Option<i32>) -> Self {
      Self { script: script.to_string(), guard_error: guard_error.map(str::to_string), exit_override }
    }
  }

  impl AgentBackend for FinalStdoutShell {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      None
    }
    fn prepare_transport(&mut self, _: &Path) -> anyhow::Result<()> {
      Ok(())
    }
    fn build_command(&self, _: &str) -> anyhow::Result<Vec<String>> {
      Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
    }
    fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
      Ok(env)
    }
    fn stdin_prompt(&self, _: &str) -> Option<String> {
      None
    }
    fn translator(&self) -> Box<dyn charliebot_registry::backend::Translate> {
      unreachable!("a final-stdout run reads no translator")
    }
    fn transport(&self) -> TransportKind {
      TransportKind::FinalStdout
    }
    fn finish_final_stdout(&mut self, stdout: &str, stderr: &str, exit_code: i32) -> FinalStdoutOutcome {
      FinalStdoutOutcome {
        events: vec![json!({"type": "seen", "stdout": stdout, "stderr": stderr, "exit_code": exit_code})],
        exit_code: self.exit_override.unwrap_or(exit_code),
        guard_error: self.guard_error.clone(),
      }
    }
  }

  impl AgentBackend for ShellBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      Some(("HARNESS.md", "harness_instructions_written"))
    }
    fn prepare_transport(&mut self, _: &Path) -> anyhow::Result<()> {
      Ok(())
    }
    fn accept_uploads(&mut self, _: &[Value]) -> Result<(), String> {
      match &self.refusal {
        Some(message) => Err(message.clone()),
        None => Ok(()),
      }
    }
    fn build_command(&self, _: &str) -> anyhow::Result<Vec<String>> {
      Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
    }
    fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
      if let Some((filled, release)) = self.pool_fill.lock().expect("pool fill state is not poisoned").take() {
        drop(tokio::task::spawn_blocking(move || release.recv()));
        filled.send(()).expect("the test waits for the filled pool");
      }
      Ok(env)
    }
    fn stdin_prompt(&self, _: &str) -> Option<String> {
      None
    }
    fn translator(&self) -> Box<dyn charliebot_registry::backend::Translate> {
      struct Identity;
      impl charliebot_registry::backend::Translate for Identity {
        fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
          Ok(vec![raw])
        }
      }
      Box::new(Identity)
    }
    fn transport(&self) -> TransportKind {
      TransportKind::RawLog
    }
    fn one_shot_text<'a>(
      &'a mut self,
      _: &'a Env,
      _: &'a str,
      _: &'a str,
      _: Option<Duration>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
      None
    }
  }
}
