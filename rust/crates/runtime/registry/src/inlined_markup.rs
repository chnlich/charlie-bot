//! The form page markup takes when the server serves it.
//!
//! A page's markup carries comments that explain the layout to an editor, and the browser reads none of them. The
//! scripts and stylesheets a page inlines already travel without theirs (`inlined_script`, `inlined_style`); this
//! module gives the markup outside a tag the same treatment. On the mock home's chat page the 33 markup comments are
//! 3.0 KB of the document's 403,518 B and 1,203 B of its 100,778 B gzip form.
//!
//! A stylesheet a template writes inline, rather than including from a file, holds the comments its author wrote too;
//! those leave the served form as well, through the owner of that language's inline form. On the mock home's pages that
//! is 1,346 B of the chat document's 400,559 B and 635 B of its 99,577 B gzip form, 3,366 B of the events viewer's
//! 35,657 B and 1,269 B of its 10,356 B, and 2,721 B of the home page's 12,551 B and 1,092 B of its 4,590 B.
//!
//! A script a template writes inline takes that strip too, unless the script minifier would misread it. The minifier
//! reads a `/` after a statement-ending brace as a division, so a regular expression a new statement starts there
//! (`if (ok) {} /a  b/.test(v)`) would lose the whitespace it requires; a script holding that shape keeps its bytes,
//! because only the author knows whether the slash starts a regular expression. A script element whose type is not
//! JavaScript holds opaque data and keeps its bytes as well. On the mock home the inline scripts carry 4,062 B of the
//! chat document's 399,213 B and 1,645 B of its 98,942 B gzip form, 4,924 B of the events viewer's 32,310 B and
//! 1,159 B of its 9,097 B, and 515 B of the home page's 9,830 B and 261 B of its 3,498 B.
//!
//! The strip is textual and keeps every other byte. A tag keeps its attributes, which may spell `<!--`; a title or a
//! text area keeps its text, which is not code; and a `<!--` that never closes keeps the bytes after it, because only
//! the author knows where that text ends.

/// The HTML `markup` with its markup comments removed.
///
/// A comment's bytes leave the text where markup outside a tag holds `<!--` through `-->`. A tag is copied whole,
/// quotes and all, so a `<!--` inside an attribute value stays. A `<style>` element keeps its open and close tags and
/// holds the form its language's owner gives its content, so the comments and the delimiter whitespace an inline style
/// block carries leave it. A `<script>` element whose type is JavaScript holds that form too, unless its content could
/// hold a regular expression the minifier would misread as a division. Every other element the browser takes whole
/// keeps its text. The close tag matches its open tag's spelling without regard to case, as the browser's does.
pub fn without_comments(markup: &str) -> String {
  let bytes = markup.as_bytes();
  let mut out = String::with_capacity(markup.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] != b'<' {
      let end = char_end(markup, i);
      out.push_str(&markup[i..end]);
      i = end;
      continue;
    }
    if bytes[i..].starts_with(b"<!--") {
      match comment_end(markup, i) {
        Some(end) => i = end,
        None => {
          out.push_str(&markup[i..]);
          break;
        }
      }
      continue;
    }
    let tag_start = i;
    let end = tag_end(markup, i);
    let raw = raw_text_name(&markup[i..end]);
    out.push_str(&markup[i..end]);
    i = end;
    if let Some(name) = raw {
      let close = close_tag_start(markup, i, name);
      let after = if close < markup.len() { tag_end(markup, close) } else { markup.len() };
      out.push_str(&inline_form(name, &markup[tag_start..end], &markup[i..close]));
      out.push_str(&markup[close..after]);
      i = after;
    }
  }
  out
}

/// The byte index after the comment that starts at `start`, or `None` when it never closes.
///
/// A comment closes where the browser's tokenizer closes it: `<!-->` closes an empty one, `<!--->` one holding a dash,
/// and every other comment at its first `-->` or `--!>`. Following only `-->` would read the text between an abrupt
/// close and the next `-->` as comment, and delete visible markup.
fn comment_end(markup: &str, start: usize) -> Option<usize> {
  let rest = &markup[start + 4..];
  if rest.starts_with('>') {
    return Some(start + 5);
  }
  if rest.starts_with("->") {
    return Some(start + 6);
  }
  let mut end = None;
  for (marker, len) in [("-->", 3), ("--!>", 4)] {
    if let Some(at) = rest.find(marker) {
      let candidate = start + 4 + at + len;
      end = Some(end.map_or(candidate, |found: usize| found.min(candidate)));
    }
  }
  end
}

/// The byte index after the tag that starts at `start`, past its closing `>`. A `>` inside a quoted attribute value
/// does not end the tag.
fn tag_end(markup: &str, start: usize) -> usize {
  let bytes = markup.as_bytes();
  let mut quote = None;
  let mut i = start + 1;
  while i < bytes.len() {
    let byte = bytes[i];
    match quote {
      Some(open) => {
        if byte == open {
          quote = None;
        }
      }
      None => match byte {
        b'"' | b'\'' => quote = Some(byte),
        b'>' => return i + 1,
        _ => {}
      },
    }
    i += 1;
  }
  bytes.len()
}

/// The end of the character that starts at `start`, so a multi-byte character is copied whole.
fn char_end(markup: &str, start: usize) -> usize {
  let mut end = start + 1;
  while end < markup.len() && !markup.is_char_boundary(end) {
    end += 1;
  }
  end
}

/// The served form of one raw-text element's content, by the owner of that language's inline form.
///
/// A style element's content is CSS, so it takes the strip its language's owner gives an included file. A script
/// element whose type is JavaScript takes that owner's strip as well, unless [`regex_may_follow_brace`] says the
/// minifier could misread it. A script whose type is anything else holds opaque data, and a title, a text area and
/// the elements the browser takes whole without running them are text the author wrote; each keeps its bytes.
fn inline_form(name: &str, tag: &str, content: &str) -> String {
  match name {
    "style" => crate::inlined_style::without_comments(content),
    "script" if javascript_type(tag) && !regex_may_follow_brace(content) => {
      crate::inlined_script::without_comments(content)
    }
    _ => content.to_string(),
  }
}

/// The JavaScript types a `type` attribute names, in the spelling the browser compares without regard to case.
///
/// An element with no `type` attribute, or with an empty one, is a JavaScript block. The browser runs a script written
/// in any of these; another type, such as `application/json` or `text/template`, holds data it never runs.
const JAVASCRIPT_TYPES: [&str; 6] =
  ["", "module", "text/javascript", "application/javascript", "text/ecmascript", "application/ecmascript"];

/// Whether a script element's `type` attribute names JavaScript.
///
/// A type outside [`JAVASCRIPT_TYPES`], a space-separated type list whose first item names something else, and an
/// element the browser never runs all answer false, so their bytes stay whole.
fn javascript_type(tag: &str) -> bool {
  let Some(value) = attribute(tag, "type") else { return true };
  let first = value.trim().split(';').next().unwrap_or("").trim();
  JAVASCRIPT_TYPES.iter().any(|known| first.eq_ignore_ascii_case(known))
}

/// The value of attribute `name` in `tag`, or `None` when the tag names it not.
///
/// A value quoted with `"` or `'` runs to the matching quote; an unquoted one runs to the next whitespace or `>`.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
  let bytes = tag.as_bytes();
  let mut i = 1;
  while i < bytes.len() {
    while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
      i += 1;
    }
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_' || bytes[i] == b':') {
      i += 1;
    }
    let found = &tag[start..i];
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if bytes.get(i) != Some(&b'=') {
      continue;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    let (value_start, value_end) = match bytes.get(i).copied() {
      Some(quote @ (b'"' | b'\'')) => {
        let open = i + 1;
        let mut close = open;
        while close < bytes.len() && bytes[close] != quote {
          close += 1;
        }
        i = close + 1;
        (open, close.min(bytes.len()))
      }
      _ => {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        (start, i)
      }
    };
    if found.eq_ignore_ascii_case(name) {
      return Some(&tag[value_start..value_end]);
    }
  }
  None
}

/// Whether a `/` that starts a new statement after a statement-ending brace could sit in `content`.
///
/// The script minifier reads a `/` after a `}` as a division, so a regular expression a new statement starts there
/// would lose the whitespace it requires. A `//` or `/*` after the brace starts a comment, which the minifier reads
/// whichever byte precedes it, so that shape is not ambiguous. The scan is textual: a string holding the ambiguous
/// shape costs the script its strip and never changes the page's bytes.
fn regex_may_follow_brace(content: &str) -> bool {
  let bytes = content.as_bytes();
  let mut i = 0;
  while let Some(found) = content[i..].find('}') {
    let mut at = i + found + 1;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
      at += 1;
    }
    if bytes.get(at) == Some(&b'/') && !matches!(bytes.get(at + 1), Some(b'/') | Some(b'*')) {
      return true;
    }
    i = i + found + 1;
  }
  false
}

/// The element a tag opens whose text the browser takes whole, if any: a raw-text element, an RCDATA one, or the
/// `plaintext` element, whose text is the rest of the document. Its text is code or a title, so a comment-shaped run
/// inside it is not markup.
fn raw_text_name(tag: &str) -> Option<&'static str> {
  let rest = &tag[1..];
  ["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "plaintext"]
    .into_iter()
    .find(|name| name_follows(rest, name))
}

/// Whether `rest` starts with the element name `name` as a whole name: the bytes after it end a tag name.
fn name_follows(rest: &str, name: &str) -> bool {
  let bytes = rest.as_bytes();
  let name = name.as_bytes();
  bytes.len() >= name.len()
    && bytes[..name.len()].eq_ignore_ascii_case(name)
    && matches!(
      bytes.get(name.len()).copied(),
      None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\x0c') | Some(b'\r')
    )
}

/// The byte index where the close tag of raw-text element `name` starts at or after `from`, or the end of `markup`
/// when the element never closes.
fn close_tag_start(markup: &str, from: usize, name: &str) -> usize {
  let bytes = markup.as_bytes();
  let name = name.as_bytes();
  let mut i = from;
  while let Some(found) = markup[i..].find("</") {
    let at = i + found;
    let after_name = at + 2 + name.len();
    if bytes.len() >= after_name
      && bytes[at + 2..after_name].eq_ignore_ascii_case(name)
      && matches!(
        bytes.get(after_name).copied(),
        None | Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\x0c') | Some(b'\r')
      )
    {
      return at;
    }
    i = at + 2;
  }
  markup.len()
}

#[cfg(test)]
mod tests {
  use super::without_comments;

  #[test]
  fn a_markup_comment_leaves_the_text_and_its_neighbours_keep_every_byte() {
    assert_eq!(without_comments("<p>a</p><!-- note --><p>b</p>"), "<p>a</p><p>b</p>");
    assert_eq!(without_comments("<!DOCTYPE html>\n<!-- x -->\n<html></html>"), "<!DOCTYPE html>\n\n<html></html>");
  }

  #[test]
  fn a_tag_keeps_an_attribute_that_spells_a_comment() {
    let markup = "<div title=\"a <!-- b\" data-x='c --> d'>x</div>";
    assert_eq!(without_comments(markup), markup);
  }

  #[test]
  fn an_inline_style_and_an_inline_script_drop_the_comments_of_their_code() {
    let markup =
      "<script>const url = '<!-- keep -->'; // <!-- js</script><!-- gone --><style>/* <!-- keep --> */</style>";
    assert_eq!(without_comments(markup), "<script>const url='<!-- keep -->';</script><style></style>");
  }

  #[test]
  fn an_inline_script_drops_its_comments_and_the_indentation_of_its_code() {
    let script = "<script>\n    // explains the step\n    if (ok) {\n      run();\n    }\n</script>";
    assert_eq!(without_comments(&format!("{script}<!-- gone -->")), "<script>\n\nif(ok){\nrun();\n}\n</script>");
  }

  #[test]
  fn an_inline_script_that_names_javascript_by_type_drops_them_too() {
    let module = "<script type=\"module\">\n    // explains the step\n    run();\n</script>";
    assert_eq!(without_comments(&format!("{module}<!-- gone -->")), "<script type=\"module\">\n\nrun();\n</script>");
  }

  #[test]
  fn an_inline_script_keeps_a_regular_expression_that_follows_a_statement_brace() {
    let script = "<script>if (ok) {} /a  b/.test(value)</script>";
    assert_eq!(without_comments(&format!("{script}<!-- gone -->")), script);
  }

  #[test]
  fn an_inline_script_keeps_its_code_when_a_line_comment_follows_a_statement_brace() {
    let script = "<script>function f() {} // explains the step\n</script>";
    assert_eq!(without_comments(&format!("{script}<!-- gone -->")), "<script>function f(){}\n</script>");
  }

  #[test]
  fn an_inline_script_keeps_a_body_whose_type_is_not_javascript() {
    let data = "<script type=\"text/template\"><p>  keep  //  this</p></script>";
    let json = "<script type=\"application/json\">{\"a\": 1}</script>";
    assert_eq!(without_comments(&format!("{data}{json}<!-- gone -->")), format!("{data}{json}"));
  }

  #[test]
  fn an_abrupt_close_ends_the_comment_where_the_browser_ends_it() {
    assert_eq!(without_comments("<!--><p>keep</p><!-- gone -->"), "<p>keep</p>");
    assert_eq!(without_comments("<!----><p>keep</p><!-- gone -->"), "<p>keep</p>");
    assert_eq!(without_comments("<p>a</p><!-- x --!><p>b</p>"), "<p>a</p><p>b</p>");
  }

  #[test]
  fn every_element_the_browser_takes_whole_keeps_its_text() {
    let raw = "<xmp><!-- keep --></xmp><iframe><!-- keep --></iframe><noembed><!-- keep --></noembed><noframes><!-- keep --></noframes>";
    assert_eq!(without_comments(&format!("{raw}<!-- gone -->")), raw);
  }

  #[test]
  fn a_form_feed_ends_a_tag_name() {
    let script = "<script\x0c>x='<!-- keep -->'</script>";
    assert_eq!(without_comments(&format!("{script}<!-- gone -->")), script);
  }

  #[test]
  fn a_textarea_or_title_keeps_its_text() {
    let textarea = "<textarea><!-- keep --></textarea>";
    let title = "<title>a <!-- b</title>";
    let markup = format!("{title}{textarea}<!-- gone -->");
    assert_eq!(without_comments(&markup), format!("{title}{textarea}"));
  }

  #[test]
  fn a_close_tag_matches_its_open_tag_without_regard_to_case() {
    let script = "<SCRIPT>x='<!-- keep -->'</sCrIpT>";
    assert_eq!(without_comments(&format!("{script}<!-- gone -->")), script);
  }

  #[test]
  fn an_unclosed_comment_keeps_the_bytes_after_it() {
    let markup = "<p>a</p><!-- never closed <p>b</p>";
    assert_eq!(without_comments(markup), markup);
  }

  #[test]
  fn a_multibyte_character_outside_a_tag_survives() {
    assert_eq!(without_comments("中文<!-- 注释 -->中文"), "中文中文");
  }
}
