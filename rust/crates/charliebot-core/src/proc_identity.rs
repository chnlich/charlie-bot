//! Process identity reads shared by run stores and task-tree liveness checks.

use std::fs;
use std::io;

use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessStat {
  pub pid_start: String,
  pub state: String,
}

/// Read field 22 and field 3 of `/proc/<pid>/stat`; return `None` when the process cannot be read.
pub fn read_pid_stat(pid: u32) -> Option<ProcessStat> {
  let contents = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
  let (_, rest) = contents.rsplit_once(')')?;
  let fields: Vec<&str> = rest.split_whitespace().collect();
  if fields.len() < 20 {
    return None;
  }
  Some(ProcessStat { pid_start: fields[19].to_string(), state: fields[0].to_string() })
}

/// Read the host boot time from `/proc/stat`'s `btime` line.
pub fn read_host_boot_time() -> io::Result<OffsetDateTime> {
  let contents = fs::read_to_string("/proc/stat")?;
  for line in contents.lines() {
    if let Some(value) = line.strip_prefix("btime ") {
      let seconds = value.parse::<i64>().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
      return OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }
  }
  Err(io::Error::new(io::ErrorKind::InvalidData, "/proc/stat missing btime line"))
}

/// Check the recorded process id, start tick, non-zombie state, and current boot interval.
pub fn is_run_alive(
  pid: Option<u32>,
  pid_start: Option<&str>,
  started_at: Option<OffsetDateTime>,
  host_boot_time: OffsetDateTime,
) -> bool {
  let (Some(pid), Some(pid_start), Some(started_at)) = (pid, pid_start, started_at) else {
    return false;
  };
  let Some(current) = read_pid_stat(pid) else {
    return false;
  };
  current.pid_start == pid_start && current.state != "Z" && started_at > host_boot_time
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_the_current_process_start_tick() {
    let stat = read_pid_stat(std::process::id()).expect("the test process has a proc stat");
    assert!(!stat.pid_start.is_empty());
    assert!(!stat.state.is_empty());
  }

  #[test]
  fn pid_stat_fields_match_the_proc_file_text() {
    let pid = std::process::id();
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = text.rsplit_once(')').unwrap().1.split_whitespace().collect();
    let stat = read_pid_stat(pid).unwrap();
    assert_eq!(stat.pid_start, fields[19]);
    assert_eq!(stat.state, fields[0]);
  }

  #[test]
  fn a_missing_pid_has_no_stat() {
    assert!(read_pid_stat(u32::MAX).is_none());
  }

  #[test]
  fn boot_time_is_the_btime_line_of_proc_stat() {
    let text = fs::read_to_string("/proc/stat").unwrap();
    let btime: i64 = text.lines().find_map(|line| line.strip_prefix("btime ")).unwrap().parse().unwrap();
    assert_eq!(read_host_boot_time().unwrap().unix_timestamp(), btime);
  }

  #[test]
  fn a_missing_identity_is_not_alive() {
    let boot = read_host_boot_time().unwrap();
    assert!(!is_run_alive(None, Some("1"), Some(boot), boot));
  }
}
