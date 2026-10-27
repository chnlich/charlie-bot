//! The single writer for session metadata and chat event logs.

pub mod aggregator;
pub mod chat_events;
pub mod compact;
mod control;
pub mod control_events;
mod event_state;
mod event_types;
mod events;
pub mod hooks;
mod listing;
pub mod message_events;
pub mod message_utils;
pub mod metadata;
mod ndjson;
pub mod projection;
mod read_memos;
mod run_facts;
pub mod run_identity;
pub mod run_records;
mod session_events;
pub mod session_fork;
pub mod sidebar_state;
mod store;
pub mod streaming;
pub mod task_child;
mod task_tree;
mod task_tree_state;
mod thinking;
pub mod trigger_files;
#[cfg(any(test, feature = "write-faults"))]
mod write_fault_types;
#[cfg(any(test, feature = "write-faults"))]
pub mod write_faults;

pub use aggregator::{MessageAggregator, TOOL_PREVIEW_CHARS};
pub use compact::CompactValue;
pub use control::ControlGuard;
pub use events::{ChatEvent, ChatEventHistory, ChatEventsDelta, SharedChatEvent, SharedChatEventRange};
pub use hooks::{AfterTurnWork, EmptyRuntimeEventHooks, EventRenderer, RuntimeEventHooks};
pub use listing::{ArchivedPage, MetadataListing, RawMetadata, SharedSubtreeRoots};
pub use message_utils::{MessageProvenance, SESSION_NOT_FOUND_DETAIL, build_scheduled_trigger_event};
pub use projection::MessageProjection;
pub use run_records::{
  RetryRunFields, RunBlocker, RunDisplayState, RunFacts, RunKind, RunOrder, RunOutcome, RunOutcomeKind, RunPage,
  RunRecord, SequenceRef, StopRequest, StopResult,
};
pub use session_events::RAW_EVENTS_REPLACED_BY_DELTAS;
pub use store::{
  AccountLabelKeeper, AnchorFieldReconciler, HistoryWork, SequenceArchiveHook, SequenceBindingMover,
  SequenceListingFields, Store, StoreHookFuture, StoreHooks,
};
pub use streaming::{SIDEBAR_CHANNEL, STREAM_COALESCE_INTERVAL, StreamingManager, Subscription, session_channel};
pub use task_tree::{
  AGENT_CREATE_SCOPE_REFUSAL, CreateTask, NodeFacts, PatchTask, PromptScope, StructuralBlockers,
  TASK_CREATE_REQUEST_ID_REQUIRED, TaskCaller, TaskFacts, TaskState, TaskTreeActivity, TaskTreeRow, TaskTreeSnapshot,
  is_read_only_exempt,
};

use std::fmt;

/// Store operation result.
pub type StoreResult<T> = Result<T, StoreError>;

/// A store refusal or failure. The refusal variants name the Python class they answer, and `status_code` gives the
/// HTTP status the routes map it to (`src/runtime/api/sessions.py::_task_http_error`). `Failure` covers I/O,
/// serialization, corrupt-state and hook errors, which carry no status.
#[derive(Debug)]
pub enum StoreError {
  /// An input, serialization, filesystem, or hook error.
  Failure(String),
  /// A missing task, session, or run: `TaskNotFoundError` and `RunNotFoundError` (404).
  NotFound(String),
  /// A malformed request: `TaskInvalidError` (400).
  Invalid(String),
  /// A refused caller identity or role: `TaskForbiddenError` and `DelegationBlockedError` (403).
  Forbidden(String),
  /// Machine input to the archived node with this session id: `TaskArchivedError` (409). Its detail is the sentence
  /// alone, so it carries no blocker list.
  Archived(String),
  /// A lifecycle conflict with its blockers: `TaskConflictError` and `RunIdentityConflictError` (409).
  Conflict(Vec<String>),
}

impl StoreError {
  /// Construct a failure with its public message.
  pub fn new(message: impl Into<String>) -> Self {
    Self::Failure(message.into())
  }

  /// Construct a client-visible not-found error.
  pub fn not_found(detail: impl Into<String>) -> Self {
    Self::NotFound(detail.into())
  }

  /// Construct a client-visible invalid-request error.
  pub fn invalid(detail: impl Into<String>) -> Self {
    Self::Invalid(detail.into())
  }

  /// Construct a client-visible refusal of the caller's identity or role.
  pub fn forbidden(detail: impl Into<String>) -> Self {
    Self::Forbidden(detail.into())
  }

  /// Construct the refusal for machine input to the archived node `session_id`.
  pub fn archived(session_id: impl Into<String>) -> Self {
    Self::Archived(session_id.into())
  }

  /// Construct a conflict with its blockers; its detail joins them with `; `.
  pub fn conflict(blockers: Vec<String>) -> Self {
    Self::Conflict(blockers)
  }

  /// Return the HTTP status associated with this error, when one exists.
  pub fn status_code(&self) -> Option<u16> {
    match self {
      Self::Failure(_) => None,
      Self::NotFound(_) => Some(404),
      Self::Invalid(_) => Some(400),
      Self::Forbidden(_) => Some(403),
      Self::Archived(_) | Self::Conflict(_) => Some(409),
    }
  }

  /// Return a conflict's blockers, the list its 409 body carries.
  pub fn blockers(&self) -> Option<&[String]> {
    match self {
      Self::Conflict(blockers) => Some(blockers),
      _ => None,
    }
  }
}

impl fmt::Display for StoreError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Failure(message) | Self::NotFound(message) | Self::Invalid(message) | Self::Forbidden(message) => {
        f.write_str(message)
      }
      Self::Archived(session_id) => write!(f, "task {session_id} is archived"),
      Self::Conflict(blockers) => f.write_str(&blockers.join("; ")),
    }
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
