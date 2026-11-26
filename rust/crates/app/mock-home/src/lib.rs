//! The synthetic session home of the improve rounds. `generate` writes it, and `check` reads a served copy back against
//! the manifest that `generate` wrote beside it.
//!
//! The home holds one directory per session, with `metadata.json` and `data/chat_events.jsonl`, one `manifest.json`
//! beside the sessions, and the scheduled-task files of `.cron.d` (`cron`). The event-log sizes follow the production
//! shape in `session::SIZE_KNOTS`. Every sentence is
//! written for this crate, so no text comes from a real session. One seed gives the same bytes on every run, so the
//! servers under test load the same data.
//!
//! The task-tree shape (`task_tree`) adds the production startup's task tree to the same sessions, and `reset` returns
//! such a home to its generated state after a server start. `recovery_check` holds the startup lab recovery check's
//! sessions and digests.

pub mod check;
mod constants;
mod cron;
pub mod generate;
mod manifest;
mod options;
pub mod recovery_check;
mod rng;
mod session;
pub mod task_tree;
mod text;

pub use constants::{DATA_VERSION, DEFAULT_SEED, PRODUCTION_SESSIONS, SEARCH_WORDS, SearchWords};
pub use options::{Language, Shape};
