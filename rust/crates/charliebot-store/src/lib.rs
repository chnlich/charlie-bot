//! The single writer for session metadata and chat event logs.

mod control;
pub mod control_events;
mod events;
mod listing;
pub mod metadata;
mod ndjson;
pub mod run_identity;
pub mod run_records;
pub mod sidebar_state;
mod store;

pub use control::ControlGuard;
pub use events::ChatEvent;
pub use listing::{ArchivedPage, MetadataListing, RawMetadata};
pub use run_records::{
  RunBlocker, RunDisplayState, RunKind, RunOutcome, RunOutcomeKind, RunPage, RunRecord, SequenceRef, StopRequest,
  StopResult,
};
pub use store::{AccountLabelKeeper, AnchorFieldReconciler, SequenceListingFields, Store, StoreHooks};

use std::fmt;

/// Store operation result.
pub type StoreResult<T> = Result<T, StoreError>;

/// An input, serialization, filesystem, or hook error from the store.
#[derive(Debug)]
pub struct StoreError {
  message: String,
}

impl StoreError {
  /// Construct an error with its public message.
  pub fn new(message: impl Into<String>) -> Self {
    Self { message: message.into() }
  }
}

impl fmt::Display for StoreError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
  fn from(error: std::io::Error) -> Self {
    Self::new(error.to_string())
  }
}

impl From<serde_json::Error> for StoreError {
  fn from(error: serde_json::Error) -> Self {
    Self::new(error.to_string())
  }
}
