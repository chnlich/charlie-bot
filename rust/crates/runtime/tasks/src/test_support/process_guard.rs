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

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REAP_TIMEOUT: Duration = Duration::from_secs(10);
const REAP_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedProcess {
  pid: u32,
  pid_start: String,
  group: i32,
}

/// Records spawned process identities and kills their process groups with SIGKILL on drop.
pub struct ProcessGroupGuard {
  processes: Mutex<Vec<TrackedProcess>>,
}

impl ProcessGroupGuard {
  pub fn new() -> Self {
    // 36 is PR_SET_CHILD_SUBREAPER.
    if unsafe { libc::prctl(36, 1, 0, 0, 0) } != 0 {
      panic!("process-group guard could not become a child subreaper: {}", std::io::Error::last_os_error());
    }
    ProcessGroupGuard { processes: Mutex::new(Vec::new()) }
  }

  /// Record the process group of a run's pid. A pid that already exited and was reaped has no
  /// group left to kill, so tracking it is a no-op.
  pub fn track(&self, pid: u32) {
    let Some(stat) = charliebot_core::proc_identity::read_pid_stat(pid) else { return };
    self.track_if_start(pid, &stat.pid_start);
  }

  /// Record `pid` only while it still has the expected start tick.
  pub fn track_if_start(&self, pid: u32, pid_start: &str) {
    let Some(before) = charliebot_core::proc_identity::read_pid_stat(pid) else { return };
    if before.pid_start != pid_start {
      return;
    }
    let group = unsafe { libc::getpgid(pid as libc::pid_t) };
    if group < 0 {
      let error = std::io::Error::last_os_error();
      if error.raw_os_error() == Some(libc::ESRCH) {
        return;
      }
      panic!("process-group guard could not inspect process {pid}: {error}");
    }
    let Some(after) = charliebot_core::proc_identity::read_pid_stat(pid) else { return };
    if after.pid_start != pid_start || unsafe { libc::getpgid(pid as libc::pid_t) } != group {
      return;
    }
    let tracked = TrackedProcess { pid, pid_start: pid_start.to_string(), group };
    let mut processes = self.processes.lock().expect("process-group guard mutex is not poisoned");
    if !processes.contains(&tracked) {
      processes.push(tracked);
    }
  }
}

impl Drop for ProcessGroupGuard {
  fn drop(&mut self) {
    let processes = self.processes.lock().expect("process-group guard mutex is not poisoned").clone();
    let mut groups = BTreeMap::<i32, Vec<TrackedProcess>>::new();
    for process in processes {
      if process_is_current(&process) {
        groups.entry(process.group).or_default().push(process);
      }
    }
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
      let mut members = groups.values().flatten().cloned().collect::<Vec<_>>();
      for (group, anchors) in &groups {
        if !anchors.iter().any(process_is_current) {
          continue;
        }
        members.extend(processes_in_group(*group));
        if !anchors.iter().any(process_is_current) {
          continue;
        }
        let result = unsafe { libc::kill(-*group, libc::SIGKILL) };
        if result != 0 {
          let error = std::io::Error::last_os_error();
          if error.raw_os_error() != Some(libc::ESRCH) {
            report_cleanup_error(format!("process-group guard could not kill process group {group}: {error}"));
          }
        }
      }
      for member in members {
        let group_members = groups.entry(member.group).or_default();
        if !group_members.contains(&member) {
          group_members.push(member);
        }
      }
      reap_adopted_zombies(&groups);
      if groups.values().flatten().all(process_is_gone_or_zombie) {
        return;
      }
      std::thread::sleep(REAP_POLL);
    }
    let remaining = groups
      .values()
      .flatten()
      .filter(|process| !process_is_gone_or_zombie(process))
      .map(|process| (process.pid, process.group))
      .collect::<Vec<_>>();
    if !remaining.is_empty() {
      report_cleanup_error(format!(
        "process-group guard could not reap processes {remaining:?} within {REAP_TIMEOUT:?}"
      ));
    }
  }
}

fn process_is_current(process: &TrackedProcess) -> bool {
  charliebot_core::proc_identity::read_pid_stat(process.pid).is_some_and(|stat| stat.pid_start == process.pid_start)
    && unsafe { libc::getpgid(process.pid as libc::pid_t) } == process.group
}

fn process_is_gone_or_zombie(process: &TrackedProcess) -> bool {
  charliebot_core::proc_identity::read_pid_stat(process.pid)
    .is_none_or(|stat| stat.pid_start != process.pid_start || stat.state == "Z")
}

/// Return each member's identity while its process group is still pinned by a tracked process.
fn processes_in_group(group: i32) -> Vec<TrackedProcess> {
  let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
  entries
    .flatten()
    .filter_map(|entry| {
      let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
      let contents = std::fs::read_to_string(entry.path().join("stat")).ok()?;
      let (_, rest) = contents.rsplit_once(')')?;
      let fields = rest.split_whitespace().collect::<Vec<_>>();
      if fields.len() < 20 || fields[2].parse::<i32>().ok()? != group {
        return None;
      }
      Some(TrackedProcess { pid, pid_start: fields[19].to_string(), group })
    })
    .collect()
}

/// Reap tracked zombies adopted by this test process; unrelated child processes are untouched.
fn reap_adopted_zombies(groups: &BTreeMap<i32, Vec<TrackedProcess>>) {
  let self_pid = unsafe { libc::getpid() };
  for process in groups.values().flatten() {
    let Some(stat) = charliebot_core::proc_identity::read_pid_stat(process.pid) else { continue };
    if stat.pid_start != process.pid_start || stat.state != "Z" {
      continue;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", process.pid)) else { continue };
    let ppid: i32 =
      status.lines().find(|line| line.starts_with("PPid:")).and_then(|line| line[5..].trim().parse().ok()).unwrap_or(0);
    if ppid == self_pid {
      let mut wait_status = 0;
      unsafe { libc::waitpid(process.pid as libc::pid_t, &mut wait_status, libc::WNOHANG) };
    }
  }
}

fn report_cleanup_error(message: String) {
  if std::thread::panicking() {
    eprintln!("{message}");
  } else {
    panic!("{message}");
  }
}

impl Default for ProcessGroupGuard {
  fn default() -> Self {
    Self::new()
  }
}
