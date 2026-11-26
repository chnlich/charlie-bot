//! Claude Code against a configured Anthropic-compatible endpoint.

mod entry;
pub mod factory;
pub mod options;
mod usage;

pub use entry::{PACKAGE, register};
pub use options::TYPE_NAME;
