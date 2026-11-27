//! The guard of one foreground call: it runs as a transient unit outside the call cgroup, so it outlives the process that
//! started the call. At the deadline it sends SIGTERM to the call's processes, waits the stop grace, writes
//! `cgroup.kill` and removes the call cgroup.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

/// How often the guard polls the call cgroup while `cgroup.kill` takes effect.
const EMPTY_POLL: Duration = Duration::from_millis(100);
/// The polls the guard makes for the call cgroup to empty after `cgroup.kill`.
const EMPTY_POLL_LIMIT: u32 = 100;

/// The effects of the guard, so its sequence can be checked without signals or cgroups.
pub trait GuardHost {
  /// Whether the call cgroup is still present. The call removes it when the call ends before the deadline.
  fn cgroup_present(&self) -> bool;
  /// The process ids in the call cgroup.
  fn procs(&self) -> io::Result<Vec<i32>>;
  /// Send SIGTERM to one process. A process that has already exited is not an error.
  fn term(&mut self, pid: i32) -> io::Result<()>;
  /// Write `cgroup.kill`, which ends every process in the call cgroup.
  fn kill_all(&mut self) -> io::Result<()>;
  /// Whether the call cgroup still holds a process.
  fn populated(&self) -> io::Result<bool>;
  /// Sleep for `duration`.
  fn sleep(&mut self, duration: Duration);
  /// Remove the call cgroup directory.
  fn remove(&mut self) -> io::Result<()>;
}

/// Run the guard's sequence: wait the deadline, SIGTERM the call's processes, wait the stop grace, `cgroup.kill`, then
/// remove the call cgroup. A call cgroup that is already gone at the deadline ended on its own, so the guard stops.
pub fn run_guard<H: GuardHost>(host: &mut H, deadline: Duration, stop: Duration) -> io::Result<()> {
  host.sleep(deadline);
  if !host.cgroup_present() {
    return Ok(());
  }
  for pid in host.procs()? {
    host.term(pid)?;
  }
  host.sleep(stop);
  host.kill_all()?;
  let mut polls = 0;
  while host.populated()? {
    if polls == EMPTY_POLL_LIMIT {
      return Err(io::Error::other("the call cgroup still holds processes after cgroup.kill"));
    }
    polls += 1;
    host.sleep(EMPTY_POLL);
  }
  host.remove()
}

/// The guard's effects on one call cgroup directory.
pub struct CgroupGuardHost {
  cgroup: PathBuf,
}

impl CgroupGuardHost {
  pub fn new(cgroup: &Path) -> Self {
    Self { cgroup: cgroup.to_path_buf() }
  }

  fn file(&self, name: &str) -> PathBuf {
    self.cgroup.join(name)
  }
}

impl GuardHost for CgroupGuardHost {
  fn cgroup_present(&self) -> bool {
    self.cgroup.is_dir()
  }

  fn procs(&self) -> io::Result<Vec<i32>> {
    let text = fs::read_to_string(self.file("cgroup.procs"))?;
    text
      .lines()
      .filter(|line| !line.is_empty())
      .map(|line| {
        line.parse::<i32>().map_err(|error| io::Error::other(format!("cgroup.procs holds {line:?}: {error}")))
      })
      .collect()
  }

  fn term(&mut self, pid: i32) -> io::Result<()> {
    match signal::kill(Pid::from_raw(pid), Signal::SIGTERM) {
      Ok(()) => Ok(()),
      // The process exited between the read of cgroup.procs and the signal: nothing is left to end.
      Err(Errno::ESRCH) => Ok(()),
      Err(error) => Err(io::Error::other(format!("SIGTERM to pid {pid} failed: {error}"))),
    }
  }

  fn kill_all(&mut self) -> io::Result<()> {
    fs::write(self.file("cgroup.kill"), "1")
  }

  fn populated(&self) -> io::Result<bool> {
    Ok(!self.procs()?.is_empty())
  }

  fn sleep(&mut self, duration: Duration) {
    thread::sleep(duration);
  }

  fn remove(&mut self) -> io::Result<()> {
    fs::remove_dir(&self.cgroup)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Records each effect in order. `procs_after_kill` is the number of polls that still see a process after
  /// `cgroup.kill`.
  #[derive(Debug, PartialEq, Eq)]
  enum Op {
    Sleep(u64),
    Term(i32),
    KillAll,
    Remove,
  }

  struct Recorder {
    present: bool,
    procs: Vec<i32>,
    polls_populated: u32,
    ops: Vec<Op>,
  }

  impl Recorder {
    fn new(procs: Vec<i32>, polls_populated: u32) -> Self {
      Self { present: true, procs, polls_populated, ops: Vec::new() }
    }
  }

  impl GuardHost for Recorder {
    fn cgroup_present(&self) -> bool {
      self.present
    }

    fn procs(&self) -> io::Result<Vec<i32>> {
      Ok(self.procs.clone())
    }

    fn term(&mut self, pid: i32) -> io::Result<()> {
      self.ops.push(Op::Term(pid));
      Ok(())
    }

    fn kill_all(&mut self) -> io::Result<()> {
      self.ops.push(Op::KillAll);
      Ok(())
    }

    fn populated(&self) -> io::Result<bool> {
      Ok(self.polls_populated > 0)
    }

    fn sleep(&mut self, duration: Duration) {
      if duration == EMPTY_POLL {
        self.polls_populated = self.polls_populated.saturating_sub(1);
      } else {
        self.ops.push(Op::Sleep(duration.as_secs()));
      }
    }

    fn remove(&mut self) -> io::Result<()> {
      self.ops.push(Op::Remove);
      Ok(())
    }
  }

  #[test]
  fn the_guard_terms_waits_kills_then_removes_in_that_order() {
    let mut host = Recorder::new(vec![11, 12], 0);
    run_guard(&mut host, Duration::from_secs(895), Duration::from_secs(5)).unwrap();
    assert_eq!(host.ops, vec![Op::Sleep(895), Op::Term(11), Op::Term(12), Op::Sleep(5), Op::KillAll, Op::Remove]);
  }

  #[test]
  fn the_guard_waits_for_the_cgroup_to_empty_before_removing_it() {
    let mut host = Recorder::new(vec![11], 3);
    run_guard(&mut host, Duration::from_secs(15), Duration::from_secs(5)).unwrap();
    assert_eq!(host.ops.last(), Some(&Op::Remove));
    assert_eq!(host.polls_populated, 0);
  }

  #[test]
  fn the_guard_stops_when_the_call_already_removed_its_cgroup() {
    let mut host = Recorder::new(vec![], 0);
    host.present = false;
    run_guard(&mut host, Duration::from_secs(15), Duration::from_secs(5)).unwrap();
    assert_eq!(host.ops, vec![Op::Sleep(15)], "the guard waits its deadline, then finds nothing to end");
  }

  #[test]
  fn a_cgroup_that_never_empties_is_an_error_and_is_kept() {
    let mut host = Recorder::new(vec![11], u32::MAX);
    let error = run_guard(&mut host, Duration::from_secs(15), Duration::from_secs(5)).unwrap_err();
    assert!(error.to_string().contains("after cgroup.kill"));
    assert!(!host.ops.contains(&Op::Remove));
  }

  #[test]
  fn the_cgroup_host_reads_pids_and_writes_cgroup_kill() {
    let dir = tempfile::tempdir().unwrap();
    let cgroup = dir.path().join("call-1");
    fs::create_dir(&cgroup).unwrap();
    fs::write(cgroup.join("cgroup.procs"), "41\n42\n").unwrap();
    fs::write(cgroup.join("cgroup.kill"), "").unwrap();
    let mut host = CgroupGuardHost::new(&cgroup);
    assert_eq!(host.procs().unwrap(), vec![41, 42]);
    assert!(host.populated().unwrap());
    host.kill_all().unwrap();
    assert_eq!(fs::read_to_string(cgroup.join("cgroup.kill")).unwrap(), "1");
  }

  #[test]
  fn an_exited_pid_is_not_an_error_to_terminate() {
    let mut host = CgroupGuardHost::new(Path::new("/nonexistent"));
    // A pid that cannot exist: the kernel's pid range stops well below i32::MAX.
    host.term(i32::MAX).unwrap();
  }
}
