//! The `devin` backend type: the Devin CLI in print mode, run headless with every tool approved. The CLI prints the
//! final answer on stdout and writes the turn's trajectory to an export file, which the runner reads once the process
//! has exited. Token usage comes from CharlieBot's run logs. Subscription quota reads use the Devin status endpoint.

mod api;
mod devin_cli;
mod export;
mod factory;
mod options;
mod usage_quota;

pub use api::{PACKAGE, USAGE_SOURCE, register, register_with_host};
