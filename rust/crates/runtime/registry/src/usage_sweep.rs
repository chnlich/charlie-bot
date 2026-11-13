//! The cold-storage sweep's per-category counter: the files one source deletes, their bytes, and the
//! [`CategoryResult`] the source reports.

use std::fs;
use std::path::Path;

use crate::usage::{CategoryResult, SWEEP_FILE_DELETE_FAILED_EVENT, SWEEP_FILE_STAT_FAILED_EVENT};

/// The count and freed bytes of one sweep category.
///
/// A file-unit source counts each deleted file. A directory-unit source adds each file's bytes with `count` false and
/// counts each directory once, after removing it.
#[derive(Default)]
pub struct SweepCounter {
  count: u64,
  bytes: u64,
}

impl SweepCounter {
  /// The deletion of one file, or only its counting in a dry run, which adds its bytes. `count` adds one to the
  /// category's count. A file that cannot be statted or removed is logged and skipped.
  pub fn delete_file(&mut self, path: &Path, dry_run: bool, count: bool) {
    let size = match fs::metadata(path) {
      Ok(metadata) => metadata.len(),
      Err(error) => {
        tracing::warn!(
          target: "charliebot_agents",
          path = %path.display(),
          error = %error,
          "{}",
          SWEEP_FILE_STAT_FAILED_EVENT
        );
        return;
      }
    };
    if !dry_run && let Err(error) = fs::remove_file(path) {
      tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        error = %error,
        "{}",
        SWEEP_FILE_DELETE_FAILED_EVENT
      );
      return;
    }
    if count {
      self.count += 1;
    }
    self.bytes += size;
  }

  /// The addition of one to the category's count, for a directory-unit source that counts each directory it
  /// removes.
  pub fn count_one(&mut self) {
    self.count += 1;
  }

  /// The category result these totals report.
  pub fn category(&self, name: &'static str, unit: &'static str) -> CategoryResult {
    CategoryResult { name, unit, count: self.count, bytes: self.bytes }
  }
}
