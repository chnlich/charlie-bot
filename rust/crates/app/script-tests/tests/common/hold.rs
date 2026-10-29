//! Checks that the processes a test starts end with its test process, even when that process is killed and no Rust
//! guard runs.
//!
//! A check starts a second copy of its test binary with one `hold_*` test, which starts its processes and writes a
//! marker file naming them. The check waits for the marker, kills that test process with SIGKILL, and asserts the
//! recorded processes end within two seconds. A `hold_*` test run outside a check does nothing.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Sandbox};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// The variable through which a `hold_*` test learns the marker file it writes its processes to.
pub const HOLD: &str = "LEAK_CHECK_HOLD";

/// How long a process gets to end after the test process that started it is killed.
pub const END_WAIT: Duration = Duration::from_secs(2);

/// How long the held test gets to start its processes and write its marker. Starting a world costs a few hundred
/// milliseconds alone, and the whole binary's tests run beside this check.
const MARKER_WAIT: Duration = Duration::from_secs(180);

/// The marker file this process was started to write, when it is a held test.
pub fn marker() -> Option<PathBuf> {
  env::var_os(HOLD).map(PathBuf::from)
}

/// Writes a held test's marker atomically, so the check never reads a file the test is still writing.
pub fn write_marker(marker: &Path, text: &str) {
  let writing = marker.with_extension("writing");
  fs::write(&writing, text).expect("write the held test's marker");
  fs::rename(&writing, marker).expect("publish the held test's marker");
}

/// A second copy of this test binary running one `hold_*` test. Dropping it kills and reaps that process, so a
/// failing check leaves nothing behind.
pub struct HeldTest {
  child: Child,
  marker: PathBuf,
  log: PathBuf,
  _sandbox: Sandbox,
}

impl HeldTest {
  pub fn start(hold_test: &str) -> Self {
    let sandbox = Sandbox::new();
    let marker = sandbox.root().join("marker");
    let log = sandbox.root().join("held-test.log");
    let log_file = fs::File::create(&log).expect("the held test's output file");
    let mut command = harness::command(env::current_exe().expect("the test binary"), &sandbox.env());
    command.args(["--exact", hold_test, "--test-threads=1"]).env(HOLD, &marker);
    command
      .stdin(Stdio::null())
      .stdout(Stdio::from(log_file.try_clone().expect("a second log handle")))
      .stderr(Stdio::from(log_file));
    let child = harness::spawn(&mut command);
    Self { child, marker, log, _sandbox: sandbox }
  }

  /// The held test's marker text, once it has written its file. A held test that ends first fails the check with
  /// its own output, which says why it ended.
  pub fn recorded(&mut self) -> String {
    let deadline = Instant::now() + MARKER_WAIT;
    loop {
      if self.marker.is_file() {
        return fs::read_to_string(&self.marker).expect("the held test's marker");
      }
      if let Some(status) = self.child.try_wait().expect("check the held test") {
        panic!("the held test ended before it wrote its marker: {status:?}\n{}", self.output());
      }
      assert!(Instant::now() <= deadline, "the held test wrote no marker in {MARKER_WAIT:?}\n{}", self.output());
      thread::sleep(Duration::from_millis(20));
    }
  }

  fn output(&self) -> String {
    fs::read_to_string(&self.log).unwrap_or_else(|error| format!("cannot read {}: {error}", self.log.display()))
  }

  pub fn kill_and_reap(&mut self) {
    match kill(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL) {
      Ok(()) | Err(Errno::ESRCH) => {}
      Err(error) => eprintln!("cannot kill the held test process {}: {error}", self.child.id()),
    }
    if let Err(error) = self.child.wait() {
      eprintln!("cannot reap the held test process {}: {error}", self.child.id());
    }
  }
}

impl Drop for HeldTest {
  fn drop(&mut self) {
    self.kill_and_reap();
  }
}

/// Whether `condition` holds within `timeout`.
pub fn within(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + timeout;
  loop {
    if condition() {
      return true;
    }
    if Instant::now() > deadline {
      return false;
    }
    thread::sleep(Duration::from_millis(20));
  }
}

/// The running processes whose command line names a path under `root`.
pub fn processes_under(root: &Path) -> Vec<u32> {
  let prefix = format!("{}/", root.display());
  running().filter(|pid| cmdline_args(*pid).iter().any(|arg| arg.starts_with(&prefix))).collect()
}

/// The running processes whose command line has `argument` as one of its arguments.
pub fn processes_with_arg(argument: &str) -> Vec<u32> {
  running().filter(|pid| cmdline_args(*pid).iter().any(|arg| arg == argument)).collect()
}

/// The running processes in process group `group_id`.
pub fn group_members(group_id: u32) -> Vec<u32> {
  running().filter(|pid| process_group(*pid) == Some(group_id)).collect()
}

/// Whether `pid` has ended: gone, or exited and not yet reaped.
pub fn has_ended(pid: u32) -> bool {
  match fs::read_to_string(format!("/proc/{pid}/stat")) {
    Ok(stat) => stat.rsplit_once(") ").is_none_or(|(_, rest)| rest.starts_with('Z')),
    Err(_) => true,
  }
}

/// The pids of the running processes this check may see: every process, less this one and any that have ended.
fn running() -> impl Iterator<Item = u32> {
  let ours = std::process::id();
  pids().filter(move |pid| *pid != ours && !has_ended(*pid))
}

fn process_group(pid: u32) -> Option<u32> {
  let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
  let (_, rest) = stat.rsplit_once(") ")?;
  // The fields after the command name are state, ppid and then the process group.
  rest.split_whitespace().nth(2)?.parse().ok()
}

/// The arguments of `pid`'s command line. A process whose /proc entry cannot be read has ended, so it has none.
fn cmdline_args(pid: u32) -> Vec<String> {
  match fs::read(format!("/proc/{pid}/cmdline")) {
    Ok(raw) => raw
      .split(|byte| *byte == 0)
      .filter(|arg| !arg.is_empty())
      .map(|arg| String::from_utf8_lossy(arg).into_owned())
      .collect(),
    Err(_) => Vec::new(),
  }
}

fn pids() -> impl Iterator<Item = u32> {
  fs::read_dir("/proc")
    .expect("the process directory")
    .filter_map(|entry| entry.ok()?.file_name().to_string_lossy().parse().ok())
}
