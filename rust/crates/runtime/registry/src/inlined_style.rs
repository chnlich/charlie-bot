//! The form a stylesheet takes when a page carries it inline.
//!
//! A page inlines its stylesheets so the first paint waits for no second request. The same source files carry the
//! comments that explain them to an editor, and a browser reads none of them. On the mock home's chat page those
//! comments are 20 KB of the 91 KB of stylesheet text and 8 KB of the document's 164 KB gzip form. The same sheets
//! carry the whitespace around a brace, a semicolon and a comma, which separates no tokens either. The strip is
//! textual and keeps every other byte, including a `/*` that a string or an unquoted `url()` holds.

use crate::inlined_literal::{char_len, escape_end, string_end};

/// The CSS `source` with its comments and its delimiter whitespace removed.
///
/// A comment's bytes leave the text, and no byte stands in for them where the tokens on either side stay apart
/// without one. So `.a/**/.b` is the compound selector `.a.b`, and `}/* note */b` is a rule followed by a rule.
/// Where the bytes on either side would run together instead, a bare `/**/` keeps them two tokens: `1px/**/2px` is
/// two values, and `1px 2px` would read as one. A whitespace run beside a brace, a semicolon or a comma leaves
/// nothing: those bytes end the token before them and start the one after, so `a { color: red }` becomes
/// `a{color: red}`. Any other run separates two selectors or two values and stays as one space. A quoted string
/// keeps its text, and so does an unquoted `url()`, which the browser takes whole, and an escape keeps the character
/// it covers. CSS has no line comment.
pub fn without_comments(source: &str) -> String {
  let bytes = source.as_bytes();
  let mut out = String::with_capacity(source.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
      let end = comment_end(source, i);
      let prev = out.as_bytes().last().copied();
      let next = bytes.get(end).copied();
      if needs_boundary(prev, next) || opens_template_tag(prev, next) {
        out.push_str("/**/");
      }
      i = end;
      continue;
    }
    if bytes[i].is_ascii_whitespace() {
      let mut end = i + 1;
      while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
        end += 1;
      }
      if needs_separating_space(out.as_bytes().last().copied(), bytes.get(end).copied()) {
        out.push(' ');
      }
      i = end;
      continue;
    }
    let end = match bytes[i] {
      b'"' | b'\'' => string_end(source, i),
      b'u' | b'U' if opens_url(bytes, i) => url_end(source, i + 3),
      b'\\' => escape_end(source, i),
      _ => i + char_len(&source[i..]),
    };
    out.push_str(&source[i..end]);
    i = end;
  }
  out
}

/// Whether a whitespace run between `prev` and `next` separates two tokens.
///
/// A brace, a semicolon and a comma end the token before them and start the one after, so a run beside one of them
/// separates nothing. Every other run is the descendant combinator of two selectors or the gap between two values,
/// and stays as one space. A space that keeps `{{`, `{%` and `{#` apart stays as well: the inline copy is also the
/// template engine's source, and those bytes open a tag where the source had two tokens.
fn needs_separating_space(prev: Option<u8>, next: Option<u8>) -> bool {
  if opens_template_tag(prev, next) {
    return true;
  }
  let (Some(prev), Some(next)) = (prev, next) else {
    return false;
  };
  !matches!(prev, b'{' | b'}' | b';' | b',') && !matches!(next, b'{' | b'}' | b';' | b',')
}

/// Whether `prev` followed by `next` opens a template tag: `{{`, `{%` or `{#`.
fn opens_template_tag(prev: Option<u8>, next: Option<u8>) -> bool {
  let (Some(prev), Some(next)) = (prev, next) else {
    return false;
  };
  matches!((prev, next), (b'{', b'{') | (b'{', b'%') | (b'{', b'#'))
}

/// The index just past the comment that starts at `start`: its `*/`, or the source's end when it never closes.
fn comment_end(source: &str, start: usize) -> usize {
  source[start + 2..].find("*/").map_or(source.len(), |end| start + 2 + end + 2)
}

/// Whether a comment between the byte `prev` and the byte `next` needs a `/**/` to keep the two tokens apart.
///
/// A name, a number, a hash or an at-keyword takes the character after it as part of itself, so the copy keeps the
/// boundary. A `/` before a `*` would open a comment of its own, and a `<` before a `!` a CDO token. Every other
/// pair ends its token outright, so the comment leaves nothing behind.
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
  fn whitespace_beside_a_delimiter_leaves_and_value_whitespace_stays() {
    assert_eq!(without_comments(".a{color:red}\n  .b{margin:0}"), ".a{color:red}.b{margin:0}");
    assert_eq!(without_comments(".a { margin: 0 auto; }"), ".a{margin: 0 auto;}");
    assert_eq!(without_comments(".a , .b{color:red}"), ".a,.b{color:red}");
    assert_eq!(without_comments(".a .b{color:red}"), ".a .b{color:red}");
    assert_eq!(without_comments(".a:hover .b{color:red}"), ".a:hover .b{color:red}");
    assert_eq!(
      without_comments("@media (min-width: 100px) and (max-width: 200px) { .a { color: red } }"),
      "@media (min-width: 100px) and (max-width: 200px){.a{color: red}}"
    );
    assert_eq!(without_comments("a{content: \" \"}"), "a{content: \" \"}");
    assert_eq!(without_comments("a{background:url( 'x y.png' )}"), "a{background:url( 'x y.png' )}");
  }

  #[test]
  fn a_space_stays_where_its_removal_would_open_a_template_tag() {
    assert_eq!(without_comments("a{ {b:c} }"), "a{ {b:c}}");
    assert_eq!(without_comments("a{/* note */{b:c}}"), "a{/**/{b:c}}");
    assert_eq!(without_comments("@media (hover: none){ #a{color:red} }"), "@media (hover: none){ #a{color:red}}");
  }

  #[test]
  fn an_escape_keeps_the_whitespace_it_covers() {
    assert_eq!(without_comments(".foo\\ { color: red }"), ".foo\\ {color: red}");
    assert_eq!(without_comments(".a\\  .b{color:red}"), ".a\\  .b{color:red}");
    assert_eq!(without_comments(".a{content:\"x\\ \"}"), ".a{content:\"x\\ \"}");
  }

  #[test]
  fn every_other_byte_is_kept() {
    assert_eq!(without_comments(".a{content:\"é\"}"), ".a{content:\"é\"}");
    assert_eq!(without_comments(".a{color:red}"), ".a{color:red}");
  }
}
