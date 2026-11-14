//! The token usage page's memo: the last finished body, kept while the ledger and the page's inputs hold their value.
//!
//! A page view reads every row of the ledger's page snapshot, renders the document and deflates it, and the ledger
//! moves only when a capture commits. The memo keeps the finished body and the ledger version it was built at. A
//! repeat view pays one version check instead of the read, the render and the deflate.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;

use crate::ledger::UsageLedger;

/// The page's inputs besides the ledger's version: a change in any of them renders a different document.
pub struct PageInputs {
  /// The host whose capture stamp the page names.
  pub host: String,
  /// The Pacific week the window's figures cover.
  pub week: String,
  /// The config the rows' accounts attribute against.
  pub config: Arc<CharlieBotConfig>,
  /// The revision of the template and static trees the document was built from.
  pub template_revision: u64,
  /// The static asset version the document names in its asset URLs.
  pub asset_version: String,
  /// The request's query string, which the page carries into its templates.
  pub query: String,
}

impl PageInputs {
  /// Whether a body built from these inputs still answers a request with `current`.
  fn holds(&self, current: &PageInputs) -> bool {
    self.host == current.host
      && self.week == current.week
      && Arc::ptr_eq(&self.config, &current.config)
      && self.template_revision == current.template_revision
      && self.asset_version == current.asset_version
      && self.query == current.query
  }
}

/// The body the last view of one ledger version built.
struct Entry {
  inputs: PageInputs,
  version: i64,
  html: String,
}

/// The file one ledger path named when the memo opened it. A path that names another file — the ledger deleted and
/// created again under the running server — holds a ledger the memo's connection does not read. The memo reopens its
/// connection for that path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileId {
  device: u64,
  inode: u64,
}

fn file_id(path: &Path) -> Option<FileId> {
  use std::os::unix::fs::MetadataExt;
  let metadata = fs::metadata(path).ok()?;
  Some(FileId { device: metadata.dev(), inode: metadata.ino() })
}

/// The page's memo: one ledger connection and the body its last view built.
#[derive(Default)]
pub struct PageMemo {
  ledger: Option<UsageLedger>,
  file: Option<FileId>,
  entry: Option<Entry>,
}

impl PageMemo {
  /// The page body for `inputs`: the memo's body while the ledger still holds the version it was built at and every
  /// input holds its value. Otherwise it is the body `build` renders.
  ///
  /// The version is read before the body is built. A capture that commits while the body builds leaves the older
  /// version in the memo, so the next view rebuilds rather than serving rows the ledger has moved past.
  pub fn body<E: From<anyhow::Error>>(
    &mut self,
    ledger_path: &Path,
    inputs: PageInputs,
    build: impl FnOnce(&mut UsageLedger) -> Result<String, E>,
  ) -> Result<&str, E> {
    self.connect(ledger_path)?;
    let version = self.connection().data_version()?;
    let held = self.entry.as_ref().is_some_and(|entry| entry.version == version && entry.inputs.holds(&inputs));
    if !held {
      let html = build(self.connection_mut())?;
      self.entry = Some(Entry { inputs, version, html });
    }
    Ok(&self.entry.as_ref().expect("the memo holds the body it served or built").html)
  }

  /// Open the ledger at `ledger_path` unless the memo already reads that file. Another path, or a path that names
  /// another file, holds another ledger's body, so the entry goes with the connection.
  fn connect(&mut self, ledger_path: &Path) -> anyhow::Result<()> {
    let reads_this_file =
      self.ledger.as_ref().is_some_and(|ledger| ledger.path() == ledger_path) && self.file == file_id(ledger_path);
    if reads_this_file {
      return Ok(());
    }
    self.ledger = Some(UsageLedger::open(ledger_path)?);
    self.file = file_id(ledger_path);
    self.entry = None;
    Ok(())
  }

  fn connection(&self) -> &UsageLedger {
    self.ledger.as_ref().expect("the memo opened its connection before it reads the version")
  }

  fn connection_mut(&mut self) -> &mut UsageLedger {
    self.ledger.as_mut().expect("the memo opened its connection before it builds a body")
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::sync::Arc;

  use super::*;
  use crate::ledger::default_ledger_path;
  use charliebot_core::config::{self, ConfigSchema};

  /// One page body per ledger version and input set: a repeat view builds nothing, and a capture's commit or a changed
  /// input builds the next body.
  #[test]
  fn a_body_is_built_once_per_ledger_version_and_input_set() {
    let home = tempfile::tempdir().expect("the home directory");
    let path = default_ledger_path(home.path());
    let config = Arc::new(config::load_config(home.path(), &ConfigSchema::default()).expect("the config loads"));
    let inputs = |query: &str| PageInputs {
      host: "host-a".to_string(),
      week: "2026-11-02 → 2026-11-08".to_string(),
      config: Arc::clone(&config),
      template_revision: 1,
      asset_version: "v1".to_string(),
      query: query.to_string(),
    };
    let builds = Cell::new(0);
    let mut memo = PageMemo::default();
    let body = |memo: &mut PageMemo, query: &str, text: &str| {
      memo
        .body(&path, inputs(query), |_| {
          builds.set(builds.get() + 1);
          Ok::<String, anyhow::Error>(text.to_string())
        })
        .expect("the body builds")
        .to_string()
    };

    assert_eq!(body(&mut memo, "", "one"), "one");
    assert_eq!(body(&mut memo, "", "two"), "one", "a repeat view serves the body already built");
    assert_eq!(builds.get(), 1);

    let mut writer = UsageLedger::open(&path).expect("the ledger opens");
    writer.record_file("host-a", "/logs/new.jsonl", "sig-new", &[]).expect("the capture commits");
    writer.close().expect("the ledger closes");
    assert_eq!(body(&mut memo, "", "three"), "three", "a capture's commit moves the version");
    assert_eq!(builds.get(), 2);

    assert_eq!(body(&mut memo, "?view=week", "four"), "four", "a changed input renders its own body");
    assert_eq!(builds.get(), 3);

    fs::remove_file(&path).expect("the ledger is deleted");
    assert_eq!(body(&mut memo, "?view=week", "five"), "five", "a ledger created again under the path is read again");
    assert_eq!(builds.get(), 4);
  }
}
