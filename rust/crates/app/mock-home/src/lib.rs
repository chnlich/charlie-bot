//! The synthetic session home of the improve rounds. `generate` writes it, and `check` reads a served copy back against
//! the manifest that `generate` wrote beside it.
//!
//! The home holds one directory per session, with `metadata.json` and `data/chat_events.jsonl`, and one `manifest.json`
//! beside the sessions. The event-log sizes follow the production shape in `session::SIZE_KNOTS`. Every sentence is
//! written for this crate, so no text comes from a real session. One seed gives the same bytes on every run, so the
//! servers under test load the same data.
//!
//! The task-tree shape (`task_tree`) adds the production startup's task tree to the same sessions, and `reset` returns
//! such a home to its generated state after a server start. `recovery_check` holds the startup lab recovery check's
//! sessions and digests.

use serde::{Deserialize, Serialize};

pub mod check;
pub mod generate;
mod manifest;
pub mod recovery_check;
mod rng;
mod session;
pub mod task_tree;
mod text;

/// The data version. It names the mock home's directory, `~/improve/mock-home/<version>/`, and changes with any change
/// to the bytes that `generate` writes.
pub const DATA_VERSION: &str = "2";
/// The session count of the production home, and the default of `generate`.
pub const PRODUCTION_SESSIONS: usize = 7160;
/// The seed of `generate` when no seed is given.
pub const DEFAULT_SEED: u64 = 1;

/// The kind of home that `generate` writes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
  /// Sessions with metadata and an event log: the home of the hourly improve rounds, named by `DATA_VERSION`.
  #[default]
  Default,
  /// Sessions of the default size curve, written as the task nodes of a production startup (`task_tree`).
  TaskTree,
  /// Sessions of the default size curve, each with a synthetic task spec sized so that its session row matches a live
  /// view's rows. The default shape writes no task spec, so its bytes do not change.
  View,
}

impl Shape {
  fn is_default(&self) -> bool {
    *self == Shape::Default
  }
}

/// The language of the seeded prose, titles and table labels that `generate` writes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
  /// Each sentence and topic takes Chinese or English with equal odds: the home of the hourly improve rounds, whose
  /// bytes stay those of the data version.
  #[default]
  Mixed,
  /// Every sentence, title and label is English. The README screenshots use it, because a public README shows one
  /// language.
  English,
}

impl Language {
  fn is_mixed(&self) -> bool {
    *self == Language::Mixed
  }
}
