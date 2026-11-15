//! The worktree-removal hook: one finished run's worktree, removed by the runtime that owns worktree lifecycle.
//!
//! The disk sweep decides which worktree to remove, and the task runtime performs the removal, with the safety
//! checks, the local-artifact pre-clean and the residue handling that removal needs. The sweep reads this hook instead
//! of importing the task crate, so the feature carries no compile-time edge to the runtime.

use std::path::Path;

use futures_util::future::BoxFuture;
use serde_json::Map;

/// One worktree removal request.
pub struct WorktreeRemoval<'a> {
  /// The repository the worktree was created from.
  pub repo_path: &'a str,
  /// The worktree's directory.
  pub worktree_path: &'a str,
  /// The branch the worktree checks out; the residue name derives from it.
  pub branch_name: &'a str,
  /// The run that owns the worktree, logged as the removal's `thread_id`.
  pub owner_id: &'a str,
  /// The directory every worktree sits under; the removal refuses a path outside it.
  pub worktree_parent: &'a Path,
  /// The caller's own log fields, carried into the removal's log lines.
  pub log_fields: &'a Map<String, serde_json::Value>,
  /// The caller's name for the removal, in its failure message.
  pub label: &'a str,
  /// The event the removal logs when the worktree cannot be removed.
  pub fail_event: &'a str,
  /// The event the removal logs when git reports the removal as failed.
  pub remove_failed_event: &'a str,
}

/// The removal of one worktree, reporting a failure as the caller's message.
pub type WorktreeRemovalFn = for<'a> fn(WorktreeRemoval<'a>) -> BoxFuture<'a, Option<String>>;
