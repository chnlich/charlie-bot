//! Landing the work branch after a loop, and the iteration's commit delta
//! (`src/features/improve/improve_command.py:303-330`, `:370-423`, `:423-432`).

use std::path::Path;

use charliebot_tasks::git;
use serde_json::{Value, json};

/// The tip after an iteration and the commits it added over `tip_before` (`_worktree_commit_delta`). A failed count
/// leaves the commit count at zero, as the Python helper does.
pub async fn commit_delta(wt_path: &Path, tip_before: &str) -> (String, i64) {
  let tip_after = git::git_rev_parse(wt_path, "HEAD").await.unwrap_or_default();
  let mut commits_added = 0;
  if !tip_before.is_empty() && !tip_after.is_empty() {
    let range = format!("{tip_before}..{tip_after}");
    if let Some(count) = git::git_rev_list_count(wt_path, &range).await {
      commits_added = count;
    }
  }
  (tip_after, commits_added)
}

/// The fixed sentence a blocked loop's report opens with.
pub fn blocked_loop_summary(iteration: i64, reason: &str) -> String {
  format!(
    "Improve loop blocked on iteration {iteration}: {reason}. \
     No further iterations were spawned; decide whether to wait, switch backend, or relaunch."
  )
}

/// The landing decision of a finished loop and the end error its report carries.
pub struct Landing {
  /// The merge result the payload carries, `None` when no landing was decided (a best-effort branch push only).
  pub merge_result: Option<Value>,
  /// The text the final report carries after the sentence, `None` when every push succeeded.
  pub end_error: Option<String>,
}

/// Decide how to land the work branch once the iteration loop ends. A merge-back with summaries and no stop
/// fast-forwards `work_branch` onto `base_branch`; when that push fails, the bare branch is pushed for the parent to
/// land by hand. Otherwise the branch is pushed best-effort.
pub async fn land_work_branch_after_loop(
  resolved_repo: &Path,
  work_branch: &str,
  base_branch: &str,
  merge_back: bool,
  stopped_by_user: bool,
  summary_count: usize,
  session_id: &str,
) -> Landing {
  if merge_back && !stopped_by_user && summary_count > 0 {
    let (ok, push_err) = git::git_push_refspec(resolved_repo, work_branch, base_branch).await;
    if ok {
      return Landing { merge_result: Some(json!({"merged": true, "base_branch": base_branch})), end_error: None };
    }
    tracing::warn!(
      target: "charliebot_improve",
      session = session_id,
      error = push_err.as_str(),
      "improve_loop_landing_ff_push_failed"
    );
    let (ok_push, push_branch_err) = git::git_push_branch(resolved_repo, work_branch).await;
    let end_error = if ok_push {
      format!(
        "The fast-forward landing onto {base_branch} failed ({push_err}); \
         the work branch was pushed to origin."
      )
    } else {
      tracing::warn!(
        target: "charliebot_improve",
        session = session_id,
        error = push_branch_err.as_str(),
        "improve_loop_work_branch_push_failed"
      );
      format!(
        "The fast-forward landing onto {base_branch} failed ({push_err}); \
         pushing the work branch to origin also failed ({push_branch_err})."
      )
    };
    return Landing {
      merge_result: Some(json!({
        "merged": false,
        "error": push_err,
        "work_branch": work_branch,
        "base_branch": base_branch,
      })),
      end_error: Some(end_error),
    };
  }
  let (ok, push_err) = git::git_push_branch(resolved_repo, work_branch).await;
  if !ok {
    tracing::warn!(target: "charliebot_improve", session = session_id, error = push_err.as_str(), "improve_loop_push_failed");
    return Landing {
      merge_result: None,
      end_error: Some(format!("Pushing the work branch to origin failed ({push_err}).")),
    };
  }
  Landing { merge_result: None, end_error: None }
}
