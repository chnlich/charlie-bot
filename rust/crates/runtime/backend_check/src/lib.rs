//! The backend check: finds each hardcoded backend choice in the text source files under the paths it is given.
//!
//! `charliebot backend check` runs this scan, and the CI `rust-lint` job runs that command. The match definition is in
//! `rules`, the vocabulary it matches against in `vocabulary`, the reviewed exceptions in `exemptions`. The Rust files
//! are read by `rust_scan` through their syntax trees. Every other text file is read by `text_scan`, which leaves a
//! code file's comments out.

mod api;
mod internals;
mod types;

pub mod exemptions;
pub mod gate;
pub mod rules;
pub mod rust_scan;
pub mod text_scan;
pub mod vocabulary;

pub use api::{PROFILE_SCAN_DIRS, default_paths, scan};
pub use exemptions::{Exemption, RootKind};
pub use rules::Class;
pub use types::{Hit, Roots};
pub use vocabulary::Vocabulary;
