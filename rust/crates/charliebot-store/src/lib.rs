//! The single writer for session metadata and chat event logs.

pub mod aggregator;
mod control;
pub mod control_events;
mod events;
pub mod hooks;
mod listing;
pub mod message_events;
pub mod message_utils;
pub mod metadata;
mod ndjson;
pub mod projection;
pub mod run_identity;
pub mod run_records;
mod session_events;
pub mod sidebar_state;
mod store;
pub mod streaming;
mod thinking;

pub use aggregator::{MessageAggregator, TOOL_PREVIEW_CHARS};
pub use control::ControlGuard;
pub use events::ChatEvent;
pub use hooks::{AfterTurnWork, EmptyRuntimeEventHooks, EventRenderer, RuntimeEventHooks};
pub use listing::{ArchivedPage, MetadataListing, RawMetadata};
pub use message_utils::{MessageProvenance, SESSION_NOT_FOUND_DETAIL, build_scheduled_trigger_event};
pub use projection::MessageProjection;
pub use run_records::{
  RetryRunFields, RunBlocker, RunDisplayState, RunKind, RunOrder, RunOutcome, RunOutcomeKind, RunPage, RunRecord,
  SequenceRef, StopRequest, StopResult,
};
pub use session_events::RAW_EVENTS_REPLACED_BY_DELTAS;
pub use store::{AccountLabelKeeper, AnchorFieldReconciler, SequenceListingFields, Store, StoreHooks};
pub use streaming::{SIDEBAR_CHANNEL, STREAM_COALESCE_INTERVAL, StreamingManager, Subscription, session_channel};

use std::fmt;

/// Store operation result.
pub type StoreResult<T> = Result<T, StoreError>;

/// An input, serialization, filesystem, or hook error from the store.
#[derive(Debug)]
pub struct StoreError {
  message: String,
  status_code: Option<u16>,
}

impl StoreError {
  /// Construct an error with its public message.
  pub fn new(message: impl Into<String>) -> Self {
    Self { message: message.into(), status_code: None }
  }

  /// Construct a client-visible not-found error.
  pub fn not_found(detail: impl Into<String>) -> Self {
    Self { message: detail.into(), status_code: Some(404) }
  }

  /// Return the HTTP status associated with this error, when one exists.
  pub fn status_code(&self) -> Option<u16> {
    self.status_code
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
