//! Run identities delegate all procfs reads to `charliebot-core`.

use time::OffsetDateTime;

pub use charliebot_core::proc_identity::{ProcessStat, read_host_boot_time, read_pid_stat};

/// The process identity pinned before the launch callback runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
  /// Spawned process id.
  pub pid: u32,
  /// Raw `/proc/<pid>/stat` field 22, when the process still has a readable stat.
  pub pid_start: Option<String>,
}

/// Read a spawned process's start tick immediately before notifying its owner.
pub fn pin_process_identity(pid: u32) -> ProcessIdentity {
  let pid_start = charliebot_core::proc_identity::read_pid_stat(pid).map(|stat| stat.pid_start);
  ProcessIdentity { pid, pid_start }
}

/// Check process identity and boot time through the shared core implementation.
pub fn is_run_alive(
  pid: Option<u32>,
  pid_start: Option<&str>,
  started_at: Option<OffsetDateTime>,
  host_boot_time: OffsetDateTime,
) -> bool {
  charliebot_core::proc_identity::is_run_alive(pid, pid_start, started_at, host_boot_time)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pinned_identity_uses_the_current_process_stat() {
    let pinned = pin_process_identity(std::process::id());
    assert_eq!(pinned.pid, std::process::id());
    assert_eq!(pinned.pid_start, read_pid_stat(pinned.pid).map(|stat| stat.pid_start));
  }

  #[test]
  fn a_partial_identity_is_never_alive() {
    let boot = read_host_boot_time().unwrap();
    assert!(!is_run_alive(None, Some("1"), Some(boot), boot));
  }

  #[test]
  fn liveness_requires_the_current_start_tick_and_boot_interval() {
    let pid = std::process::id();
    let stat = read_pid_stat(pid).unwrap();
    let boot = read_host_boot_time().unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(is_run_alive(Some(pid), Some(&stat.pid_start), Some(now), boot));
    assert!(!is_run_alive(Some(pid), Some("1"), Some(now), boot));
    assert!(!is_run_alive(Some(pid), Some(&stat.pid_start), Some(boot), boot));
    assert!(!is_run_alive(Some(pid), Some(&stat.pid_start), None, boot));
  }
}
