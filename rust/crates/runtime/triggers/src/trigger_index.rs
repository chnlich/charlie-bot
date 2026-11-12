//! The startup-only index of trigger files a boot proved not pending.
//!
//! The recovery trigger walk reads every session's `triggers` directory and opens every trigger file, because only a
//! file's own content names its status, and a file's status is what decides whether the trigger starts a task. A trigger
//! that fired or was cancelled never returns to pending, and every status write replaces its file, so a row carrying the
//! file's `(size, mtime_ns)` serves that file's non-pending status to a later boot without opening it. The boot stats
//! every trigger file anyway, so the row costs one lookup and saves the read and parse.
//!
//! A row is used only while the file carries the identity it recorded, so a file rewritten after the row was written is
//! read as before; a file the index has no row for (a new trigger, or a file that was pending) is read too. A missing,
//! torn or foreign-version index serves nothing, and the boot reads every trigger file as it did before. The index is
//! startup-only and is never synced: an unserved row costs one file read, never a wrong status.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use charliebot_core::home::boot_trigger_index_path;
use charliebot_core::home::startup_rows;

/// The format version of the index file. A file of another version serves no row.
const TRIGGER_INDEX_VERSION: u64 = 1;

/// A trigger file's `(size, mtime_ns)`, the identity a row is valid under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TriggerFileIdentity {
  size: u64,
  mtime_ns: i128,
}

impl TriggerFileIdentity {
  /// The identity of a file's stat, the same stat the walk already made for it.
  pub(crate) fn of(metadata: &std::fs::Metadata) -> Self {
    use std::os::unix::fs::MetadataExt;
    Self {
      size: metadata.size(),
      mtime_ns: charliebot_core::timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()),
    }
  }
}

/// One trigger file a boot proved not pending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TriggerIndexRow {
  pub(crate) session_id: String,
  pub(crate) file_name: String,
  pub(crate) identity: TriggerFileIdentity,
}

#[cfg(test)]
static SERVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The rows a boot can serve, keyed by session id and then by trigger file name.
#[derive(Debug, Default)]
pub(crate) struct TriggerIndex {
  rows: HashMap<String, HashMap<String, TriggerFileIdentity>>,
}

impl TriggerIndex {
  /// The index a home holds, or an empty index when the file is absent or not an index of this version. An unreadable
  /// file other than an absence is logged rather than failing the boot: the index only saves work.
  pub(crate) fn read(home: &Path) -> Self {
    let path = boot_trigger_index_path(home);
    let raw = match std::fs::read(&path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
      Err(error) => {
        tracing::warn!(target: "charliebot_triggers", path = %path.display(), error = %error, "trigger_index_unreadable");
        return Self::default();
      }
    };
    match parse(&raw) {
      Some(rows) => {
        let mut index = Self::default();
        for row in rows {
          index.rows.entry(row.session_id).or_default().insert(row.file_name, row.identity);
        }
        index
      }
      None => {
        tracing::warn!(target: "charliebot_triggers", path = %path.display(), size = raw.len(), "trigger_index_malformed");
        Self::default()
      }
    }
  }

  /// Whether this boot may skip reading a file: the index proved this file not pending under this identity.
  pub(crate) fn serves(&self, session_id: &str, file_name: &str, identity: TriggerFileIdentity) -> bool {
    let served = self.rows.get(session_id).and_then(|files| files.get(file_name)) == Some(&identity);
    #[cfg(test)]
    if served {
      SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    served
  }
}

/// Write the rows this boot proved, replacing the previous index. The boot writes the rows of the files it read and
/// the rows it served, so a file's row survives while its identity holds and disappears when the file does.
pub(crate) fn write(home: &Path, rows: &[TriggerIndexRow]) -> io::Result<()> {
  let fields = rows
    .iter()
    .map(|row| {
      vec![
        row.session_id.clone(),
        row.file_name.clone(),
        row.identity.size.to_string(),
        row.identity.mtime_ns.to_string(),
      ]
    })
    .collect::<Vec<_>>();
  startup_rows::write(&boot_trigger_index_path(home), TRIGGER_INDEX_VERSION, &fields)
}

/// The rows an index file holds, or `None` when its header is not this version. A row whose fields do not parse is
/// dropped: its file is read, which only costs the read the row would have saved.
fn parse(raw: &[u8]) -> Option<Vec<TriggerIndexRow>> {
  let rows = startup_rows::parse(raw, TRIGGER_INDEX_VERSION)?;
  Some(rows.iter().filter_map(|fields| parse_row(fields)).collect())
}

/// One row of the index file, or `None` when a field does not parse.
fn parse_row(fields: &[String]) -> Option<TriggerIndexRow> {
  let [session_id, file_name, size, mtime_ns] = fields else {
    return None;
  };
  if session_id.is_empty() || file_name.is_empty() {
    return None;
  }
  let identity = TriggerFileIdentity { size: size.parse().ok()?, mtime_ns: mtime_ns.parse().ok()? };
  Some(TriggerIndexRow { session_id: session_id.clone(), file_name: file_name.clone(), identity })
}

/// The rows this boot served from the index, and a reset for the test that reads it. A test uses the count to tell a
/// served file from a read one.
#[cfg(test)]
pub(crate) fn served_count() -> usize {
  SERVED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Reset the served-row count a test reads.
#[cfg(test)]
pub(crate) fn reset_served_count() {
  SERVED.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn identity(size: u64, mtime_ns: i128) -> TriggerFileIdentity {
    TriggerFileIdentity { size, mtime_ns }
  }

  fn row(session: &str, file: &str, identity: TriggerFileIdentity) -> TriggerIndexRow {
    TriggerIndexRow { session_id: session.to_string(), file_name: file.to_string(), identity }
  }

  #[test]
  fn a_written_index_serves_only_the_files_that_keep_their_identity() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    write(home.path(), &[row("session-a", "trigger-1.json", identity(12, 34))]).expect("the index writes");
    let index = TriggerIndex::read(home.path());
    assert!(index.serves("session-a", "trigger-1.json", identity(12, 34)), "the recorded identity is served");
    assert!(!index.serves("session-a", "trigger-1.json", identity(13, 34)), "a changed size is read");
    assert!(!index.serves("session-a", "trigger-1.json", identity(12, 35)), "a changed mtime is read");
    assert!(!index.serves("session-b", "trigger-1.json", identity(12, 34)), "another session's file is read");
    assert!(!index.serves("session-a", "trigger-2.json", identity(12, 34)), "a file with no row is read");
  }

  #[test]
  fn a_missing_torn_or_foreign_index_serves_nothing() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    assert!(!TriggerIndex::read(home.path()).serves("session-a", "trigger-1.json", identity(12, 34)));

    let path = boot_trigger_index_path(home.path());
    std::fs::create_dir_all(path.parent().expect("the index path has a parent")).expect("the state directory exists");
    std::fs::write(&path, b"{\"version\": 1}\nsession-a\ttrigger-1.json\t12\n").expect("the torn index writes");
    assert!(!TriggerIndex::read(home.path()).serves("session-a", "trigger-1.json", identity(12, 34)));

    std::fs::write(&path, b"{\"version\": 2}\nsession-a\ttrigger-1.json\t12\t34\n").expect("the foreign index writes");
    assert!(!TriggerIndex::read(home.path()).serves("session-a", "trigger-1.json", identity(12, 34)));
  }

  #[test]
  fn a_row_holds_the_identity_of_the_file_it_names() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    let dir = home.path().join("session-a").join("triggers");
    std::fs::create_dir_all(&dir).expect("the trigger directory exists");
    let file = dir.join("trigger-1.json");
    std::fs::write(&file, b"{}").expect("the trigger file writes");
    let stat = std::fs::metadata(&file).expect("the trigger file stats");
    let identity = TriggerFileIdentity::of(&stat);
    write(home.path(), &[row("session-a", "trigger-1.json", identity)]).expect("the index writes");
    assert!(TriggerIndex::read(home.path()).serves("session-a", "trigger-1.json", identity));
  }
}
