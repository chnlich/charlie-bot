//! The task-execution stack that this crate's integration tests and `charliebot-improve`'s port tests run against.
//!
//! A test-only crate cannot share files by relative path without hiding the dependency from Cargo, so this module is a
//! library surface behind the `test-fixtures` feature. A production build leaves the feature off and compiles neither
//! module.

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
