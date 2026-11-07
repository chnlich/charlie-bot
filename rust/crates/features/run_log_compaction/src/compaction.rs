//! The compaction of a finished run's log files to zstd.
//!
//! A run whose record holds an end at least [`PLAIN_RETENTION`] old has each of its log files stored as `<name>.zst`.
//! The handler writes the compressed file, decompresses it and compares it byte for byte with the plain file, and only
//! then removes the plain file. A run without a recorded end is left alone, and a second pass finds nothing to do.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_store::layout;
use charliebot_store::metadata::UtcDateTime;
use charliebot_store::run_logs::{self, COMPRESSED_SUFFIX, RunLogKind};
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_METADATA_NAME, read_run_record_file};
use time::{Duration, OffsetDateTime};

/// The switch that turns the handler's work off. The module's default is on: a caller turns it off with `0` or
/// `false`.
pub const ENABLE_RUN_LOG_COMPRESSION: &str = "ENABLE_RUN_LOG_COMPRESSION";

/// How long a finished run keeps its plain log files: a run whose recorded end is at least this old is compacted.
pub const PLAIN_RETENTION: Duration = Duration::hours(24);

/// The zstd level the handler writes. A log is read far more often than it is written, so the level trades a little
/// handler time for a smaller file.
const COMPRESSION_LEVEL: i32 = 3;

/// The bytes compared per round when the handler checks a compressed file against its plain one.
const VERIFY_CHUNK_BYTES: usize = 64 * 1024;

/// The handler's work: compact every finished run of every session under `sessions_dir`.
pub async fn run(sessions_dir: PathBuf) -> anyhow::Result<String> {
  if !enabled() {
    return Ok(format!("{ENABLE_RUN_LOG_COMPRESSION} is off; nothing was compacted"));
  }
  let now = OffsetDateTime::now_utc();
  tokio::task::spawn_blocking(move || compact_finished_runs(&sessions_dir, now))
    .await
    .context("joining the run-log compaction")?
}

/// Whether the switch leaves the compaction on: unset means on, and `0` or `false` (any case) means off.
pub fn enabled() -> bool {
  match std::env::var(ENABLE_RUN_LOG_COMPRESSION) {
    Ok(value) => {
      let value = value.trim();
      value != "0" && !value.eq_ignore_ascii_case("false")
    }
    Err(std::env::VarError::NotPresent | std::env::VarError::NotUnicode(_)) => true,
  }
}

/// What one blocking pass compacted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
  /// The log files written as `.zst` and removed as plain files.
  pub compacted: usize,
  /// The finished runs that held no plain log file left to compact.
  pub already_compacted: usize,
}

impl Report {
  /// The handler's one-line result.
  pub fn summary(&self) -> String {
    format!("compacted {} run log files; {} runs were already compacted", self.compacted, self.already_compacted)
  }
}

/// The handler's line from compacting the log files of every run whose record ends at least [`PLAIN_RETENTION`]
/// before `now`.
pub fn compact_finished_runs(sessions_dir: &Path, now: OffsetDateTime) -> anyhow::Result<String> {
  Ok(compact(sessions_dir, now)?.summary())
}

/// [`compact_finished_runs`] with the counts, for callers that report more than one line.
pub fn compact(sessions_dir: &Path, now: OffsetDateTime) -> anyhow::Result<Report> {
  let mut report = Report::default();
  for session in subdirectories(sessions_dir)? {
    for run in subdirectories(&layout::runs_dir(&session))? {
      let Some(ended_at) = recorded_end(&run)? else { continue };
      if now - ended_at < PLAIN_RETENTION {
        continue;
      }
      let files = plain_log_files(&run)?;
      if files.is_empty() {
        if holds_compressed_log(&run)? {
          report.already_compacted += 1;
        }
        continue;
      }
      for plain in files {
        match compress_file(&plain) {
          Ok(Compaction::Compacted) => report.compacted += 1,
          Ok(Compaction::Skipped) => {}
          Err(cause) => bail!("compacting {} failed: {cause}", plain.display()),
        }
      }
    }
  }
  Ok(report)
}

/// When the run in `run_dir` recorded its end, `None` for a run without a recorded end or without a record.
fn recorded_end(run_dir: &Path) -> anyhow::Result<Option<OffsetDateTime>> {
  let path = run_dir.join(RUN_METADATA_NAME);
  match read_run_record_file(&path) {
    Ok(record) => Ok(record.ended_at.map(UtcDateTime::into_inner)),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(anyhow::anyhow!("reading {}: {error}", path.display())),
  }
}

/// Every plain log file the run directory holds: its event log, its raw log, and the rotated raw logs.
fn plain_log_files(run_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let mut files = Vec::new();
  for kind in [RunLogKind::Events, RunLogKind::Raw] {
    let plain = run_dir.join(kind.file_name());
    if plain.is_file() {
      files.push(plain);
    }
  }
  for (name, path) in directory_entries(run_dir)? {
    let Some(index) = name.strip_prefix(RAW_LOG_NAME).and_then(|rest| rest.strip_prefix('.')) else {
      continue;
    };
    if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) && path.is_file() {
      files.push(path);
    }
  }
  files.sort();
  Ok(files)
}

/// Whether the run directory holds a compressed log file.
fn holds_compressed_log(run_dir: &Path) -> anyhow::Result<bool> {
  Ok(
    directory_entries(run_dir)?
      .into_iter()
      .any(|(name, _)| name.ends_with(COMPRESSED_SUFFIX) && run_logs::is_run_log_name(&name)),
  )
}

/// What compressing one plain log file did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compaction {
  /// The plain file became its compressed form.
  Compacted,
  /// The file was already stored compressed, or no longer exists.
  Skipped,
}

/// The compression of one plain log file: write `<name>.zst`, verify it against the plain file byte for byte, then
/// remove the plain file. On any failure the plain file stays and the partial `.zst` is removed.
fn compress_file(plain: &Path) -> anyhow::Result<Compaction> {
  let Some(log) = run_logs::resolve(plain) else { return Ok(Compaction::Skipped) };
  if log.is_compressed() {
    return Ok(Compaction::Skipped);
  }
  let compressed = run_logs::compressed_path(plain);
  if let Err(cause) = write_compressed(plain, &compressed) {
    remove_partial(&compressed);
    return Err(cause);
  }
  verify_or_remove(plain, &compressed)?;
  fs::remove_file(plain).with_context(|| format!("removing the plain log {}", plain.display()))?;
  Ok(Compaction::Compacted)
}

/// The check of `compressed` against `plain`. A mismatch removes the compressed file and fails, leaving the plain file.
fn verify_or_remove(plain: &Path, compressed: &Path) -> anyhow::Result<()> {
  match verify_equal(plain, compressed) {
    Ok(()) => Ok(()),
    Err(cause) => {
      remove_partial(compressed);
      Err(cause)
    }
  }
}

/// The compressed write of `plain` into `compressed`, synced to disk.
fn write_compressed(plain: &Path, compressed: &Path) -> anyhow::Result<()> {
  let input = File::open(plain).with_context(|| format!("opening {}", plain.display()))?;
  let mut input = BufReader::new(input);
  let output = File::create(compressed).with_context(|| format!("creating {}", compressed.display()))?;
  let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(output), COMPRESSION_LEVEL)
    .with_context(|| format!("starting the zstd frame for {}", compressed.display()))?;
  io::copy(&mut input, &mut encoder)
    .with_context(|| format!("compressing {} into {}", plain.display(), compressed.display()))?;
  let mut writer = encoder.finish().with_context(|| format!("finishing {}", compressed.display()))?;
  writer.flush().with_context(|| format!("flushing {}", compressed.display()))?;
  writer.get_ref().sync_all().with_context(|| format!("syncing {}", compressed.display()))?;
  Ok(())
}

/// The compressed file's decompressed bytes equal the plain file's, byte for byte.
fn verify_equal(plain: &Path, compressed: &Path) -> anyhow::Result<()> {
  let expected_file = File::open(plain).with_context(|| format!("opening {}", plain.display()))?;
  let mut expected = BufReader::new(expected_file);
  let decoder = File::open(compressed).with_context(|| format!("opening {}", compressed.display()))?;
  let mut actual = zstd::stream::read::Decoder::new(BufReader::new(decoder))
    .with_context(|| format!("reading the zstd frame of {}", compressed.display()))?;
  let mut expected_bytes = vec![0u8; VERIFY_CHUNK_BYTES];
  let mut actual_bytes = vec![0u8; VERIFY_CHUNK_BYTES];
  loop {
    let expected_len =
      fill(&mut expected, &mut expected_bytes).with_context(|| format!("reading {}", plain.display()))?;
    let actual_len =
      fill(&mut actual, &mut actual_bytes).with_context(|| format!("decompressing {}", compressed.display()))?;
    if expected_len != actual_len || expected_bytes[..expected_len] != actual_bytes[..actual_len] {
      bail!("the decompressed content of {} does not match {}", compressed.display(), plain.display());
    }
    if expected_len == 0 {
      return Ok(());
    }
  }
}

/// The count of bytes read from `reader` until `buffer` is full or the reader ends.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
  let mut filled = 0;
  while filled < buffer.len() {
    match reader.read(&mut buffer[filled..])? {
      0 => break,
      read => filled += read,
    }
  }
  Ok(filled)
}

/// The removal of a partial compressed file, leaving the plain log in place.
fn remove_partial(path: &Path) {
  match fs::remove_file(path) {
    Ok(()) => {}
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => tracing::warn!(path = %path.display(), error = %error, "run_log_compaction_partial_cleanup_failed"),
  }
}

/// The subdirectories of `path`, in name order; a missing directory has none.
fn subdirectories(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
  let entries = match fs::read_dir(path) {
    Ok(entries) => entries,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error).with_context(|| format!("listing {}", path.display())),
  };
  let mut directories = Vec::new();
  for entry in entries {
    let entry = entry.with_context(|| format!("listing {}", path.display()))?;
    if entry.file_type().with_context(|| format!("reading an entry of {}", path.display()))?.is_dir() {
      directories.push(entry.path());
    }
  }
  directories.sort();
  Ok(directories)
}

/// The entry names and paths of `path`, unsorted.
fn directory_entries(path: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
  let entries = fs::read_dir(path).with_context(|| format!("listing {}", path.display()))?;
  let mut named = Vec::new();
  for entry in entries {
    let entry = entry.with_context(|| format!("listing {}", path.display()))?;
    named.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
  }
  Ok(named)
}

#[cfg(test)]
mod tests {
  use charliebot_store::run_records::RUN_EVENTS_NAME;
  use serde_json::json;

  use super::*;

  fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-11-06T12:00:00Z", &time::format_description::well_known::Rfc3339).unwrap()
  }

  /// A session holding one run, with a metadata record that ends at `ended_at` and three log files.
  fn session_with_run(root: &Path, run_id: &str, ended_at: Option<&str>) -> PathBuf {
    let session = root.join("session-1");
    let run = layout::runs_dir(&session).join(run_id);
    fs::create_dir_all(&run).unwrap();
    let mut record = json!({"id": run_id, "session_id": "session-1", "kind": "work"});
    if let Some(ended_at) = ended_at {
      record["ended_at"] = json!(ended_at);
    }
    fs::write(run.join(RUN_METADATA_NAME), serde_json::to_string(&record).unwrap()).unwrap();
    fs::write(run.join(RUN_EVENTS_NAME), b"{\"type\": \"result\"}\n").unwrap();
    fs::write(run.join(RAW_LOG_NAME), b"{\"type\": \"user\"}\n{\"type\": \"assistant\"}\n").unwrap();
    fs::write(run.join(format!("{RAW_LOG_NAME}.1")), b"{\"rotated\": true}\n").unwrap();
    run
  }

  fn log_names() -> [String; 3] {
    [RUN_EVENTS_NAME.to_string(), RAW_LOG_NAME.to_string(), format!("{RAW_LOG_NAME}.1")]
  }

  #[test]
  fn a_run_ended_a_day_ago_has_every_log_compressed() {
    let dir = tempfile::tempdir().unwrap();
    let run = session_with_run(dir.path(), "run-1", Some("2026-11-05T12:00:00Z"));
    let report = compact(dir.path(), now()).unwrap();
    assert_eq!(report.compacted, 3, "the event log, the raw log and the rotated raw log");
    for name in log_names() {
      assert!(!run.join(&name).exists(), "{name} is removed once its content is stored compressed");
      let compressed = run.join(format!("{name}{COMPRESSED_SUFFIX}"));
      let bytes = fs::read(&compressed).unwrap_or_else(|error| panic!("{compressed:?}: {error}"));
      assert!(!zstd::stream::decode_all(bytes.as_slice()).unwrap().is_empty());
    }
  }

  #[test]
  fn an_unended_run_and_a_young_run_stay_plain() {
    let dir = tempfile::tempdir().unwrap();
    let unended = session_with_run(dir.path(), "run-1", None);
    let young = session_with_run(dir.path(), "run-2", Some("2026-11-06T00:00:00Z"));
    let report = compact(dir.path(), now()).unwrap();
    assert_eq!(report.compacted, 0);
    for run in [unended, young] {
      for name in log_names() {
        assert!(run.join(&name).is_file(), "{} stays plain", run.join(name).display());
        assert!(!run.join(format!("{name}{COMPRESSED_SUFFIX}")).exists());
      }
    }
  }

  #[test]
  fn a_second_pass_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let run = session_with_run(dir.path(), "run-1", Some("2026-11-01T12:00:00Z"));
    assert_eq!(compact(dir.path(), now()).unwrap().compacted, 3);
    let compressed = run.join(format!("{RUN_EVENTS_NAME}{COMPRESSED_SUFFIX}"));
    let before = fs::read(&compressed).unwrap();
    let report = compact(dir.path(), now()).unwrap();
    assert_eq!(report.compacted, 0);
    assert_eq!(report.already_compacted, 1);
    assert_eq!(fs::read(&compressed).unwrap(), before, "the stored bytes are untouched");
  }

  #[test]
  fn a_mismatch_keeps_the_plain_file_and_removes_the_compressed_one() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join(RUN_EVENTS_NAME);
    fs::write(&plain, b"{\"type\": \"result\"}\n").unwrap();
    let compressed = run_logs::compressed_path(&plain);
    fs::write(&compressed, zstd::stream::encode_all(b"something else\n".as_slice(), 3).unwrap()).unwrap();
    let error = verify_or_remove(&plain, &compressed).expect_err("different content fails the check");
    assert!(error.to_string().contains(RUN_EVENTS_NAME), "the failure names the compressed path: {error}");
    assert!(plain.is_file(), "the plain log stays");
    assert!(!compressed.exists(), "the mismatched compressed file is removed");
  }

  #[test]
  fn an_equal_compressed_file_passes_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join(RAW_LOG_NAME);
    let content = b"{\"type\": \"user\"}\n".repeat(1000);
    fs::write(&plain, &content).unwrap();
    let compressed = run_logs::compressed_path(&plain);
    fs::write(&compressed, zstd::stream::encode_all(content.as_slice(), 3).unwrap()).unwrap();
    verify_or_remove(&plain, &compressed).expect("equal content passes");
    assert!(compressed.is_file());
  }
}
