//! The `memory` CLI verbs.

mod add;
mod digest;
mod entry;
mod error;
mod lint;
mod proposal;
mod proposal_check;
mod query;
mod store;
#[cfg(test)]
mod tests;

pub use entry::main;
