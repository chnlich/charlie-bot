//! The `antigravity` backend type: `agy acp`, which runs each turn over the Agent Client Protocol. The turn's output is
//! translated from the raw log and its live messages are sent over the same protocol. Quota-panel readings come from the
//! CLI's `/usage` command.

mod antigravity_cli;
mod api;
mod envelope;
pub mod factory;
mod lifecycle;
mod options;
mod usage_quota;

pub use api::{PACKAGE, USAGE_SOURCE, register, register_with_host};

/// The backend type this package registers: the `type` a `backends.options` entry names.
pub use options::TYPE_NAME;
