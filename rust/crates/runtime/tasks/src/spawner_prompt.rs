//! Worker and verify prompt support: the marker-section template reader, token substitution, and the per-task-type
//! workflow section selection.
//!
//! A template splits on its `<!-- section: <id> -->` marker lines. Every call reads the file again, so an edit takes
//! effect on the next launch. A missing file or section fails with the path and the likely cause; there is no
//! embedded-text fallback.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{anyhow, bail};
use charliebot_core::json_loads::codec_error_text;
use charliebot_core::text_repr::str_repr;
use charliebot_store::metadata::{TaskSpec, TaskType};

use crate::verify_trailer;

const SECTION_MARKER_PREFIX: &str = "<!-- section: ";
const SECTION_MARKER_SUFFIX: &str = " -->";

/// The worker contract under the checkout's `prompts/`: the file every worker-template reader loads.
pub const WORKER_PROMPT_PATH: &str = "prompts/worker.md";

/// The extraction name a failed worker-template read reports: it names the commit a stale checkout predates. The
/// prompt-body reads of `WORKER_PROMPT_PATH` pass it, so their failure text names one extraction.
pub const WORKER_PROMPT_EXTRACTION: &str = "worker-prompt";

/// The `{{name}}` tokens of one template, each replaced in order (`str.replace`, never `str.format`).
pub type Tokens = Vec<(&'static str, String)>;

/// The section that holds the repo-less source-files rule (`REPO_LESS_SOURCE_FILES_SECTION`).
pub const REPO_LESS_SOURCE_FILES_SECTION: &str = "task_spec_source_files_repo_less";

/// A failed template or source read. A read raises either an `OSError` or a `UnicodeDecodeError`; the class name and
/// the text this type reports match that error.
#[derive(Debug)]
pub enum TextReadError {
  Io(io::Error),
  Decode(String),
}

impl TextReadError {
  /// The name of the exception class this failure raises.
  pub fn class_name(&self) -> &'static str {
    match self {
      Self::Io(error) => match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
      },
      Self::Decode(_) => "UnicodeDecodeError",
    }
  }

  /// The text of the exception for a read of `path`.
  pub fn error_text(&self, path: &Path) -> String {
    match self {
      Self::Io(error) => match error.raw_os_error() {
        Some(errno) => {
          // Rust's Display appends " (os error N)" to the strerror text, so the suffix is stripped here.
          let rendered = error.to_string();
          let strerror = rendered.strip_suffix(&format!(" (os error {errno})")).unwrap_or(&rendered);
          format!("[Errno {errno}] {strerror}: {}", str_repr(&path.display().to_string()))
        }
        None => error.to_string(),
      },
      Self::Decode(message) => message.clone(),
    }
  }
}

/// Reads a UTF-8 text file the way `Path.read_text(encoding="utf-8")` does: universal newlines turn `\r\n` and a lone
/// `\r` into `\n`.
pub fn read_text(path: &Path) -> Result<String, TextReadError> {
  let bytes = fs::read(path).map_err(TextReadError::Io)?;
  let text = String::from_utf8(bytes)
    .map_err(|error| TextReadError::Decode(utf8_decode_error_text(error.as_bytes(), &error.utf8_error())))?;
  Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// The text of a `UnicodeDecodeError` for the bad bytes of a UTF-8 decode.
fn utf8_decode_error_text(bytes: &[u8], error: &std::str::Utf8Error) -> String {
  let start = error.valid_up_to();
  let (end, reason) = match error.error_len() {
    Some(_) => {
      let lead = bytes[start];
      let expected_len = match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return codec_error_text("utf-8", bytes, start, start + 1, "invalid start byte"),
      };
      let invalid_offset = (1..expected_len)
        .find(|offset| bytes.get(start + offset).is_none_or(|byte| !valid_continuation(lead, *offset, *byte)));
      let Some(offset) = invalid_offset else {
        unreachable!("a complete valid UTF-8 sequence cannot produce a decode error")
      };
      (start + offset, "invalid continuation byte")
    }
    None => (bytes.len(), "unexpected end of data"),
  };
  codec_error_text("utf-8", bytes, start, end, reason)
}

/// Whether this byte is a permitted continuation at this position in the UTF-8 sequence.
fn valid_continuation(lead: u8, offset: usize, byte: u8) -> bool {
  match (lead, offset) {
    (0xE0, 1) => (0xA0..=0xBF).contains(&byte),
    (0xED, 1) => (0x80..=0x9F).contains(&byte),
    (0xF0, 1) => (0x90..=0xBF).contains(&byte),
    (0xF4, 1) => (0x80..=0x8F).contains(&byte),
    _ => (0x80..=0xBF).contains(&byte),
  }
}

/// The section that states the contract of a task whose record carries no type (`WORKFLOW_NO_REVIEWER_SECTION`).
pub const WORKFLOW_NO_REVIEWER_SECTION: &str = "workflow_no_reviewer";

/// The section that holds the iteration report contract the improve loop renders (`ITERATION_REPORTS_SECTION`).
pub const ITERATION_REPORTS_SECTION: &str = "iteration_reports";

// The marker-section ids of `prompts/task_base.md` and `prompts/worker.md`. `load_worker_prompt_sections` merges the
// two templates into one section namespace, and `required_worker_sections` lists what that merge must hold, so a
// reader in `prompts` names a section here.

/// The section that states the coding principles a managed contract starts from (`CODING_PRINCIPLES_SECTION`).
pub const CODING_PRINCIPLES_SECTION: &str = "coding_principles";

/// The section that states when to check for skills and where they live (`SKILLS_DISCOVERY_SECTION`).
pub const SKILLS_DISCOVERY_SECTION: &str = "skills_discovery";

/// The section that names the profile home (`PROFILE_HOME_SECTION`).
pub const PROFILE_HOME_SECTION: &str = "profile_home";

/// The section that states where a remote host's scratch files go (`REMOTE_SCRATCH_SECTION`).
pub const REMOTE_SCRATCH_SECTION: &str = "remote_scratch";

/// The section that points a writer at the model-text rules (`TEXT_MODEL_READS_SECTION`).
pub const TEXT_MODEL_READS_SECTION: &str = "text_model_reads";

/// The section that names the session (`SESSION_INFO_SECTION`).
pub const SESSION_INFO_SECTION: &str = "session_info";

/// The section that states a worker's role (`ROLE_SECTION`).
pub const ROLE_SECTION: &str = "role";

/// The section that states a dedicated worktree is already created (`INTRO_NEW_SECTION`).
pub const INTRO_NEW_SECTION: &str = "intro_new";

/// The section that states the run continues work in an existing worktree (`INTRO_CONTINUATION_SECTION`).
pub const INTRO_CONTINUATION_SECTION: &str = "intro_continuation";

/// The section that holds the task description (`TASK_SECTION`).
pub const TASK_SECTION: &str = "task";

/// The section that states the worktree persists after the reviewer merges (`WORKTREE_PERSISTENCE_SECTION`).
pub const WORKTREE_PERSISTENCE_SECTION: &str = "worktree_persistence";

/// The section that states the user-decision search and attribution rule (`USER_DECISIONS_SECTION`).
pub const USER_DECISIONS_SECTION: &str = "user_decisions";

/// The persistent workflow sections of a task type, bindings section first (`WORKFLOW_PROMPT_SECTION`). Verify and
/// cold-read tasks have no workflow section.
pub fn workflow_prompt_sections(task_type: TaskType) -> Option<&'static [&'static str]> {
  match task_type {
    TaskType::Implement => Some(&["worktree_bindings", "workflow_steps", "workflow_implement"]),
    TaskType::QuickEdit => Some(&["worktree_bindings", "workflow_steps", "workflow_quick_edit"]),
    TaskType::ScriptRun => Some(&["workflow_script_run_bindings", "workflow_script_run"]),
    TaskType::Verify | TaskType::ColdRead => None,
  }
}

/// Whether a reviewer run follows a successful work run (`reviewer_run_follows`). A node without a task record is an
/// implement node, and a record whose task type is implement gets a reviewer; every other task takes the
/// non-implement branch. The harness assembly reads the same decision, so a
/// harness cannot promise a reviewer that the after-run branch will not spawn.
pub fn reviewer_run_follows(task: Option<&TaskSpec>) -> bool {
  task.is_none_or(|task| task.task_type == Some(TaskType::Implement))
}

/// The persistent workflow sections of one task, bindings section first. A task record whose type is unset renders
/// the implement workflow; when no reviewer run follows it, the implement STOP line gives way to the type-less
/// contract (`WORKFLOW_NO_REVIEWER_SECTION`).
pub fn task_workflow_sections(task: Option<&TaskSpec>) -> Option<&'static [&'static str]> {
  let task_type = task.and_then(|task| task.task_type).unwrap_or(TaskType::Implement);
  if task_type == TaskType::Implement && !reviewer_run_follows(task) {
    return Some(&["worktree_bindings", "workflow_steps", WORKFLOW_NO_REVIEWER_SECTION]);
  }
  workflow_prompt_sections(task_type)
}

/// The repo-less workflow sections of a task type (`REPO_LESS_WORKFLOW_SECTION`).
pub fn repo_less_workflow_sections(task_type: TaskType) -> Option<&'static [&'static str]> {
  match task_type {
    TaskType::Implement | TaskType::QuickEdit => Some(&["workflow_repo_less"]),
    TaskType::ScriptRun => Some(&["workflow_script_run"]),
    TaskType::Verify | TaskType::ColdRead => None,
  }
}

/// The persistent workflow rule sections of one work Run (`workflow_rule_section_ids`). The repo case drops the
/// bindings section, which renders per run; a repo-less task with no repo-less workflow falls back to the repo case.
pub fn workflow_rule_section_ids(task: Option<&TaskSpec>, repo_less: bool) -> anyhow::Result<Vec<&'static str>> {
  let task_type = task.and_then(|task| task.task_type).unwrap_or(TaskType::Implement);
  if repo_less && let Some(ids) = repo_less_workflow_sections(task_type) {
    let mut sections = ids.to_vec();
    sections.push(REPO_LESS_SOURCE_FILES_SECTION);
    return Ok(sections);
  }
  let ids = task_workflow_sections(task).ok_or_else(|| anyhow!("no workflow sections for task type {task_type:?}"))?;
  let mut sections = ids[1..].to_vec();
  sections.push("task_spec_source_files");
  Ok(sections)
}

/// Every section the worker prompt requires, in the order listed here.
fn required_worker_sections() -> Vec<&'static str> {
  let workflow = [TaskType::Implement, TaskType::QuickEdit, TaskType::ScriptRun]
    .into_iter()
    .filter_map(workflow_prompt_sections)
    .flatten()
    .copied();
  let repo_less = [TaskType::Implement, TaskType::QuickEdit, TaskType::ScriptRun]
    .into_iter()
    .filter_map(repo_less_workflow_sections)
    .flatten()
    .copied();
  let sections = [
    SESSION_INFO_SECTION,
    CODING_PRINCIPLES_SECTION,
    SKILLS_DISCOVERY_SECTION,
    REMOTE_SCRATCH_SECTION,
    ROLE_SECTION,
    INTRO_NEW_SECTION,
    INTRO_CONTINUATION_SECTION,
  ]
  .into_iter()
  .chain(workflow)
  .chain(repo_less)
  .chain([
    REPO_LESS_SOURCE_FILES_SECTION,
    "task_spec_source_files",
    WORKFLOW_NO_REVIEWER_SECTION,
    TASK_SECTION,
    ITERATION_REPORTS_SECTION,
    WORKTREE_PERSISTENCE_SECTION,
    USER_DECISIONS_SECTION,
    "memory",
  ]);
  let mut seen = HashSet::new();
  sections.filter(|section| seen.insert(*section)).collect()
}

/// Splits a template into its marker sections and checks each `required` id is present
/// (`load_marker_sections`). `extraction` names the commit the message blames for a stale checkout.
pub fn load_marker_sections(
  path: &Path,
  required: &[&str],
  extraction: &str,
) -> anyhow::Result<HashMap<String, String>> {
  if !path.is_file() {
    bail!(
      "prompt template not found at {} — the repo checkout most likely predates the {extraction} extraction commit",
      path.display()
    );
  }
  let text = read_text(path).map_err(|error| anyhow!(error.error_text(path)))?;
  let sections = split_sections(&text);
  let missing: Vec<&str> = required.iter().copied().filter(|id| !sections.contains_key(*id)).collect();
  if !missing.is_empty() {
    bail!(
      "{} is missing required section(s): {} — the repo checkout most likely predates the {extraction} extraction commit",
      path.display(),
      missing.join(", ")
    );
  }
  Ok(sections)
}

/// The sections of a template text: each marker line opens a section that runs to the next marker. A later section
/// with the same id replaces an earlier one.
fn split_sections(text: &str) -> HashMap<String, String> {
  let mut sections = HashMap::new();
  let mut current: Option<String> = None;
  let mut lines: Vec<&str> = Vec::new();
  for line in text.split('\n') {
    if line.starts_with(SECTION_MARKER_PREFIX) && line.ends_with(SECTION_MARKER_SUFFIX) {
      if let Some(id) = current.take() {
        sections.insert(id, lines.join("\n"));
      }
      current = Some(marker_id(line).to_string());
      lines.clear();
      continue;
    }
    if current.is_some() {
      lines.push(line);
    }
  }
  if let Some(id) = current {
    sections.insert(id, lines.join("\n"));
  }
  sections
}

/// The id a marker line names. A line too short to hold both markers names the empty id.
fn marker_id(line: &str) -> &str {
  let end = line.len().saturating_sub(SECTION_MARKER_SUFFIX.len()).max(SECTION_MARKER_PREFIX.len());
  &line[SECTION_MARKER_PREFIX.len()..end]
}

/// The worker prompt's sections: `prompts/task_base.md` read first, then `prompts/worker.md`. A section id that both
/// files define is a template error, never a silent override (`load_worker_prompt_sections`).
pub fn load_worker_prompt_sections(repo: &Path) -> anyhow::Result<HashMap<String, String>> {
  let worker = load_marker_sections(&repo.join(WORKER_PROMPT_PATH), &[], WORKER_PROMPT_EXTRACTION)?;
  let base = load_marker_sections(&repo.join("prompts/task_base.md"), &[], "task-base-prompt")?;
  let mut duplicate: Vec<&String> = base.keys().filter(|id| worker.contains_key(*id)).collect();
  if !duplicate.is_empty() {
    duplicate.sort();
    let names: Vec<&str> = duplicate.iter().map(|id| id.as_str()).collect();
    bail!("prompts/task_base.md and prompts/worker.md both define section(s): {}", names.join(", "));
  }
  let merged: HashMap<String, String> = base.into_iter().chain(worker).collect();
  let missing: Vec<&str> = required_worker_sections().into_iter().filter(|id| !merged.contains_key(*id)).collect();
  if !missing.is_empty() {
    bail!("the worker prompt sections are missing required section(s): {}", missing.join(", "));
  }
  Ok(merged)
}

/// Replaces each token in order (`substitute_tokens`).
pub fn substitute_tokens(template: &str, tokens: &Tokens) -> String {
  tokens.iter().fold(template.to_string(), |text, (token, value)| text.replace(token, value))
}

/// The tokens of `verify.md`: the expected result trailer and the canonical plan template's path (`verify_contract_tokens`).
pub fn verify_contract_tokens(repo: &Path) -> anyhow::Result<Tokens> {
  let template = resolve_path(&repo.join("prompts/plan_template.html"))?;
  Ok(vec![
    ("{{result_trailer_expected}}", verify_trailer::verify_result_trailer_expected()),
    ("{{canonical_template_path}}", template.display().to_string()),
  ])
}

/// Canonicalize existing path components, while retaining a missing leaf as an absolute path.
fn resolve_path(path: &Path) -> io::Result<std::path::PathBuf> {
  match fs::canonicalize(path) {
    Ok(path) => Ok(path),
    Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
      let Some(name) = path.file_name() else {
        return Err(error);
      };
      let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
      Ok(resolve_path(parent)?.join(name))
    }
    Err(error) => Err(error),
  }
}

/// The tokens of the bindings section: the intro line plus the branch, worktree and repo (`worktree_binding_tokens`).
pub fn worktree_binding_tokens(
  intro_line: &str,
  branch_name: &str,
  base_branch_origin: &str,
  wt_path: &str,
  repo_path: &str,
) -> Tokens {
  vec![
    ("{{intro_line}}", intro_line.to_string()),
    ("{{branch_name}}", branch_name.to_string()),
    ("{{base_branch_origin}}", base_branch_origin.to_string()),
    ("{{wt_path}}", wt_path.to_string()),
    ("{{repo_path}}", repo_path.to_string()),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn missing_worker_prompt_names_the_path_and_the_stale_checkout() {
    let repo = tempfile::tempdir().expect("temp repo");
    let error = load_worker_prompt_sections(repo.path()).expect_err("a checkout without prompts/worker.md fails");
    let message = error.to_string();
    assert!(message.contains("predates the worker-prompt extraction commit"), "{message}");
    assert!(message.contains(&repo.path().join("prompts/worker.md").display().to_string()), "{message}");
  }

  #[test]
  #[ignore = "integration"]
  fn missing_plan_template_resolves_to_its_absolute_path() {
    let repo = tempfile::tempdir().expect("temp repo");
    fs::create_dir(repo.path().join("prompts")).expect("prompts dir");
    let tokens = verify_contract_tokens(repo.path()).expect("a missing template still has a resolved path");
    let actual = tokens
      .iter()
      .find(|(name, _)| *name == "{{canonical_template_path}}")
      .expect("canonical template path token")
      .1
      .as_str();
    assert_eq!(actual, repo.path().join("prompts/plan_template.html").display().to_string());
    assert!(Path::new(actual).is_absolute());
  }

  #[test]
  fn utf8_errors_match_ranges_for_invalid_sequences() {
    let cases: &[(&[u8], &str)] = &[
      (&[0xE2, 0x28, 0xA1], "'utf-8' codec can't decode byte 0xe2 in position 0: invalid continuation byte"),
      (&[0xE2, 0x82, 0x28], "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte"),
      (&[0xED, 0xA0, 0x80], "'utf-8' codec can't decode byte 0xed in position 0: invalid continuation byte"),
      (&[0xF4, 0x90, 0x80, 0x80], "'utf-8' codec can't decode byte 0xf4 in position 0: invalid continuation byte"),
      (&[0xF0, 0x90, 0x80], "'utf-8' codec can't decode bytes in position 0-2: unexpected end of data"),
    ];
    for (bytes, expected) in cases {
      let error = std::str::from_utf8(bytes).expect_err("the test bytes are not valid UTF-8");
      assert_eq!(utf8_decode_error_text(bytes, &error), *expected);
    }
  }
}
