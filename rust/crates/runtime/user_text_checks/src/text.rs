//! Span and block helpers shared by the checks: line blocks, containment, and URL or path detection.

use std::ops::Range;

use charliebot_compat::file_urls;

use crate::markup::MarkdownLink;

pub(crate) fn within_linear_window(block: &str, internal: &Range<usize>, linear: &Range<usize>) -> bool {
  let gap = if linear.end <= internal.start {
    block[linear.end..internal.start].chars().count()
  } else if internal.end <= linear.start {
    block[internal.end..linear.start].chars().count()
  } else {
    0
  };
  gap <= 40
}

pub(crate) fn line_blocks(text: &str) -> Vec<(usize, &str)> {
  let mut offset = 0;
  text
    .split_inclusive('\n')
    .map(|line| {
      let without_newline = line.strip_suffix('\n').unwrap_or(line);
      let content = without_newline.strip_suffix('\r').unwrap_or(without_newline);
      let block = (offset, content);
      offset += line.len();
      block
    })
    .collect()
}

pub(crate) fn line_for_span<'a>(text: &'a str, span: &Range<usize>) -> &'a str {
  let start = text[..span.start].rfind('\n').map_or(0, |index| index + '\n'.len_utf8());
  let end = text[span.end..].find('\n').map_or(text.len(), |index| span.end + index);
  &text[start..end]
}

pub(crate) fn display_block(block: &str) -> String {
  block.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn is_url_or_path(text: &str, span: &Range<usize>, links: &[MarkdownLink]) -> bool {
  if file_urls::url_regex().find_iter(text).any(|url| url.start() <= span.start && span.end <= url.end())
    || links.iter().any(|link| link.target.start <= span.start && span.end <= link.target.end)
  {
    return true;
  }
  let previous = text[..span.start].chars().next_back();
  let next = text[span.end..].chars().next();
  if previous.is_some_and(|character| matches!(character, '/' | '\\'))
    || next.is_some_and(|character| matches!(character, '/' | '\\'))
  {
    return true;
  }
  // The matched whitespace can be multi-byte, such as U+3000, so the token starts after all of its bytes.
  let start = text[..span.start]
    .char_indices()
    .rev()
    .find(|(_, character)| character.is_whitespace())
    .map_or(0, |(index, character)| index + character.len_utf8());
  let end = text[span.end..].find(char::is_whitespace).map_or(text.len(), |index| span.end + index);
  let token = &text[start..end];
  let uri = token.trim_start_matches(['(', '[', '{', '<']);
  if uri.split_once(':').is_some_and(|(scheme, _)| {
    scheme.chars().next().is_some_and(|first| first.is_ascii_alphabetic())
      && scheme.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'))
  }) {
    return true;
  }
  token.starts_with('/') && token.contains('?')
}

pub(crate) fn is_inside(ranges: &[Range<usize>], span: &Range<usize>) -> bool {
  ranges.iter().any(|range| range.start <= span.start && span.end <= range.end)
}
