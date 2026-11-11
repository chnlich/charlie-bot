//! The process identity the switch reads: a recorded `(pid, pid start)` names one process instance, and the boot time
//! bounds a run's recorded start. The `/proc` reads come from `charliebot_core::proc_identity`, the owner of process
//! identity, so no second reader of the same fields lives here.

use anyhow::{Context as _, Result};
use charliebot_core::proc_identity::{read_host_boot_time, read_pid_stat};
use chrono::DateTime;

/// Whether `(pid, pid_start)` still names one live process instance. A zombie has ended: its argv is gone and nothing
/// can follow it, so it never counts.
pub fn instance(pid: u32, pid_start: &str) -> bool {
  matches!(read_pid_stat(pid), Some(stat) if stat.pid_start == pid_start && stat.state != "Z")
}

/// Whether a pid still names a live process, for a recorded pid that carries no start time. A zombie has ended: its
/// argv is gone and nothing can follow it, so it never counts.
pub fn alive(pid: u32) -> bool {
  matches!(read_pid_stat(pid), Some(stat) if stat.state != "Z")
}

/// The kernel's boot time in seconds since the epoch.
pub fn boot_time() -> Result<i64> {
  Ok(read_host_boot_time().context("read the host boot time")?.unix_timestamp())
}

/// Whether the run's recorded start is after this boot. A start at the boot second's fraction counts as after it.
pub fn started_after_boot(started_at: &str, boot: i64) -> Result<bool> {
  let started = DateTime::parse_from_rfc3339(started_at).with_context(|| format!("parse started_at {started_at:?}"))?;
  Ok(started.timestamp_millis() > boot.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[ignore = "integration"]
  fn a_started_process_instance_is_alive_and_a_wrong_start_is_not() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("spawn sleep");
    let pid = child.id();
    let stat = read_pid_stat(pid).expect("the child's stat");
    assert_ne!(stat.state, "Z");
    assert!(instance(pid, &stat.pid_start));
    assert!(!instance(pid, &format!("{}0", stat.pid_start)));
    child.kill().expect("kill sleep");
    child.wait().expect("reap sleep");
  }

  #[test]
  #[ignore = "integration"]
  fn a_start_is_after_the_boot_only_when_its_instant_is() {
    let boot = boot_time().expect("the boot time");
    assert!(started_after_boot("2999-01-01T00:00:00+00:00", boot).expect("a parsed start"));
    assert!(!started_after_boot("2000-01-01T00:00:00Z", boot).expect("a parsed start"));
    assert!(started_after_boot("2000-01-01T00:00:00.500+00:00", 946684800).expect("a parsed start"));
  }
}
