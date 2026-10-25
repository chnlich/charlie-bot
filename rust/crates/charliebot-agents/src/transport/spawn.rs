//! Child setup follows the launch sequence in `src/runtime/agent_process/_vfkspawn.c:54-76` and
//! `src/runtime/agent_process/base.py:996-1013`: session and parent-death setup precede exec, and raw-log nice/cgroup
//! limits are applied parent-side after the exec handshake.
//!
//! Children with a parent-death signal spawn on one dedicated thread that lives for the process lifetime.
//! `PR_SET_PDEATHSIG` follows the thread that created the child, so those spawns avoid Tokio's retiring blocking pool.
//! Raw-log runs, codex and Claude one-shots, and Claude compaction use the blocking pool because they set no signal.
//!
//! Parent-side turn-tree setup sets nice to 10 after exec. Child-side turn-tree setup raises nice by 10 before exec.
//! Child-side cgroup setup moves the child before exec without changing nice.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::thread;

use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest, ReadBuf};
use tokio::sync::{oneshot, watch};

use super::cgroup::{self, SessionCgroup, SessionCgroupConfig};
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
  /// The session cgroup placement and caps of this spawn.
  pub cgroup: SessionCgroupConfig,
}

impl SpawnSpec {
  /// Build a spawn request with every environment and cgroup value explicit.
  pub fn new(
    program: impl Into<OsString>,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: HashMap<OsString, OsString>,
    session_id: Option<String>,
    cgroup: SessionCgroupConfig,
  ) -> Self {
    Self { program: program.into(), args, cwd, env, session_id, cgroup }
  }

  async fn prepare_cgroup(&self) -> io::Result<Option<SessionCgroup>> {
    cgroup::prepare_session_cgroup(&self.cgroup, self.session_id.as_deref()).await
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

/// Stdin shape for a process with captured output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildStdin {
  /// Connect `/dev/null` to stdin.
  Null,
  /// Keep stdin piped for a prompt writer.
  Piped,
}

/// Timing and scope of turn-tree setup for one child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnSetup {
  /// Set nice to 10 and move into the session cgroup after exec.
  ParentSideTurnTree,
  /// Raise nice by 10 and move into the session cgroup before exec.
  ChildSideTurnTree,
  /// Move into the session cgroup before exec without changing nice.
  ChildSideCgroup,
  /// Start a new session without turn-tree limits.
  None,
}

/// Child handle and cgroup counters for exit attribution.
pub struct SpawnedChild {
  /// Spawned process handle.
  pub child: AgentChild,
  /// Cgroup state captured before spawn, when enabled.
  pub cgroup: Option<SessionCgroup>,
}

/// The raw log path and identity of each child that `spawn_raw_log` spawned in this crate's tests. A test whose launch
/// is dropped before the spawn returns finds its child here.
#[cfg(test)]
pub(crate) static TEST_RAW_LOG_CHILDREN: std::sync::Mutex<Vec<(PathBuf, ProcessIdentity)>> =
  std::sync::Mutex::new(Vec::new());

/// Spawn a detached raw-log child with inherited append-only stdout and stderr files.
///
/// `claim` runs on the spawning thread right after the spawn, and its value travels with the child. A caller dropped
/// while the spawn is in flight never receives the child, and dropping that value is then the child's only cleanup.
pub async fn spawn_raw_log<C: Send + 'static>(
  spec: &SpawnSpec,
  raw_path: &Path,
  stderr_path: &Path,
  stdin: RawStdin,
  claim: impl FnOnce(&AgentChild) -> C + Send + 'static,
) -> io::Result<(SpawnedChild, C)> {
  if let Some(parent) = raw_path.parent() {
    fs::create_dir_all(parent)?;
  }
  if let Some(parent) = stderr_path.parent() {
    fs::create_dir_all(parent)?;
  }
  let raw = OpenOptions::new().create(true).append(true).mode(0o644).open(raw_path)?;
  let stderr = OpenOptions::new().create(true).append(true).mode(0o644).open(stderr_path)?;
  let cgroup = spec.prepare_cgroup().await?;
  let (child_stdin, parent_stdin) = match stdin {
    RawStdin::Piped => piped_stdin()?,
    RawStdin::Null => (devnull_fd()?, None),
  };
  let child_fds = ChildFds {
    stdin: child_stdin,
    stdout: ensure_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(raw.into_raw_fd()) })?,
    stderr: ensure_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(stderr.into_raw_fd()) })?,
  };
  let parent_io = ParentIo { stdin: parent_stdin, stdout: None, stderr: None };
  let prepared = PreparedSpawn::new(spec, None, SpawnSetup::ParentSideTurnTree, cgroup.clone(), child_fds)?;
  #[cfg(test)]
  let test_raw_path = raw_path.to_path_buf();
  let (child, claimed) = tokio::task::spawn_blocking(move || {
    let outcome = clone_vfork_exec(prepared)?;
    let child = AgentChild::new(outcome, parent_io, None)?;
    #[cfg(test)]
    TEST_RAW_LOG_CHILDREN
      .lock()
      .expect("test raw-log child list is not poisoned")
      .push((test_raw_path, identity::pin_process_identity(child.id().expect("spawned child has a process id"))));
    let claimed = claim(&child);
    io::Result::Ok((child, claimed))
  })
  .await
  .map_err(io::Error::other)??;
  apply_turn_tree_limits(child.id().expect("spawned child has a process id"), cgroup.as_ref());
  Ok((SpawnedChild { child, cgroup }, claimed))
}

/// Spawn a piped child with a new session and parent-death SIGKILL.
pub async fn spawn_piped(spec: &SpawnSpec) -> io::Result<SpawnedChild> {
  spawn_with_setup(spec, Some(Signal::SIGKILL), ChildStdin::Null, SpawnSetup::ParentSideTurnTree).await
}

/// Spawn a one-shot child with `/dev/null` stdin, piped output, and optional parent-death signal.
pub async fn spawn_one_shot(spec: &SpawnSpec, pdeathsig: Option<Signal>) -> io::Result<SpawnedChild> {
  spawn_with_setup(spec, pdeathsig, ChildStdin::Null, SpawnSetup::ParentSideTurnTree).await
}

/// Spawn a child with captured output and an explicit session and turn-tree setup policy.
pub async fn spawn_with_setup(
  spec: &SpawnSpec,
  pdeathsig: Option<Signal>,
  stdin: ChildStdin,
  setup: SpawnSetup,
) -> io::Result<SpawnedChild> {
  let cgroup = match setup {
    SpawnSetup::None => None,
    SpawnSetup::ParentSideTurnTree | SpawnSetup::ChildSideTurnTree | SpawnSetup::ChildSideCgroup => {
      spec.prepare_cgroup().await?
    }
  };
  let (child_fds, parent_io) = captured_io(stdin)?;
  let prepared = PreparedSpawn::new(spec, pdeathsig, setup, cgroup.clone(), child_fds)?;
  let (outcome, status) = spawn_prepared(prepared).await?;
  let child = AgentChild::new(outcome, parent_io, status)?;
  if setup == SpawnSetup::ParentSideTurnTree {
    apply_turn_tree_limits(child.id().expect("spawned child has a process id"), cgroup.as_ref());
  }
  Ok(SpawnedChild { child, cgroup })
}

async fn spawn_prepared(
  prepared: PreparedSpawn,
) -> io::Result<(VforkOutcome, Option<watch::Receiver<Option<Result<libc::c_int, libc::c_int>>>>)> {
  if prepared.pdeath_signal.is_some() {
    let (response, result) = oneshot::channel();
    thread::Builder::new().name("charliebot-parent-death-spawn".to_string()).spawn(move || {
      match clone_vfork_exec(prepared) {
        Ok(outcome) => {
          let pid = outcome.pid;
          let (status_sender, status_receiver) = watch::channel(None);
          if let Err(Ok((outcome, _))) = response.send(Ok((outcome, status_receiver))) {
            report_unclaimed_parent_death_spawn(Ok(outcome));
            return;
          }
          reap_process(pid, status_sender);
        }
        Err(error) => {
          if let Err(Err(error)) = response.send(Err(error)) {
            tracing::warn!(target: "charliebot_agents", error = %error, "parent_death_spawn_error_owner_gone");
          }
        }
      }
    })?;
    return result.await.map_err(io::Error::other)?.map(|(outcome, status)| (outcome, Some(status)));
  }
  let outcome = tokio::task::spawn_blocking(move || clone_vfork_exec(prepared)).await.map_err(io::Error::other)??;
  Ok((outcome, None))
}

fn report_unclaimed_parent_death_spawn(result: io::Result<VforkOutcome>) {
  match result {
    Ok(outcome) => {
      let pid = outcome.pid;
      super::kill::kill_process_group(pid, Signal::SIGKILL);
      if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        tracing::warn!(target: "charliebot_agents", pid, error = %error, "unclaimed_spawn_kill_failed");
      }
      reap_pid(pid);
      tracing::warn!(target: "charliebot_agents", pid, "parent_death_spawn_owner_gone");
    }
    Err(error) => {
      tracing::warn!(target: "charliebot_agents", error = %error, "parent_death_spawn_error_owner_gone");
    }
  }
}

struct ChildFds {
  stdin: OwnedFd,
  stdout: OwnedFd,
  stderr: OwnedFd,
}

struct ParentIo {
  stdin: Option<AgentPipeWriter>,
  stdout: Option<AgentPipeReader>,
  stderr: Option<AgentPipeReader>,
}

struct PreparedSpawn {
  argv: Vec<CString>,
  envp: Vec<CString>,
  exec_paths: Vec<CString>,
  cwd: Option<CString>,
  cgroup_path: Option<CString>,
  cgroup_oom_score_adj: libc::c_int,
  pdeath_signal: Option<libc::c_int>,
  child_nice: bool,
  fds: ChildFds,
}

impl PreparedSpawn {
  fn new(
    spec: &SpawnSpec,
    pdeathsig: Option<Signal>,
    setup: SpawnSetup,
    cgroup: Option<SessionCgroup>,
    fds: ChildFds,
  ) -> io::Result<Self> {
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(c_string(spec.program.as_os_str().as_bytes())?);
    argv.extend(spec.args.iter().map(|arg| c_string(arg.as_os_str().as_bytes())).collect::<io::Result<Vec<_>>>()?);
    let envp = spec
      .env
      .iter()
      .map(|(key, value)| {
        let mut item = Vec::with_capacity(key.as_os_str().as_bytes().len() + value.as_os_str().as_bytes().len() + 1);
        item.extend_from_slice(key.as_os_str().as_bytes());
        item.push(b'=');
        item.extend_from_slice(value.as_os_str().as_bytes());
        c_string(&item)
      })
      .collect::<io::Result<Vec<_>>>()?;
    let program = spec.program.as_os_str().as_bytes();
    // The non-vfork Python path delegates command lookup to Popen's PATH search. Prepare those candidates here so
    // the CLONE_VM child can stay syscall-only instead of calling execvpe (`src/runtime/agent_process/spawn.py:177-190`).
    let exec_paths = if program.contains(&b'/') {
      vec![c_string(program)?]
    } else {
      let path = spec.env.get(OsStr::new("PATH")).map(|value| value.as_os_str().as_bytes()).unwrap_or(b"/bin:/usr/bin");
      path
        .split(|byte| *byte == b':')
        .map(|directory| {
          let mut candidate = Vec::with_capacity(directory.len() + program.len() + 1);
          candidate.extend_from_slice(directory);
          if !directory.is_empty() {
            candidate.push(b'/');
          }
          candidate.extend_from_slice(program);
          c_string(&candidate)
        })
        .collect::<io::Result<Vec<_>>>()?
    };
    let cwd = spec.cwd.as_ref().map(|path| c_string(path.as_os_str().as_bytes())).transpose()?;
    let (child_cgroup, child_oom_score_adj) = match (setup, cgroup.as_ref()) {
      (SpawnSetup::ChildSideTurnTree | SpawnSetup::ChildSideCgroup, Some(state)) => (
        Some(c_string(state.path.join("cgroup.procs").as_os_str().as_bytes())?),
        libc::c_int::try_from(state.oom_score_adj()).expect("oom_score_adj fits c_int"),
      ),
      _ => (None, 0),
    };
    Ok(Self {
      argv,
      envp,
      exec_paths,
      cwd,
      cgroup_path: child_cgroup,
      cgroup_oom_score_adj: child_oom_score_adj,
      pdeath_signal: pdeathsig.map(|signal| signal as libc::c_int),
      child_nice: setup == SpawnSetup::ChildSideTurnTree,
      fds,
    })
  }
}

struct VforkOutcome {
  pid: u32,
  #[cfg(test)]
  clone_calls: usize,
  #[cfg(test)]
  clone_flags: libc::c_int,
}

struct ChildSetupArgs {
  argv: *const *const libc::c_char,
  envp: *const *const libc::c_char,
  exec_paths: *const *const libc::c_char,
  exec_path_count: usize,
  cwd: *const libc::c_char,
  cgroup_path: *const libc::c_char,
  cgroup_oom_score_adj: libc::c_int,
  stdin_fd: RawFd,
  stdout_fd: RawFd,
  stderr_fd: RawFd,
  report_fd: RawFd,
  parent_pid: libc::pid_t,
  pdeath_signal: libc::c_int,
  child_nice: bool,
  oldmask: libc::sigset_t,
}

/// A child handle backed by a CLONE_VM | CLONE_VFORK exec handshake. The persistent reaper keeps waitpid off Tokio.
pub struct AgentChild {
  pid: u32,
  status: watch::Receiver<Option<Result<libc::c_int, libc::c_int>>>,
  status_observed: bool,
  pub stdin: Option<AgentPipeWriter>,
  pub stdout: Option<AgentPipeReader>,
  pub stderr: Option<AgentPipeReader>,
  #[cfg(test)]
  clone_calls: usize,
  #[cfg(test)]
  clone_flags: libc::c_int,
}

impl AgentChild {
  fn new(
    outcome: VforkOutcome,
    io: ParentIo,
    status: Option<watch::Receiver<Option<Result<libc::c_int, libc::c_int>>>>,
  ) -> io::Result<Self> {
    let pid = outcome.pid;
    let status = match status {
      Some(status) => status,
      None => {
        let (sender, status) = watch::channel(None);
        if let Err(error) =
          thread::Builder::new().name(format!("agent-reap-{pid}")).spawn(move || reap_process(pid, sender))
        {
          super::kill::kill_process_group(pid, Signal::SIGKILL);
          reap_pid(pid);
          return Err(error);
        }
        status
      }
    };
    Ok(Self {
      pid,
      status,
      status_observed: false,
      stdin: io.stdin,
      stdout: io.stdout,
      stderr: io.stderr,
      #[cfg(test)]
      clone_calls: outcome.clone_calls,
      #[cfg(test)]
      clone_flags: outcome.clone_flags,
    })
  }

  pub fn id(&self) -> Option<u32> {
    (!self.status_observed).then_some(self.pid)
  }

  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    match *self.status.borrow() {
      Some(Ok(status)) => {
        self.status_observed = true;
        Ok(Some(ExitStatus::from_raw(status)))
      }
      Some(Err(errno)) => {
        self.status_observed = true;
        Err(io::Error::from_raw_os_error(errno))
      }
      None => Ok(None),
    }
  }

  pub async fn wait(&mut self) -> io::Result<ExitStatus> {
    self.stdin.take();
    loop {
      if let Some(result) = *self.status.borrow() {
        self.status_observed = true;
        return result.map(ExitStatus::from_raw).map_err(io::Error::from_raw_os_error);
      }
      self
        .status
        .changed()
        .await
        .map_err(|_| io::Error::other("agent child reaper stopped before publishing status"))?;
    }
  }

  pub fn start_kill(&mut self) -> io::Result<()> {
    if self.try_wait()?.is_some() {
      return Ok(());
    }
    if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) } == -1 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  }

  pub async fn kill(&mut self) -> io::Result<()> {
    self.start_kill()?;
    self.wait().await.map(|_| ())
  }

  pub async fn wait_with_output(mut self) -> io::Result<Output> {
    self.stdin.take();
    let stdout = self.stdout.take();
    let stderr = self.stderr.take();
    let (stdout, stderr, status) = tokio::join!(read_pipe_to_end(stdout), read_pipe_to_end(stderr), self.wait());
    Ok(Output { status: status?, stdout: stdout?, stderr: stderr? })
  }
}

/// A nonblocking pipe reader registered with Tokio's IO driver.
pub struct AgentPipeReader {
  fd: AsyncFd<OwnedFd>,
}

impl AgentPipeReader {
  fn new(fd: OwnedFd) -> io::Result<Self> {
    // SAFETY: `fd` owns a stable open descriptor for the entire AsyncFd lifetime.
    Ok(Self { fd: unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE)? } })
  }
}

impl AsyncRead for AgentPipeReader {
  fn poll_read(
    self: std::pin::Pin<&mut Self>,
    context: &mut std::task::Context<'_>,
    buffer: &mut ReadBuf<'_>,
  ) -> std::task::Poll<io::Result<()>> {
    let this = self.get_mut();
    if buffer.remaining() == 0 {
      return std::task::Poll::Ready(Ok(()));
    }
    loop {
      let mut ready = std::task::ready!(this.fd.poll_read_ready(context))?;
      let available = buffer.initialize_unfilled();
      match ready.try_io(|fd| {
        let count = unsafe { libc::read(fd.get_ref().as_raw_fd(), available.as_mut_ptr().cast(), available.len()) };
        if count < 0 { Err(io::Error::last_os_error()) } else { Ok(count as usize) }
      }) {
        Ok(Ok(count)) => {
          buffer.advance(count);
          return std::task::Poll::Ready(Ok(()));
        }
        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
        Ok(Err(error)) => return std::task::Poll::Ready(Err(error)),
        Err(_) => continue,
      }
    }
  }
}

/// A nonblocking pipe writer registered with Tokio's IO driver.
pub struct AgentPipeWriter {
  fd: Option<AsyncFd<OwnedFd>>,
}

impl AgentPipeWriter {
  fn new(fd: OwnedFd) -> io::Result<Self> {
    // SAFETY: `fd` owns a stable open descriptor for the entire AsyncFd lifetime.
    Ok(Self { fd: Some(unsafe { AsyncFd::register_with_interest(fd, Interest::WRITABLE)? }) })
  }
}

impl AsyncWrite for AgentPipeWriter {
  fn poll_write(
    self: std::pin::Pin<&mut Self>,
    context: &mut std::task::Context<'_>,
    buffer: &[u8],
  ) -> std::task::Poll<io::Result<usize>> {
    let this = self.get_mut();
    if buffer.is_empty() {
      return std::task::Poll::Ready(Ok(0));
    }
    loop {
      let Some(fd) = this.fd.as_ref() else {
        return std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "agent stdin is closed")));
      };
      let mut ready = std::task::ready!(fd.poll_write_ready(context))?;
      match ready.try_io(|fd| {
        let count = unsafe { libc::write(fd.get_ref().as_raw_fd(), buffer.as_ptr().cast(), buffer.len()) };
        if count < 0 { Err(io::Error::last_os_error()) } else { Ok(count as usize) }
      }) {
        Ok(Ok(count)) => return std::task::Poll::Ready(Ok(count)),
        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
        Ok(Err(error)) => return std::task::Poll::Ready(Err(error)),
        Err(_) => continue,
      }
    }
  }

  fn poll_flush(
    self: std::pin::Pin<&mut Self>,
    _context: &mut std::task::Context<'_>,
  ) -> std::task::Poll<io::Result<()>> {
    std::task::Poll::Ready(Ok(()))
  }

  fn poll_shutdown(
    self: std::pin::Pin<&mut Self>,
    _context: &mut std::task::Context<'_>,
  ) -> std::task::Poll<io::Result<()>> {
    self.get_mut().fd.take();
    std::task::Poll::Ready(Ok(()))
  }
}

async fn read_pipe_to_end(mut pipe: Option<AgentPipeReader>) -> io::Result<Vec<u8>> {
  let mut bytes = Vec::new();
  if let Some(pipe) = pipe.as_mut() {
    pipe.read_to_end(&mut bytes).await?;
  }
  Ok(bytes)
}

fn captured_io(stdin: ChildStdin) -> io::Result<(ChildFds, ParentIo)> {
  let (child_stdin, parent_stdin) = match stdin {
    ChildStdin::Null => (devnull_fd()?, None),
    ChildStdin::Piped => piped_stdin()?,
  };
  let (child_stdout, parent_stdout) = piped_reader()?;
  let (child_stderr, parent_stderr) = piped_reader()?;
  Ok((
    ChildFds { stdin: child_stdin, stdout: child_stdout, stderr: child_stderr },
    ParentIo { stdin: parent_stdin, stdout: Some(parent_stdout), stderr: Some(parent_stderr) },
  ))
}

fn piped_stdin() -> io::Result<(OwnedFd, Option<AgentPipeWriter>)> {
  let (child_read, parent_write) = pipe_pair()?;
  set_nonblocking(parent_write.as_raw_fd())?;
  Ok((child_read, Some(AgentPipeWriter::new(parent_write)?)))
}

fn piped_reader() -> io::Result<(OwnedFd, AgentPipeReader)> {
  let (parent_read, child_write) = pipe_pair()?;
  set_nonblocking(parent_read.as_raw_fd())?;
  Ok((child_write, AgentPipeReader::new(parent_read)?))
}

fn pipe_pair() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [-1; 2];
  if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
    return Err(io::Error::last_os_error());
  }
  let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
  let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
  Ok((ensure_fd_above_stdio(read)?, ensure_fd_above_stdio(write)?))
}

fn devnull_fd() -> io::Result<OwnedFd> {
  OpenOptions::new()
    .read(true)
    .open("/dev/null")
    .and_then(|file| ensure_fd_above_stdio(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) }))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags == -1 {
    return Err(io::Error::last_os_error());
  }
  if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
  CString::new(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn ensure_fd_above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
  if fd.as_raw_fd() > libc::STDERR_FILENO {
    return Ok(fd);
  }
  duplicate_cloexec(fd.as_raw_fd(), libc::STDERR_FILENO + 1)
}

fn clone_vfork_exec(prepared: PreparedSpawn) -> io::Result<VforkOutcome> {
  let mut argv = prepared.argv.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
  let mut envp = prepared.envp.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
  let exec_paths = prepared.exec_paths.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
  argv.push(std::ptr::null());
  envp.push(std::ptr::null());
  let (report_read, report_base_write) = pipe_pair()?;
  let report_write = duplicate_cloexec(report_base_write.as_raw_fd(), 10)?;
  drop(report_base_write);
  let stack_size = 256 * 1024;
  let stack = unsafe {
    libc::mmap(
      std::ptr::null_mut(),
      stack_size,
      libc::PROT_READ | libc::PROT_WRITE,
      libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
      -1,
      0,
    )
  };
  if stack == libc::MAP_FAILED {
    return Err(io::Error::last_os_error());
  }
  let mut oldmask: libc::sigset_t = unsafe { std::mem::zeroed() };
  let mut all_signals: libc::sigset_t = unsafe { std::mem::zeroed() };
  if unsafe { libc::sigfillset(&mut all_signals) } == -1 {
    unsafe { libc::munmap(stack, stack_size) };
    return Err(io::Error::last_os_error());
  }
  let mask_result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &all_signals, &mut oldmask) };
  if mask_result != 0 {
    unsafe { libc::munmap(stack, stack_size) };
    return Err(io::Error::from_raw_os_error(mask_result));
  }
  let mut args = ChildSetupArgs {
    argv: argv.as_ptr(),
    envp: envp.as_ptr(),
    exec_paths: exec_paths.as_ptr(),
    exec_path_count: exec_paths.len(),
    cwd: prepared.cwd.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
    cgroup_path: prepared.cgroup_path.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
    cgroup_oom_score_adj: prepared.cgroup_oom_score_adj,
    stdin_fd: prepared.fds.stdin.as_raw_fd(),
    stdout_fd: prepared.fds.stdout.as_raw_fd(),
    stderr_fd: prepared.fds.stderr.as_raw_fd(),
    report_fd: report_write.as_raw_fd(),
    parent_pid: unsafe { libc::getpid() },
    pdeath_signal: prepared.pdeath_signal.unwrap_or(0),
    child_nice: prepared.child_nice,
    oldmask,
  };
  let flags = libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD;
  #[cfg(test)]
  let clone_calls = 1;
  let pid = unsafe {
    libc::clone(child_main, (stack as *mut u8).add(stack_size).cast(), flags, (&mut args as *mut ChildSetupArgs).cast())
  };
  let clone_error = io::Error::last_os_error();
  let restore_result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &oldmask, std::ptr::null_mut()) };
  unsafe { libc::munmap(stack, stack_size) };
  drop(report_write);
  if restore_result != 0 {
    if pid > 0 {
      unsafe { libc::kill(pid, libc::SIGKILL) };
      reap_pid(pid as u32);
    }
    return Err(io::Error::from_raw_os_error(restore_result));
  }
  if pid == -1 {
    return Err(clone_error);
  }
  let error_number = match read_spawn_error(report_read.as_raw_fd()) {
    Ok(error_number) => error_number,
    Err(error) => {
      unsafe { libc::kill(pid, libc::SIGKILL) };
      reap_pid(pid as u32);
      return Err(error);
    }
  };
  if let Some(error_number) = error_number {
    reap_pid(pid as u32);
    return Err(io::Error::from_raw_os_error(error_number));
  }
  Ok(VforkOutcome {
    pid: pid as u32,
    #[cfg(test)]
    clone_calls,
    #[cfg(test)]
    clone_flags: flags,
  })
}

fn duplicate_cloexec(fd: RawFd, minimum: RawFd) -> io::Result<OwnedFd> {
  let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
  if duplicate == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn read_spawn_error(fd: RawFd) -> io::Result<Option<libc::c_int>> {
  let mut errno_bytes = [0u8; std::mem::size_of::<libc::c_int>()];
  let mut read = 0;
  while read < errno_bytes.len() {
    let count = unsafe { libc::read(fd, errno_bytes[read..].as_mut_ptr().cast(), errno_bytes.len() - read) };
    if count == 0 {
      return if read == 0 {
        Ok(None)
      } else {
        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "agent spawn error pipe ended mid-errno"))
      };
    }
    if count == -1 {
      let error = io::Error::last_os_error();
      if error.kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return Err(error);
    }
    read += count as usize;
  }
  Ok(Some(libc::c_int::from_ne_bytes(errno_bytes)))
}

extern "C" fn child_main(argument: *mut libc::c_void) -> libc::c_int {
  let args = unsafe { &*(argument.cast::<ChildSetupArgs>()) };
  unsafe {
    if args.pdeath_signal != 0 {
      if libc::prctl(libc::PR_SET_PDEATHSIG, args.pdeath_signal as libc::c_ulong, 0, 0, 0) == -1 {
        report_spawn_error(args, current_errno());
      }
      if libc::getppid() != args.parent_pid {
        report_spawn_error(args, libc::EPERM);
      }
    }
    if libc::setsid() == -1 {
      report_spawn_error(args, current_errno());
    }
    if args.child_nice {
      *libc::__errno_location() = 0;
      if libc::nice(TURN_TREE_NICE) == -1 && current_errno() != 0 {
        report_spawn_error(args, current_errno());
      }
    }
    if !args.cgroup_path.is_null() {
      move_child_to_cgroup(args, args.cgroup_path);
    }
    if libc::dup2(args.stdin_fd, libc::STDIN_FILENO) == -1
      || libc::dup2(args.stdout_fd, libc::STDOUT_FILENO) == -1
      || libc::dup2(args.stderr_fd, libc::STDERR_FILENO) == -1
    {
      report_spawn_error(args, current_errno());
    }
    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
    if libc::syscall(libc::SYS_close_range, 3u32, (args.report_fd - 1) as u32, 0u32) == -1
      || libc::syscall(libc::SYS_close_range, (args.report_fd + 1) as u32, u32::MAX, 0u32) == -1
    {
      report_spawn_error(args, current_errno());
    }
    if !args.cwd.is_null() && libc::chdir(args.cwd) == -1 {
      report_spawn_error(args, current_errno());
    }
    if libc::sigprocmask(libc::SIG_SETMASK, &args.oldmask, std::ptr::null_mut()) == -1 {
      report_spawn_error(args, current_errno());
    }
    let mut denied = false;
    let mut index = 0;
    while index < args.exec_path_count {
      libc::execve(*args.exec_paths.add(index), args.argv, args.envp);
      let error = current_errno();
      if error == libc::EACCES {
        denied = true;
      } else if error != libc::ENOENT && error != libc::ENOTDIR {
        report_spawn_error(args, error);
      }
      index += 1;
    }
    report_spawn_error(args, if denied { libc::EACCES } else { libc::ENOENT });
  }
}

unsafe fn report_spawn_error(args: &ChildSetupArgs, errno: libc::c_int) -> ! {
  unsafe {
    libc::write(args.report_fd, (&errno as *const libc::c_int).cast(), std::mem::size_of_val(&errno));
    libc::_exit(127);
  }
}

/// Move the calling child into its session cgroup, then set its OOM priority, both before exec.
unsafe fn move_child_to_cgroup(args: &ChildSetupArgs, path: *const libc::c_char) {
  unsafe {
    write_decimal_to_file(args, path, libc::getpid() as u32);
    write_decimal_to_file(args, c"/proc/self/oom_score_adj".as_ptr(), args.cgroup_oom_score_adj as u32);
  }
}

/// Write `value` in decimal to the file at `path`. A failure reports to the parent: pre-exec code allocates nothing.
unsafe fn write_decimal_to_file(args: &ChildSetupArgs, path: *const libc::c_char, mut value: u32) {
  unsafe {
    let fd = libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC);
    if fd == -1 {
      report_spawn_error(args, current_errno());
    }
    let mut digits = [0u8; 10];
    let mut start = digits.len();
    loop {
      start -= 1;
      digits[start] = b'0' + (value % 10) as u8;
      value /= 10;
      if value == 0 {
        break;
      }
    }
    let count = libc::write(fd, digits[start..].as_ptr().cast(), digits.len() - start);
    let write_error = if count < 0 {
      Some(current_errno())
    } else if count as usize != digits.len() - start {
      Some(libc::EIO)
    } else {
      None
    };
    libc::close(fd);
    if let Some(errno) = write_error {
      report_spawn_error(args, errno);
    }
  }
}

unsafe fn current_errno() -> libc::c_int {
  unsafe { *libc::__errno_location() }
}

fn reap_process(pid: u32, sender: watch::Sender<Option<Result<libc::c_int, libc::c_int>>>) {
  let mut status = 0;
  loop {
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
    if result == pid as libc::pid_t {
      sender.send_replace(Some(Ok(status)));
      return;
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    let errno = error.raw_os_error().expect("waitpid errors have an OS errno");
    tracing::error!(target: "charliebot_agents", pid, error = %error, "agent_child_wait_failed");
    sender.send_replace(Some(Err(errno)));
    return;
  }
}

fn reap_pid(pid: u32) {
  let mut status = 0;
  loop {
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
    if result == pid as libc::pid_t {
      return;
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    tracing::warn!(target: "charliebot_agents", pid, error = %error, "agent_child_reap_failed");
    return;
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

pub(crate) async fn wait_until_stopped(child: &mut AgentChild) -> io::Result<()> {
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

  struct ProcessGroupGuard {
    process_group: i32,
    armed: bool,
  }

  impl ProcessGroupGuard {
    fn new(pid: u32) -> Self {
      Self { process_group: i32::try_from(pid).unwrap(), armed: true }
    }

    fn disarm(&mut self) {
      self.armed = false;
    }
  }

  impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
      if self.armed && unsafe { libc::kill(-self.process_group, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
          eprintln!("could not kill test process group {}: {error}", self.process_group);
        }
      }
    }
  }

  fn temp_cgroup(root: &Path) -> (PathBuf, PathBuf) {
    let base = root.join("user@1001.service");
    let slice = charliebot_core::cgroup::session_slice_dir(&base, charliebot_core::cgroup::ResourceClass::Agent);
    fs::create_dir_all(&slice).unwrap();
    let path = slice.join("charliebot-sess-test1234");
    fs::create_dir(&path).unwrap();
    let procs = path.join("cgroup.procs");
    fs::write(&procs, "").unwrap();
    let session = root.join("sessions").join("test1234-session");
    fs::create_dir_all(&session).unwrap();
    fs::write(session.join("metadata.json"), r#"{"name":"synthetic","profile":"manager"}"#).unwrap();
    (base, procs)
  }

  async fn stop_child(child: &mut AgentChild, guard: &mut ProcessGroupGuard) {
    assert_ne!(unsafe { libc::kill(-guard.process_group, libc::SIGKILL) }, -1);
    child.wait().await.unwrap();
    guard.disarm();
  }

  fn spec(program: &str, args: &[&str], base: &Path) -> SpawnSpec {
    SpawnSpec::new(
      program,
      args.iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      None,
      SessionCgroupConfig { base_dir: base.to_path_buf(), ..SessionCgroupConfig::disabled() },
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
      |_| (),
    )
    .await
    .unwrap()
    .0;
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
  async fn parent_death_spawn_uses_one_shared_vm_vfork_clone() {
    let root = tempfile::tempdir().unwrap();
    let mut command = spec("/bin/sh", &["-c", "while [ ! -e release ]; do /bin/sleep 0.01; done"], root.path());
    command.env.insert("PATH".into(), "/bin:/usr/bin".into());
    command.cwd = Some(root.path().to_path_buf());
    let mut spawned = spawn_piped(&command).await.unwrap();
    let pid = spawned.child.id().unwrap();

    assert_eq!(spawned.child.clone_calls, 1);
    assert_eq!(spawned.child.clone_flags & (libc::CLONE_VM | libc::CLONE_VFORK), libc::CLONE_VM | libc::CLONE_VFORK);
    assert_eq!(unsafe { libc::getsid(pid as libc::pid_t) }, pid as libc::pid_t);
    fs::write(root.path().join("release"), "").unwrap();
    assert_eq!(spawned.child.wait().await.unwrap().code(), Some(0));
  }

  #[tokio::test]
  async fn spawn_applies_nice_and_moves_into_the_session_cgroup_parent_side() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    fs::create_dir_all(sessions.join("test1234-session")).unwrap();
    fs::write(sessions.join("test1234-session").join("metadata.json"), r#"{"name":"synthetic","profile":"manager"}"#)
      .unwrap();
    let base = root.path().join("user@1001.service");
    let slice = charliebot_core::cgroup::session_slice_dir(&base, charliebot_core::cgroup::ResourceClass::Agent);
    fs::create_dir_all(&slice).unwrap();
    let cgroup_path = slice.join("charliebot-sess-test1234");
    fs::create_dir(&cgroup_path).unwrap();
    fs::write(cgroup_path.join("cgroup.procs"), "").unwrap();
    let spec = SpawnSpec::new(
      "/bin/sleep",
      ["30"].into_iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      Some("test1234-session".into()),
      SessionCgroupConfig { base_dir: base, sessions_dir: sessions, memory_max_mb: 64, swap_max_mb: 32, tasks_max: 0 },
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
  async fn shared_opencode_server_keeps_the_test_nice_and_stays_outside_the_session_cgroup() {
    let root = tempfile::tempdir().unwrap();
    let (base, procs) = temp_cgroup(root.path());
    let spec = SpawnSpec::new(
      "/bin/sleep",
      ["30"].into_iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      None,
      SessionCgroupConfig {
        base_dir: base,
        sessions_dir: root.path().join("sessions"),
        memory_max_mb: 0,
        swap_max_mb: 0,
        tasks_max: 0,
      },
    );
    let mut spawned = spawn_with_setup(&spec, Some(Signal::SIGKILL), ChildStdin::Null, SpawnSetup::None).await.unwrap();
    let pid = spawned.child.id().unwrap();
    let mut guard = ProcessGroupGuard::new(pid);
    let own_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let child_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let child_session = unsafe { libc::getsid(pid as i32) };
    let cgroup_procs = fs::read_to_string(procs).unwrap();
    assert_eq!(child_nice, own_nice);
    assert_eq!(child_session, pid as i32);
    assert_eq!(cgroup_procs, "");
    stop_child(&mut spawned.child, &mut guard).await;
  }

  #[tokio::test]
  async fn opencode_one_shot_uses_its_session_cgroup() {
    let root = tempfile::tempdir().unwrap();
    let (base, procs) = temp_cgroup(root.path());
    let spec = SpawnSpec::new(
      "/bin/sleep",
      ["30"].into_iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      Some("test1234-session".into()),
      SessionCgroupConfig {
        base_dir: base,
        sessions_dir: root.path().join("sessions"),
        memory_max_mb: 64,
        swap_max_mb: 0,
        tasks_max: 16,
      },
    );
    let mut spawned = spawn_one_shot(&spec, Some(Signal::SIGKILL)).await.unwrap();
    let pid = spawned.child.id().unwrap();
    let mut guard = ProcessGroupGuard::new(pid);
    let child_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let child_session = unsafe { libc::getsid(pid as i32) };
    let cgroup_procs = fs::read_to_string(procs).unwrap();
    assert_eq!(cgroup_procs, pid.to_string());
    assert_eq!(child_nice, 10);
    assert_eq!(child_session, pid as i32);
    stop_child(&mut spawned.child, &mut guard).await;
  }

  #[tokio::test]
  async fn codex_one_shot_uses_its_session_cgroup() {
    let root = tempfile::tempdir().unwrap();
    let (base, procs) = temp_cgroup(root.path());
    let spec = SpawnSpec::new(
      "/bin/sleep",
      ["30"].into_iter().map(OsString::from).collect(),
      None,
      HashMap::new(),
      Some("test1234-session".into()),
      SessionCgroupConfig {
        base_dir: base,
        sessions_dir: root.path().join("sessions"),
        memory_max_mb: 64,
        swap_max_mb: 0,
        tasks_max: 16,
      },
    );
    let mut spawned = spawn_one_shot(&spec, None).await.unwrap();
    let pid = spawned.child.id().unwrap();
    let mut guard = ProcessGroupGuard::new(pid);
    let child_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let child_session = unsafe { libc::getsid(pid as i32) };
    let cgroup_procs = fs::read_to_string(procs).unwrap();
    assert_eq!(cgroup_procs, pid.to_string());
    assert_eq!(child_nice, 10);
    assert_eq!(child_session, pid as i32);
    stop_child(&mut spawned.child, &mut guard).await;
  }

  #[tokio::test]
  async fn claude_one_shot_applies_nice_and_session_cgroup_before_exec() {
    let root = tempfile::tempdir().unwrap();
    let (base, procs) = temp_cgroup(root.path());
    let own_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let expected_nice = (own_nice + TURN_TREE_NICE).min(19);
    let script = r#"printf '%s\n' "$(ps -o ni= -p $$ | tr -d ' ')" "$(cat "$CGROUP_PROCS")"; read -r line"#;
    let env = HashMap::from([
      (OsString::from("CGROUP_PROCS"), procs.as_os_str().to_os_string()),
      (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
    ]);
    let spec = SpawnSpec::new(
      "/bin/sh",
      ["-c", script].into_iter().map(OsString::from).collect(),
      None,
      env,
      Some("test1234-session".into()),
      SessionCgroupConfig {
        base_dir: base,
        sessions_dir: root.path().join("sessions"),
        memory_max_mb: 64,
        swap_max_mb: 0,
        tasks_max: 16,
      },
    );
    let mut child =
      spawn_with_setup(&spec, None, ChildStdin::Piped, SpawnSetup::ChildSideTurnTree).await.unwrap().child;
    let pid = child.id().unwrap();
    let mut guard = ProcessGroupGuard::new(pid);
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut recorded_nice = String::new();
    let mut recorded_cgroup = String::new();
    stdout.read_line(&mut recorded_nice).await.unwrap();
    stdout.read_line(&mut recorded_cgroup).await.unwrap();
    let child_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let child_session = unsafe { libc::getsid(pid as i32) };
    let cgroup_procs = fs::read_to_string(procs).unwrap();
    assert_eq!(recorded_nice.trim().parse::<i32>().unwrap(), expected_nice);
    assert_eq!(recorded_cgroup.trim(), pid.to_string());
    assert_eq!(child_nice, expected_nice);
    assert_eq!(child_session, pid as i32);
    assert_eq!(cgroup_procs, pid.to_string());
    stop_child(&mut child, &mut guard).await;
  }

  #[tokio::test]
  async fn claude_compaction_keeps_its_nice_and_enters_the_session_cgroup_before_exec() {
    let root = tempfile::tempdir().unwrap();
    let (base, procs) = temp_cgroup(root.path());
    let own_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    assert!(own_nice < 19, "test process nice must be below 19, got {own_nice}");
    let script = r#"printf '%s\n' "$(ps -o ni= -p $$ | tr -d ' ')" "$(cat "$CGROUP_PROCS")"; read -r line"#;
    let env = HashMap::from([
      (OsString::from("CGROUP_PROCS"), procs.as_os_str().to_os_string()),
      (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
    ]);
    let spec = SpawnSpec::new(
      "/bin/sh",
      ["-c", script].into_iter().map(OsString::from).collect(),
      None,
      env,
      Some("test1234-session".into()),
      SessionCgroupConfig {
        base_dir: base,
        sessions_dir: root.path().join("sessions"),
        memory_max_mb: 64,
        swap_max_mb: 0,
        tasks_max: 16,
      },
    );
    let mut child = spawn_with_setup(&spec, None, ChildStdin::Piped, SpawnSetup::ChildSideCgroup).await.unwrap().child;
    let pid = child.id().unwrap();
    let mut guard = ProcessGroupGuard::new(pid);
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut recorded_nice = String::new();
    let mut recorded_cgroup = String::new();
    stdout.read_line(&mut recorded_nice).await.unwrap();
    stdout.read_line(&mut recorded_cgroup).await.unwrap();
    let child_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid) };
    let child_session = unsafe { libc::getsid(pid as i32) };
    let cgroup_procs = fs::read_to_string(procs).unwrap();
    assert_eq!(recorded_nice.trim().parse::<i32>().unwrap(), own_nice);
    assert_eq!(recorded_cgroup.trim(), pid.to_string());
    assert_eq!(child_nice, own_nice);
    assert_eq!(child_session, pid as i32);
    assert_eq!(cgroup_procs, pid.to_string());
    stop_child(&mut child, &mut guard).await;
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
