//! A holder of the host release lock, for the tests of the scripts that take it.
//!
//! The holder takes the lock the way `release_lock_take`'s callers do: through the shared definition, from the copy
//! beside the script under test. It sleeps while it holds the lock. Killing it leaves the lock free and its record
//! stale, which is the state a holder that died leaves.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// How long the holder gets to take the lock and write its record.
const RECORD_WAIT: Duration = Duration::from_secs(10);

/// The paths of the host release lock and of the holder record beside it, read from the lock definition at
/// `lock_sh`, so no test copies the paths.
pub fn paths(lock_sh: &Path, env: &Env) -> (PathBuf, PathBuf) {
  let script = "source \"$1\" && printf '%s\\n%s\\n' \"$RELEASE_LOCK_FILE\" \"$RELEASE_LOCK_RECORD\"";
  let mut command = harness::command("bash", env);
  command.args(["-c", script, "bash"]).arg(lock_sh);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "{}", run.describe());
  let mut lines = run.stdout.lines();
  let file = PathBuf::from(lines.next().expect("the lock file path"));
  let record = PathBuf::from(lines.next().expect("the holder record path"));
  (file, record)
}

/// One holder of the host release lock. Dropping it kills its process group, so the sleeping shell ends with the
/// test even when the test panics between the start and its own kill.
pub struct LockHolder {
  pid: u32,
  child: Child,
  record: PathBuf,
  reaped: bool,
}

impl LockHolder {
  /// Takes the lock for `script` working on `commit`, through the lock definition at `lock_sh` under `env`.
  pub fn start(lock_sh: &Path, env: &Env, script: &str, commit: &str) -> Self {
    let (_, record) = paths(lock_sh, env);
    let body = "source \"$1\" && release_lock_take \"$2\" \"$3\" && sleep 300";
    let mut command = harness::command("bash", env);
    command.args(["-c", body, "bash"]).arg(lock_sh).arg(script).arg(commit);
    command.process_group(0).stdin(Stdio::null());
    let mut child = harness::spawn(&mut command);
    let pid = child.id();
    let deadline = Instant::now() + RECORD_WAIT;
    while !record.is_file() {
      assert!(Instant::now() <= deadline, "the holder wrote no record in {RECORD_WAIT:?}");
      if let Some(status) = child.try_wait().expect("check the lock holder") {
        panic!("the lock holder ended before it wrote its record: {status:?}");
      }
      thread::sleep(Duration::from_millis(20));
    }
    Self { pid, child, record, reaped: false }
  }

  /// The pid the holder's record names.
  pub fn pid(&self) -> u32 {
    self.pid
  }

  /// The record the holder wrote beside the lock.
  pub fn record(&self) -> &Path {
    &self.record
  }

  /// Kills the holder's process group: the kernel frees the lock and the record stays, stale.
  pub fn kill(&mut self) {
    match kill(Pid::from_raw(-(self.pid as i32)), Signal::SIGKILL) {
      Ok(()) | Err(Errno::ESRCH) => {}
      Err(error) => eprintln!("cannot kill the lock holder {}: {error}", self.pid),
    }
    if let Err(error) = self.child.wait() {
      eprintln!("cannot reap the lock holder {}: {error}", self.pid);
    }
    self.reaped = true;
  }
}

impl Drop for LockHolder {
  fn drop(&mut self) {
    if !self.reaped {
      self.kill();
    }
  }
}
