//! The JSON field names that an internal API request carries on both sides of the contract.
//!
//! The CLI builds the request and a server route reads it, and both sides depend on this crate, so a field name they
//! agree on sits here. A rename on either side cannot leave the other posting a key the route rejects.

/// The field naming the session a request applies to.
pub const SESSION_ID: &str = "session_id";
