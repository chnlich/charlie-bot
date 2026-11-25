//! The store's error type: the refusal and failure classes that the routes map to HTTP statuses, and the result alias
//! that each store operation returns.

use std::fmt;
use std::path::Path;

/// Store operation result.
pub type StoreResult<T> = Result<T, StoreError>;

/// A store refusal or failure. The refusal variants name the failure class they answer, and `status_code` gives the
/// HTTP status the routes map it to. `Failure` covers I/O, serialization, corrupt-state and hook errors, which carry
/// no status.
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

  /// Construct a conflict with its blockers. Its detail joins them with `; `.
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

/// The store error for a failure at `path`: the path, a colon, and the failure's display.
pub fn path_error(path: &Path, error: impl fmt::Display) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
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
