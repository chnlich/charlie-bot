//! The text a prompt task's fire gives its bound session: the task's prompt for `mode: master`, otherwise the task's
//! name on the first line and the category template for the task's repo.

use std::path::{Path, PathBuf};

use crate::config::{ScheduledTaskConfig, TaskMode};

/// The template of a prompt task with a repo, relative to the checkout.
const ROUND_TEMPLATE: &str = "prompts/cron/session/round.md";
/// The template of a prompt task without a repo, relative to the checkout.
const REPO_LESS_TEMPLATE: &str = "prompts/cron/session/repo_less.md";

/// A category template that the server cannot read at fire time.
#[derive(Debug)]
pub(crate) struct TemplateUnreadable {
  path: PathBuf,
  error: std::io::Error,
}

impl TemplateUnreadable {
  /// The skip reason that the fire records: `prompt template unreadable: <path>`.
  pub(crate) fn reason(&self) -> String {
    format!("prompt template unreadable: {}", self.path.display())
  }

  /// The error event's text: the reason and the read error.
  pub(crate) fn message(&self) -> String {
    format!("{} ({})", self.reason(), self.error)
  }
}

/// The trigger input text of a fire of `task`, read against `repo_root`. A template that cannot be read is an error.
pub(crate) fn trigger_text(repo_root: &Path, task: &ScheduledTaskConfig) -> Result<String, TemplateUnreadable> {
  if task.mode == Some(TaskMode::Master) {
    return Ok(task.prompt.clone().unwrap_or_default());
  }
  let template = if task.repo.is_some() { ROUND_TEMPLATE } else { REPO_LESS_TEMPLATE };
  let path = repo_root.join(template);
  let body = std::fs::read_to_string(&path).map_err(|error| TemplateUnreadable { path, error })?;
  Ok(format!("task: {}\n{body}", task.name))
}
