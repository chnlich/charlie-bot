//! The task-execution stack that this crate's integration tests and `charliebot-improve`'s port tests run against.
//!
//! A test-only crate cannot share files by relative path without hiding the dependency from Cargo, so this module is a
//! library surface behind the `test-fixtures` feature. A production build leaves the feature off and compiles neither
//! module.

use std::path::{Path, PathBuf};

use charliebot_store::{StoreError, StoreResult};
use futures_util::future::BoxFuture;

use crate::completion::PendingTriggerCancel;

pub mod process_guard;
pub mod recovery_support;

/// The pending-trigger owner of a test stack whose workers hold no triggers. The trigger crate depends on this crate,
/// so a test here cannot register the real owner; the stand-in answers the cancel with no trigger cancelled.
pub struct NoPendingTriggers;

impl PendingTriggerCancel for NoPendingTriggers {
  fn cancel_pending<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async { Ok(Vec::new()) })
  }
}

/// A pending-trigger owner whose cancel fails, as a trigger store that cannot save a record does.
pub struct FailingPendingTriggers;

impl PendingTriggerCancel for FailingPendingTriggers {
  fn cancel_pending<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async { Err(StoreError::new("synthetic trigger store refused the cancel")) })
  }
}

/// The repo a test stack serves through `RepoRoot`: every top-level entry of `repo` linked into `home/checkout`
/// except `hooks`, which a fresh directory replaces with an empty turn-end check list, so a turn's checkpoint runs
/// none of the served repo's checks.
pub fn served_checkout(repo: &Path, home: &Path) -> anyhow::Result<PathBuf> {
  let checkout = home.join("checkout");
  std::fs::create_dir_all(&checkout)?;
  for entry in std::fs::read_dir(repo)? {
    let entry = entry?;
    if entry.file_name() == "hooks" {
      continue;
    }
    let link = checkout.join(entry.file_name());
    if !link.exists() {
      std::os::unix::fs::symlink(entry.path(), link)?;
    }
  }
  std::fs::create_dir_all(checkout.join("hooks"))?;
  std::fs::write(checkout.join("hooks/turn_end.json"), "[]")?;
  Ok(checkout)
}
