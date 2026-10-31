//! HTMLParser callback events with Python code-point positions and UTF-8 byte offsets.

use std::error::Error;
use std::fmt;
use std::sync::LazyLock;

use charliebot_core::str_methods;
use fancy_regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::entities::{unescape, unescape_attribute};

// These patterns port tagfind_tolerant, attrfind_tolerant, and locatetagend from CPython.
// ATTRFIND and LOCATETAGEND retain the fixed-width lookbehind at attribute boundaries.
static TAGFIND_TOLERANT: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"([a-zA-Z][^\t\n\r\x0c />]*)(?:[\t\n\r\x0c ]|/(?!>))*").expect("valid tagfind_tolerant pattern")
});
static ATTRFIND_TOLERANT: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(concat!(
    r#"(?x)(?<=['"\t\n\r\x0c /])"#,
    r#"([^\t\n\r\x0c />][^\t\n\r\x0c /=>]*)"#,
    r#"([\t\n\r\x0c ]*=[\t\n\r\x0c ]*"#,
    r#"('[^']*'|"[^"]*"|(?!['"])[^>\t\n\r\x0c ]*))?"#,
    r#"(?:[\t\n\r\x0c ]|/(?!>))*"#,
  ))
  .expect("valid attrfind_tolerant pattern")
});
static LOCATETAGEND: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(concat!(
    r#"(?x)[a-zA-Z][^\t\n\r\x0c />]*[\t\n\r\x0c /]*"#,
    r#"(?:(?<=['"\t\n\r\x0c /])[^\t\n\r\x0c />][^\t\n\r\x0c /=>]*"#,
    r#"(?:[\t\n\r\x0c ]*=[\t\n\r\x0c ]*(?:'[^']*'|"[^"]*"|"#,
    r#"(?!['"])[^>\t\n\r\x0c ]*))?[\t\n\r\x0c /]*)*>?"#,
  ))
  .expect("valid locatetagend pattern")
});

/// A one-based line and zero-based Unicode code-point column from `HTMLParser.getpos()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
  pub line: usize,
  pub column: usize,
}

/// The `HTMLParser` callback represented by an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
  StartTag,
  StartEndTag,
  EndTag,
  Data,
  EntityReference,
  CharacterReference,
  Comment,
  Declaration,
  ProcessingInstruction,
  UnknownDeclaration,
}

/// One callback's positional arguments, source position, and optional raw start-tag text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
  pub kind: EventKind,
  pub args: Value,
  pub position: Position,
  pub byte_offset: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub starttag_text: Option<String>,
}

/// One streaming callback's arguments and source byte offset, without line and column data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
  pub kind: EventKind,
  pub args: Value,
  pub byte_offset: usize,
  pub starttag_text: Option<String>,
}

/// A Python exception category and message produced while tokenizing a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
  pub exception: String,
  pub message: String,
}

impl ParseError {
  fn new(exception: &str, message: impl Into<String>) -> Self {
    Self { exception: exception.to_owned(), message: message.into() }
  }
}

impl fmt::Display for ParseError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.exception, self.message)
  }
}

impl Error for ParseError {}

/// Tokenize a document as `feed(source)` followed by `close()` on a fresh parser.
pub fn parse(source: &str, convert_charrefs: bool) -> Result<Vec<Event>, ParseError> {
  let mut events = Vec::new();
  let mut positions = SourcePositions::new(source);
  parse_with(source, convert_charrefs, |event| {
    events.push(Event {
      kind: event.kind,
      args: event.args,
      position: positions.at(source, event.byte_offset),
      byte_offset: event.byte_offset,
      starttag_text: event.starttag_text,
    });
  })?;
  Ok(events)
}

/// Tokenize a document as `feed(source)` followed by `close()` on a fresh parser, passing each callback to `handler`
/// as soon as it is emitted. The streamed form carries the byte offset alone; a consumer that needs line and column
/// positions calls `parse` instead.
pub fn parse_with<F>(source: &str, convert_charrefs: bool, handler: F) -> Result<(), ParseError>
where
  F: FnMut(StreamEvent),
{
  let mut parser = Parser::new(source, convert_charrefs, handler);
  let remaining = parser.goahead(false, 0)?;
  parser.goahead(true, remaining)?;
  Ok(())
}

struct Parser<'a, F> {
  source: &'a str,
  convert_charrefs: bool,
  cdata_elem: Option<String>,
  escapable: bool,
  support_cdata: bool,
  handler: F,
}

impl<'a, F: FnMut(StreamEvent)> Parser<'a, F> {
  fn new(source: &'a str, convert_charrefs: bool, handler: F) -> Self {
    Self { source, convert_charrefs, cdata_elem: None, escapable: true, support_cdata: true, handler }
  }

  fn emit(&mut self, kind: EventKind, args: Value, at: usize, starttag_text: Option<String>) {
    (self.handler)(StreamEvent { kind, args, byte_offset: at, starttag_text });
  }

  fn emit_data(&mut self, data: &str, at: usize) {
    self.emit(EventKind::Data, json!([data]), at, None);
  }

  fn goahead(&mut self, end: bool, mut i: usize) -> Result<usize, ParseError> {
    let n = self.source.len();
    while i < n {
      let next = if self.convert_charrefs && self.cdata_elem.is_none() {
        if let Some(position) = self.source[i..].find('<') {
          Some(i + position)
        } else {
          // CPython's n-34 tail window counts Unicode code points, while this scanner uses byte offsets.
          let tail_start = i.max(char_offset_from_end(self.source, 34));
          let amp = self.source[tail_start..].rfind('&').map(|position| tail_start + position);
          if amp.is_some_and(|position| !self.source[position..].chars().skip(1).any(is_reference_delimiter)) {
            None
          } else {
            Some(n)
          }
        }
      } else {
        self.find_interesting(i)
      };

      let Some(j) = next else {
        break;
      };

      if i < j {
        let data = &self.source[i..j];
        if self.convert_charrefs && self.escapable {
          let decoded = unescape(data);
          self.emit_data(&decoded, i);
        } else {
          self.emit_data(data, i);
        }
      }
      i = j;
      if i == n {
        break;
      }

      if self.source[i..].starts_with('<') {
        let result = if starts_with_start_tag(self.source, i) {
          self.parse_starttag(i)?
        } else if self.source[i..].starts_with("</") {
          self.parse_endtag(i)?
        } else if self.source[i..].starts_with("<!--") {
          self.parse_comment(i)
        } else if self.source[i..].starts_with("<?") {
          self.parse_pi(i)
        } else if self.source[i..].starts_with("<!") {
          self.parse_html_declaration(i)
        } else if self.source[i..].chars().count() > 1 || end {
          self.emit_data("<", i);
          Some(i + 1)
        } else {
          None
        };

        let Some(k) = result else {
          if !end {
            break;
          }
          let tail = &self.source[i..];
          if starts_with_start_tag(self.source, i) {
            i = n;
            continue;
          }
          if let Some(end_tag_data) = tail.strip_prefix("</") {
            if tail.chars().count() == 2 {
              self.emit_data("</", i);
            } else if !starts_with_end_tag(self.source, i) {
              self.emit(EventKind::Comment, json!([end_tag_data]), i, None);
            }
          } else if let Some(comment_data) = tail.strip_prefix("<!--") {
            let mut stop = n;
            for suffix in ["--!", "--", "-"] {
              if comment_data.ends_with(suffix) {
                stop -= suffix.len();
                break;
              }
            }
            self.emit(EventKind::Comment, json!([&self.source[i + 4..stop]]), i, None);
          } else if tail.starts_with("<![CDATA[") && self.support_cdata {
            self.emit(EventKind::UnknownDeclaration, json!([&self.source[i + 3..]]), i, None);
          } else if starts_with_doctype(tail) {
            self.emit(EventKind::Declaration, json!([&self.source[i + 2..]]), i, None);
          } else if tail.starts_with("<!") {
            self.emit(EventKind::Comment, json!([&self.source[i + 2..]]), i, None);
          } else if tail.starts_with("<?") {
            self.emit(EventKind::ProcessingInstruction, json!([&self.source[i + 2..]]), i, None);
          } else {
            return Err(ParseError::new("AssertionError", "we should not get here!"));
          }
          i = n;
          continue;
        };
        i = k;
      } else if self.source[i..].starts_with("&#") {
        if let Some((end_at, name)) = character_reference(self.source, i) {
          self.emit(EventKind::CharacterReference, json!([name]), i, None);
          i =
            if self.source[..end_at].ends_with(';') { end_at } else { end_at - previous_char_len(self.source, end_at) };
          continue;
        }
        if incomplete_character_reference(self.source, i) {
          if end {
            self.emit(EventKind::CharacterReference, json!([&self.source[i + 2..]]), i, None);
            i = n;
            break;
          }
          break;
        }
        if self.source[i..].chars().count() > 3 {
          self.emit_data("&#", i);
          i += 2;
        } else {
          break;
        }
      } else if self.source[i..].starts_with('&') {
        if let Some((end_at, name)) = entity_reference(self.source, i) {
          self.emit(EventKind::EntityReference, json!([name]), i, None);
          i =
            if self.source[..end_at].ends_with(';') { end_at } else { end_at - previous_char_len(self.source, end_at) };
          continue;
        }
        if incomplete_reference(self.source, i) {
          if end {
            self.emit(EventKind::EntityReference, json!([&self.source[i + 1..]]), i, None);
            i = n;
            break;
          }
          break;
        }
        if i + 1 < n {
          self.emit_data("&", i);
          i += 1;
        } else {
          break;
        }
      } else {
        return Err(ParseError::new("AssertionError", "interesting.search() lied"));
      }
    }

    if end && i < n {
      let data = &self.source[i..n];
      if self.convert_charrefs && self.escapable {
        let decoded = unescape(data);
        self.emit_data(&decoded, i);
      } else {
        self.emit_data(data, i);
      }
      i = n;
    }
    Ok(i)
  }

  fn find_interesting(&self, start: usize) -> Option<usize> {
    let Some(element) = self.cdata_elem.as_deref() else {
      return self.source[start..].find(['<', '&']).map(|position| start + position);
    };
    if element == "plaintext" {
      return None;
    }
    let end_tag = find_raw_end_tag(self.source, start, element);
    if self.escapable && !self.convert_charrefs {
      let entity = self.source[start..].find('&').map(|position| start + position);
      match (end_tag, entity) {
        (Some(tag), Some(reference)) => Some(tag.min(reference)),
        (Some(tag), None) | (None, Some(tag)) => Some(tag),
        (None, None) => None,
      }
    } else {
      end_tag
    }
  }

  fn parse_starttag(&mut self, i: usize) -> Result<Option<usize>, ParseError> {
    let Some(endpos) = self.check_for_whole_start_tag(i)? else {
      return Ok(None);
    };
    let raw = &self.source[i..endpos];
    let tag_match = capture_at_exact(&TAGFIND_TOLERANT, self.source, i + 1)?
      .ok_or_else(|| ParseError::new("AssertionError", "unexpected call to parse_starttag()"))?;
    let tag = tag_match.get(1).expect("tag name capture exists").as_str().to_lowercase();
    let mut cursor = tag_match.get(0).expect("tag match exists").end();
    let mut attrs: Vec<(String, Option<String>)> = Vec::new();
    while cursor < endpos {
      let Some(attribute_match) = captures_from_pos(&ATTRFIND_TOLERANT, self.source, cursor)? else {
        break;
      };
      let whole = attribute_match.get(0).expect("attribute match exists");
      if whole.start() != cursor || whole.end() > endpos {
        break;
      }
      let name = attribute_match.get(1).expect("attribute name capture exists").as_str().to_lowercase();
      let rest = attribute_match.get(2);
      let mut value = if rest.is_none() {
        None
      } else {
        let raw_value = attribute_match.get(3).expect("attribute value capture exists").as_str();
        Some(if raw_value.len() >= 2 && matching_quote(raw_value) {
          raw_value[1..raw_value.len() - 1].to_owned()
        } else {
          raw_value.to_owned()
        })
      };
      if let Some(value_text) = value.as_mut()
        && !value_text.is_empty()
      {
        *value_text = unescape_attribute(value_text);
      }
      attrs.push((name, value));
      cursor = whole.end();
    }

    let end = str_methods::strip(&self.source[cursor..endpos]);
    if end != ">" && end != "/>" {
      self.emit_data(raw, i);
      return Ok(Some(endpos));
    }

    let event_kind = if end.ends_with("/>") { EventKind::StartEndTag } else { EventKind::StartTag };
    let attr_values: Vec<Value> = attrs.iter().map(|(name, value)| json!([name, value])).collect();
    self.emit(event_kind, json!([tag, attr_values]), i, Some(raw.to_owned()));
    if event_kind == EventKind::StartTag {
      if is_cdata_element(&tag) || tag == "plaintext" {
        self.set_cdata_mode(&tag, false);
      } else if tag == "textarea" || tag == "title" {
        self.set_cdata_mode(&tag, true);
      }
    }
    Ok(Some(endpos))
  }

  fn check_for_whole_start_tag(&self, i: usize) -> Result<Option<usize>, ParseError> {
    let tag_start = i + 1;
    let Some(match_) = capture_at_exact(&LOCATETAGEND, self.source, tag_start)? else {
      return Err(ParseError::new("AssertionError", "check_for_whole_start_tag() expected a match"));
    };
    let end = match_.get(0).expect("tag end match exists").end();
    if self.source[..end].ends_with('>') { Ok(Some(end)) } else { Ok(None) }
  }

  fn parse_endtag(&mut self, i: usize) -> Result<Option<usize>, ParseError> {
    if !self.source[i..].starts_with("</") {
      return Err(ParseError::new("AssertionError", "unexpected call to parse_endtag"));
    }
    if !self.source[i + 2..].contains('>') {
      return Ok(None);
    }
    if !starts_with_end_tag(self.source, i) {
      if self.source[i + 2..].starts_with('>') {
        return Ok(Some(i + 3));
      }
      return Ok(self.parse_bogus_comment(i, true));
    }
    let Some(tag_end) = capture_at_exact(&LOCATETAGEND, self.source, i + 2)? else {
      return Err(ParseError::new("AssertionError", "parse_endtag() expected a match"));
    };
    let end = tag_end.get(0).expect("tag end match exists").end();
    if !self.source[..end].ends_with('>') {
      return Ok(None);
    }
    let Some(tag_match) = capture_at_exact(&TAGFIND_TOLERANT, self.source, i + 2)? else {
      return Err(ParseError::new("AssertionError", "parse_endtag() expected a tag name"));
    };
    let tag = tag_match.get(1).expect("tag name capture exists").as_str().to_lowercase();
    self.emit(EventKind::EndTag, json!([tag]), i, None);
    self.clear_cdata_mode();
    Ok(Some(end))
  }

  fn parse_comment(&mut self, i: usize) -> Option<usize> {
    if !self.source[i..].starts_with("<!--") {
      return None;
    }
    let body_start = i + 4;
    let (end, body_end) = if self.source[body_start..].starts_with('>') {
      (body_start + 1, body_start)
    } else if self.source[body_start..].starts_with("->") {
      (body_start + 2, body_start)
    } else {
      let end = find_comment_close(self.source, body_start)?;
      let closing_len = if self.source[..end].ends_with("--!>") { 4 } else { 3 };
      (end, end - closing_len)
    };
    self.emit(EventKind::Comment, json!([&self.source[body_start..body_end]]), i, None);
    Some(end)
  }

  fn parse_bogus_comment(&mut self, i: usize, report: bool) -> Option<usize> {
    let tail = &self.source[i..];
    if !tail.starts_with("<!") && !tail.starts_with("</") {
      return None;
    }
    let position = self.source[i + 2..].find('>')? + i + 2;
    if report {
      self.emit(EventKind::Comment, json!([&self.source[i + 2..position]]), i, None);
    }
    Some(position + 1)
  }

  fn parse_pi(&mut self, i: usize) -> Option<usize> {
    if !self.source[i..].starts_with("<?") {
      return None;
    }
    let end = self.source[i + 2..].find('>')? + i + 2;
    self.emit(EventKind::ProcessingInstruction, json!([&self.source[i + 2..end]]), i, None);
    Some(end + 1)
  }

  fn parse_html_declaration(&mut self, i: usize) -> Option<usize> {
    // HTMLParser dispatches declarations here; ParserBase.parse_declaration is not its declaration path.
    let tail = &self.source[i..];
    if !tail.starts_with("<!") {
      return None;
    }
    if tail.starts_with("<!--") {
      return self.parse_comment(i);
    }
    if self.support_cdata && tail.starts_with("<![CDATA[") {
      let end = self.source[i + 9..].find("]]>")? + i + 9;
      self.emit(EventKind::UnknownDeclaration, json!([&self.source[i + 3..end]]), i, None);
      return Some(end + 3);
    }
    if starts_with_doctype(tail) {
      let end = self.source[i + 9..].find('>')? + i + 9;
      self.emit(EventKind::Declaration, json!([&self.source[i + 2..end]]), i, None);
      return Some(end + 1);
    }
    self.parse_bogus_comment(i, true)
  }

  fn set_cdata_mode(&mut self, element: &str, escapable: bool) {
    self.cdata_elem = Some(element.to_lowercase());
    self.escapable = escapable;
  }

  fn clear_cdata_mode(&mut self) {
    self.cdata_elem = None;
    self.escapable = true;
  }
}

struct SourcePositions {
  line_starts: Vec<usize>,
  line_index: usize,
  last_byte: usize,
  position: Position,
}

impl SourcePositions {
  fn new(source: &str) -> Self {
    // `HTMLParser.getpos()` measures a column from its line's first character, so `line_starts` records one
    // start offset per line.
    let line_count = source.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let mut line_starts = Vec::with_capacity(line_count);
    line_starts.push(0);
    for (byte, ch) in source.char_indices() {
      if ch == '\n' {
        line_starts.push(byte + 1);
      }
    }
    Self { line_starts, line_index: 0, last_byte: 0, position: Position { line: 1, column: 0 } }
  }

  fn at(&mut self, source: &str, byte: usize) -> Position {
    let line_index = self.line_starts.partition_point(|start| *start <= byte) - 1;
    if line_index == self.line_index {
      self.position.column += source[self.last_byte..byte].chars().count();
    } else {
      self.position.line = line_index + 1;
      self.position.column = source[self.line_starts[line_index]..byte].chars().count();
      self.line_index = line_index;
    }
    self.last_byte = byte;
    self.position
  }
}

fn captures_from_pos<'a>(regex: &Regex, source: &'a str, pos: usize) -> Result<Option<Captures<'a>>, ParseError> {
  regex.captures_from_pos(source, pos).map_err(|error| ParseError::new("RuntimeError", error.to_string()))
}

fn capture_at_exact<'a>(regex: &Regex, source: &'a str, pos: usize) -> Result<Option<Captures<'a>>, ParseError> {
  Ok(
    captures_from_pos(regex, source, pos)?.filter(|captures| captures.get(0).is_some_and(|whole| whole.start() == pos)),
  )
}

fn starts_with_start_tag(source: &str, position: usize) -> bool {
  source[position..].strip_prefix('<').and_then(|tail| tail.chars().next()).is_some_and(|ch| ch.is_ascii_alphabetic())
}

fn starts_with_end_tag(source: &str, position: usize) -> bool {
  source[position..].strip_prefix("</").and_then(|tail| tail.chars().next()).is_some_and(|ch| ch.is_ascii_alphabetic())
}

fn starts_with_doctype(source: &str) -> bool {
  source.get(..9).is_some_and(|prefix| prefix.eq_ignore_ascii_case("<!doctype"))
}

fn is_cdata_element(tag: &str) -> bool {
  matches!(tag, "script" | "style" | "xmp" | "iframe" | "noembed" | "noframes")
}

fn find_raw_end_tag(source: &str, start: usize, element: &str) -> Option<usize> {
  let mut search = start;
  while let Some(relative) = source[search..].find("</") {
    let candidate = search + relative;
    let name_start = candidate + 2;
    let name_end = name_start + element.len();
    if let Some(candidate_name) = source.get(name_start..name_end)
      && candidate_name.eq_ignore_ascii_case(element)
      && source[name_end..].chars().next().is_some_and(is_end_tag_boundary)
    {
      return Some(candidate);
    }
    search = candidate + 2;
  }
  None
}

fn is_end_tag_boundary(ch: char) -> bool {
  matches!(ch, '\t' | '\n' | '\r' | '\u{c}' | ' ' | '/' | '>')
}

fn is_reference_delimiter(ch: char) -> bool {
  matches!(ch, '\t' | '\n' | '\r' | '\u{c}' | ' ' | ';')
}

fn entity_reference(source: &str, start: usize) -> Option<(usize, String)> {
  let after_amp = start + 1;
  let mut chars = source[after_amp..].char_indices();
  let (_, first) = chars.next()?;
  if !first.is_ascii_alphabetic() {
    return None;
  }
  let mut end = after_amp + first.len_utf8();
  for (offset, ch) in chars {
    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
      end = after_amp + offset + ch.len_utf8();
    } else {
      if ch.is_ascii_alphanumeric() {
        return None;
      }
      return Some((end + ch.len_utf8(), source[after_amp..end].to_owned()));
    }
  }
  None
}

fn character_reference(source: &str, start: usize) -> Option<(usize, String)> {
  let bytes = source.as_bytes();
  let mut cursor = start + 2;
  if matches!(bytes.get(cursor), Some(b'x' | b'X')) {
    cursor += 1;
    let digits_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_hexdigit() {
      cursor += 1;
    }
    if cursor == digits_start || cursor == bytes.len() || bytes[cursor].is_ascii_hexdigit() {
      return None;
    }
  } else {
    let digits_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
      cursor += 1;
    }
    if cursor == digits_start || cursor == bytes.len() || bytes[cursor].is_ascii_hexdigit() {
      return None;
    }
  }
  let delimiter = source[cursor..].chars().next()?;
  let end = cursor + delimiter.len_utf8();
  Some((end, source[start + 2..cursor].to_owned()))
}

fn incomplete_character_reference(source: &str, start: usize) -> bool {
  let bytes = source.as_bytes();
  if bytes.get(start..start + 2) != Some(b"&#") {
    return false;
  }
  let next = bytes.get(start + 2).copied();
  match next {
    Some(byte) if byte.is_ascii_digit() => true,
    Some(b'x' | b'X') => bytes.get(start + 3).is_some_and(u8::is_ascii_hexdigit),
    _ => false,
  }
}

fn incomplete_reference(source: &str, start: usize) -> bool {
  source[start..]
    .strip_prefix('&')
    .and_then(|tail| tail.chars().next())
    .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '#')
}

fn find_comment_close(source: &str, start: usize) -> Option<usize> {
  let mut search = start;
  while let Some(relative) = source[search..].find("--") {
    let candidate = search + relative;
    let after_dashes = candidate + 2;
    if source[after_dashes..].starts_with("!>") {
      return Some(after_dashes + 2);
    }
    if source[after_dashes..].starts_with('>') {
      return Some(after_dashes + 1);
    }
    search = candidate + 1;
  }
  None
}

fn previous_char_len(source: &str, end: usize) -> usize {
  source[..end].chars().next_back().expect("regex delimiter exists").len_utf8()
}

fn char_offset_from_end(source: &str, count: usize) -> usize {
  source.char_indices().rev().nth(count - 1).map_or(0, |(offset, _)| offset)
}

fn matching_quote(value: &str) -> bool {
  matches!((value.chars().next(), value.chars().last()), (Some('\''), Some('\'')) | (Some('"'), Some('"')))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn source_positions_store_line_offsets_for_one_megabyte_html() {
    let size = 1024 * 1024;
    let line = format!("{}\n", "x".repeat(99));
    let mut source = line.repeat(size / 100);
    source.push_str(&"x".repeat(size % 100));
    assert_eq!(source.len(), size);

    let positions = SourcePositions::new(&source);
    assert_eq!(positions.line_starts.len(), 10_486);
    assert_eq!(positions.line_starts.capacity(), 10_486);
  }

  #[test]
  fn source_positions_keep_codepoint_columns_and_newline_boundaries() {
    let source = "aé\n𐍈x";
    let mut positions = SourcePositions::new(source);
    assert_eq!(positions.at(source, 1), Position { line: 1, column: 1 });
    assert_eq!(positions.at(source, 3), Position { line: 1, column: 2 });
    assert_eq!(positions.at(source, 4), Position { line: 2, column: 0 });
    assert_eq!(positions.at(source, 8), Position { line: 2, column: 1 });
  }

  #[test]
  fn streamed_events_match_collected_events() {
    let source = "<p>é&amp;x</p>";
    let expected = parse(source, true).expect("collected parsing succeeds");
    let mut streamed = Vec::new();
    parse_with(source, true, |event| streamed.push(event)).expect("streamed parsing succeeds");
    assert_eq!(streamed.len(), expected.len());
    for (streamed, collected) in streamed.iter().zip(expected) {
      assert_eq!(streamed.kind, collected.kind);
      assert_eq!(streamed.args, collected.args);
      assert_eq!(streamed.byte_offset, collected.byte_offset);
      assert_eq!(streamed.starttag_text, collected.starttag_text);
    }
  }
}
