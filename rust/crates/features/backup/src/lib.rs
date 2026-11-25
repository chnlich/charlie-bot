//! The scheduled profile backup to the configured restic repository and its cron handler.

pub mod backup;
pub mod config;
mod entry;

pub use entry::register;

#[cfg(test)]
mod lib_tests;
