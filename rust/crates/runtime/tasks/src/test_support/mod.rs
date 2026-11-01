//! The task-execution stack that this crate's integration tests and `charliebot-improve`'s port tests run against.
//!
//! A test-only crate cannot share files by relative path without hiding the dependency from Cargo, so this module is a
//! library surface behind the `test-fixtures` feature. A production build leaves the feature off and compiles neither
//! module.

pub mod process_guard;
pub mod recovery_support;
