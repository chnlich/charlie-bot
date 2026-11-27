//! The labeled-entry memory store's server half. It reads and scaffolds the store, selects the
//! memory each run's instructions take, and exposes the store root to the diff view. The `memory` CLI lives in
//! this crate beside it.

pub mod cli;
mod entry;
pub mod memory;
mod store_root;
mod turn_contribution;

pub use entry::register;

#[cfg(test)]
mod tests;
