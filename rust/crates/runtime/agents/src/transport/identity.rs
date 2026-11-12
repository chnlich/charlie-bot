//! Run identities delegate all procfs reads to `charliebot-core`.

pub use charliebot_core::proc_identity::{ProcessStat, is_run_alive, read_host_boot_time, read_pid_stat};

/// The process identity pinned before the launch callback runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
  /// Spawned process id.
  pub pid: u32,
  /// Raw `/proc/<pid>/stat` field 22, when the process still has a readable stat.
  pub pid_start: Option<String>,
}

/// The start tick of a spawned process, read immediately before its owner is notified.
pub fn pin_process_identity(pid: u32) -> ProcessIdentity {
  let pid_start = charliebot_core::proc_identity::read_pid_stat(pid).map(|stat| stat.pid_start);
  ProcessIdentity { pid, pid_start }
}

#[cfg(test)]
mod tests {
  use time::OffsetDateTime;

  use super::*;

  #[test]
  #[ignore = "integration"]
  fn pinned_identity_uses_the_current_process_stat() {
    let pinned = pin_process_identity(std::process::id());
    assert_eq!(pinned.pid, std::process::id());
    assert_eq!(pinned.pid_start, read_pid_stat(pinned.pid).map(|stat| stat.pid_start));
  }

  #[test]
  #[ignore = "integration"]
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
