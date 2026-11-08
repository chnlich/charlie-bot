//! A run log's file name and bytes, plain or compressed.
//!
//! The `run_log_compaction` handler replaces a finished run's log files with `<name>.zst`. Metadata refs keep naming
//! the plain file, so every reader resolves the name through this module: a plain file wins when it exists, otherwise
//! the module opens the `.zst` sibling and yields the decompressed bytes. A writer always names the plain file, and
//! only the compaction handler ever creates the compressed one.

use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use charliebot_core::fsutil::{self, FileBytes};

/// The backend process output of a run, before any rotation suffix.
pub const RAW_LOG_NAME: &str = "agent.raw.ndjson";

/// The translated event stream of a run.
pub const RUN_EVENTS_NAME: &str = "events.jsonl";

/// The suffix of a compressed run log: `events.jsonl` is stored as `events.jsonl.zst`.
pub const COMPRESSED_SUFFIX: &str = ".zst";

/// One of the log files a run directory holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunLogKind {
  /// The backend process output, `agent.raw.ndjson`.
  Raw,
  /// A rotated raw log, `agent.raw.ndjson.N`, where `.1` is the oldest.
  RawRotated(u64),
  /// The run's translated event stream, `events.jsonl`.
  Events,
}

impl RunLogKind {
  /// The file name of the log while it is plain.
  pub fn file_name(&self) -> String {
    match self {
      Self::Raw => RAW_LOG_NAME.to_string(),
      Self::RawRotated(index) => format!("{RAW_LOG_NAME}.{index}"),
      Self::Events => RUN_EVENTS_NAME.to_string(),
    }
  }
}

/// A run log's plain path and the file that holds its bytes.
#[derive(Clone, Debug)]
pub struct RunLog {
  plain: PathBuf,
  stored: PathBuf,
}

impl RunLog {
  /// The path a writer and the stored metadata refs name.
  pub fn plain_path(&self) -> &Path {
    &self.plain
  }

  /// The file that exists: the plain path, or its `.zst` sibling.
  pub fn stored_path(&self) -> &Path {
    &self.stored
  }

  /// Whether the bytes sit compressed.
  pub fn is_compressed(&self) -> bool {
    self.stored != self.plain
  }

  /// The log's bytes: read or mapped from the plain file, decompressed from the `.zst`. `None` for an empty log.
  pub fn read_bytes(&self) -> io::Result<Option<FileBytes>> {
    if !self.is_compressed() {
      return fsutil::read_or_map_file(&self.stored);
    }
    let bytes = decompress(&self.stored)?;
    Ok(if bytes.is_empty() { None } else { Some(FileBytes::Read(bytes)) })
  }

  /// A reader over the log's decompressed bytes, `None` when the stored file is missing.
  pub fn open(&self) -> io::Result<Option<Box<dyn Read + Send>>> {
    let file = match File::open(&self.stored) {
      Ok(file) => file,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(with_path(error, &self.stored)),
    };
    if !self.is_compressed() {
      return Ok(Some(Box::new(BufReader::new(file))));
    }
    let decoder =
      zstd::stream::read::Decoder::new(BufReader::new(file)).map_err(|error| with_path(error, &self.stored))?;
    Ok(Some(Box::new(decoder)))
  }
}

/// The `.zst` path of a plain run-log path.
pub fn compressed_path(plain: &Path) -> PathBuf {
  let mut name = plain.file_name().unwrap_or_default().to_os_string();
  name.push(COMPRESSED_SUFFIX);
  plain.with_file_name(name)
}

/// The stored file of the log at `plain`: the plain file when it exists, else its `.zst`, else `None`.
pub fn resolve(plain: &Path) -> Option<RunLog> {
  if plain.is_file() {
    return Some(RunLog { plain: plain.to_path_buf(), stored: plain.to_path_buf() });
  }
  let compressed = compressed_path(plain);
  compressed.is_file().then(|| RunLog { plain: plain.to_path_buf(), stored: compressed })
}

/// [`resolve`] for one log file of a run directory.
pub fn resolve_in(run_dir: &Path, kind: &RunLogKind) -> Option<RunLog> {
  resolve(&run_dir.join(kind.file_name()))
}

/// The path a reader should open for the log at `plain`: the file that exists, plain or compressed.
pub fn existing_path(plain: &Path) -> Option<PathBuf> {
  resolve(plain).map(|log| log.stored)
}

/// The metadata of the file that exists for the log at `plain`, `None` when neither exists.
pub fn stored_metadata(plain: &Path) -> io::Result<Option<Metadata>> {
  let Some(log) = resolve(plain) else { return Ok(None) };
  fs::metadata(log.stored_path()).map(Some)
}

/// The bytes of the log at `plain`, resolved to the file that exists. `None` for a missing or empty log.
pub fn read_bytes(plain: &Path) -> io::Result<Option<FileBytes>> {
  match resolve(plain) {
    Some(log) => log.read_bytes(),
    None => Ok(None),
  }
}

/// A reader over the decompressed bytes of the log at `plain`, `None` when neither file exists.
pub fn open(plain: &Path) -> io::Result<Option<Box<dyn Read + Send>>> {
  match resolve(plain) {
    Some(log) => log.open(),
    None => Ok(None),
  }
}

/// Whether `name` is one of a run's log files, plain or compressed: `agent.raw.ndjson`, a rotated
/// `agent.raw.ndjson.N`, `events.jsonl`, or either name with [`COMPRESSED_SUFFIX`].
pub fn is_run_log_name(name: &str) -> bool {
  let plain = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
  if plain == RAW_LOG_NAME || plain == RUN_EVENTS_NAME {
    return true;
  }
  match plain.strip_prefix(RAW_LOG_NAME).and_then(|rest| rest.strip_prefix('.')) {
    Some(index) => !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()),
    None => false,
  }
}

/// The whole decompressed content of a `.zst` run log.
fn decompress(path: &Path) -> io::Result<Vec<u8>> {
  let file = File::open(path).map_err(|error| with_path(error, path))?;
  zstd::stream::decode_all(BufReader::new(file)).map_err(|error| with_path(error, path))
}

/// `error` with the path that raised it.
fn with_path(error: io::Error, path: &Path) -> io::Error {
  io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
impl RunLog {
  /// The log's decompressed content as text, for tests.
  fn read_to_string(&self) -> io::Result<String> {
    let mut text = String::new();
    match self.open()? {
      Some(mut reader) => {
        reader.read_to_string(&mut text)?;
      }
      None => return Err(io::Error::new(io::ErrorKind::NotFound, self.plain.display().to_string())),
    }
    Ok(text)
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  fn compressed_log(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let plain = dir.join(name);
    let bytes = zstd::stream::encode_all(content, 3).unwrap();
    fs::write(compressed_path(&plain), bytes).unwrap();
    plain
  }

  #[test]
  fn resolves_a_plain_log_to_itself() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join(RUN_EVENTS_NAME);
    fs::write(&plain, b"{\"a\": 1}\n").unwrap();
    let log = resolve_in(dir.path(), &RunLogKind::Events).expect("the plain log resolves");
    assert_eq!(log.plain_path(), plain);
    assert_eq!(log.stored_path(), plain);
    assert!(!log.is_compressed());
    assert_eq!(log.read_to_string().unwrap(), "{\"a\": 1}\n");
  }

  #[test]
  fn resolves_a_compressed_log_and_yields_its_decompressed_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let content = b"{\"a\": 1}\n{\"b\": 2}\n".repeat(100);
    let plain = compressed_log(dir.path(), RAW_LOG_NAME, &content);
    let log = resolve_in(dir.path(), &RunLogKind::Raw).expect("the compressed log resolves");
    assert_eq!(log.plain_path(), plain);
    assert_eq!(log.stored_path(), compressed_path(&plain));
    assert!(log.is_compressed());
    assert_eq!(log.read_to_string().unwrap().as_bytes(), content);
    assert_eq!(log.read_bytes().unwrap().unwrap().as_ref(), content);
  }

  #[test]
  fn a_missing_log_resolves_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(resolve_in(dir.path(), &RunLogKind::Events).is_none());
    assert!(resolve_in(dir.path(), &RunLogKind::RawRotated(1)).is_none());
    assert!(read_bytes(&dir.path().join(RUN_EVENTS_NAME)).unwrap().is_none());
    assert!(open(&dir.path().join(RUN_EVENTS_NAME)).unwrap().is_none());
  }

  #[test]
  fn a_plain_file_wins_over_its_compressed_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join(RUN_EVENTS_NAME);
    fs::write(&plain, b"plain").unwrap();
    compressed_log(dir.path(), RUN_EVENTS_NAME, b"compressed");
    let log = resolve(&plain).expect("the log resolves");
    assert!(!log.is_compressed());
    assert_eq!(log.read_to_string().unwrap(), "plain");
  }

  #[test]
  fn rotated_names_resolve_like_the_raw_name() {
    let dir = tempfile::tempdir().unwrap();
    compressed_log(dir.path(), &RunLogKind::RawRotated(3).file_name(), b"rotated");
    let log = resolve_in(dir.path(), &RunLogKind::RawRotated(3)).expect("the rotated log resolves");
    assert_eq!(log.read_to_string().unwrap(), "rotated");
  }

  #[test]
  fn run_log_names_cover_the_plain_and_compressed_forms_only() {
    for name in ["agent.raw.ndjson", "agent.raw.ndjson.zst", "agent.raw.ndjson.1", "agent.raw.ndjson.12.zst"] {
      assert!(is_run_log_name(name), "{name} is a run log");
    }
    for name in ["events.jsonl", "events.jsonl.zst"] {
      assert!(is_run_log_name(name), "{name} is a run log");
    }
    for name in ["agent.raw.cursor", "agent.stderr.log", "agent.raw.ndjson.x", "events.jsonl.bak", "metadata.json"] {
      assert!(!is_run_log_name(name), "{name} is not a run log");
    }
  }
}
