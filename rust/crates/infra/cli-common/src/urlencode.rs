//! Query-parameter encoding: `key=value` pairs, `&`-joined, percent-quoted.
//!
//! The encoder itself lives in `charliebot_compat::urlquote`, the owner the CLI's calls and the server's routes
//! share. This module is the path the CLI's command modules reach it by.

pub use charliebot_compat::urlquote::encode_pairs as urlencode;
