//! The text of a temporary provider or gateway failure, readable without a login pool.
//!
//! The pool that selects a member and the availability owner that records its failure both ask whether an error text
//! marks such a failure. The patterns live here, a leaf both read, so neither has to reach into the other.

/// Error substrings of a temporary provider or gateway failure, or of a no-output stall. A pool-chosen run that hits
/// one fails over to the next member. Bare HTTP status digits, which also occur in request ids, and BadRequest or HTTP
/// 400 text stay out: those are request errors that must fail fast.
pub const BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS: &[&str] = &[
  "at capacity",
  "bad gateway",
  "badgateway",
  "internal server error",
  "internalservererror",
  "service unavailable",
  "serviceunavailable",
  "gateway timeout",
  "apiconnectionerror",
  "connection error",
  "peer closed connection",
  "no output for",
  "no session progress for",
];

/// Return whether an error string contains a temporary backend failure substring.
pub fn is_temporary_unavailable_error(text: &str) -> bool {
  let lowered = text.to_lowercase();
  BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}
