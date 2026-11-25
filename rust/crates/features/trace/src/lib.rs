//! The Perfetto trace viewer and merged-trace cache.

mod api;
mod entry;
mod merge;

pub use entry::register;
