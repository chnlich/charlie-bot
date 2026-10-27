//! The feature-owned subcommands `charliebot artifact`, `charliebot plan` and `charliebot publish`.
//! Deleting this package removes each subcommand with one dispatcher line.

pub mod artifact;
pub mod plan;
pub mod publish;
mod wrap;
