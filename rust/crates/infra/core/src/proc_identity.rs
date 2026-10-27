//! Process identity reads shared by run stores and task-tree liveness checks.

use std::io;

use procfs::process::Process;
use procfs::{CurrentSI, KernelStats};
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessStat {
  pub pid_start: String,
  pub state: String,
}

/// Field 22 (start tick) and field 3 (state) of `/proc/<pid>/stat`; `None` when the process cannot be read.
pub fn read_pid_stat(pid: u32) -> Option<ProcessStat> {
  let stat = Process::new(i32::try_from(pid).ok()?).ok()?.stat().ok()?;
  Some(ProcessStat { pid_start: stat.starttime.to_string(), state: stat.state.to_string() })
}

/// Read the host boot time from `/proc/stat`'s `btime` line.
pub fn read_host_boot_time() -> io::Result<OffsetDateTime> {
  let seconds = i64::try_from(KernelStats::current().map_err(io::Error::other)?.btime)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
  OffsetDateTime::from_unix_timestamp(seconds).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
  use std::fs;
  use std::process::Command;
  use std::thread;
  use std::time::{Duration, Instant};

  use super::*;

  #[test]
  fn reads_the_current_process_start_tick() {
    let stat = read_pid_stat(std::process::id()).expect("the test process has a proc stat");
    assert!(!stat.pid_start.is_empty());
    assert!(!stat.state.is_empty());
  }

  #[test]
  fn pid_stat_fields_match_the_proc_file_text() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(1);
    let comparison = (|| -> Result<_, String> {
      loop {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|error| error.to_string())?;
        let fields: Vec<&str> =
          text.rsplit_once(')').ok_or("proc stat has no command delimiter")?.1.split_whitespace().collect();
        if fields.first() == Some(&"S") {
          let stat = read_pid_stat(pid).ok_or("procfs did not read the sleeping child")?;
          return Ok((fields[19].to_owned(), fields[0].to_owned(), stat));
        }
        if Instant::now() >= deadline {
          return Err("sleep child did not enter the sleeping state".to_owned());
        }
        thread::sleep(Duration::from_millis(1));
      }
    })();
    child.kill().unwrap();
    child.wait().unwrap();
    let (pid_start, state, stat) = comparison.unwrap();
    assert_eq!(stat.pid_start, pid_start);
    assert_eq!(stat.state, state);
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
