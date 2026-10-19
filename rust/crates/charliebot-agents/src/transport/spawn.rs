//! Child setup keeps each transport's session and parent-death policy explicit.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use super::cgroup::{self, SessionCgroup};
use super::identity::{self, ProcessIdentity};

const TURN_TREE_NICE: i32 = 10;
const PUMP_CHUNK_BYTES: usize = 8192;
const LOG_FLUSH_BYTES: usize = 64 * 1024;
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Required process inputs for each transport spawn.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
  /// Executable path or name resolved by the caller.
  pub program: OsString,
  /// Argument vector without the executable.
  pub args: Vec<OsString>,
  /// Optional working directory.
  pub cwd: Option<PathBuf>,
  /// Complete child environment.
  pub env: HashMap<OsString, OsString>,
  /// Session id used for cgroup placement.
  pub session_id: Option<String>,
  /// Caller-supplied delegated cgroup base.
  pub cgroup_base: PathBuf,
  /// Session memory cap in megabytes; zero or below disables cgroups.
  pub memory_max_mb: i64,
  /// Session swap cap in megabytes.
  pub swap_max_mb: i64,
  /// Session task cap; zero or below writes `max`.
  pub tasks_max: i64,
}

impl SpawnSpec {
  /// Build a spawn request with every environment and cgroup value explicit.
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    program: impl Into<OsString>,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: HashMap<OsString, OsString>,
    session_id: Option<String>,
    cgroup_base: PathBuf,
    memory_max_mb: i64,
    swap_max_mb: i64,
    tasks_max: i64,
  ) -> Self {
    Self { program: program.into(), args, cwd, env, session_id, cgroup_base, memory_max_mb, swap_max_mb, tasks_max }
  }

  fn prepare_cgroup(&self) -> Option<SessionCgroup> {
    cgroup::prepare_session_cgroup(
      &self.cgroup_base,
      self.session_id.as_deref(),
      self.memory_max_mb,
      self.swap_max_mb,
      self.tasks_max,
    )
  }

  fn command(&self) -> Command {
    let mut command = Command::new(&self.program);
    command.args(&self.args).env_clear().envs(&self.env).kill_on_drop(false);
    if let Some(cwd) = &self.cwd {
      command.current_dir(cwd);
    }
    command
  }
}

/// Stdin shape for a raw-log child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawStdin {
  /// Inherit an input pipe from the backend caller.
  Piped,
  /// Connect `/dev/null` to stdin.
  Null,
}

/// Child handle and cgroup counters for exit attribution.
pub struct SpawnedChild {
  /// Spawned process handle.
  pub child: Child,
  /// Cgroup state captured before spawn, when enabled.
  pub cgroup: Option<SessionCgroup>,
}

/// Spawn a detached raw-log child with inherited append-only stdout and stderr files.
pub async fn spawn_raw_log(
  spec: &SpawnSpec,
  raw_path: &Path,
  stderr_path: &Path,
  stdin: RawStdin,
) -> io::Result<SpawnedChild> {
  if let Some(parent) = raw_path.parent() {
    fs::create_dir_all(parent)?;
  }
  if let Some(parent) = stderr_path.parent() {
    fs::create_dir_all(parent)?;
  }
  let raw = OpenOptions::new().create(true).append(true).mode(0o644).open(raw_path)?;
  let stderr = OpenOptions::new().create(true).append(true).mode(0o644).open(stderr_path)?;
  let cgroup = spec.prepare_cgroup();
  let mut command = spec.command();
  command
    .stdin(match stdin {
      RawStdin::Piped => std::process::Stdio::piped(),
      RawStdin::Null => std::process::Stdio::null(),
    })
    .stdout(std::process::Stdio::from(raw))
    .stderr(std::process::Stdio::from(stderr));
  configure_child_setup(&mut command, None);
  let child = spawn_command(command).await?;
  apply_turn_tree_limits(child.id().expect("spawned child has a process id"), cgroup.as_ref());
  Ok(SpawnedChild { child, cgroup })
}

/// Spawn a piped child with a new session and parent-death SIGKILL.
pub async fn spawn_piped(spec: &SpawnSpec) -> io::Result<SpawnedChild> {
  spawn_piped_with_signal(spec, Some(Signal::SIGKILL)).await
}

/// Spawn a one-shot child with `/dev/null` stdin, piped output, and optional parent-death signal.
pub async fn spawn_one_shot(spec: &SpawnSpec, pdeathsig: Option<Signal>) -> io::Result<SpawnedChild> {
  spawn_piped_with_signal(spec, pdeathsig).await
}

async fn spawn_piped_with_signal(spec: &SpawnSpec, pdeathsig: Option<Signal>) -> io::Result<SpawnedChild> {
  let cgroup = spec.prepare_cgroup();
  let mut command = spec.command();
  command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
  configure_child_setup(&mut command, pdeathsig);
  let child = spawn_command(command).await?;
  apply_turn_tree_limits(child.id().expect("spawned child has a process id"), cgroup.as_ref());
  Ok(SpawnedChild { child, cgroup })
}

async fn spawn_command(mut command: Command) -> io::Result<Child> {
  tokio::task::spawn_blocking(move || command.spawn()).await.map_err(io::Error::other)?
}

fn configure_child_setup(command: &mut Command, pdeathsig: Option<Signal>) {
  let parent_pid = unsafe { libc::getpid() };
  unsafe {
    command.as_std_mut().pre_exec(move || {
      if libc::setsid() == -1 {
        return Err(io::Error::last_os_error());
      }
      if let Some(signal) = pdeathsig {
        if libc::prctl(libc::PR_SET_PDEATHSIG, signal as libc::c_ulong, 0, 0, 0) == -1 {
          return Err(io::Error::last_os_error());
        }
        if libc::getppid() != parent_pid {
          libc::kill(libc::getpid(), libc::SIGKILL);
          return Err(io::Error::other("parent exited during child setup"));
        }
      }
      Ok(())
    });
  }
}

fn apply_turn_tree_limits(pid: u32, cgroup: Option<&SessionCgroup>) {
  if let Some(cgroup) = cgroup {
    cgroup::move_process_to_cgroup(pid, cgroup);
  }
  let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, TURN_TREE_NICE) };
  if result == -1 {
    let error = io::Error::last_os_error();
    tracing::warn!(target: "charliebot_agents", pid, error = %error, "turn_tree_nice_failed");
  }
}

/// Pin `pid_start`, then await the owner callback; a callback error terminates and reaps the child.
pub async fn pin_identity_and_on_spawn<E, F, Fut>(
  mut spawned: SpawnedChild,
  on_spawn: F,
) -> Result<(SpawnedChild, ProcessIdentity), E>
where
  F: FnOnce(ProcessIdentity) -> Fut,
  Fut: Future<Output = Result<(), E>>,
{
  let pid = spawned.child.id().expect("spawned child has a process id");
  let identity = identity::pin_process_identity(pid);
  if let Err(error) = on_spawn(identity.clone()).await {
    if let Err(terminate_error) = super::kill::terminate_child(&mut spawned.child).await {
      tracing::warn!(target: "charliebot_agents", pid, error = %terminate_error, "backend_terminate_timeout");
    }
    return Err(error);
  }
  Ok((spawned, identity))
}

pub(crate) async fn wait_until_stopped(child: &mut Child) -> io::Result<()> {
  let pid = child.id().expect("spawned child has a process id");
  let process = Pid::from_raw(i32::try_from(pid).expect("process id fits pid_t"));
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
  loop {
    match waitpid(process, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)) {
      Ok(WaitStatus::Stopped(_, _)) => return Ok(()),
      Ok(WaitStatus::StillAlive | WaitStatus::Continued(_)) => {}
      Ok(status) => return Err(io::Error::other(format!("spawn gate child ended before stopping: {status:?}"))),
      Err(nix::errno::Errno::EINTR) => continue,
      Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
    }
    if tokio::time::Instant::now() >= deadline {
      return Err(io::Error::new(io::ErrorKind::TimedOut, "spawn gate child did not stop within 30 s"));
    }
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
  }
}

pub(crate) fn continue_stopped_child(pid: u32) -> io::Result<()> {
  let process = Pid::from_raw(i32::try_from(pid).expect("process id fits pid_t"));
  nix::sys::signal::kill(process, Signal::SIGCONT).map_err(|error| io::Error::from_raw_os_error(error as i32))
}

/// Read and parse each complete NDJSON stream line, awaiting the caller for every valid event.
pub async fn iter_ndjson_events<R, F, Fut>(reader: &mut R, mut on_event: F) -> io::Result<()>
where
  R: AsyncBufRead + Unpin,
  F: FnMut(Value) -> Fut,
  Fut: Future<Output = io::Result<()>>,
{
  let mut line = Vec::new();
  loop {
    line.clear();
    if reader.read_until(b'\n', &mut line).await? == 0 {
      return Ok(());
    }
    if let Some(event) = parse_backend_line(&line) {
      on_event(event).await?;
    }
  }
}

pub(crate) fn parse_backend_line(line: &[u8]) -> Option<Value> {
  if line.is_empty() || line.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  match serde_json::from_slice(line) {
    Ok(event) => Some(event),
    Err(error) => match std::str::from_utf8(line) {
      Ok(_) => {
        tracing::debug!(target: "charliebot_agents", error = %error, "backend_line_not_json");
        None
      }
      Err(_) => match serde_json::from_str(&String::from_utf8_lossy(line)) {
        Ok(event) => Some(event),
        Err(repaired_error) => {
          tracing::debug!(target: "charliebot_agents", error = %repaired_error, "backend_line_not_json");
          None
        }
      },
    },
  }
}

/// Tee one async byte stream to a truncated log and retain its last 64 KB.
pub async fn stream_stderr<R>(mut reader: R, log_path: Option<&Path>) -> io::Result<Vec<u8>>
where
  R: AsyncRead + Unpin,
{
  tee_stream(&mut reader, log_path, |_| {}).await
}

/// Pump chunks to an optional log and callback while retaining the final 64 KB.
pub async fn tee_stream<R, F>(reader: &mut R, log_path: Option<&Path>, mut on_chunk: F) -> io::Result<Vec<u8>>
where
  R: AsyncRead + Unpin,
  F: FnMut(&[u8]),
{
  let mut output = match log_path {
    Some(path) => Some(tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path).await?),
    None => None,
  };
  let result: io::Result<Vec<u8>> = async {
    let mut tail = Vec::with_capacity(STDERR_TAIL_BYTES);
    let mut buffer = Vec::with_capacity(LOG_FLUSH_BYTES);
    let mut chunk = vec![0; PUMP_CHUNK_BYTES];
    loop {
      let count = reader.read(&mut chunk).await?;
      if count == 0 {
        break;
      }
      let data = &chunk[..count];
      on_chunk(data);
      if let Some(output_file) = output.as_mut() {
        buffer.extend_from_slice(data);
        if buffer.len() >= LOG_FLUSH_BYTES || count < PUMP_CHUNK_BYTES {
          output_file.write_all(&buffer).await?;
          buffer.clear();
        }
      }
      tail.extend_from_slice(data);
      if tail.len() > STDERR_TAIL_BYTES {
        let excess = tail.len() - STDERR_TAIL_BYTES;
        tail.drain(..excess);
      }
    }
    if !buffer.is_empty() {
      output.as_mut().expect("log file exists when buffer is nonempty").write_all(&buffer).await?;
    }
    Ok(tail)
  }
  .await;
  let flush_result = match output.as_mut() {
    Some(output_file) => output_file.flush().await,
    None => Ok(()),
  };
  match (result, flush_result) {
    (Ok(tail), Ok(())) => Ok(tail),
    (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
    (Err(error), Err(flush_error)) => {
      Err(io::Error::new(error.kind(), format!("{error}; flushing the log also failed: {flush_error}")))
    }
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::Duration;

  use super::*;

  fn spec(program: &str, args: &[&str], base: &Path) -> SpawnSpec {
    SpawnSpec::new(
      program,
      args.iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      None,
      base.to_path_buf(),
      0,
      0,
      0,
    )
  }

  #[tokio::test]
  async fn raw_log_spawn_inherits_append_files_and_session() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    let stderr = root.path().join("agent.stderr.log");
    fs::write(&raw, b"old\n").unwrap();
    let mut spawned = spawn_raw_log(
      &spec("/bin/sh", &["-c", "printf '%s ' \"$$\"; ps -o sid= -p $$; printf 'err\\n' >&2"], root.path()),
      &raw,
      &stderr,
      RawStdin::Null,
    )
    .await
    .unwrap();
    let pid = spawned.child.id().unwrap();
    assert_eq!(spawned.child.wait().await.unwrap().code(), Some(0));
    let raw_text = fs::read_to_string(&raw).unwrap();
    assert!(raw_text.starts_with("old\n"));
    let ids = raw_text
      .lines()
      .nth(1)
      .unwrap()
      .split_whitespace()
      .map(|value| value.parse::<u32>().unwrap())
      .collect::<Vec<_>>();
    assert_eq!(ids, [pid, pid]);
    assert_eq!(fs::read(&stderr).unwrap(), b"err\n");
  }

  #[tokio::test]
  async fn piped_spawn_returns_both_streams() {
    let root = tempfile::tempdir().unwrap();
    let mut spawned =
      spawn_piped(&spec("/bin/sh", &["-c", "printf 'out\\n'; printf 'err\\n' >&2"], root.path())).await.unwrap();
    let mut stdout = spawned.child.stdout.take().unwrap();
    let mut stderr = spawned.child.stderr.take().unwrap();
    let mut out_bytes = Vec::new();
    let mut err_bytes = Vec::new();
    stdout.read_to_end(&mut out_bytes).await.unwrap();
    stderr.read_to_end(&mut err_bytes).await.unwrap();
    assert_eq!(spawned.child.wait().await.unwrap().code(), Some(0));
    assert_eq!(out_bytes, b"out\n");
    assert_eq!(err_bytes, b"err\n");
  }

  #[tokio::test]
  async fn spawn_applies_nice_and_moves_into_the_session_cgroup_parent_side() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("app.slice");
    fs::create_dir(&base).unwrap();
    let cgroup_path = base.join("charliebot-sess-test1234");
    fs::create_dir(&cgroup_path).unwrap();
    fs::write(cgroup_path.join("cgroup.procs"), "").unwrap();
    let spec = SpawnSpec::new(
      "/bin/sleep",
      ["30"].into_iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      Some("test1234-session".into()),
      base,
      64,
      32,
      0,
    );
    let mut spawned = spawn_piped(&spec).await.unwrap();
    let pid = spawned.child.id().unwrap();
    assert_eq!(fs::read_to_string(cgroup_path.join("cgroup.procs")).unwrap(), pid.to_string());
    assert!(spawned.cgroup.is_some());
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    spawned.child.kill().await.unwrap();
    spawned.child.wait().await.unwrap();
    assert_eq!(nice, 10);
  }

  #[tokio::test]
  async fn one_shot_uses_null_stdin_and_piped_output() {
    let root = tempfile::tempdir().unwrap();
    let mut spawned = spawn_one_shot(
      &spec(
        "/bin/sh",
        &["-c", "if read line; then printf 'input\\n'; else printf 'eof\\n'; fi; printf 'err\\n' >&2"],
        root.path(),
      ),
      None,
    )
    .await
    .unwrap();
    let mut stdout = spawned.child.stdout.take().unwrap();
    let mut stderr = spawned.child.stderr.take().unwrap();
    let mut out_bytes = Vec::new();
    let mut err_bytes = Vec::new();
    stdout.read_to_end(&mut out_bytes).await.unwrap();
    stderr.read_to_end(&mut err_bytes).await.unwrap();
    assert_eq!(spawned.child.wait().await.unwrap().code(), Some(0));
    assert_eq!(out_bytes, b"eof\n");
    assert_eq!(err_bytes, b"err\n");
  }

  #[tokio::test]
  async fn pins_identity_before_awaiting_on_spawn_callback() {
    let root = tempfile::tempdir().unwrap();
    let spawned = spawn_piped(&spec("/bin/sleep", &["30"], root.path())).await.unwrap();
    let pid = spawned.child.id().unwrap();
    let (mut spawned, pinned) = pin_identity_and_on_spawn(spawned, |identity| async move {
      let stat = identity::read_pid_stat(identity.pid).unwrap();
      assert_eq!(identity.pid, pid);
      assert_eq!(identity.pid_start.as_deref(), Some(stat.pid_start.as_str()));
      Ok::<(), io::Error>(())
    })
    .await
    .unwrap();
    assert_eq!(pinned.pid, pid);
    super::super::kill::terminate_child(&mut spawned.child).await.unwrap();
  }

  #[tokio::test]
  async fn callback_failure_terminates_the_child() {
    let root = tempfile::tempdir().unwrap();
    let spawned = spawn_piped(&spec("/bin/sleep", &["30"], root.path())).await.unwrap();
    let pid = spawned.child.id().unwrap();
    let result = pin_identity_and_on_spawn(spawned, |_| async { Err("callback failed") }).await;
    assert!(matches!(result, Err("callback failed")));
    assert!(identity::read_pid_stat(pid).is_none());
  }

  #[tokio::test]
  async fn stdout_reader_skips_blank_and_malformed_lines() {
    let data = b"{\"type\":\"result\"}\n\nnot json\n{\"usage\":NaN}\n";
    let mut reader = tokio::io::BufReader::new(&data[..]);
    let mut events = Vec::new();
    iter_ndjson_events(&mut reader, |event| {
      events.push(event);
      async { Ok(()) }
    })
    .await
    .unwrap();
    assert_eq!(events, [serde_json::json!({"type":"result"})]);
  }

  #[tokio::test]
  async fn wait_timeout_kills_the_process_group() {
    let root = tempfile::tempdir().unwrap();
    let mut child = spawn_piped(&spec("/bin/sleep", &["30"], root.path())).await.unwrap().child;
    let pid = child.id().unwrap();
    let stderr_task = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
    let result =
      super::super::kill::wait_or_kill_group(child.wait(), Some(Duration::from_millis(10)), pid, stderr_task).await;
    assert!(result.is_err());
    assert_eq!(super::super::kill::python_exit_code(child.wait().await.unwrap()), -libc::SIGKILL);
  }

  #[tokio::test]
  async fn stderr_tee_keeps_only_the_last_64_kb() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("stderr.log");
    for pass in 0..200 {
      let tail_byte = !(pass as u8);
      let mut input = vec![pass as u8; 32];
      input.extend(vec![tail_byte; STDERR_TAIL_BYTES]);

      let tail = stream_stderr(&input[..], Some(&path)).await.unwrap();
      assert_eq!(tail, input[32..]);
      assert_eq!(fs::metadata(&path).unwrap().len(), input.len() as u64);
      let logged = fs::read(&path).unwrap();
      assert_eq!(&logged[logged.len() - STDERR_TAIL_BYTES..], &input[32..]);
    }
  }
}
