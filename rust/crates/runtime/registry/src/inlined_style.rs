//! The form a stylesheet takes when a page carries it inline.
//!
//! A page inlines its stylesheets so the first paint waits for no second request. The same source files carry the
//! comments that explain them to an editor, and a browser reads no comment: on the mock home's chat page they are
//! 23 KB of the 93 KB of stylesheet text and 9 KB of the document's 164 KB gzip form. The strip is textual and keeps
//! every other byte, including a `/*` that a string holds.

/// The CSS `source` with its comments removed.
///
/// A block comment becomes one newline when it spans lines and one space otherwise, so a comment between two tokens
/// still separates them, as it does before the strip. The scanner follows quoted strings, whose text it keeps. CSS
/// has no line comment.
pub fn without_comments(source: &str) -> String {
  let bytes = source.as_bytes();
  let mut out = String::with_capacity(source.len());
  let mut i = 0;
  while i < bytes.len() {
    match bytes[i] {
      b'/' if bytes.get(i + 1) == Some(&b'*') => {
        let end = source[i + 2..].find("*/").map_or(bytes.len(), |end| i + 2 + end + 2);
        out.push(if spans_lines(&bytes[i..end]) { '\n' } else { ' ' });
        i = end;
      }
      b'"' | b'\'' => {
        let end = string_end(bytes, i);
        out.push_str(&source[i..end]);
        i = end;
      }
      _ => {
        let end = i + source[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&source[i..end]);
        i = end;
      }
    }
  }
  out
}

/// Whether the text holds a line terminator.
fn spans_lines(bytes: &[u8]) -> bool {
  bytes.iter().any(|byte| matches!(byte, b'\n' | b'\r'))
}

/// The index after the quoted string that starts at `start`: its closing quote, or the line's end when the line
/// ends first.
fn string_end(bytes: &[u8], start: usize) -> usize {
  let quote = bytes[start];
  let mut i = start + 1;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => i += 2,
      b'\n' | b'\r' => return i,
      byte if byte == quote => return i + 1,
      _ => i += 1,
    }
  }
  bytes.len()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_block_comment_keeps_the_tokens_apart() {
    assert_eq!(without_comments("a{color:red}/* note */b{color:blue}"), "a{color:red} b{color:blue}");
    assert_eq!(without_comments("a{/* one\ntwo */color:red}"), "a{\ncolor:red}");
    assert_eq!(without_comments("/* only a comment */"), " ");
  }

  #[test]
  fn a_comment_marker_inside_a_string_is_text() {
    assert_eq!(without_comments(r#"a{content:"/* not a comment */"}"#), r#"a{content:"/* not a comment */"}"#);
    assert_eq!(without_comments("a{content:'b // c'}"), "a{content:'b // c'}");
  }

  #[test]
  fn every_other_byte_is_kept() {
    assert_eq!(without_comments(".a{color:red}\n  .b{margin:0}"), ".a{color:red}\n  .b{margin:0}");
    assert_eq!(without_comments(".a{content:\"é\"}"), ".a{content:\"é\"}");
  }
}
