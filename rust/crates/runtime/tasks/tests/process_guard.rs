//! The guard now lives in the crate's `test_support` module; this file keeps the module name the test targets import.

pub use charliebot_tasks::test_support::process_guard::ProcessGroupGuard;
