//! Process identity reads shared by run stores and task-tree liveness checks, and the `/proc/<pid>/stat` text and its
//! fields that the readback and renderer process trees read for their state, parent and start time.

use std::fmt;
use std::fs;
use std::io;
use std::num::ParseIntError;

use procfs::process::Process;
use procfs::{CurrentSI, KernelStats};
use time::OffsetDateTime;

/// The record field holding a process instance's start tick, which tells a process from an earlier one that held the
/// same pid. A run record and a fence holder record carry it under this name.
pub const PID_START_FIELD: &str = "pid_start";

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

/// The text of `/proc/<pid>/stat`. A process that has ended has no entry, so the read fails with `NotFound`, or with
/// `ESRCH` when the entry goes away during the read; each caller decides what that absence means.
pub fn read_stat_text(pid: u32) -> io::Result<String> {
  fs::read_to_string(format!("/proc/{pid}/stat"))
}

/// The fields of a stat text that follow the command name, or `None` when the text has no `)`. The command name sits
/// in parentheses and may hold spaces, so the fields start after its last `)`: the state, then the parent pid, then
/// the process group, in the kernel's order.
fn stat_fields_after_command(stat: &str) -> Option<&str> {
  stat.rsplit_once(')').map(|(_, fields)| fields)
}

/// The fields of a `/proc/<pid>/stat` text that the process-tree readers use. The numbers are the field numbers that
/// `proc(5)` gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatFields {
  /// Field 3: the state letter, such as `R`, `S` or `Z`.
  pub state: char,
  /// Field 4: the parent pid.
  pub parent_pid: u32,
  /// Field 22: the start tick, which tells a process from an earlier one that held the same pid.
  pub start_tick: u64,
}

/// Why a stat text does not give its [`StatFields`]. Each message completes a sentence that names the stat file.
#[derive(Debug)]
pub enum StatFieldError {
  NoCommandDelimiter,
  MissingState,
  MissingParentPid,
  InvalidParentPid(ParseIntError),
  MissingStartTick,
  InvalidStartTick(ParseIntError),
}

impl fmt::Display for StatFieldError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      StatFieldError::NoCommandDelimiter => write!(f, "has no command delimiter"),
      StatFieldError::MissingState => write!(f, "has no process state"),
      StatFieldError::MissingParentPid => write!(f, "has no parent pid"),
      StatFieldError::InvalidParentPid(error) => write!(f, "has an invalid parent pid: {error}"),
      StatFieldError::MissingStartTick => write!(f, "has no process start time"),
      StatFieldError::InvalidStartTick(error) => write!(f, "has an invalid process start time: {error}"),
    }
  }
}

/// The [`StatFields`] of a `/proc/<pid>/stat` text. Every field the struct names must be present and parse, because the
/// kernel writes each one on every line.
pub fn parse_stat_fields(stat: &str) -> Result<StatFields, StatFieldError> {
  let fields: Vec<&str> =
    stat_fields_after_command(stat).ok_or(StatFieldError::NoCommandDelimiter)?.split_whitespace().collect();
  let state = fields.first().and_then(|field| field.chars().next()).ok_or(StatFieldError::MissingState)?;
  let parent_pid: u32 =
    fields.get(1).ok_or(StatFieldError::MissingParentPid)?.parse().map_err(StatFieldError::InvalidParentPid)?;
  let start_tick: u64 =
    fields.get(19).ok_or(StatFieldError::MissingStartTick)?.parse().map_err(StatFieldError::InvalidStartTick)?;
  Ok(StatFields { state, parent_pid, start_tick })
}

/// The host boot time from `/proc/stat`'s `btime` line.
pub fn read_host_boot_time() -> io::Result<OffsetDateTime> {
  let seconds = i64::try_from(KernelStats::current().map_err(io::Error::other)?.btime)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
  OffsetDateTime::from_unix_timestamp(seconds).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// This process's storage reads from `/proc/self/io`: the bytes fetched from the storage layer (`read_bytes`)
/// and the read syscalls (`syscr`). `None` when the kernel does not expose the file.
pub fn read_self_storage_reads() -> Option<(u64, u64)> {
  let io = Process::myself().ok()?.io().ok()?;
  Some((io.read_bytes, io.syscr))
}

/// Whether `pid` still names the process instance that started at tick `pid_start` and has not ended. A reused pid
/// carries another start tick, and a zombie's argv is gone, so neither counts.
pub fn is_same_process(pid: u32, pid_start: &str) -> bool {
  read_pid_stat(pid).is_some_and(|stat| stat.pid_start == pid_start && stat.state != "Z")
}

/// Whether the recorded process id, start tick, non-zombie state, and current boot interval match a live process.
pub fn is_run_alive(
  pid: Option<u32>,
  pid_start: Option<&str>,
  started_at: Option<OffsetDateTime>,
  host_boot_time: OffsetDateTime,
) -> bool {
  let (Some(pid), Some(pid_start), Some(started_at)) = (pid, pid_start, started_at) else {
    return false;
  };
  is_same_process(pid, pid_start) && started_at > host_boot_time
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::process::Command;
  use std::thread;
  use std::time::{Duration, Instant};

  use super::*;

  #[test]
  fn parses_the_fields_after_a_command_name_that_holds_spaces_and_parentheses() {
    // The fields after `(chrome (renderer) x)`: state `S`, parent pid 4321, and start tick 98765 at field 22.
    let stat = "1234 (chrome (renderer) x) S 4321 1234 1234 0 -1 4194560 1 2 3 4 5 6 7 8 9 10 11 12 98765 0";
    let fields = parse_stat_fields(stat).unwrap();
    assert_eq!(fields, StatFields { state: 'S', parent_pid: 4321, start_tick: 98765 });
  }

  #[ignore = "integration"]
  #[test]
  fn reads_the_current_process_start_tick() {
    let stat = read_pid_stat(std::process::id()).expect("the test process has a proc stat");
    assert!(!stat.pid_start.is_empty());
    assert!(!stat.state.is_empty());
  }

  #[ignore = "integration"]
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

  #[ignore = "integration"]
  #[test]
  fn a_missing_pid_has_no_stat() {
    assert!(read_pid_stat(u32::MAX).is_none());
  }

  #[ignore = "integration"]
  #[test]
  fn boot_time_is_the_btime_line_of_proc_stat() {
    let text = fs::read_to_string("/proc/stat").unwrap();
    let btime: i64 = text.lines().find_map(|line| line.strip_prefix("btime ")).unwrap().parse().unwrap();
    assert_eq!(read_host_boot_time().unwrap().unix_timestamp(), btime);
  }

  #[ignore = "integration"]
  #[test]
  fn a_missing_identity_is_not_alive() {
    let boot = read_host_boot_time().unwrap();
    assert!(!is_run_alive(None, Some("1"), Some(boot), boot));
  }
}
