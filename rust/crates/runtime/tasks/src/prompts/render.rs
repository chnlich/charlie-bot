//! The task and input context a launch renders from the same maintained template sections as the managed rules
//! (`src/runtime/task_prompts.py:477-575`). Its text is the launch's own evidence and never enters the instruction hash.

use std::path::Path;

use anyhow::anyhow;
use charliebot_core::RepoRoot;
use charliebot_store::RunRecord;
use charliebot_store::metadata::TaskType;

use crate::review_text;
use crate::spawner_prompt::{self, Tokens};

/// The session-info line with the session's name filled in.
pub fn render_session_info(checkout: &RepoRoot, session_name: &str) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_marker_sections(
    &checkout.path().join(spawner_prompt::WORKER_PROMPT_PATH),
    &["session_info"],
    "worker-prompt",
  )?;
  Ok(sections["session_info"].replace("{{session_name}}", session_name))
}

/// The bindings intro line: a retry that continues the worktree says so (`binding_intro`).
pub fn binding_intro(checkout: &RepoRoot, run: &RunRecord) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_worker_prompt_sections(checkout.path())?;
  let key =
    if run.retry_of_run_id.is_some() && run.worktree_path.is_some() { "intro_continuation" } else { "intro_new" };
  Ok(sections[key].trim().to_string())
}

/// The workflow's binding header with the intro line, branch, worktree and repo filled in (`render_worktree_bindings`).
pub fn render_worktree_bindings(
  checkout: &RepoRoot,
  task_type: TaskType,
  intro_line: &str,
  branch_name: &str,
  base_branch_origin: &str,
  wt_path: &str,
  repo_path: &str,
) -> anyhow::Result<String> {
  // Element 0 of the task type's section list is its bindings section.
  let section = spawner_prompt::workflow_prompt_sections(task_type)
    .ok_or_else(|| anyhow!("no workflow sections for task type {task_type:?}"))?[0];
  let sections = spawner_prompt::load_marker_sections(
    &checkout.path().join(spawner_prompt::WORKER_PROMPT_PATH),
    &[section],
    "worker-prompt",
  )?;
  let tokens: Tokens =
    spawner_prompt::worktree_binding_tokens(intro_line, branch_name, base_branch_origin, wt_path, repo_path);
  Ok(spawner_prompt::substitute_tokens(&sections[section], &tokens))
}

/// The task section with the description filled in (`render_task_body`).
pub fn render_task_body(checkout: &RepoRoot, description: &str) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_marker_sections(
    &checkout.path().join(spawner_prompt::WORKER_PROMPT_PATH),
    &["task"],
    "worker-prompt",
  )?;
  let tokens: Tokens = vec![("{{description}}", description.to_string())];
  Ok(spawner_prompt::substitute_tokens(&sections["task"], &tokens))
}

/// The worktree persistence section, as the template states it (`render_worktree_persistence`).
pub fn render_worktree_persistence(checkout: &RepoRoot) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_marker_sections(
    &checkout.path().join(spawner_prompt::WORKER_PROMPT_PATH),
    &["worktree_persistence"],
    "worker-prompt",
  )?;
  Ok(sections["worktree_persistence"].clone())
}

/// The review run's context: the context section, the log pointer, and the run's git steps (`review_task_context`).
pub fn review_task_context(
  branch_name: &str,
  wt_path: &str,
  base_branch: &str,
  chat_log_path: &Path,
  worker_log_path: &Path,
  context_section: &str,
) -> String {
  [
    format!("## Context\n{context_section}"),
    review_text::review_log_pointer(chat_log_path, worker_log_path),
    review_text::review_git_venue(branch_name, wt_path),
    review_text::review_numbered_steps(branch_name, wt_path, base_branch),
  ]
  .join("\n\n")
}

/// The repo-less review run's context: the spec, the later messages when the work claimed input after it, the work
/// report's paths, and the note that there is no git to read (`repo_less_review_task_context`).
pub fn repo_less_review_task_context(
  chat_log_path: &Path,
  worker_log_path: &Path,
  spec_text: &str,
  later_messages: &str,
  work_report: Option<&str>,
) -> String {
  let report = work_report
    .filter(|report| !report.trim().is_empty())
    .unwrap_or("(the work Run left no final report; judge the paths the task names)");
  let later = if later_messages.is_empty() { String::new() } else { format!("\n\n{later_messages}") };
  format!(
    "## Context\n\
This task changed host paths directly; it has no repository, so there is no diff to read and\n\
nothing to merge or push. Review the CURRENT state of the paths the work report lists.\n\n\
### Task spec (acceptance tests included)\n{spec_text}{later}\n\n\
### Work report (every created, modified and deleted path, and each acceptance test's result)\n{report}\n\n\
{}",
    review_text::review_log_pointer(chat_log_path, worker_log_path)
  )
}
