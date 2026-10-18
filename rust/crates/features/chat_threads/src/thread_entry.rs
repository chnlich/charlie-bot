//! The shared thread's reply contract (`src/features/chat_threads/thread_entry.py`): the prompt-doc loader, the contract
//! join and its rule segment. Each platform's turn contribution states its platform part and reuses these. The rest of
//! `thread_entry.py` (summons, replies, follow wakes) is not ported yet.

use std::fs;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_registry::turn::{PromptSource, RuleSegment};

/// The scope of a rule segment's sources (`task_prompts.SCOPE_BASE`).
const SCOPE_BASE: &str = "base";
/// The delivery of a full text segment (`task_prompts.DELIVERY_FULL`).
const DELIVERY_FULL: &str = "full";
/// The red-line doc every platform's contract carries.
const REDLINE_DOC: &str = "thread_reply_redline.md";
/// The reply-format doc every platform's contract carries.
const REPLY_FORMAT_DOC: &str = "thread_reply_format.md";

/// The fields of `thread_entry.ThreadPlatform` that the reply contract reads.
pub struct ThreadPlatform {
  /// The platform's scope doc under `prompts/`: what personal information may enter the platform's threads.
  pub scope_doc: &'static str,
}

/// Reads one `prompts/` doc fresh from disk, with universal newlines and Python's `str.strip()`
/// (`load_prompt_doc`). A missing or unreadable doc fails with its name, its path and the likely cause.
pub fn load_prompt_doc(repo: &RepoRoot, name: &str, likely_cause: &str) -> anyhow::Result<String> {
  let path = repo.path().join("prompts").join(name);
  let bytes =
    fs::read(&path).with_context(|| format!("{name} prompt not found at {} — {likely_cause}", path.display()))?;
  let text = String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
  Ok(py_strip(&text.replace("\r\n", "\n").replace('\r', "\n")).to_string())
}

/// The thread reply contract: `platform_part`, a blank line, then the platform's scope doc, the red line and the
/// reply format, one per line (`thread_reply_contract`). Each doc is read on every call.
pub fn thread_reply_contract(
  repo: &RepoRoot,
  platform: &ThreadPlatform,
  platform_part: &str,
) -> anyhow::Result<String> {
  let scope_doc = load_prompt_doc(
    repo,
    platform.scope_doc,
    &format!("the repo checkout most likely predates the {} addition commit", platform.scope_doc),
  )?;
  let red_line = load_prompt_doc(
    repo,
    REDLINE_DOC,
    "the repo checkout most likely predates the thread-reply-redline rename commit",
  )?;
  let reply_format = load_prompt_doc(
    repo,
    REPLY_FORMAT_DOC,
    "the repo checkout most likely predates the thread-reply-format rename commit",
  )?;
  Ok(format!("{platform_part}\n\n{scope_doc}\n{red_line}\n{reply_format}"))
}

/// The contract text as one full rule segment. Its sources name the three docs the text joins, so the prompt-source
/// audit reaches each file (`thread_reply_segment`).
pub fn thread_reply_segment(platform: &ThreadPlatform, text: String) -> RuleSegment {
  let sources = [platform.scope_doc, REDLINE_DOC, REPLY_FORMAT_DOC]
    .into_iter()
    .map(|doc| PromptSource {
      scope: SCOPE_BASE.to_string(),
      source_ref: format!("prompts/{doc}"),
      source_session_id: None,
    })
    .collect();
  RuleSegment { text, sources, delivery: DELIVERY_FULL.to_string() }
}

/// Python's `str.strip()`: trims what `char::is_whitespace` trims plus the information separators U+001C to U+001F,
/// which Python counts as whitespace and Rust does not.
pub fn py_strip(text: &str) -> &str {
  text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use super::*;

  const SCOPE: &str = "slack_reply_scope.md";

  /// A synthetic checkout whose `prompts/` holds the given docs.
  fn checkout(docs: &[(&str, &str)]) -> (tempfile::TempDir, RepoRoot) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("prompts")).unwrap();
    for (name, text) in docs {
      fs::write(dir.path().join("prompts").join(name), text).unwrap();
    }
    let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
    (dir, repo)
  }

  fn all_docs(scope_text: &str) -> Vec<(&str, &str)> {
    vec![(SCOPE, scope_text), (REDLINE_DOC, "red line\n"), (REPLY_FORMAT_DOC, "\treply format\r\n")]
  }

  #[test]
  fn a_doc_is_read_fresh_stripped_and_with_universal_newlines() {
    let (dir, repo) = checkout(&[(SCOPE, "\u{1c}first\r\nsecond\rthird\u{3000}\n")]);
    assert_eq!(load_prompt_doc(&repo, SCOPE, "cause").unwrap(), "first\nsecond\nthird");

    fs::write(dir.path().join("prompts").join(SCOPE), "edited\n").unwrap();
    assert_eq!(load_prompt_doc(&repo, SCOPE, "cause").unwrap(), "edited");
  }

  #[test]
  fn a_missing_doc_names_its_path_and_the_likely_cause() {
    let (dir, repo) = checkout(&[]);
    let error = load_prompt_doc(&repo, SCOPE, "the cause").unwrap_err();
    assert_eq!(
      error.to_string(),
      format!("{SCOPE} prompt not found at {}/prompts/{SCOPE} — the cause", dir.path().display())
    );
  }

  #[test]
  fn the_contract_joins_the_platform_part_and_the_three_docs() {
    let (_dir, repo) = checkout(&all_docs("scope text"));
    let platform = ThreadPlatform { scope_doc: SCOPE };
    let text = thread_reply_contract(&repo, &platform, "platform part").unwrap();
    assert_eq!(text, "platform part\n\nscope text\nred line\nreply format");
  }

  #[test]
  fn a_missing_contract_doc_fails_with_its_own_likely_cause() {
    let (_dir, repo) = checkout(&[(SCOPE, "scope"), (REDLINE_DOC, "red")]);
    let platform = ThreadPlatform { scope_doc: SCOPE };
    let error = thread_reply_contract(&repo, &platform, "part").unwrap_err().to_string();
    assert!(error.starts_with("thread_reply_format.md prompt not found at "), "{error}");
    assert!(
      error.ends_with("— the repo checkout most likely predates the thread-reply-format rename commit"),
      "{error}"
    );
  }

  #[test]
  fn the_segment_names_the_three_docs_as_base_full_sources() {
    let platform = ThreadPlatform { scope_doc: SCOPE };
    let segment = thread_reply_segment(&platform, "text".to_string());
    assert_eq!(segment.text, "text");
    assert_eq!(segment.delivery, "full");
    let refs: Vec<&str> = segment.sources.iter().map(|source| source.source_ref.as_str()).collect();
    assert_eq!(
      refs,
      ["prompts/slack_reply_scope.md", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
    );
    assert!(segment.sources.iter().all(|source| source.scope == "base" && source.source_session_id.is_none()));
  }
}
