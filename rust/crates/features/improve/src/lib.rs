//! The improve loop: `charliebot improve start` runs an iterative worker loop on the task tree as one sequence.

/// The improve loop's log target, which every `tracing` call of this crate names.
pub(crate) const LOG_TARGET: &str = "charliebot_improve";

pub mod api;
pub mod cli;
pub mod command;
pub mod controller;
mod entry;
pub mod judgment;
pub mod landing;
pub mod sequence;
pub(crate) mod wire;

pub use entry::register;
