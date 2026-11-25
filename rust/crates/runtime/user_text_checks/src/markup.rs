//! The Markdown scan of a reply: its links, its fenced code blocks and its inline code spans.

use std::ops::Range;
use std::sync::LazyLock;

use fancy_regex::Regex;

const MARKDOWN_LINK_PATTERN: &str = concat!(
  r"\[(?P<label>[^\]\n]*)\]\([ \t]*(?:",
  r"<(?P<angle>[^>\n]*)>|(?P<bare>[^)\s]+)",
  r")(?:[ \t]+[^)\n]*)?[ \t]*\)",
);

static MARKDOWN_LINK_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(MARKDOWN_LINK_PATTERN).expect("the Markdown link pattern is valid"));

pub(crate) struct MarkdownLink {
  pub(crate) label: Range<usize>,
  pub(crate) target: Range<usize>,
  pub(crate) target_text: String,
}

pub(crate) struct InlineCode {
  pub(crate) content: Range<usize>,
}

pub(crate) fn markdown_links(text: &str) -> anyhow::Result<Vec<MarkdownLink>> {
  let mut links = Vec::new();
  for captures in MARKDOWN_LINK_RE.captures_iter(text) {
    let captures = captures?;
    let label = captures.name("label").expect("the pattern captures the link label");
    let target =
      captures.name("angle").or_else(|| captures.name("bare")).expect("the pattern captures the link target");
    links.push(MarkdownLink { label: label.range(), target: target.range(), target_text: target.as_str().to_string() });
  }
  Ok(links)
}

pub(crate) fn session_target_matches(target: &str, full_id: &str) -> bool {
  let target = target.split_once('#').map_or(target, |(before, _)| before);
  let Some((before_query, query)) = target.split_once('?') else {
    return false;
  };
  link_path(before_query) == "/"
    && query
      .split('&')
      .any(|parameter| parameter.split_once('=').is_some_and(|(name, value)| name == "session" && value == full_id))
}

pub(crate) fn fenced_code_ranges(text: &str) -> Vec<Range<usize>> {
  let mut ranges = Vec::new();
  let mut fence: Option<(u8, usize, usize)> = None;
  let mut offset = 0;
  for line in text.split_inclusive('\n') {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    let content = without_newline.strip_suffix('\r').unwrap_or(without_newline);
    let indent_len = content.bytes().take_while(|byte| *byte == b' ').count();
    let rest = &content[indent_len..];
    let marker = rest.as_bytes().first().copied();
    let marker_count = marker.map_or(0, |marker| rest.bytes().take_while(|byte| *byte == marker).count());
    if let Some((active, minimum, start)) = fence {
      if indent_len <= 3 && marker == Some(active) && marker_count >= minimum && rest[marker_count..].trim().is_empty()
      {
        ranges.push(start..offset + line.len());
        fence = None;
      }
    } else if indent_len <= 3 && marker.is_some_and(|byte| matches!(byte, 0x60 | b'~')) && marker_count >= 3 {
      fence = Some((marker.expect("a fence has a marker"), marker_count, offset));
    }
    offset += line.len();
  }
  if let Some((_, _, start)) = fence {
    ranges.push(start..text.len());
  }
  ranges
}

pub(crate) fn inline_code_spans(text: &str, fenced: &[Range<usize>]) -> Vec<InlineCode> {
  let bytes = text.as_bytes();
  let mut spans = Vec::new();
  let mut cursor = 0;
  while cursor < bytes.len() {
    if let Some(fence) = fenced.iter().find(|fence| fence.start <= cursor && cursor < fence.end) {
      cursor = fence.end;
      continue;
    }
    if bytes[cursor] != 0x60 {
      cursor += 1;
      continue;
    }
    let start = cursor;
    while cursor < bytes.len() && bytes[cursor] == 0x60 {
      cursor += 1;
    }
    let count = cursor - start;
    if escaped_backtick(bytes, start) {
      continue;
    }
    if let Some(close) = matching_backtick_run(bytes, cursor, count, fenced) {
      spans.push(InlineCode { content: cursor..close });
      cursor = close + count;
    }
  }
  spans
}

fn escaped_backtick(bytes: &[u8], start: usize) -> bool {
  let mut slashes = 0;
  let mut cursor = start;
  while cursor > 0 && bytes[cursor - 1] == b'\\' {
    slashes += 1;
    cursor -= 1;
  }
  slashes % 2 == 1
}

fn matching_backtick_run(bytes: &[u8], mut cursor: usize, count: usize, fenced: &[Range<usize>]) -> Option<usize> {
  while cursor < bytes.len() {
    if let Some(fence) = fenced.iter().find(|fence| fence.start <= cursor && cursor < fence.end) {
      cursor = fence.end;
      continue;
    }
    if bytes[cursor] != 0x60 {
      cursor += 1;
      continue;
    }
    let start = cursor;
    while cursor < bytes.len() && bytes[cursor] == 0x60 {
      cursor += 1;
    }
    if cursor - start == count {
      return Some(start);
    }
  }
  None
}

/// The path part of a link: its text before any query or fragment, with the scheme and authority removed.
fn link_path(link: &str) -> &str {
  let no_fragment = link.split_once('#').map_or(link, |(before, _)| before);
  let no_query = no_fragment.split_once('?').map_or(no_fragment, |(before, _)| before);
  if let Some((_, after_scheme)) = no_query.split_once("://") {
    after_scheme.find('/').map_or("", |slash| &after_scheme[slash..])
  } else if let Some(authority) = no_query.strip_prefix("//") {
    authority.find('/').map_or("", |slash| &authority[slash..])
  } else {
    no_query
  }
}
