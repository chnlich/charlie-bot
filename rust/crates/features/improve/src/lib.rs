//! The improve loop: `charliebot improve start` runs an iterative worker loop on the task tree as one sequence.

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
