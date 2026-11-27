//! The file server at `/absolute_filepath/{path:path}`.
//!
//! The route answers `GET` and `HEAD` for any path the server process can read. The registered file views are asked
//! first. The path then resolves to its real path, and the answer is a directory listing, a file, or a readable page
//! that refuses a path the reader cannot open.

mod api;
mod document;
mod error_page;
mod human_size;
mod internals;
mod listing;
mod mount;

pub use api::register;
pub use mount::MOUNT;
