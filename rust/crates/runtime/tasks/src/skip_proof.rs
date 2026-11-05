//! The startup-only skip proof of the boot walk.
//!
//! A boot's skip check answers one node's verdict from that node's own files: its log's tail for the task state, its
//! run records for the keep reasons, and its worktree paths' existence. This file carries the verdicts of the nodes
//! the last pass skipped, each beside the identities of the two durable files the verdict was proven from. A later
//! boot serves a node's verdict from this file when both files still carry the recorded identities, so a steady-state
//! boot's skip checks cost one stat of each node's log and metadata plus one lookup, and only a node whose files
//! changed since the proof run its own check.
//!
//! Only skip verdicts are recorded. A node the pass kept has no row, so the next boot runs its own check, and a node
//! whose files changed after its row was written fails the identity comparison and runs its own check too.
//!
//! The file is startup-only and is never synced: a torn or stale copy names identities that no longer match, so every
//! node it serves was validated against its own files first, and the nodes it cannot serve read their files as before.
//! A marker whose `pass_started_at` is later than the proof's is not a case the proof has to reject: the row's
//! identities, not a timestamp, decide whether a node is served.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

/// The format version of the proof file. A file of another version serves no node.
const SKIP_PROOF_VERSION: u64 = 1;
/// The proof's home-relative path, beside the boot marker.
pub(crate) const SKIP_PROOF_PATH: &str = "state/boot_skip_proof.json";
/// The separator inside one row. Session ids and backend names hold no whitespace other than this separator's bytes.
const FIELD_SEPARATOR: char = '\t';

/// The identities of the two durable files a node's skip verdict was proven from: its chat-event log and its metadata.
///
/// Both are compared for exact equality, so a node whose log grew, whose metadata was rewritten, or whose file was
/// replaced runs its own check. [`SkipProofRow::proven`] reads the pair from the node's session directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SkipProofSource {
  log_size: u64,
  log_mtime_ns: i128,
  meta_size: u64,
  meta_mtime_ns: i128,
}

impl SkipProofSource {
  /// The identity of `session_dir`'s metadata file, or `None` when it is absent or not a regular file. A directory with
  /// no metadata file is not a node, so this one stat answers the walk's existence test as well.
  ///
  /// Any stat error reads as absent, which is what the walk's own `Path::is_file` test answers; a directory entry that
  /// is not a directory fails here with `ENOTDIR`.
  pub(crate) fn metadata_file(session_dir: &Path) -> Option<FileIdentity> {
    file_identity(&charliebot_store::layout::metadata_path(session_dir)).ok().flatten().filter(|identity| identity.2)
  }

  /// The identities of `session_dir`'s log and metadata from a metadata identity the caller already read, or `None`
  /// when the log is absent. A node without a log cannot be skipped by the walk's own check either, so the proof writes
  /// no row for it.
  pub(crate) fn read(session_dir: &Path, meta: FileIdentity) -> io::Result<Option<Self>> {
    let Some(log) = file_identity(&charliebot_store::layout::chat_events_path(session_dir))? else {
      return Ok(None);
    };
    Ok(Some(Self { log_size: log.0, log_mtime_ns: log.1, meta_size: meta.0, meta_mtime_ns: meta.1 }))
  }

  /// Whether the log was modified at or after `threshold_ns`, the boot marker's time less the coarse-clock slack.
  pub(crate) fn log_changed_since(&self, threshold_ns: i128, slack_ns: i128) -> bool {
    self.log_mtime_ns >= threshold_ns - slack_ns
  }

  /// Whether the metadata file was modified at or after `threshold_ns`, the boot marker's time less the slack.
  pub(crate) fn meta_changed_since(&self, threshold_ns: i128, slack_ns: i128) -> bool {
    self.meta_mtime_ns >= threshold_ns - slack_ns
  }
}

/// One skipped node's verdict and the identities it was proven from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkipProofRow {
  pub(crate) session_id: String,
  pub(crate) source: SkipProofSource,
  /// The newest run's backend, which the skip serves to the display-backend map.
  pub(crate) newest_backend: Option<String>,
}

/// The rows a boot can serve, keyed by session id.
#[derive(Debug, Default)]
pub(crate) struct SkipProof {
  rows: HashMap<String, SkipProofRow>,
}

impl SkipProof {
  /// The proof a home holds, or an empty proof when the file is absent or not a proof of this version. An unreadable
  /// file other than an absence is logged rather than failing the boot: the proof only saves work.
  pub(crate) fn read(home: &Path) -> Self {
    let path = home.join(SKIP_PROOF_PATH);
    let raw = match std::fs::read(&path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
      Err(error) => {
        tracing::warn!(target: "charliebot_tasks", path = %path.display(), error = %error, "skip_proof_unreadable");
        return Self::default();
      }
    };
    match parse(&raw) {
      Some(rows) => Self { rows: rows.into_iter().map(|row| (row.session_id.clone(), row)).collect() },
      None => {
        tracing::warn!(target: "charliebot_tasks", path = %path.display(), size = raw.len(), "skip_proof_malformed");
        Self::default()
      }
    }
  }

  /// The newest run's backend for a node whose files still carry the identities its row was written with, or `None`
  /// when no row serves this node. The outer `Some` carries the possibly absent backend.
  pub(crate) fn served(&self, session_id: &str, source: &SkipProofSource) -> Option<Option<String>> {
    self.rows.get(session_id).filter(|row| row.source == *source).map(|row| row.newest_backend.clone())
  }
}

/// Write the proof the pass left, holding one row per skipped node. The file is startup-only, so the write takes no
/// sync. A write failure leaves the previous proof in place, whose rows the next boot validates by identity anyway.
pub(crate) fn write(home: &Path, rows: &[SkipProofRow]) -> io::Result<()> {
  let path = home.join(SKIP_PROOF_PATH);
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  let mut text = String::with_capacity(rows.len().saturating_mul(96) + 64);
  text.push_str(&charliebot_core::text_repr::json_dumps(
    &json!({"version": SKIP_PROOF_VERSION}),
    &charliebot_core::text_repr::DEFAULT_JSON,
  ));
  text.push('\n');
  for row in rows {
    text.push_str(&row.session_id);
    text.push(FIELD_SEPARATOR);
    text.push_str(&row.source.log_size.to_string());
    text.push(FIELD_SEPARATOR);
    text.push_str(&row.source.log_mtime_ns.to_string());
    text.push(FIELD_SEPARATOR);
    text.push_str(&row.source.meta_size.to_string());
    text.push(FIELD_SEPARATOR);
    text.push_str(&row.source.meta_mtime_ns.to_string());
    text.push(FIELD_SEPARATOR);
    if let Some(backend) = row.newest_backend.as_deref() {
      text.push_str(backend);
    }
    text.push('\n');
  }
  charliebot_core::fsutil::atomic_write_text(&path, &text)
}

/// The rows a proof file holds, or `None` when its header is not this version. A row whose fields do not parse is
/// dropped: its node then runs its own check instead of being served a verdict nobody recorded.
fn parse(raw: &[u8]) -> Option<Vec<SkipProofRow>> {
  let text = std::str::from_utf8(raw).ok()?;
  let mut lines = text.split('\n');
  let header: Value = serde_json::from_str(lines.next()?).ok()?;
  if header.get("version")?.as_u64()? != SKIP_PROOF_VERSION {
    return None;
  }
  let rows = lines.filter(|line| !line.is_empty()).filter_map(parse_row).collect();
  Some(rows)
}

/// One row of the proof file, or `None` when a field does not parse.
fn parse_row(line: &str) -> Option<SkipProofRow> {
  let mut fields = line.split(FIELD_SEPARATOR);
  let session_id = fields.next()?.to_string();
  let source = SkipProofSource {
    log_size: fields.next()?.parse().ok()?,
    log_mtime_ns: fields.next()?.parse().ok()?,
    meta_size: fields.next()?.parse().ok()?,
    meta_mtime_ns: fields.next()?.parse().ok()?,
  };
  let backend = fields.next()?.to_string();
  if fields.next().is_some() {
    return None;
  }
  Some(SkipProofRow { session_id, source, newest_backend: (!backend.is_empty()).then_some(backend) })
}

/// A file's `(size, mtime_ns, is_file)`. The third field lets the one stat of the metadata file answer both the walk's
/// existence test and the proof's identity check.
pub(crate) type FileIdentity = (u64, i128, bool);

/// A file's [`FileIdentity`], or `None` when it is absent. Another read error is returned to the caller, which the
/// skip check turns into a failing node, as its own stat does.
fn file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
  use std::os::unix::fs::MetadataExt;
  match std::fs::metadata(path) {
    Ok(metadata) => Ok(Some((
      metadata.size(),
      charliebot_core::timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()),
      metadata.is_file(),
    ))),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(session: &str, backend: Option<&str>) -> SkipProofRow {
    SkipProofRow {
      session_id: session.to_string(),
      source: SkipProofSource { log_size: 12, log_mtime_ns: 34, meta_size: 56, meta_mtime_ns: 78 },
      newest_backend: backend.map(str::to_string),
    }
  }

  #[test]
  fn a_written_proof_serves_only_the_rows_whose_identities_still_hold() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    write(home.path(), &[row("open-none", None), row("open-backend", Some("claude:main"))]).expect("the proof writes");
    let proof = SkipProof::read(home.path());
    let source = row("open-none", None).source;
    assert_eq!(proof.served("open-none", &source), Some(None));
    assert_eq!(proof.served("open-backend", &source), Some(Some("claude:main".to_string())));
    assert_eq!(proof.served("absent", &source), None, "a node with no row runs its own check");

    let changed = SkipProofSource { log_size: 13, ..source };
    assert_eq!(proof.served("open-none", &changed), None, "a changed log runs the node's own check");
    let changed = SkipProofSource { meta_mtime_ns: 79, ..source };
    assert_eq!(proof.served("open-none", &changed), None, "a changed metadata file runs the node's own check");
  }

  #[test]
  fn a_missing_torn_or_foreign_proof_serves_no_node() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    let source = row("open-none", None).source;
    assert_eq!(SkipProof::read(home.path()).served("open-none", &source), None, "an absent proof serves nothing");

    let path = home.path().join(SKIP_PROOF_PATH);
    std::fs::create_dir_all(path.parent().expect("the proof path has a parent")).expect("the state directory exists");
    std::fs::write(&path, b"{\"version\": 1}\nopen-none\t12\t34\t56\n").expect("the torn proof writes");
    assert_eq!(SkipProof::read(home.path()).served("open-none", &source), None, "a torn row serves nothing");

    std::fs::write(&path, b"{\"version\": 2}\nopen-none\t12\t34\t56\t78\t\n").expect("the foreign proof writes");
    assert_eq!(SkipProof::read(home.path()).served("open-none", &source), None, "a foreign version serves nothing");
  }
}
