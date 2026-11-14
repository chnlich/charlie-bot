//! The shared pieces of every `charliebot` subcommand: argument parsing, session resolution, the
//! internal-API client, and the JSON error shapes.
//!
//! The client contract distinguishes the two failure classes the restart-crossing rule turns on. A
//! connection that never got established is retried with bounded backoff, because the request
//! provably was not sent. A sent request whose response was lost is never re-issued. When the call
//! has a readback, that readback answers instead. Every failure prints one JSON object on stderr
//! with `code` and `effect` fields and exits 1, unless the command maps the status elsewhere.

pub mod args;
pub mod base_url;
pub mod channel_verb;
pub mod client;
pub mod draft;
pub mod exit;
pub mod fields;
pub mod files;
pub mod headers;
pub mod http;
pub mod json_out;
pub mod request;
pub mod resolve;
pub mod response;
pub mod route;
pub mod skew;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod timeouts;
pub mod urlencode;
pub mod wire;
