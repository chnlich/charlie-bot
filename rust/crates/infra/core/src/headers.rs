//! HTTP header names that more than one crate spells.
//!
//! A header name compares case-insensitively, but every sender writes the same spelling, so the spelling lives here
//! once.

/// The `Authorization` header name. A request names its bearer credential in this header.
pub const AUTHORIZATION: &str = "Authorization";
