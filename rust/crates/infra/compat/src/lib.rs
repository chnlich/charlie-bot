//! The request and response pieces every crate shares: the JSON bodies, the validation errors, the
//! Jinja2-compatible template environment, the static-file response, and the outbound HTTP client. A
//! crate that answers the same routes takes them from here rather than writing its own.

pub mod compression;
pub mod file_urls;
pub mod form_body;
pub mod headers;
pub mod http_client;
pub mod jinja;
pub mod json;
pub mod json_loads;
pub mod mimetypes;
mod mimetypes_table;
pub mod request_body;
pub mod responses;
pub mod routing;
pub mod static_files;
mod transport;
mod urlquote;
pub mod validation;
pub mod ws_client;
