//! The synthetic session home of the improve rounds. `generate` writes it, and `check` reads a served copy back against
//! the manifest that `generate` wrote beside it.
//!
//! The home holds one directory per session, with `metadata.json` and `data/chat_events.jsonl`, and one `manifest.json`
//! beside the sessions. The event-log sizes follow the production shape in `generate::SIZE_KNOTS`. Every sentence is
//! written for this crate, so no text comes from a real session. One seed gives the same bytes on every run, so the
//! servers under test load the same data.

pub mod check;
pub mod generate;
mod manifest;
mod rng;
mod text;

/// The data version. It names the mock home's directory, `~/improve/mock-home/<version>/`, and changes with any change
/// to the bytes that `generate` writes. The digest test in `tests/digest.rs` fails until it changes.
pub const DATA_VERSION: &str = "2";
/// The session count of the production home, and the default of `generate`.
pub const PRODUCTION_SESSIONS: usize = 7160;
/// The seed of `generate` when no seed is given.
pub const DEFAULT_SEED: u64 = 1;
