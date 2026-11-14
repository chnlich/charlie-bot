//! The remote end-time readback that rides the ssh calls a remote watch already makes.
//!
//! A remote watch that fires logs when its job ended (the new fields of the `trigger_fired` line). Three sources
//! feed that log: the `End` column of the remote `sacct` query, the mtime of a `--cmd` launch's exit file, and an
//! end recorder for a pid-only target. The verify-on-create ssh leaves that recorder on the remote host. Every
//! readback rides the ssh call that discovers the end, so a wake starts no extra ssh process. This module holds the
//! remote command segments and the parsing and clock math; `triggers.rs` wires them into the probes.
//!
//! Clock math, stated once: one readback ssh carries a remote `date +%s.%N` and has a local midpoint
//! (midway between its start and its end, on the server clock). `offset = remote epoch − local midpoint`, in ms.
//! An end instant recorded on the remote clock converts to the server clock as `end_server = end_remote − offset`.
//! The remote end line always names the remote clock. The recorder and `date -r` print `date +%s.%N`, and the
//! sacct `End` column states the remote host's local zone, which the readback ssh also prints.

use std::time::SystemTime;

use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

/// The remote directory prefix that holds one watch's end files, under the remote home. The remote shell expands
/// `$HOME`, so the word stays unexpanded until it runs there.
const WATCH_DIR_WORD_PREFIX: &str = "$HOME/charliebot_runs/watch-";

/// The sacct columns one query reads: the state and exit code the fire decision uses, then `End` for the end
/// reading. The local and the remote query use the same list.
pub const SACCT_FORMAT_COLUMNS: [&str; 4] = ["JobID", "State", "ExitCode", "End"];

/// The Slurm states that end a job. A job in one of them has finished, whatever its exit code; the held session's
/// remote script checks the same list.
pub const SLURM_TERMINAL_STATES: [&str; 11] = [
  "COMPLETED",
  "FAILED",
  "CANCELLED",
  "TIMEOUT",
  "OUT_OF_MEMORY",
  "NODE_FAIL",
  "BOOT_FAIL",
  "DEADLINE",
  "PREEMPTED",
  "REVOKED",
  "SPECIAL_EXIT",
];

/// The remote command prefix that measures the remote clock and zone in the sacct readback ssh.
pub const SACCT_REMOTE_CLOCK_PREFIX: &str = "date +%s.%N; date +%z";

/// The format the sacct `End` column prints, without a zone.
const SACCT_END_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
  format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

/// Where a logged end time came from. The `trigger_fired` line prints `none` when no remote target recorded an
/// end, which is the absence of a reading rather than a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndSource {
  /// The `End` column of the remote sacct query.
  SacctEnd,
  /// The modification time of a `--cmd` launch's exit file.
  ExitFileMtime,
  /// The end recorder that the verify-on-create ssh started for a pid-only target.
  EndRecorder,
}

impl std::fmt::Display for EndSource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(end_source_label(Some(*self)))
  }
}

/// The `end_source` label of one remote target's optional source.
pub fn end_source_label(source: Option<EndSource>) -> &'static str {
  match source {
    Some(EndSource::SacctEnd) => "sacct_end",
    Some(EndSource::ExitFileMtime) => "exit_file_mtime",
    Some(EndSource::EndRecorder) => "end_recorder",
    None => "none",
  }
}

/// One remote target's end reading, taken when the server learned the target had ended. `end_at` is set exactly
/// when the readback produced an end instant on the server clock, and `source` and `clock_offset_ms` are then set
/// with it. A target the server saw gone without a readable end carries none of the three.
#[derive(Clone, Debug)]
pub struct RemoteTargetEnd {
  /// The end instant on the server clock, when the readback produced one.
  pub end_at: Option<OffsetDateTime>,
  /// Where the end reading came from; set exactly when `end_at` is.
  pub source: Option<EndSource>,
  /// Remote clock minus server clock, measured in the ssh that read the end back; set with `end_at`.
  pub clock_offset_ms: Option<i64>,
  /// When the server learned the target had ended.
  pub seen_gone_at: OffsetDateTime,
}

/// The end instant one finished remote target carries, from a remote end line, the date line of the ssh that read
/// it, and that ssh's local midpoint. `None` means the readback ran but the clock math failed; the caller logs a
/// warning and fires without an end.
pub fn reading(
  end_remote_ns: i128,
  remote_now_ns: i128,
  local_midpoint_ns: i128,
  source: EndSource,
  seen_gone_at: OffsetDateTime,
) -> Option<RemoteTargetEnd> {
  let offset_ms = clock_offset_ms(remote_now_ns, local_midpoint_ns);
  let end_at = ns_to_datetime(end_remote_ns - i128::from(offset_ms) * 1_000_000)?;
  Some(RemoteTargetEnd { end_at: Some(end_at), source: Some(source), clock_offset_ms: Some(offset_ms), seen_gone_at })
}

/// The end-file path of one watched pid, as a remote shell word. Double quotes keep `$HOME` expandable while the
/// rest of the path, a UUID and a pid, survives any remote home with spaces in it.
pub fn end_file_word(trigger_id: &str, pid: i64) -> String {
  format!("\"{WATCH_DIR_WORD_PREFIX}{trigger_id}/{pid}.end\"")
}

/// The verify-command segment that starts the end recorder for one watched pid, before the probe verdict.
///
/// The recorder detaches like the `--cmd` launcher: `setsid`, with stdin, stdout and stderr redirected, so the
/// verify ssh still returns within its limit. It is bounded by the trigger's max wait and writes one epoch.ns line
/// to the end file only when the watched pid died first. An expiry of the timeout writes nothing.
pub fn recorder_command(pid: i64, trigger_id: &str, max_wait_seconds: i64) -> String {
  let dir = format!("\"{WATCH_DIR_WORD_PREFIX}{trigger_id}\"");
  let end_file = end_file_word(trigger_id, pid);
  let payload =
    format!("timeout {max_wait_seconds} tail --pid={pid} -f /dev/null >/dev/null 2>&1 && date +%s.%N > {end_file}");
  format!("mkdir -p {dir} && {{ setsid sh -c '{payload}' </dev/null >/dev/null 2>&1 & }}; ")
}

/// The probe-command segment that measures the remote clock and reads and deletes the end file inside the probe
/// that checks the pid, before the probe verdict. The verdict stays the last stdout line the probe parser reads.
pub fn probe_read_prefix(end_file: &str) -> String {
  format!("date +%s.%N; cat {end_file} 2>/dev/null; rm -f {end_file}; ")
}

/// The exit-read command for one `--cmd` launch: the date line the offset math reads, the exit file's content,
/// and the file's mtime as the launch's end instant. A missing file prints only the date line.
pub fn exit_read_command(exit_file: &str) -> Result<String, shlex::QuoteError> {
  let quoted = shlex::try_quote(exit_file)?;
  Ok(format!("date +%s.%N; cat {quoted} 2>/dev/null; date -r {quoted} +%s.%N 2>/dev/null"))
}

/// The date and zone lines a pid probe's stdout carries between its first line and the verdict word. The date line
/// is the probe command's first output and the end-file line follows it. A probe without the read prefix, such as
/// one from a static-output fake, parses to no reading.
pub struct PidProbeRead {
  /// The remote clock at the probe, in epoch ns.
  pub now_ns: Option<i128>,
  /// The end-file line the probe read, in remote-clock epoch ns.
  pub end_ns: Option<i128>,
}

pub fn pid_probe_read(stdout: &str) -> PidProbeRead {
  let lines: Vec<&str> = stdout.trim().lines().collect();
  PidProbeRead {
    now_ns: lines.first().and_then(|line| parse_epoch_ns(line)).filter(|_| lines.len() >= 2),
    end_ns: lines.get(1).and_then(|line| parse_epoch_ns(line)).filter(|_| lines.len() >= 3),
  }
}

/// The tag the held pid readback prints before its first remote date, and the one it prints after any wait for the
/// end file. The two dates bracket the wait, so their midpoint is the remote clock at the ssh's local midpoint
/// however long the wait ran.
pub const READBACK_START_TAG: &str = "T1";
/// The tag of the readback's second remote date.
pub const READBACK_END_TAG: &str = "T2";

/// Parse one held pid readback's stdout: the two tagged dates and the bare end-file line between them. The clock the
/// offset math uses is the midpoint of the two dates, because the local midpoint spans the same wait.
pub fn pid_readback_read(stdout: &str) -> PidProbeRead {
  let lines: Vec<&str> = stdout.trim().lines().collect();
  let tagged =
    |tag: &str| lines.iter().find_map(|line| line.strip_prefix(tag).and_then(|rest| parse_epoch_ns(rest.trim())));
  let start = tagged(READBACK_START_TAG);
  let now_ns = match (start, tagged(READBACK_END_TAG)) {
    (Some(start), Some(end)) => Some(start + (end - start) / 2),
    _ => start,
  };
  PidProbeRead { now_ns, end_ns: lines.iter().find_map(|line| parse_epoch_ns(line)) }
}

/// What one exit-read ssh printed: the exit code, the file's mtime, and the remote clock of the read.
pub struct ExitRead {
  /// The exit file's content, when the file existed and held text.
  pub code: Option<String>,
  /// The file's mtime, in remote-clock epoch ns.
  pub mtime_ns: Option<i128>,
  /// The remote clock at the read, in epoch ns.
  pub now_ns: Option<i128>,
}

/// Parse one exit-read ssh's stdout. A stdout that starts with an epoch.ns date line carries the content and the
/// mtime after it. Any other shape, such as a static-output fake that prints only the exit file, keeps the whole
/// stdout as the code. That is what the exit read printed before the readback existed.
pub fn exit_read(stdout: &str) -> ExitRead {
  let lines: Vec<&str> = stdout.trim().lines().collect();
  if let Some(now_ns) = lines.first().and_then(|line| parse_epoch_ns(line)) {
    let (mtime_ns, code) = match lines[1..].split_last() {
      None => (None, None),
      Some((last, head)) => match parse_epoch_ns(last) {
        Some(mtime_ns) => (Some(mtime_ns), non_empty(&head.join("\n"))),
        None => (None, non_empty(&lines[1..].join("\n"))),
      },
    };
    return ExitRead { code, mtime_ns, now_ns: Some(now_ns) };
  }
  ExitRead { code: non_empty(stdout.trim()), mtime_ns: None, now_ns: None }
}

/// The date and zone lines a remote sacct probe's stdout leads with, before the query's rows.
pub struct SacctProbePrefix {
  /// The remote clock at the probe, in epoch ns.
  pub now_ns: Option<i128>,
  /// The remote host's zone offset east of UTC, in seconds.
  pub zone_seconds: Option<i32>,
}

pub fn sacct_probe_prefix(stdout: &str) -> SacctProbePrefix {
  let lines: Vec<&str> = stdout.trim().lines().collect();
  SacctProbePrefix {
    now_ns: lines.first().and_then(|line| parse_epoch_ns(line)),
    zone_seconds: lines.get(1).and_then(|line| zone_seconds(line)),
  }
}

/// One epoch.ns line, as GNU `date +%s.%N` prints it, as an epoch in ns.
pub fn parse_epoch_ns(text: &str) -> Option<i128> {
  let (seconds, nanos) = text.split_once('.')?;
  if seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  if nanos.is_empty() || !nanos.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  let mut value = seconds.parse::<i128>().ok()? * 1_000_000_000;
  if nanos.len() <= 9 {
    value += nanos.parse::<i128>().ok()? * 10i128.pow(9 - nanos.len() as u32);
  } else {
    value += nanos[..9].parse::<i128>().ok()?;
  }
  Some(value)
}

/// One `date +%z` output, as seconds east of UTC.
pub fn zone_seconds(text: &str) -> Option<i32> {
  let (sign, digits) = text.split_at_checked(1)?;
  let sign = match sign {
    "+" => 1,
    "-" => -1,
    _ => return None,
  };
  if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  let hours: i32 = digits[..2].parse().ok()?;
  let minutes: i32 = digits[2..].parse().ok()?;
  Some(sign * (hours * 3600 + minutes * 60))
}

/// One sacct `End` value, printed in the remote host's local zone, as a remote-clock epoch in ns.
pub fn sacct_end_remote_ns(end_raw: &str, zone: i32) -> Option<i128> {
  // The `End` column states no zone; the readback ssh's `date +%z` carries it, so the parsed wall time is taken as
  // UTC first and shifted by the zone after.
  let local = PrimitiveDateTime::parse(end_raw, SACCT_END_FORMAT).ok()?;
  Some((local.assume_utc() - time::Duration::seconds(i64::from(zone))).unix_timestamp_nanos())
}

/// The offset between the remote clock and the server clock, in ms: the remote epoch minus the local midpoint of
/// the ssh that read the remote time.
pub fn clock_offset_ms(remote_epoch_ns: i128, local_midpoint_ns: i128) -> i64 {
  ((remote_epoch_ns - local_midpoint_ns) / 1_000_000) as i64
}

/// The midpoint of one ssh call, on the server clock, in epoch ns.
pub fn local_midpoint_ns(started: SystemTime, ended: SystemTime) -> Option<i128> {
  let start = system_epoch_ns(started)?;
  let end = system_epoch_ns(ended)?;
  Some((start + end) / 2)
}

/// One server-clock instant, in epoch ns.
pub fn system_epoch_ns(at: SystemTime) -> Option<i128> {
  let delta = at.duration_since(SystemTime::UNIX_EPOCH).ok()?;
  Some(delta.as_nanos() as i128)
}

/// One epoch ns instant, as a UTC datetime.
pub fn ns_to_datetime(ns: i128) -> Option<OffsetDateTime> {
  OffsetDateTime::from_unix_timestamp_nanos(ns).ok().map(|datetime| datetime.to_offset(time::UtcOffset::UTC))
}

/// One UTC datetime, as the RFC 3339 text the `trigger_fired` fields carry.
pub fn rfc3339(at: OffsetDateTime) -> String {
  at.format(&Rfc3339).expect("a UTC datetime formats as RFC 3339")
}

fn non_empty(text: &str) -> Option<String> {
  let text = text.trim();
  (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn readback_clock_is_the_midpoint_of_the_dates_that_bracket_the_wait() {
    let stdout = "T1 1762612345.000000000\n1762612347.500000000\nT2 1762612349.000000000\nDEAD\n";
    let read = pid_readback_read(stdout);
    assert_eq!(read.end_ns, Some(1_762_612_347_500_000_000));
    assert_eq!(read.now_ns, Some(1_762_612_347_000_000_000));
  }

  #[test]
  fn readback_without_an_end_file_still_carries_its_clock() {
    let stdout = "T1 1762612345.000000000\nT2 1762612345.000000000\nDEAD\n";
    let read = pid_readback_read(stdout);
    assert_eq!(read.end_ns, None);
    assert_eq!(read.now_ns, Some(1_762_612_345_000_000_000));
  }
}
