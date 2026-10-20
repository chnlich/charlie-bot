//! The cold-read run's shared contract: the prompt texts, the probe input, and the report body
//! (`src/runtime/cold_read.py`).
//!
//! `prompts/cold_read.md` is the only home of the cold-reader instructions and the seven questions. The artifact check
//! and the cold-read child session both build from here, so the foreground output and the completion report carry the
//! same lines.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::spawner_prompt;

/// The cold-reader template under `prompts/`.
pub const PROMPT_FILENAME: &str = "cold_read.md";
const SYSTEM_SECTION: &str = "system_prompt";
const QUESTIONS_SECTION: &str = "questions";
/// The placeholder in the questions that the trigger message replaces.
pub const TRIGGER_PLACEHOLDER: &str = "<trigger message verbatim>";

/// The run-directory file one cold-read run writes at launch: the page path, the page's sha256, and the error its
/// failure judgment recorded. The completion report reads each attempt back from it.
pub const EVIDENCE_FILENAME: &str = "cold_read.json";

/// The outcome the report states: the model that answered, or the trailer for every model failing.
pub enum ColdReadOutcome<'a> {
  Answered { backend_id: &'a str, answer: &'a str },
  AllFailed,
}

/// The SHA-256 digest of the page bytes a cold read read, as lowercase hexadecimal (`hashlib.sha256(...).hexdigest()`).
pub fn page_sha256(page_bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(page_bytes))
}

/// The system prompt and the seven questions, read fresh from `prompts/cold_read.md`. Trailing newlines stay out of
/// both, so the probe input is byte-identical to the file's section text.
pub fn load_texts(repo: &Path) -> anyhow::Result<(String, String)> {
  let sections = spawner_prompt::load_marker_sections(
    &repo.join("prompts").join(PROMPT_FILENAME),
    &[SYSTEM_SECTION, QUESTIONS_SECTION],
    "cold-read",
  )?;
  Ok((
    sections[SYSTEM_SECTION].trim_end_matches('\n').to_string(),
    sections[QUESTIONS_SECTION].trim_end_matches('\n').to_string(),
  ))
}

/// The cold-read run's whole input: the page text, one blank line, then the questions with the trigger filled in.
pub fn probe_input(page_text: &str, questions: &str, trigger: &str) -> String {
  format!("{page_text}\n\n{}", questions.replace(TRIGGER_PLACEHOLDER, trigger))
}

/// The cold-read report body: the page and its sha256, one `attempt` line per failed run in order, then the answering
/// backend and its verbatim answer, or the all-failed trailer (`format_report`).
pub fn format_report(
  page_path: &str,
  page_sha256: &str,
  attempts: &[(String, String)],
  outcome: ColdReadOutcome,
) -> String {
  let mut lines = vec![format!("cold read of {page_path}"), format!("page sha256 {page_sha256}")];
  for (backend, error) in attempts {
    lines.push(format!("attempt {backend} failed: {error}"));
  }
  match outcome {
    ColdReadOutcome::AllFailed => lines.push(format!("every model failed ({} tried)", attempts.len())),
    ColdReadOutcome::Answered { backend_id, answer } => {
      lines.push(format!("backend {backend_id}"));
      lines.push(answer.to_string());
    }
  }
  lines.join("\n")
}
