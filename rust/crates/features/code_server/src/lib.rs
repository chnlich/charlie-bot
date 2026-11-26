//! `web/features/code_server`: the `code_server:` config section and the `code_server_enabled` template global that
//! shows the diff page's open button. The module also holds the `/api/code-server/open` route that opens a folder in
//! code-server.

// The helpers answer a failure as the HTTP response it sends, as `charliebot_registry::caller` does, so their
// `Err` is a `Response`.
#![allow(clippy::result_large_err)]

mod api;
mod config;
mod entry;
mod which;

pub use entry::register;

#[cfg(test)]
mod tests;
