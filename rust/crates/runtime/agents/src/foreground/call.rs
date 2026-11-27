//! The call entry: runs one agent shell command in a call cgroup under the session cgroup, with a guard unit that ends it
//! at the deadline.
//!
//! The entry stays in the session cgroup and moves only its child into the call cgroup, so a `cgroup.kill` at the
//! deadline ends the command and leaves the entry alive to report it. The guard starts before the command, and a
//! command that cannot be placed under a guard never runs.

use std::ffi::CStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use charliebot_core::cgroup::PROCS_FILE_NAME;

use super::guard::{CgroupGuardHost, GuardHost};
use super::{CALL_DIR_PREFIX, DEADLINE_EXIT_CODE, GUARD_UNIT_PREFIX, deadline_error_text};

/// One foreground call: the command, its deadline, the guard's executable and the session cgroup that holds the call.
pub struct CallSpec {
  /// The session cgroup directory, from `CHARLIEBOT_SESSION_CGROUP`.
  pub session_cgroup: PathBuf,
  /// The shell command, run as `bash -c`.
  pub command: String,
  /// Seconds until the guard sends SIGTERM.
  pub deadline_secs: u64,
  /// Seconds from SIGTERM to `cgroup.kill`.
  pub stop_secs: u64,
  /// The charliebot executable the guard unit runs as `foreground-guard`.
  pub guard_exe: PathBuf,
}

/// Run the call and return the exit status the agent sees. A command that the deadline ended prints the deadline error
/// text on stderr and returns 143.
pub fn run_call(spec: &CallSpec) -> io::Result<i32> {
  // The clock starts before the guard does: the guard's deadline then never falls before this call's, so a signalled
  // exit past the deadline below is the guard's doing.
  let started = Instant::now();
  let id = call_id()?;
  let call_dir = spec.session_cgroup.join(format!("{CALL_DIR_PREFIX}{id}"));
  let unit = format!("{GUARD_UNIT_PREFIX}{id}");
  fs::create_dir(&call_dir)
    .map_err(|error| io::Error::other(format!("cannot create call cgroup {}: {error}", call_dir.display())))?;
  if let Err(error) = start_guard(spec, &unit, &call_dir) {
    return Err(match fs::remove_dir(&call_dir) {
      Ok(()) => error,
      Err(removal) => io::Error::other(format!("{error}; the call cgroup also could not be removed: {removal}")),
    });
  }
  let status = match run_in_call(&spec.command, &call_dir) {
    Ok(status) => status,
    Err(error) => {
      finish_idle(&unit, &call_dir)?;
      return Err(error);
    }
  };
  let deadline = Duration::from_secs(spec.deadline_secs);
  // The guard signals only after its deadline, which starts after this call's, so a signalled exit past the deadline is
  // the guard's doing.
  if started.elapsed() >= deadline && status.signal().is_some() {
    eprintln!("{}", deadline_error_text(spec.deadline_secs));
    return Ok(DEADLINE_EXIT_CODE);
  }
  finish_idle(&unit, &call_dir)?;
  Ok(exit_code(status))
}

/// A random id for one call, as 16 lowercase hex digits.
fn call_id() -> io::Result<String> {
  let mut bytes = [0u8; 8];
  getrandom::fill(&mut bytes).map_err(|error| io::Error::other(format!("cannot draw a call id: {error}")))?;
  Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Start the guard unit that ends the call at its deadline. A unit that does not start is an error, so the command does
/// not run.
fn start_guard(spec: &CallSpec, unit: &str, call_dir: &Path) -> io::Result<()> {
  let output = Command::new("systemd-run")
    // Agent processes disable the session bus so their tools cannot start arbitrary user units. This trusted client
    // must reach the user manager to start the deadline guard.
    .env_remove("DBUS_SESSION_BUS_ADDRESS")
    .args(["--user", "--quiet", "--collect"])
    .arg(format!("--unit={unit}"))
    .arg("--")
    .arg(&spec.guard_exe)
    .arg("foreground-guard")
    .arg("--cgroup")
    .arg(call_dir)
    .arg("--deadline-secs")
    .arg(spec.deadline_secs.to_string())
    .arg("--stop-secs")
    .arg(spec.stop_secs.to_string())
    .output()?;
  if output.status.success() {
    return Ok(());
  }
  Err(io::Error::other(format!(
    "the foreground guard unit {unit} did not start, so the command did not run: {}",
    String::from_utf8_lossy(&output.stderr).trim()
  )))
}

/// Run `command` as `bash -c` with the child moved into the call cgroup before its exec.
fn run_in_call(command: &str, call_dir: &Path) -> io::Result<ExitStatus> {
  let procs = std::ffi::CString::new(call_dir.join(PROCS_FILE_NAME).as_os_str().as_bytes())
    .map_err(|error| io::Error::other(format!("call cgroup path has a NUL byte: {error}")))?;
  let mut child = Command::new("/bin/bash");
  child.arg("-c").arg(command);
  // SAFETY: the closure runs after fork and before exec, and calls only open, write and close on a path that was
  // prepared before the fork.
  unsafe {
    child.pre_exec(move || move_self_into(&procs));
  }
  child.spawn()?.wait()
}

/// Write `0` to the calling process's `cgroup.procs`, which moves the caller into that cgroup.
fn move_self_into(procs: &CStr) -> io::Result<()> {
  // SAFETY: `procs` is a valid NUL-terminated path, and the descriptor is closed before the function returns.
  let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }
  let written = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
  let error = io::Error::last_os_error();
  unsafe {
    libc::close(fd);
  }
  if written == 1 { Ok(()) } else { Err(error) }
}

/// After a call that ended before its deadline: when no process is left in the call cgroup, stop the guard and remove the
/// cgroup. A process still in it is a background job the agent did not move out, and the guard ends it at the deadline.
fn finish_idle(unit: &str, call_dir: &Path) -> io::Result<()> {
  if CgroupGuardHost::new(call_dir).populated()? {
    return Ok(());
  }
  let output = Command::new("systemctl").args(["--user", "stop", unit]).output()?;
  if !output.status.success() {
    return Err(io::Error::other(format!(
      "cannot stop the foreground guard unit {unit}: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    )));
  }
  fs::remove_dir(call_dir)
}

/// The exit status a shell reports: the code, or 128 plus the signal that ended the command.
fn exit_code(status: ExitStatus) -> i32 {
  match (status.code(), status.signal()) {
    (Some(code), _) => code,
    (None, Some(signal)) => 128 + signal,
    (None, None) => 1,
  }
}
