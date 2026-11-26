//! `web/features/github`: the `charliebot github` command group, its draft-target validation, and the sender that
//! opens a draft pull request or posts a comment with the operator's own token. The token command comes from the
//! core config's `github:` section, which this crate does not own.

pub mod cli;
mod draft;
mod entry;
mod target;

pub use entry::register;
