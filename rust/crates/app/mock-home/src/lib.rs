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

use serde::{Deserialize, Serialize};

pub mod check;
mod cron;
pub mod generate;
mod manifest;
pub mod recovery_check;
mod rng;
mod session;
pub mod task_tree;
mod text;

/// The data version. It names the mock home's directory, `~/improve/mock-home/<version>/`, and changes with any change
/// to the bytes that `generate` writes. Version 7 is the task-tree shape, which the improve rounds generate: its boot
/// marker lets a start skip every session that holds no open task, run or worktree.
pub const DATA_VERSION: &str = "7";
/// The session count of the production home, and the default of `generate`: the live store's session directories when
/// it was measured (14,451 at 18:04Z on 2026-11-19).
pub const PRODUCTION_SESSIONS: usize = 14_451;
/// The seed of `generate` when no seed is given.
pub const DEFAULT_SEED: u64 = 1;

/// The kind of home that `generate` writes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
  /// Sessions with metadata and an event log, and no task-tree node. The newest rows, `NEWEST_WEB_STATUS_ROWS` of them,
  /// carry a `web_status` goal. Four of its manager sessions, in the middle of the listing, carry the cron fixture's
  /// tasks. A start of this home logs a node failure for each session, since no boot marker skips them.
  #[default]
  Default,
  /// Sessions of the default size curve, written as the task nodes of a production startup (`task_tree`), with the
  /// boot marker and the cron fixture. Each open task's last run is live, and `live-runs.txt` lists it, so a release lab
  /// reattaches a process to it.
  TaskTree,
  /// The task-tree shape with no live run. A serve trial starts no process to reattach, so a live run would drain at its
  /// start and log a node failure. Each run keeps the end its record states. The improve rounds' shared home is this
  /// shape, named by `DATA_VERSION`.
  ServeTaskTree,
  /// Sessions of the default size curve, each with a synthetic task spec sized so that its session row matches a live
  /// view's rows. The default shape writes no task spec, so its bytes do not change.
  View,
  /// The view shape, plus the match sets of the sidebar search view: archived sessions whose names carry each query the
  /// view searches for, so each answer's body matches a live home's answer (`SEARCH_MATCH_SETS` in `generate.rs`).
  Search,
}

impl Shape {
  fn is_default(&self) -> bool {
    *self == Shape::Default
  }

  /// Whether the shape writes the task tree (`task_tree`), with or without its live runs.
  pub fn is_task_tree(self) -> bool {
    matches!(self, Shape::TaskTree | Shape::ServeTaskTree)
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
