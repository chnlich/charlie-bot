use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub struct SweepRoots {
  pub worktree_root: PathBuf,
  pub scratch_root: PathBuf,
  pub sessions_root: PathBuf,
  pub proc_root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SweepRow {
  pub kind: String,
  pub path: PathBuf,
  pub action: String,
  pub reason: String,
  pub freed_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SweepReport {
  pub rows: Vec<SweepRow>,
  pub deleted_count: usize,
  pub freed_bytes: u64,
  pub skipped_count: usize,
  pub unreadable_proc_count: usize,
  pub failures: Vec<(PathBuf, String)>,
}

impl SweepRow {
  pub fn to_tsv(&self) -> String {
    format!("{}\t{}\t{}\t{}\t{}", self.kind, self.path.display(), self.freed_bytes, self.action, self.reason)
  }
}

impl SweepReport {
  pub fn total_line(&self) -> String {
    format!(
      "total\t{}\t{}\t{}\t{}",
      self.deleted_count, self.freed_bytes, self.skipped_count, self.unreadable_proc_count
    )
  }
}

pub fn run_sweep_blocking(_roots: SweepRoots, _apply: bool, _now: DateTime<Utc>) -> Result<SweepReport> {
  Ok(SweepReport::default())
}

#[cfg(test)]
mod tests;
