//! Review prompt text: the reviewer's managed rules, the numbered git steps, and the context lines.
//! The event-log reads and reviewer selection stay with the review chain.
//!
//! The stable judgment rules are one constant each, so the managed rules and the numbered steps quote the same text.
//! The volatile steps (the worktree, the branch and the base) render per run, and the launch path keeps them out of
//! the instruction hash.

use std::path::Path;

use anyhow::bail;
use charliebot_store::metadata::TaskSpec;

const ROLE_TEXT: &str = "## Code Review\n\
You are reviewing another worker's code changes.\n\
- The repository guide is the repository's `CLAUDE.md` or `AGENTS.md` file and its `.claude/rules/` files.\n\
- The repository guide sets the rules for the repository's code, environment and tests.\n\
- These instructions set the rules for the task lifecycle, git operations and delegation.\n\
- Follow `~/.claude/skills/writing-style/genres/commits-and-prs.md` for commit messages and gated PR title/body.\n\
- When the repository guide conflicts with these instructions, follow these instructions.";

const CHECKLIST_INTRO: &str = "IMPORTANT: Make minimal changes. Prefer approving the worker's code as-is. \
Only fix clear bugs, correctness issues, or scope violations. \
Do not refactor, restyle, or improve code that is functionally correct.\n\n\
The Context section carries the task spec. When the spec has a `## Source Files` section, read every path \
it lists before judging the diff. Apply the spec's `## Reviewer Checklist`. For control-flow or \
state-machine tasks, verify the implementation against `## Required Behavior`; do not rely only \
on tests.\n\n\
The Later Messages section holds each message that the worker received\n\
after the task spec, in claim order.\n\
When a later message changes the task spec, judge the work against the later\n\
message.\n\
When the task spec and a skill disagree, follow the task spec.";

const SCOPE_CHECK: &str = "**Scope check**: Flag any changes NOT requested in the task — extra flags, altered defaults,\n   \
new parameters, behavioral changes. Workers must only do what was asked.";

const DIVERGENT_CHECK: &str = "**Think divergently**: Beyond the diff, consider what could go wrong.\n   \
- Do changed values make sense? Cross-check against existing defaults and conventions.\n   \
- Are there edge cases, regressions, or interactions with other code the worker missed?\n   \
- Would this change surprise someone reading the code for the first time?";

const CORRECTNESS_CHECK: &str = "Check for: correctness, bugs, unintended side effects, missing edge cases.";

const STYLE_CHECK: &str = "Style: Google Style, 2-space indent, 120-col (only flag if egregious — YAPF handles most).";

const COMMIT_STEP: &str = "10. Before the rebase, confirm every change you keep is committed and restore tool-generated files with `git restore <path>`; untracked files stay in place.";

const LAND_STEP: &str = "11. `charliebot land` takes a gated PR's title and body from the branch's single commit. A branch of several commits lands with an explicit `--title` and `--body` or `--body-file`, both written by `~/.claude/skills/writing-style/genres/commits-and-prs.md`. Act on its JSON state: `landed` and `landed-no-ci` complete the delivery; `waiting` ends the run, and the trigger the command registered wakes it; on `ci-failed`, fix only the failures this diff causes and run the command again; report every other state as blocked and leave the base branch untouched. A run that a trigger wake resumes runs this command before anything else.";

/// The reviewer contract's stable rules: the managed instruction block of a review Run (`review_rules_text`).
pub fn review_rules_text() -> String {
  let checklist = format!("## Review Checklist\n{CHECKLIST_INTRO}\n\n");
  let stable_rules = [SCOPE_CHECK, DIVERGENT_CHECK, CORRECTNESS_CHECK, STYLE_CHECK]
    .iter()
    .map(|body| format!("- {body}"))
    .collect::<Vec<_>>()
    .join("\n");
  format!("{ROLE_TEXT}\n\n{checklist}{stable_rules}")
}

/// The review prompt's numbered git steps for one run's branch, worktree and base (`review_numbered_steps`).
pub fn review_numbered_steps(branch_name: &str, wt_path: &str, base_branch: &str) -> String {
  let published = review_published_branch(base_branch);
  let landing = review_landing_target(base_branch);
  [
    format!("1. `cd {wt_path}`"),
    format!("2. Fetch the latest base branch: `git fetch origin {published}`"),
    format!("3. Review the changes: `git diff {landing}...{branch_name}`"),
    "4. Verify the changes address the user's actual intent (from context research above).".to_string(),
    format!("5. {SCOPE_CHECK}"),
    format!("6. {DIVERGENT_CHECK}"),
    format!("7. {CORRECTNESS_CHECK}"),
    format!("8. {STYLE_CHECK}"),
    "9. If you find issues, fix them and commit each kept change with a message that follows the commits-and-PRs genre.".to_string(),
    COMMIT_STEP.to_string(),
    LAND_STEP.to_string(),
  ]
  .join("\n")
}

/// The branch name on origin that a run's recorded base names: `origin/<b>` names `<b>` (`review_published_branch`).
pub fn review_published_branch(base_branch: &str) -> &str {
  base_branch.strip_prefix("origin/").unwrap_or(base_branch)
}

/// The origin ref that step 13's push lands on, which the landing check fetches before judging (`review_landing_target`).
pub fn review_landing_target(base_branch: &str) -> String {
  format!("origin/{}", review_published_branch(base_branch))
}

/// The spec a review judges the work against: the task record's goal. A task with an empty goal has nothing to judge
/// against, so it fails.
pub fn review_spec_text(task: &TaskSpec) -> anyhow::Result<String> {
  if task.goal.trim().is_empty() {
    bail!("review has no task spec: the task record's goal is empty");
  }
  Ok(task.goal.clone())
}

/// The review prompt's context lines: the spec, the later messages when the worker got any, the worker summary when
/// one was extracted, and the delegator's hint (`review_context_lines`).
pub fn review_context_lines(
  spec_text: &str,
  later_messages: &str,
  worker_summary: Option<&str>,
  delegator_hint: &str,
) -> Vec<String> {
  let mut lines = vec![format!("**Task spec:**\n{spec_text}")];
  if !later_messages.is_empty() {
    lines.push(later_messages.to_string());
  }
  if let Some(summary) = worker_summary.filter(|summary| !summary.is_empty()) {
    lines.push(format!("**Worker summary:** {summary}"));
  }
  lines.push(format!("**Delegator hint:** {delegator_hint}"));
  lines
}

/// The context footer that sends the reviewer to the run's full logs (`review_log_pointer`).
pub fn review_log_pointer(chat_log_path: &Path, worker_log_path: &Path) -> String {
  format!(
    "If the summary above is insufficient or you are unsure about intent, \
read the full logs: Session: `{}`, Worker: `{}`",
    chat_log_path.display(),
    worker_log_path.display()
  )
}

/// The sentence that pins the numbered git steps to this run's worktree (`review_git_venue`).
pub fn review_git_venue(branch_name: &str, wt_path: &str) -> String {
  format!(
    "The work is on branch `{branch_name}` in worktree `{wt_path}`. \
All git operations below run from the worktree."
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn numbered_steps_are_eleven_and_steps_ten_and_eleven_are_constants() {
    let rendered = review_numbered_steps("task-1", "/wt", "origin/main");
    let steps: Vec<&str> = rendered
      .lines()
      .filter(|line| line.split_once(". ").is_some_and(|(number, _)| number.parse::<usize>().is_ok()))
      .collect();
    assert_eq!(steps.len(), 11);
    assert!(steps.iter().enumerate().all(|(index, step)| step.starts_with(&format!("{}.", index + 1))));
    assert_eq!(steps[9], COMMIT_STEP);
    assert_eq!(steps[10], LAND_STEP);
  }

  #[test]
  fn rendered_review_prompt_never_stashes() {
    let rendered = review_numbered_steps("task-1", "/wt", "origin/main") + &review_rules_text();
    assert!(!rendered.to_lowercase().contains("stash"));
  }
}
