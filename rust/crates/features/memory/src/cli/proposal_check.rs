//! The commit gate's writing checks (`memory_proposal.py`: `_prose_violations`, `_negation_warnings`). The prose check
//! refuses a commit; the negation check only returns the warnings the commit prints and records.

use std::collections::HashSet;
use std::sync::LazyLock;

use charliebot_core::str_methods;
use regex::Regex;

use super::store::{BODY_LINE_COLUMN_LIMIT, display_width, entry_body};

const SENTENCE_WORD_LIMIT: usize = 25;
const BODY_LINE_LIMIT: usize = 12;
const NEGATION_ADVICE: &str = "state the standing reality or the action to take, and keep the fact it carried";

/// Negation and contrast words, whole words, case-insensitive.
static NEGATION: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(concat!(
    r"(?i)\b(not|no|never|none|nothing|nobody|nowhere|neither|nor|without|instead of|rather than|cannot|lacks?)\b",
    r"|n't\b",
  ))
  .expect("the negation pattern compiles")
});

/// The writing-rule violations of `text` over the same file's base version. A unit the base version holds verbatim
/// escapes the sentence checks; the column limit binds every body line. `skip_body_line_limit` waives the line limit
/// for a pure deletion.
pub(super) fn prose_violations(text: &str, base_text: Option<&str>, skip_body_line_limit: bool) -> Vec<String> {
  let body = entry_body(text);
  let base_units = base_units(base_text);
  let mut violations = Vec::new();
  for unit in prose_units(&body) {
    if base_units.contains(&unit) {
      continue;
    }
    for sentence in sentences(&unit) {
      let (masked, _) = mask_backticks(&sentence);
      let words = str_methods::split_whitespace(&masked).len();
      if words > SENTENCE_WORD_LIMIT {
        violations.push(format!("sentence over {SENTENCE_WORD_LIMIT} words ({words}): {sentence}"));
      }
      if masked.contains(';') {
        violations.push(format!("semicolon joins clauses: {sentence}"));
      }
    }
  }
  for line in &body {
    let width = display_width(line);
    if width > BODY_LINE_COLUMN_LIMIT {
      let preview: String = line.chars().take(60).collect();
      violations.push(format!("body line over {BODY_LINE_COLUMN_LIMIT} columns ({width}): {preview}"));
    }
  }
  if !skip_body_line_limit && body.len() > BODY_LINE_LIMIT {
    violations.push(format!("body has {} lines (limit {BODY_LINE_LIMIT})", body.len()));
  }
  violations
}

/// One warning payload per added or rewritten sentence, and per changed title, that carries a negation or contrast
/// word. The exemptions mirror [`prose_violations`]: a unit the base version holds verbatim escapes, and so does a
/// title equal to the base version's.
pub(super) fn negation_warnings(text: &str, base_text: Option<&str>) -> Vec<String> {
  let base_units = base_units(base_text);
  let mut warnings = Vec::new();
  for unit in prose_units(&entry_body(text)) {
    if base_units.contains(&unit) {
      continue;
    }
    for sentence in sentences(&unit) {
      warnings.extend(negation_payload(&sentence));
    }
  }
  let title = title_line(text);
  if let Some(title) = title.filter(|title| Some(*title) != base_text.and_then(title_line)) {
    let title_text = title.strip_prefix("title:").expect("a title line starts with 'title:'");
    warnings.extend(negation_payload(str_methods::strip(title_text)));
  }
  warnings
}

fn base_units(base_text: Option<&str>) -> HashSet<String> {
  base_text.map(|text| prose_units(&entry_body(text)).into_iter().collect()).unwrap_or_default()
}

/// The text each unit of the body reads: the lead, then each bullet. A bullet is its `- ` (or nested `  - `) line plus
/// the continuation lines after it, and a unit's lines join with one space.
fn prose_units(body: &[&str]) -> Vec<String> {
  let mut units: Vec<String> = Vec::new();
  for line in body {
    if let Some(text) = line.strip_prefix("- ") {
      units.push(text.to_string());
    } else if let Some(text) = line.strip_prefix("  - ") {
      units.push(text.to_string());
    } else if let Some(unit) = units.last_mut() {
      unit.push(' ');
      unit.push_str(str_methods::strip(line));
    } else {
      units.push(str_methods::strip(line).to_string());
    }
  }
  units
}

/// The text with each backticked span replaced by `CODE`, and for each masked character the index of the character it
/// reads from. A backticked span reads from its opening backtick; a backtick with no closing one stays literal.
fn mask_backticks(text: &str) -> (String, Vec<usize>) {
  let chars: Vec<char> = text.chars().collect();
  let mut masked = String::new();
  let mut mapping = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    if chars[i] == '`'
      && let Some(offset) = chars[i + 1..].iter().position(|&character| character == '`')
    {
      masked.push_str("CODE");
      mapping.extend([i; 4]);
      i = i + 1 + offset + 1;
    } else {
      masked.push(chars[i]);
      mapping.push(i);
      i += 1;
    }
  }
  (masked, mapping)
}

/// The unit's sentences: split after a period followed by whitespace, with backticked spans masked first so their
/// punctuation never splits. The unit's end also ends a sentence.
fn sentences(unit: &str) -> Vec<String> {
  let chars: Vec<char> = unit.chars().collect();
  let (masked_text, mapping) = mask_backticks(unit);
  let masked: Vec<char> = masked_text.chars().collect();
  let mut out = Vec::new();
  let mut start = 0;
  let mut i = 0;
  while i < masked.len() {
    if masked[i] == '.' && i + 1 < masked.len() && str_methods::is_python_whitespace(masked[i + 1]) {
      out.push(chars[mapping[start]..=mapping[i]].iter().collect());
      i += 1;
      while i < masked.len() && str_methods::is_python_whitespace(masked[i]) {
        i += 1;
      }
      start = i;
    } else {
      i += 1;
    }
  }
  if start < masked.len() && masked[start..].iter().any(|&character| !str_methods::is_python_whitespace(character)) {
    out.push(chars[mapping[start]..].iter().collect());
  }
  out
}

/// The warning payload when `sentence` carries a negation or contrast word, else `None`. The match runs on the
/// backtick-masked sentence and names the first list word as the sentence writes it.
fn negation_payload(sentence: &str) -> Option<String> {
  let (masked, mapping) = mask_backticks(sentence);
  let found = NEGATION.find(&masked)?;
  let first = masked[..found.start()].chars().count();
  let last = first + masked[found.start()..found.end()].chars().count() - 1;
  let chars: Vec<char> = sentence.chars().collect();
  let word: String = chars[mapping[first]..=mapping[last]].iter().collect();
  Some(format!("negation or contrast '{word}': {NEGATION_ADVICE}: {sentence}"))
}

fn title_line(text: &str) -> Option<&str> {
  text.split('\n').find(|line| line.starts_with("title:"))
}
