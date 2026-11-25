//! The check scans of one reply's text or one page: internal ids, session ids, links, commit hashes and vocabulary.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::LazyLock;

use charliebot_compat::file_urls;
use fancy_regex::Regex;

use crate::api::{
  COMMIT_NAMED, LINK_RENDERABLE, RULE_DEFINED_TERMS, SESSION_LINKED, TRACKER_ID_NEARBY, TextViolation, UserTextChecks,
  VOCABULARY_RULE_TERM,
};
use crate::links::check_page_link;
use crate::markup::{fenced_code_ranges, inline_code_spans, markdown_links, session_target_matches};
use crate::page::PageScan;
use crate::text::{display_block, is_inside, is_url_or_path, line_for_span, within_linear_window};

const LINEAR_ID_PATTERN: &str = r"[A-Z]{2,}-\d+";
const SESSION_ID_PATTERN: &str = concat!(
  r"(?<![\p{L}\p{N}_])(?:",
  r"(?P<full>[A-Fa-f0-9]{8}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{4}-[A-Fa-f0-9]{12})",
  r"|(?P<short>[0-9a-f]{8}))",
  r"(?![\p{L}\p{N}_])",
);
const COMMIT_HASH_PATTERN: &str = r"(?<![\p{L}\p{N}_])(?P<hash>[0-9a-fA-F]{7,40})(?![\p{L}\p{N}_])";

static LINEAR_ID_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(LINEAR_ID_PATTERN).expect("the Linear id pattern is valid"));
static SESSION_ID_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(SESSION_ID_PATTERN).expect("the session id pattern is valid"));
static COMMIT_HASH_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(COMMIT_HASH_PATTERN).expect("the commit hash pattern is valid"));

#[derive(Clone)]
struct SessionMatch {
  range: Range<usize>,
  snippet: String,
  full_id: String,
}

impl UserTextChecks {
  pub(crate) fn tracker_violations<'a>(
    &self,
    blocks: impl IntoIterator<Item = (usize, &'a str)>,
  ) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for (_, block) in blocks {
      let mut linear = Vec::new();
      for found in LINEAR_ID_RE.find_iter(block) {
        linear.push(found?.range());
      }
      let internal = self.internal_matches(block)?;
      for (range, snippet) in internal {
        if !linear.iter().any(|linear| within_linear_window(block, &range, linear)) {
          violations.push(TextViolation::new(TRACKER_ID_NEARBY, snippet, Some(display_block(block))));
        }
      }
    }
    Ok(violations)
  }

  fn internal_matches(&self, text: &str) -> anyhow::Result<Vec<(Range<usize>, String)>> {
    let mut matches = BTreeSet::new();
    for pattern in &self.config.patterns {
      for found in pattern.compiled.find_iter(text) {
        let found = found?;
        matches.insert((found.start(), found.end()));
      }
    }
    Ok(matches.into_iter().map(|(start, end)| ((start..end), text[start..end].to_string())).collect())
  }

  pub(crate) fn reply_session_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let links = markdown_links(text)?;
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let mut violations = Vec::new();
    for matched in self.session_matches(text)? {
      if is_inside(&fenced, &matched.range)
        || is_url_or_path(text, &matched.range, &links)
        || inline_code.iter().any(|code| {
          code.content.start <= matched.range.start
            && matched.range.end <= code.content.end
            && text[code.content.clone()].trim() != matched.snippet
        })
      {
        continue;
      }
      if links.iter().any(|link| {
        link.label.start <= matched.range.start
          && matched.range.end <= link.label.end
          && session_target_matches(&link.target_text, &matched.full_id)
      }) {
        continue;
      }
      violations.push(TextViolation::new(SESSION_LINKED, matched.snippet, None));
    }
    Ok(violations)
  }

  pub(crate) fn page_session_violations(&self, page: &PageScan) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for text in &page.text {
      if text.in_pre {
        continue;
      }
      for matched in self.session_matches(&text.text)? {
        if is_url_or_path(&text.text, &matched.range, &[]) {
          continue;
        }
        if text.anchor_href.as_deref().is_some_and(|href| session_target_matches(href, &matched.full_id)) {
          continue;
        }
        violations.push(TextViolation::new(
          SESSION_LINKED,
          matched.snippet,
          violation_block(page, text.block, &text.text),
        ));
      }
    }
    for code in &page.code {
      if code.in_pre || code.text.trim().is_empty() {
        continue;
      }
      for matched in self.session_matches(&code.text)? {
        if code.text.trim() != matched.snippet || is_url_or_path(&code.text, &matched.range, &[]) {
          continue;
        }
        if code.anchor_href.as_deref().is_some_and(|href| session_target_matches(href, &matched.full_id)) {
          continue;
        }
        violations.push(TextViolation::new(
          SESSION_LINKED,
          matched.snippet,
          violation_block(page, code.block, &code.text),
        ));
      }
    }
    Ok(violations)
  }

  pub(crate) fn reply_link_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let targets = file_urls::scan_link_targets(text);
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let mut violations = Vec::new();
    for target in targets {
      if is_inside(&fenced, &target.span)
        || inline_code.iter().any(|code| {
          let closing_delimiters = text.as_bytes()[code.content.end..].iter().take_while(|byte| **byte == b'`').count();
          code.content.start <= target.span.start && target.span.end <= code.content.end + closing_delimiters
        })
      {
        continue;
      }
      if check_page_link(Some(&target.target)).is_err() {
        violations.push(TextViolation::new(
          LINK_RENDERABLE,
          target.target,
          Some(display_block(line_for_span(text, &target.span))),
        ));
      }
    }
    Ok(violations)
  }

  pub(crate) fn page_link_violations(&self, page: &PageScan) -> anyhow::Result<Vec<TextViolation>> {
    let mut violations = Vec::new();
    for link in &page.links {
      if check_page_link(Some(&link.href)).is_err() {
        violations.push(TextViolation::new(
          LINK_RENDERABLE,
          link.href.clone(),
          violation_block(page, link.block, &link.label),
        ));
      }
    }
    Ok(violations)
  }

  fn session_matches(&self, text: &str) -> anyhow::Result<Vec<SessionMatch>> {
    let mut matches = Vec::new();
    for captures in SESSION_ID_RE.captures_iter(text) {
      let captures = captures?;
      let id = captures.name("full").or_else(|| captures.name("short")).expect("the pattern captures one id");
      let Some(full_id) = self.sessions.resolve(id.as_str(), captures.name("full").is_some()) else {
        continue;
      };
      matches.push(SessionMatch { range: id.range(), snippet: id.as_str().to_string(), full_id: full_id.to_string() });
    }
    Ok(matches)
  }

  pub(crate) fn reply_commit_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let links = markdown_links(text)?;
    let fenced = fenced_code_ranges(text);
    let inline_code = inline_code_spans(text, &fenced);
    let session_matches = self.session_matches(text)?;
    let mut violations = Vec::new();
    for captures in COMMIT_HASH_RE.captures_iter(text) {
      let captures = captures?;
      let matched = captures.name("hash").expect("the pattern captures hash");
      let s = matched.as_str();
      if !s.chars().any(|c| c.is_ascii_digit()) || !s.chars().any(|c| c.is_ascii_alphabetic()) {
        continue;
      }
      let range = matched.range();
      let before = text[..range.start].chars().next_back();
      let after = text[range.end..].chars().next();
      if before == Some('-') || after == Some('-') {
        continue;
      }
      if session_matches.iter().any(|sm| sm.range == range) {
        continue;
      }
      if is_inside(&fenced, &range) || is_url_or_path(text, &range, &links) {
        continue;
      }
      if inline_code.iter().any(|code| {
        code.content.start <= range.start && range.end <= code.content.end && text[code.content.clone()].trim() != s
      }) {
        continue;
      }
      if let Some(link) = links.iter().find(|link| link.label.start <= range.start && range.end <= link.label.end) {
        let label = &text[link.label.clone()];
        let label_stripped = label.trim().trim_matches('`').trim();
        if label_stripped != s && label.chars().any(|c| c.is_alphabetic() && !c.is_ascii_hexdigit()) {
          continue;
        }
      }
      violations.push(TextViolation::new(COMMIT_NAMED, s.to_string(), None));
    }
    Ok(violations)
  }

  pub(crate) fn reply_vocabulary_violations(&self, text: &str) -> anyhow::Result<Vec<TextViolation>> {
    let fenced = fenced_code_ranges(text);
    let mut violations = Vec::new();
    for &term in RULE_DEFINED_TERMS {
      let mut cursor = 0;
      while let Some(index) = text[cursor..].find(term) {
        let start = cursor + index;
        let end = start + term.len();
        cursor = end;
        let range = start..end;
        if is_inside(&fenced, &range) {
          continue;
        }
        violations.push(TextViolation::new(VOCABULARY_RULE_TERM, term.to_string(), None));
      }
    }
    Ok(violations)
  }
}

/// The violation's reported block: the text of the block that holds it, or the display form of `fallback`.
fn violation_block(page: &PageScan, block: Option<usize>, fallback: &str) -> Option<String> {
  block.map(|index| display_block(&page.blocks[index])).or_else(|| Some(display_block(fallback)))
}
