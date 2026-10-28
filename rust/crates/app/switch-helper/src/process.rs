//! The process identity the switch reads from `/proc`: a recorded `(pid, pid start)` names one process instance, and
//! the boot time bounds a run's recorded start.

use std::fs;

use anyhow::{Context as _, Result, anyhow};
use chrono::DateTime;

/// Field 22 of `/proc/<pid>/stat` (the start time) and the state field. `None` once the process is gone.
fn stat(pid: u32) -> Option<(String, String)> {
  let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
  let mut fields = text.rsplit_once(')')?.1.split_whitespace();
  let state = fields.next()?.to_string();
  let start = fields.nth(18)?.to_string();
  Some((state, start))
}

/// Whether `(pid, pid_start)` still names one live process instance. A zombie has ended: its argv is gone and nothing
/// can follow it, so it never counts.
pub fn instance(pid: u32, pid_start: &str) -> bool {
  matches!(stat(pid), Some((state, start)) if start == pid_start && state != "Z")
}

/// The kernel's boot time in seconds since the epoch, from `/proc/stat`.
pub fn boot_time() -> Result<i64> {
  let text = fs::read_to_string("/proc/stat").context("read /proc/stat")?;
  text
    .lines()
    .find_map(|line| line.strip_prefix("btime "))
    .and_then(|rest| rest.split_whitespace().next())
    .and_then(|seconds| seconds.parse().ok())
    .ok_or_else(|| anyhow!("/proc/stat has no btime line"))
}

/// Whether the run's recorded start is after this boot. A start at the boot second's fraction counts as after it, as
/// Python's float `timestamp()` compares.
pub fn started_after_boot(started_at: &str, boot: i64) -> Result<bool> {
  let started = DateTime::parse_from_rfc3339(started_at).with_context(|| format!("parse started_at {started_at:?}"))?;
  Ok(started.timestamp_millis() > boot.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_started_process_instance_is_alive_and_a_wrong_start_is_not() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("spawn sleep");
    let pid = child.id();
    let (state, start) = stat(pid).expect("the child's stat");
    assert_ne!(state, "Z");
    assert!(instance(pid, &start));
    assert!(!instance(pid, &format!("{start}0")));
    child.kill().expect("kill sleep");
    child.wait().expect("reap sleep");
  }

  #[test]
  fn a_start_is_after_the_boot_only_when_its_instant_is() {
    let boot = boot_time().expect("the boot time");
    assert!(started_after_boot("2999-01-01T00:00:00+00:00", boot).expect("a parsed start"));
    assert!(!started_after_boot("2000-01-01T00:00:00Z", boot).expect("a parsed start"));
    assert!(started_after_boot("2000-01-01T00:00:00.500+00:00", 946684800).expect("a parsed start"));
  }
}
