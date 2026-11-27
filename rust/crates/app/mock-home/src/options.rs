//! The options of `generate`: the kind of home that it writes and the language of its prose.

use serde::{Deserialize, Serialize};

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
  /// shape, named by `DATA_VERSION`. It also writes the fixture row that a ux round captures: one sidebar row that
  /// carries the six row indicators, with a long display name and a long backend name (`task_tree::ServeFixture`).
  ServeTaskTree,
  /// Sessions of the default size curve, each with a synthetic task spec sized so that its session row matches a live
  /// view's rows. The default shape writes no task spec, so its bytes do not change.
  View,
  /// The view shape, plus the match sets of the sidebar search view: archived sessions whose names carry each query the
  /// view searches for, so each answer's body matches a live home's answer (`SEARCH_MATCH_SETS` in `generate.rs`).
  Search,
}

impl Shape {
  pub(crate) fn is_default(&self) -> bool {
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
  pub(crate) fn is_mixed(&self) -> bool {
    *self == Language::Mixed
  }
}
