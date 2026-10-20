// Test guard that SIGKILLs every recorded process group when dropped, on success and on failure.
//
// Fake agents outlive the run pid the server stops: a cancel hits the wrapper shell, and the
// agent under it keeps looping. Each test records a run's pid here as soon as the store
// persists it; the drop path resolves the group id early for exactly that reason, because a
// dead wrapper's pid can no longer be mapped to its group.
//
// Killing is not enough: a killed agent is a zombie until someone waits for it, and a zombie
// still counts as a process group member. The constructor turns the test process into a child
// subreaper, so an orphaned fake agent is adopted by the test process instead of the outer
// harness, and the drop path reaps the zombies of the groups it recorded.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const REAP_TIMEOUT: Duration = Duration::from_secs(10);
const REAP_POLL: Duration = Duration::from_millis(50);

/// Records the process group of every spawned run and kills each group with SIGKILL on drop.
pub struct ProcessGroupGuard {
  groups: Mutex<Vec<i32>>,
}

impl ProcessGroupGuard {
  pub fn new() -> Self {
    // 36 is PR_SET_CHILD_SUBREAPER.
    if unsafe { libc::prctl(36, 1, 0, 0, 0) } != 0 {
      panic!("process-group guard could not become a child subreaper: {}", std::io::Error::last_os_error());
    }
    ProcessGroupGuard { groups: Mutex::new(Vec::new()) }
  }

  /// Record the process group of a run's pid. A pid that already exited and was reaped has no
  /// group left to kill, so tracking it is a no-op.
  pub fn track(&self, pid: u32) {
    let group = unsafe { libc::getpgid(pid as libc::pid_t) };
    if group < 0 {
      let error = std::io::Error::last_os_error();
      if error.raw_os_error() == Some(libc::ESRCH) {
        return;
      }
      panic!("process-group guard could not inspect process {pid}: {error}");
    }
    self.groups.lock().expect("process-group guard mutex is not poisoned").push(group);
  }

  /// Every recorded process group id, for liveness assertions in the test itself.
  pub fn tracked(&self) -> Vec<i32> {
    self.groups.lock().expect("process-group guard mutex is not poisoned").clone()
  }
}

impl Drop for ProcessGroupGuard {
  fn drop(&mut self) {
    let groups = self.groups.lock().expect("process-group guard mutex is not poisoned").clone();
    for group in &groups {
      unsafe { libc::kill(-*group, libc::SIGKILL) };
    }
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
      reap_adopted_zombies(&groups);
      if groups.iter().all(group_gone) {
        return;
      }
      std::thread::sleep(REAP_POLL);
    }
    let remaining = groups.iter().copied().filter(|group| !group_gone(group)).collect::<Vec<_>>();
    if !remaining.is_empty() {
      let message = format!("process-group guard could not reap process groups {remaining:?} within {REAP_TIMEOUT:?}");
      if std::thread::panicking() {
        eprintln!("{message}");
      } else {
        panic!("{message}");
      }
    }
  }
}

fn group_gone(group: &i32) -> bool {
  let alive = unsafe { libc::kill(-*group, 0) };
  alive == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Reap the adopted zombies that belong to the recorded groups; other zombies are not ours to wait for.
fn reap_adopted_zombies(groups: &[i32]) {
  let self_pid = unsafe { libc::getpid() };
  let entries = match std::fs::read_dir("/proc") {
    Ok(entries) => entries,
    Err(error) => panic!("process-group guard could not read /proc: {error}"),
  };
  for entry in entries.flatten() {
    let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() < 5 {
      continue;
    }
    let Ok(group) = fields[4].parse::<i32>() else { continue };
    if !groups.contains(&group) {
      continue;
    }
    let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
    let state = status.lines().find(|line| line.starts_with("State:")).unwrap_or("");
    if !state.contains('Z') {
      continue;
    }
    let ppid: i32 =
      status.lines().find(|line| line.starts_with("PPid:")).and_then(|line| line[5..].trim().parse().ok()).unwrap_or(0);
    if ppid != self_pid {
      continue;
    }
    let pid: libc::pid_t = fields[0].parse().expect("/proc stat pid is a number");
    let mut wait_status = 0;
    unsafe { libc::waitpid(pid, &mut wait_status, libc::WNOHANG) };
  }
}

impl Default for ProcessGroupGuard {
  fn default() -> Self {
    Self::new()
  }
}
