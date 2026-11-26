//! The writable paths of one improve round's sandbox allow-list.
//!
//! `direction_paths` returns the paths that one direction may write and the paths that every direction shares.
//! `tmp_directories` names the two `/tmp` directories that the round creates for one direction.
//! `TEST_SCRATCH` names the fixed `/tmp` directories that a test writes under the boundary. Each entry names the one
//! test that needs it; no other test gets an allowance.

mod paths;

pub use paths::{direction_paths, tmp_directories};
