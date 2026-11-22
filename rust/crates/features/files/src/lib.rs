//! The file server at `/absolute_filepath/{path:path}`.
//!
//! The route answers `GET` and `HEAD` for any path the server process can read. The registered file views are asked
//! first. The path then resolves to its real path, and the answer is a directory listing, a file,
//! or the error that the route raises for that path.

mod api;
mod human_size;
mod internals;
mod listing;

pub use api::{MOUNT, register};
