//! LESSONS retrieval: the entries of `$CHARLIEBOT_HOME/LESSONS.md`, ranked by how many distinct cause-line tokens each
//! one matches.
//!
//! One entry is one `## ` heading and the body under it, up to the next heading. The search runs through `rg`, and the
//! ranking maps each matching line back to the entry that holds it. The top three entries become the judge's candidate
//! list, each carried in full, so a cause an entry states deep in its body reaches the judge prompt. The limit of three
//! keeps the judge input inside one short call.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// The file name of the host's lessons.
pub const LESSONS_FILE_NAME: &str = "LESSONS.md";
/// The number of candidate entries the judge reads.
pub const CANDIDATE_LIMIT: usize = 3;
/// Tokens that name a normalized placeholder rather than a phrase, so they match no entry.
const PLACEHOLDER_TOKENS: [&str; 6] = ["uuid", "path", "session", "redacted", "hex", "ts"];

/// One LESSONS entry offered to the judge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LessonCandidate {
  /// The heading text after `## `.
  pub heading: String,
  /// The entry's whole body, up to the next heading.
  pub body: String,
  /// The number of distinct cause-line tokens the entry matched.
  pub score: usize,
}

/// The entries of one LESSONS.md, with the line each one starts at.
#[derive(Clone, Debug)]
pub struct LessonIndex {
  file: std::path::PathBuf,
  entries: Vec<Entry>,
}

#[derive(Clone, Debug)]
struct Entry {
  /// The line number of the `## ` heading, from 1.
  start_line: usize,
  heading: String,
  /// The entry's body, up to the next heading.
  body: String,
}

impl LessonIndex {
  /// The entries of `path`. A missing or unreadable file is an error: a scan cannot rank candidates without it.
  pub fn load(path: &Path) -> Result<LessonIndex> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    let mut heading: Option<(usize, String)> = None;
    let mut body = String::new();
    for (index, line) in text.lines().enumerate() {
      let line_number = index + 1;
      if let Some(rest) = line.strip_prefix("## ") {
        if let Some((start_line, heading)) = heading.take() {
          entries.push(Entry { start_line, heading, body: std::mem::take(&mut body) });
        }
        heading = Some((line_number, rest.trim().to_string()));
        body.clear();
      } else if heading.is_some() {
        body.push_str(line);
        body.push('\n');
      }
    }
    if let Some((start_line, heading)) = heading.take() {
      entries.push(Entry { start_line, heading, body });
    }
    Ok(LessonIndex { file: path.to_path_buf(), entries })
  }

  /// The heading of the entry whose text holds `signature` verbatim. A signature archived this way needs no judge
  /// call.
  pub fn archive_heading(&self, signature: &str) -> Option<String> {
    if signature.is_empty() {
      return None;
    }
    self
      .entries
      .iter()
      .find(|entry| entry.heading.contains(signature) || entry.body.contains(signature))
      .map(|entry| entry.heading.clone())
  }

  /// The top three entries that match distinct tokens of `cause_line`.
  pub fn retrieve(&self, cause_line: &str) -> Result<Vec<LessonCandidate>> {
    let tokens = tokens(cause_line);
    if tokens.is_empty() || self.entries.is_empty() {
      return Ok(Vec::new());
    }
    let matches = crate::ripgrep::matching_lines(&self.file, &tokens)?;
    let mut scores: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for matched in matches {
      let Some(index) = self.entry_index(matched.line_number) else { continue };
      let lowered = matched.text.to_lowercase();
      for token in &tokens {
        if lowered.contains(&token.to_lowercase()) {
          scores.entry(index).or_default().insert(token.clone());
        }
      }
    }
    let mut ranked: Vec<(usize, usize)> = scores.into_iter().map(|(index, matched)| (index, matched.len())).collect();
    ranked.retain(|(_, score)| *score > 0);
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    ranked.truncate(CANDIDATE_LIMIT);
    Ok(
      ranked
        .into_iter()
        .map(|(index, score)| {
          let entry = &self.entries[index];
          LessonCandidate { heading: entry.heading.clone(), body: entry.body.clone(), score }
        })
        .collect(),
    )
  }

  fn entry_index(&self, line_number: usize) -> Option<usize> {
    self.entries.iter().rposition(|entry| entry.start_line <= line_number)
  }
}

/// The distinct searchable tokens of a cause line.
fn tokens(cause_line: &str) -> Vec<String> {
  let mut seen = BTreeSet::new();
  for token in cause_line
    .to_lowercase()
    .split(|character: char| !(character.is_alphanumeric() || matches!(character, '-' | '_' | '.')))
  {
    let placeholder = PLACEHOLDER_TOKENS.contains(&token);
    if token.len() >= 2 && !placeholder {
      seen.insert(token.to_string());
    }
  }
  seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIXTURE: &str = "\
# LESSONS

## 2026-11-01 — synthetic scope guard

The improve runner reports `the previous round still runs: unit improve-ux.scope exists` when a round
is still active. Treat it as a skip, not a failure.

## 2026-10-30 — an unrelated entry about worktrees

A worktree that no longer exists fails the runner at once.

## 2026-10-29 — another unrelated entry

A cron task file names no session id, so the scheduler refuses it.

## 2026-10-28 — backend quota

The gateway returned a quota error.

## 2026-10-27 — a fourth unrelated entry

Nothing here matches the scope guard.
";

  fn index() -> (tempfile::TempDir, LessonIndex) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(LESSONS_FILE_NAME);
    std::fs::write(&path, FIXTURE).unwrap();
    (root, LessonIndex::load(&path).unwrap())
  }

  #[test]
  fn the_scope_guard_entry_ranks_in_the_top_three_for_the_exit_five_text() {
    let (_root, index) = index();
    let candidates = index.retrieve("the previous round still runs: unit improve-ux.scope exists").unwrap();
    assert_eq!(candidates[0].heading, "2026-11-01 — synthetic scope guard");
    assert_eq!(candidates[0].score, 8);
    assert_eq!(candidates.len(), 3);
  }

  #[test]
  fn tokens_drop_placeholders_and_short_fragments() {
    assert_eq!(tokens("run <uuid> failed at <path> <n>"), ["at", "failed", "run"]);
    assert_eq!(tokens("improve-ux.scope exists"), ["exists", "improve-ux.scope"]);
  }

  #[test]
  fn an_entry_with_no_matching_token_is_not_a_candidate() {
    let (_root, index) = index();
    let candidates = index.retrieve("zzzz-unmatched-token").unwrap();
    assert!(candidates.is_empty());
  }

  #[test]
  fn an_entry_that_quotes_a_signature_verbatim_archives_it() {
    let signature = "run:work:5:the previous round still runs: unit improve-ux.scope exists";
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(LESSONS_FILE_NAME);
    std::fs::write(&path, format!("## 2026-11-02 — archived\n\nChao declined the fix for `{signature}`.\n")).unwrap();
    let index = LessonIndex::load(&path).unwrap();
    assert_eq!(index.archive_heading(signature).as_deref(), Some("2026-11-02 — archived"));
    assert_eq!(index.archive_heading("run:work:5:the previous round still runs: unit improve-ux.scopa exists"), None);
  }

  #[test]
  fn an_entry_whose_cause_text_sits_past_the_first_400_characters_reaches_the_judge_prompt_in_full() {
    let filler = "The summary runner archives one report per round and lists every skipped scope with the reason it \
                  recorded, so a later reader can trace why a round did not run its usual work and which round \
                  produced the text. The report keeps the round's start time, the scopes it considered, and the \
                  outcome of each one, and a reader who wants the numbers opens the round's own summary file. The \
                  archive keeps one report per round even when two rounds share a scope, so the counts a reader sees \
                  are per round, not per scope.";
    let cause_text =
      "the launcher assumed every run on its backend is a round worker, so the cron task file lookup ran too late";
    let intro = "The failure came from how the launcher started rounds: ";
    let entry = format!("## 2026-10-29 — synthetic cron launcher\n\n{filler}\n\n{intro}{cause_text}.\n");
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(LESSONS_FILE_NAME);
    std::fs::write(&path, format!("# LESSONS\n\n{entry}\n")).unwrap();
    let index = LessonIndex::load(&path).unwrap();

    let candidates = index.retrieve("no cron task file under <path> names session_id").unwrap();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0].heading, "2026-10-29 — synthetic cron launcher");
    let offset = candidates[0].body.find(cause_text).expect("the cause text is in the body");
    assert!(offset > 400, "the cause text starts at {offset}, inside the old 400-character cap");

    let prompt = crate::judge::user_prompt(&crate::judge::JudgeInput {
      signature: "run:manager_turn:6:no cron task file under <path> names session_id".to_string(),
      kind: "manager_turn".to_string(),
      error_text: "no cron task file under <path> names session_id".to_string(),
      candidates,
    });
    assert!(prompt.contains(cause_text), "{prompt}");
  }
}
