//! The form a stylesheet takes when a page carries it inline.
//!
//! A page inlines its stylesheets so the first paint waits for no second request. The same source files carry the
//! comments that explain them to an editor, and a browser reads no comment: on the mock home's chat page they are
//! 20 KB of the 91 KB of stylesheet text and 8 KB of the document's 164 KB gzip form. The strip is textual and keeps
//! every other byte, including a `/*` that a string or an unquoted `url()` holds.

/// The CSS `source` with its comments removed.
///
/// A comment's bytes leave the text, and no byte stands in for them where the tokens on either side stay apart
/// without one: `.a/**/.b` is the compound selector `.a.b`, and `}/* note */b` is a rule followed by a rule. Where the
/// bytes on either side would run together instead, a bare `/**/` keeps them two tokens: `1px/**/2px` is two values,
/// and `1px 2px` would read as one. A quoted string keeps its text, and so does an unquoted `url()`, which the
/// browser takes whole. CSS has no line comment.
pub fn without_comments(source: &str) -> String {
  let bytes = source.as_bytes();
  let mut out = String::with_capacity(source.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
      let end = comment_end(source, i);
      if needs_boundary(out.as_bytes().last().copied(), bytes.get(end).copied()) {
        out.push_str("/**/");
      }
      i = end;
      continue;
    }
    let end = match bytes[i] {
      b'"' | b'\'' => string_end(source, i),
      b'u' | b'U' if opens_url(bytes, i) => url_end(source, i + 3),
      _ => i + char_len(&source[i..]),
    };
    out.push_str(&source[i..end]);
    i = end;
  }
  out
}

/// The index just past the comment that starts at `start`: its `*/`, or the source's end when it never closes.
fn comment_end(source: &str, start: usize) -> usize {
  source[start + 2..].find("*/").map_or(source.len(), |end| start + 2 + end + 2)
}

/// Whether a comment between the byte `prev` and the byte `next` needs a `/**/` to keep the two tokens apart.
///
/// A name, a number, a hash or an at-keyword takes the character after it as part of itself, so the copy keeps the
/// boundary; a `/` before a `*` would open a comment of its own, and a `<` before a `!` a CDO token. Every other pair
/// ends its token outright, so the comment leaves nothing behind.
fn needs_boundary(prev: Option<u8>, next: Option<u8>) -> bool {
  let (Some(prev), Some(next)) = (prev, next) else {
    return false;
  };
  let continues_with = |next: u8| is_name_char(next) || next == b'\\';
  match prev {
    b'/' => next == b'*',
    b'<' => next == b'!',
    b'\\' => true,
    b'#' | b'@' => continues_with(next),
    b'.' | b'+' | b'-' => next.is_ascii_digit() || next == b'.' || next == b'\\',
    byte if byte.is_ascii_digit() => continues_with(next) || matches!(next, b'.' | b'%'),
    byte => is_name_char(byte) && continues_with(next),
  }
}

/// Whether `byte` can be part of a CSS name: an identifier, a number or a hash value.
fn is_name_char(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') || byte >= 0x80
}

/// Whether the `u` at `start` opens `url(`, so the text inside the parentheses is URL text rather than CSS tokens.
fn opens_url(bytes: &[u8], start: usize) -> bool {
  let after_ident = start > 0 && is_name_char(bytes[start - 1]);
  !after_ident
    && bytes.get(start..start + 4).is_some_and(|name| name[..3].eq_ignore_ascii_case(b"url") && name[3] == b'(')
}

/// The index past the `url(...)` whose `(` sits at `paren`.
///
/// After the parenthesis the browser skips whitespace, takes a quoted string when one starts there, and otherwise
/// reads a whole URL token to its closing `)`. None of that text holds a comment.
fn url_end(source: &str, paren: usize) -> usize {
  let bytes = source.as_bytes();
  let mut i = paren + 1;
  while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')) {
    i += 1;
  }
  if matches!(bytes.get(i), Some(b'"' | b'\'')) {
    return string_end(source, i);
  }
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => i = escape_end(source, i),
      b')' => return i + 1,
      _ => i += 1,
    }
  }
  bytes.len()
}

/// The index past the quoted string that starts at `start`: its closing quote, or the line's end when the line ends
/// first. A backslash escapes the character after it, and a backslash that ends a line continues the string on the
/// next one.
fn string_end(source: &str, start: usize) -> usize {
  let bytes = source.as_bytes();
  let quote = bytes[start];
  let mut i = start + 1;
  while i < bytes.len() {
    match bytes[i] {
      b'\\' => i = escape_end(source, i),
      b'\n' | b'\r' => return i,
      byte if byte == quote => return i + 1,
      _ => i += 1,
    }
  }
  bytes.len()
}

/// The index past the character that the escape at `start` consumes: the line terminator after its backslash, the
/// escaped character, or the backslash alone at the source's end.
fn escape_end(source: &str, start: usize) -> usize {
  let bytes = source.as_bytes();
  let i = start + 1;
  match bytes.get(i) {
    None => i,
    Some(b'\r') if bytes.get(i + 1) == Some(&b'\n') => i + 2,
    Some(_) => i + char_len(&source[i..]),
  }
}

/// The length in bytes of the first character of `text`.
fn char_len(text: &str) -> usize {
  text.chars().next().map_or(1, char::len_utf8)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_comment_between_whole_tokens_leaves_nothing() {
    assert_eq!(without_comments("a{color:red}/* note */b{color:blue}"), "a{color:red}b{color:blue}");
    assert_eq!(without_comments("/* only a comment */"), "");
    assert_eq!(without_comments(".a/**/.b{color:red}"), ".a.b{color:red}");
    assert_eq!(without_comments(".a /**/ .b{color:red}"), ".a  .b{color:red}");
  }

  #[test]
  fn a_comment_between_joined_tokens_keeps_a_bare_boundary() {
    assert_eq!(without_comments("a{width:1px/**/2px}"), "a{width:1px/**/2px}");
    assert_eq!(without_comments("a{width:1/**/.5}"), "a{width:1/**/.5}");
    assert_eq!(without_comments("#/**/a{color:red}"), "#/**/a{color:red}");
    assert_eq!(without_comments("a{/* one\ntwo */color:red}"), "a{color:red}");
  }

  #[test]
  fn a_comment_marker_inside_a_string_is_text() {
    assert_eq!(without_comments(r#"a{content:"/* not a comment */"}"#), r#"a{content:"/* not a comment */"}"#);
    assert_eq!(without_comments("a{content:'b // c'}"), "a{content:'b // c'}");
  }

  #[test]
  fn a_comment_marker_inside_an_unquoted_url_is_text() {
    assert_eq!(without_comments(".a{background:url(foo/*bar*/baz)}"), ".a{background:url(foo/*bar*/baz)}");
    assert_eq!(without_comments(".a{background:url( 'x/*y*/z.png' )}"), ".a{background:url( 'x/*y*/z.png' )}");
    assert_eq!(without_comments(".a{background:myurl/*gone*/(x)}"), ".a{background:myurl(x)}");
  }

  #[test]
  fn a_string_escape_keeps_the_text_it_covers() {
    assert_eq!(without_comments("a{content:\"one\\\r\n/* two */\"}"), "a{content:\"one\\\r\n/* two */\"}");
    assert_eq!(without_comments("a{content:\"\\é/* kept */\"}"), "a{content:\"\\é/* kept */\"}");
  }

  #[test]
  fn every_other_byte_is_kept() {
    assert_eq!(without_comments(".a{color:red}\n  .b{margin:0}"), ".a{color:red}\n  .b{margin:0}");
    assert_eq!(without_comments(".a{content:\"é\"}"), ".a{content:\"é\"}");
  }
}
