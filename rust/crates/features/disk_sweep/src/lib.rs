//! The task-worktree and scratch-directory cleanup feature.

mod archive;
mod config;
mod entry;
mod git;
mod linked;
mod process;
mod references;
pub mod sweep;

pub use entry::register;
