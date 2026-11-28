//! Places the process tree of a `--cmd` watch in the session cgroup from its first instruction.
//!
//! The wrapper writes its own pid into the session's `cgroup.procs` in the child, between fork and exec. Every process
//! the command forks then inherits the session cgroup and its memory and process limits, so the watch outlives the
//! calling tool call's cgroup. The parent reads the same file back after the spawn. A placement that cannot be written
//! or read back is an error, and the caller starts nothing.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use charliebot_core::cgroup::{PROCS_FILE_NAME, SESSION_CGROUP_ENV};
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::unistd::{close, getpid, write};

/// The `cgroup.procs` file of the session cgroup that `CHARLIEBOT_SESSION_CGROUP` names. A missing or empty variable is
/// an error: a `--cmd` watch has no other place to run.
pub fn procs_from_env() -> io::Result<PathBuf> {
  match std::env::var_os(SESSION_CGROUP_ENV) {
    Some(dir) if !dir.is_empty() => Ok(Path::new(&dir).join(PROCS_FILE_NAME)),
    _ => Err(io::Error::other(format!(
      "{SESSION_CGROUP_ENV} is unset: a --cmd watch runs in the session cgroup, and nothing was started"
    ))),
  }
}

/// The C string of the `cgroup.procs` path, built before the fork so the pre-exec step allocates nothing.
pub fn procs_cstring(procs: &Path) -> io::Result<CString> {
  CString::new(procs.as_os_str().as_bytes()).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Child side, between fork and exec: move the calling process into the cgroup whose `cgroup.procs` is `procs`.
///
/// Only syscalls run here, with no allocation, because the forked child of a multithreaded parent may use only
/// async-signal-safe calls.
pub fn move_self_before_exec(procs: &std::ffi::CStr) -> io::Result<()> {
  let fd = open(procs, OFlag::O_WRONLY | OFlag::O_CLOEXEC, Mode::empty()).map_err(io::Error::from)?;
  let mut digits = [0u8; 12];
  let mut start = digits.len();
  let mut value = getpid().as_raw().unsigned_abs();
  start -= 1;
  digits[start] = b'\n';
  loop {
    start -= 1;
    digits[start] = b'0' + (value % 10) as u8;
    value /= 10;
    if value == 0 {
      break;
    }
  }
  let line = &digits[start..];
  let count = write(&fd, line).map_err(io::Error::from)?;
  if count != line.len() {
    return Err(io::Error::from(Errno::EIO));
  }
  close(fd).map_err(io::Error::from)
}

/// Parent side, after the spawn: whether `pid` is listed in `procs`. The file lists the pids of the cgroup's members.
pub fn lists_pid(procs: &Path, pid: i32) -> io::Result<bool> {
  let text = std::fs::read_to_string(procs)?;
  Ok(text.split_whitespace().any(|entry| entry.parse::<i32>().ok() == Some(pid)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_pid_listed_among_the_members_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let procs = dir.path().join(PROCS_FILE_NAME);
    std::fs::write(&procs, "12\n345\n").unwrap();
    assert!(lists_pid(&procs, 345).unwrap());
    assert!(!lists_pid(&procs, 34).unwrap());
  }

  #[test]
  fn a_missing_procs_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(lists_pid(&dir.path().join(PROCS_FILE_NAME), 1).is_err());
  }
}
