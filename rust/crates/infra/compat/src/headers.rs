//! Request header values, decoded as latin-1.

use http::{HeaderMap, HeaderName};

/// The first value of header `name`, decoded as latin-1, or `""`.
pub fn header_text(headers: &HeaderMap, name: &HeaderName) -> String {
  headers.get(name).map(|value| value.as_bytes().iter().map(|b| *b as char).collect()).unwrap_or_default()
}
