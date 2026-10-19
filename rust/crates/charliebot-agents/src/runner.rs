//! The raw-log `AgentBackend.run` template method.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, TransportKind};
use charliebot_registry::context::FeatureContext;
use charliebot_store::run_records::{RAW_LOG_NAME, STDERR_LOG_NAME};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::watch;

use crate::agent_environment::AgentBaseEnvironment;
use crate::launch_loop::{self, ChildEnvPolicy};
use crate::transport::cgroup::SessionCgroup;
use crate::transport::follow::{self, FollowSummary};
use crate::transport::identity::ProcessIdentity;
use crate::transport::kill::{self, python_exit_code};
use crate::transport::rotation::rotate_stale_transport;
use crate::transport::spawn::{self, RawStdin, SpawnSpec};

pub const DEFAULT_POST_RESULT_TIMEOUT: Duration = Duration::from_secs(90);
pub const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_TAIL_BYTES: u64 = 64 * 1024;
const DIAGNOSTIC_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// Callback invoked after the child identity is pinned and before tail-follow begins.
pub type OnSpawnFn = Arc<dyn Fn(ProcessIdentity) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Callback invoked once for each translated event.
pub type EventFn = Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Inputs owned by the runtime for one backend process.
pub struct RunRequest {
  pub prompt: String,
  pub cwd: PathBuf,
  pub agent_environment: AgentBaseEnvironment,
  pub child_env_policy: ChildEnvPolicy,
  pub environment_overlay: Env,
  pub registry: Arc<Registry>,
  pub session_id: Option<String>,
  pub cgroup_base: PathBuf,
  pub memory_max_mb: u64,
  pub swap_max_mb: u64,
  pub tasks_max: u64,
  pub on_spawn: Option<OnSpawnFn>,
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
    cgroup_base: PathBuf,
    registry: Arc<Registry>,
  ) -> Self {
    Self {
      prompt,
      cwd,
      agent_environment,
      child_env_policy: ChildEnvPolicy::RegisteredEdits,
      environment_overlay: Env::new(),
      registry,
      session_id: None,
      cgroup_base,
      memory_max_mb: 0,
      swap_max_mb: 0,
      tasks_max: 0,
      on_spawn: None,
      on_event: Arc::new(|_| Box::pin(async { Ok(()) })),
      post_result_timeout: DEFAULT_POST_RESULT_TIMEOUT,
      cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
    }
  }

  /// Construct a request from the environment snapshot inserted during startup.
  pub fn from_features(
    prompt: String,
    cwd: PathBuf,
    features: &FeatureContext,
    cgroup_base: PathBuf,
  ) -> anyhow::Result<Self> {
    let agent_environment = crate::agent_environment::require_agent_base_environment(&features.startup)?;
    Ok(Self::new(prompt, cwd, agent_environment.as_ref().clone(), cgroup_base, Arc::clone(&features.registry)))
  }
}

/// Process status and artifacts recorded after the run finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
  pub exit_code: Option<i32>,
  pub stderr_text: String,
  pub cgroup_exit_report: Option<String>,
  pub hang_diagnostics: Option<Value>,
  pub identity: ProcessIdentity,
  pub follow: FollowSummary,
}

struct ActiveProcess {
  pid: u32,
  exit: watch::Receiver<Option<Result<i32, String>>>,
  detached: AtomicBool,
}

/// One registered backend instance and its single raw-log launch.
pub struct AgentRunner {
  backend: tokio::sync::Mutex<Box<dyn AgentBackend>>,
  launch: LaunchArgs,
  active: Mutex<Option<Arc<ActiveProcess>>>,
  started: AtomicBool,
  terminated: AtomicBool,
}

impl AgentRunner {
  /// Create a runner from the backend built by `Registry`.
  pub fn new(backend: Box<dyn AgentBackend>, launch: LaunchArgs) -> Self {
    Self {
      backend: tokio::sync::Mutex::new(backend),
      launch,
      active: Mutex::new(None),
      started: AtomicBool::new(false),
      terminated: AtomicBool::new(false),
    }
  }

  /// Prepare, spawn and follow one registered raw-log backend.
  pub async fn run(&self, request: RunRequest) -> anyhow::Result<RunOutcome> {
    if self.started.swap(true, Ordering::AcqRel) {
      bail!("an AgentRunner can run only once");
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
    let raw_path = log_dir.join(RAW_LOG_NAME);
    let stderr_path = log_dir.join(STDERR_LOG_NAME);
    let cursor_path = log_dir.join(charliebot_store::run_records::CURSOR_NAME);
    rotate_stale_transport(&log_dir, &raw_path, &stderr_path, &cursor_path)?;

    let mut backend = self.backend.lock().await;
    if backend.transport() != TransportKind::RawLog {
      bail!("the agent runner does not support the Serve transport");
    }
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
    let buffer_limit = self.launch.buffer_limit.unwrap_or(follow::DEFAULT_BUFFER_LIMIT);
    let program = command.first().context("backend command is empty")?.clone();
    let spec = SpawnSpec::new(
      program,
      command.iter().skip(1).cloned().map(Into::into).collect(),
      Some(request.cwd),
      final_env.into_iter().map(|(key, value)| (key.into(), value.into())).collect::<HashMap<_, _>>(),
      self.launch.cgroup_session_id.clone(),
      request.cgroup_base,
      request.memory_max_mb,
      request.swap_max_mb,
      request.tasks_max,
    );
    let mut spawned = spawn::spawn_raw_log(
      &spec,
      &raw_path,
      &stderr_path,
      if stdin_prompt.is_some() { RawStdin::Piped } else { RawStdin::Null },
    )
    .await?;
    let stdin = spawned.child.stdin.take();
    let stdin_task = match stdin_prompt {
      Some(prompt) => {
        let mut stdin = stdin.context("agent stdin was not piped")?;
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

    let cgroup = spawned.cgroup.take();
    let child = spawned.child;
    let pid = identity.pid;
    let (sender, receiver) = watch::channel(None);
    tokio::spawn(reap_child(child, sender));
    let active = Arc::new(ActiveProcess { pid, exit: receiver, detached: AtomicBool::new(false) });
    *self.active.lock().expect("active process lock poisoned") = Some(Arc::clone(&active));

    drop(backend);

    let mut translator = self.backend.lock().await.translator();
    let translate = move |raw| translator.translate(raw);
    let exit_receiver = active.exit.clone();
    let follow_active = Arc::clone(&active);
    let is_alive = move || !follow_active.detached.load(Ordering::Acquire) && exit_receiver.borrow().is_none();
    let handler = request.on_event.clone();
    let follow = follow::tail_follow_events(
      &raw_path,
      translate,
      move |event| {
        let handler = handler.clone();
        async move { handler(event).await.map_err(|error| std::io::Error::other(error.to_string())) }
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
        identity,
        follow,
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
    Ok(RunOutcome { exit_code, stderr_text, cgroup_exit_report, hang_diagnostics, identity, follow })
  }

  /// Send SIGTERM to the process group and escalate to SIGKILL after five seconds.
  pub async fn terminate(&self) -> anyhow::Result<()> {
    self.terminated.store(true, Ordering::Release);
    let active = self.active.lock().expect("active process lock poisoned").clone();
    if let Some(active) = active {
      self.graceful_shutdown(&active, kill::KILL_ESCALATION_GRACE, "backend_terminate_timeout").await?;
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

  fn clear_active(&self, active: &Arc<ActiveProcess>) {
    let mut slot = self.active.lock().expect("active process lock poisoned");
    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, active)) {
      slot.take();
    }
  }
}

async fn reap_child(mut child: Child, sender: watch::Sender<Option<Result<i32, String>>>) {
  let result = child.wait().await.map(python_exit_code).map_err(|error| error.to_string());
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

async fn capture_process_diagnostics(pid: u32) -> Value {
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
  let captures = [
    (
      "process_tree",
      "ps",
      vec![
        "--forest".to_string(),
        "-o".to_string(),
        "pid,pgid,stat,etime,wchan,cmd".to_string(),
        "-g".to_string(),
        pgid_for_command.to_string(),
      ],
    ),
    ("fds", "ls", vec!["-l".to_string(), format!("/proc/{pid}/fd")]),
    ("status", "cat", vec![format!("/proc/{pid}/status")]),
    ("children", "ps", vec!["-o".to_string(), "pid,stat,etime,cmd".to_string(), "--ppid".to_string(), pid.to_string()]),
  ];
  for (key, program, args) in captures {
    diagnostics.insert(key.to_string(), capture_command(pid, key, program, &args).await);
  }
  Value::Object(diagnostics)
}

async fn capture_command(pid: u32, key: &str, program: &str, args: &[String]) -> Value {
  let mut command = tokio::process::Command::new(program);
  command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
  match tokio::time::timeout(DIAGNOSTIC_CAPTURE_TIMEOUT, command.output()).await {
    Ok(Ok(output)) => {
      let mut bytes = output.stdout;
      if !output.stderr.is_empty() {
        bytes.extend_from_slice(&output.stderr);
      }
      Value::String(String::from_utf8_lossy(&bytes).into_owned())
    }
    Ok(Err(error)) => {
      tracing::warn!(target: "charliebot_agents", pid, key, error = %error, "hang_diagnostics_capture_failed");
      Value::String(format!("<capture failed: {error}>"))
    }
    Err(error) => {
      tracing::warn!(target: "charliebot_agents", pid, key, error = %error, "hang_diagnostics_capture_failed");
      Value::String(format!("<capture failed: {error}>"))
    }
  }
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

fn read_stderr_tail(path: &Path) -> anyhow::Result<String> {
  let mut file = File::open(path).with_context(|| format!("opening stderr log {}", path.display()))?;
  let length = file.metadata()?.len();
  let start = length.saturating_sub(STDERR_TAIL_BYTES);
  file.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::with_capacity((length - start) as usize);
  file.read_to_end(&mut bytes)?;
  Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

#[cfg(test)]
mod tests {
  use std::fs;

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
    let runner = AgentRunner::new(backend, launch);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let mut request = test_request(
      "prompt".to_string(),
      cwd.clone(),
      Env::from([("PATH".to_string(), "/bin".to_string())]),
      root.path().join("cgroups"),
    );
    request.memory_max_mb = 0;
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
  async fn run_instructions_are_written_in_the_log_dir_without_touching_the_worktree() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("worktree");
    fs::create_dir(&cwd).unwrap();
    let logs = root.path().join("logs");
    fs::create_dir(&logs).unwrap();
    let instructions = logs.join("harness_instructions.md");
    let runner = AgentRunner::new(
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
      Box::new(ShellBackend::new(r#"printf '%s\n' '{"type":"result"}'; exec sleep 5"#)),
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

  #[tokio::test]
  async fn outcome_reports_a_session_cgroup_memory_kill_from_the_injected_base() {
    let root = tempfile::tempdir().unwrap();
    let cgroup_base = root.path().join("cgroups");
    fs::create_dir(&cgroup_base).unwrap();
    let cgroup_dir = cgroup_base.join("charliebot-sess-test1234");
    fs::create_dir(&cgroup_dir).unwrap();
    fs::write(cgroup_dir.join("memory.events"), "max 0\noom_kill 0\n").unwrap();
    fs::write(cgroup_dir.join("cgroup.procs"), "").unwrap();
    let script =
      format!("printf 'max 1\\noom_kill 0\\n' > {}; kill -KILL $$", cgroup_dir.join("memory.events").display());
    let runner = AgentRunner::new(
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
    request.memory_max_mb = 64;
    let outcome = runner.run(request).await.unwrap();
    assert_eq!(outcome.exit_code, Some(-libc::SIGKILL));
    assert!(outcome.cgroup_exit_report.as_deref().unwrap().contains("session 内存上限触发"));
  }

  /// Waits until the runner holds the spawned child, failing after 5 s.
  async fn wait_for_active_child(runner: &AgentRunner) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while runner.active.lock().expect("active process lock poisoned").is_none() {
      assert!(tokio::time::Instant::now() < deadline, "runner never acquired the child within 5 s");
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  }

  #[tokio::test]
  async fn terminate_signals_the_child_group_and_waits_for_exit() {
    let root = tempfile::tempdir().unwrap();
    let runner = Arc::new(AgentRunner::new(
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
    let outcome = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(outcome.exit_code.is_some_and(|code| code != 0));
  }

  #[tokio::test]
  async fn detach_stops_following_without_signalling_the_child() {
    let root = tempfile::tempdir().unwrap();
    let finished = root.path().join("finished");
    let script = format!("sleep 0.2; touch {}; printf '%s\\n' '{{\"type\":\"result\"}}'", finished.display());
    let runner = Arc::new(AgentRunner::new(
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
    let outcome = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert_eq!(outcome.exit_code, None);
    assert!(!finished.exists());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(finished.exists());
  }

  #[tokio::test]
  async fn registered_claude_child_edits_apply_at_the_spawn_boundary() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.backends.claude_code", crate::backends::claude_code::register).unwrap();
    let registry = Arc::new(registry);
    let inherited = Env::from([
      ("PATH".to_string(), "/bin".to_string()),
      ("CHARLIEBOT_SESSION_ID".to_string(), "stale-session".to_string()),
      ("CLAUDECODE".to_string(), "1".to_string()),
      ("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(), "0".to_string()),
    ]);
    let script = r#"printf '%s\n' '{"type":"result","session":"'"$CHARLIEBOT_SESSION_ID"'","claudecode":"'"$CLAUDECODE"'","auto_memory":"'"$CLAUDE_CODE_DISABLE_AUTO_MEMORY"'"}'"#;
    let master_event =
      run_one_shell(root.path(), "master", inherited.clone(), Arc::clone(&registry), Some("current-session"), script)
        .await;
    assert_eq!(master_event["session"], "current-session");
    assert_eq!(master_event["claudecode"], "");
    assert_eq!(master_event["auto_memory"], "1");

    let worker_event = run_one_shell(root.path(), "worker", inherited, registry, None, script).await;
    assert_eq!(worker_event["session"], "");
    assert_eq!(worker_event["claudecode"], "");
    assert_eq!(worker_event["auto_memory"], "1");
  }

  async fn run_one_shell(
    root: &Path,
    name: &str,
    env: Env,
    registry: Arc<Registry>,
    session_id: Option<&str>,
    script: &str,
  ) -> Value {
    let log_dir = root.join(name);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::clone(&seen);
    let runner = AgentRunner::new(
      Box::new(ShellBackend::new(script.to_string())),
      LaunchArgs { log_dir: Some(log_dir), ..LaunchArgs::default() },
    );
    let mut request = RunRequest::new(
      "prompt".to_string(),
      root.to_path_buf(),
      AgentBaseEnvironment { values: env },
      root.join("cgroups"),
      registry,
    );
    request.session_id = session_id.map(str::to_string);
    request.on_event = Arc::new(move |event| {
      let events = Arc::clone(&events);
      Box::pin(async move {
        events.lock().unwrap().push(event);
        Ok(())
      })
    });
    runner.run(request).await.unwrap();
    seen.lock().unwrap()[0].clone()
  }

  fn test_request(prompt: String, cwd: PathBuf, values: Env, cgroup_base: PathBuf) -> RunRequest {
    RunRequest::new(prompt, cwd, AgentBaseEnvironment { values }, cgroup_base, Arc::new(Registry::new()))
  }

  struct ShellBackend {
    script: String,
  }

  impl ShellBackend {
    fn new(script: impl Into<String>) -> Self {
      Self { script: script.into() }
    }
  }

  impl AgentBackend for ShellBackend {
    fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
      Some(("HARNESS.md", "harness_instructions_written"))
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
    ) -> BoxFuture<'a, anyhow::Result<String>> {
      Box::pin(async { bail!("not used") })
    }
  }
}
