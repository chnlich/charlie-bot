//! The `antigravity` backend type: `agy --print`, whose JSON envelope the runner reads once
//! the process has exited. Token usage comes from CharlieBot's own run logs; quota-panel readings come from the CLI's
//! `/usage` command.

mod antigravity_cli;
mod api;
mod envelope;
pub mod factory;
mod options;
mod usage_quota;

pub use api::{PACKAGE, USAGE_SOURCE, register, register_with_host};

/// The backend type this package registers: the `type` a `backends.options` entry names.
pub use options::TYPE_NAME;
