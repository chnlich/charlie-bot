//! The file server at `/absolute_filepath/{*path}`.
//!
//! The route answers `GET` and `HEAD` for every path under the mount. The registered file views are asked first. The
//! path then resolves to its real path. A path that the server process can read gets its answer, a directory listing or
//! a file. A path it cannot read, or an entry that is not a regular file, gets the 403 page.

mod api;
mod document;
mod error_page;
mod human_size;
mod internals;
mod listing;
mod mount;

pub use api::register;
pub use mount::MOUNT;
