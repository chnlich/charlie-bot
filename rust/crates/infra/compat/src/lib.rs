//! The Python-compatibility pieces that every crate of the Rust port shares: the JSON bodies, the
//! FastAPI validation errors, the Jinja2-compatible template environment, the static-file response,
//! the outbound HTTP client. Each piece reproduces one Python module's bytes; a crate that answers
//! the same routes takes them from here rather than writing its own.

pub mod compression;
pub mod fastapi_body;
pub mod fastapi_form;
pub mod file_urls;
pub mod headers;
pub mod http_client;
pub mod jinja;
pub mod json;
pub mod mimetypes;
mod mimetypes_table;
pub mod python_json;
pub mod responses;
pub mod routing;
pub mod static_files;
mod transport;
mod urlquote;
pub mod validation;
pub mod ws_client;
