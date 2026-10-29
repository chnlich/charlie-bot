//! `json.loads` as CPython 3.14 runs it, for the bodies and stored texts the Python server parses.
//!
//! A document that `serde_json` reads to the same value as `json.loads` is taken from `serde_json` (see
//! [`loads_serde`]). Every other document goes through the scanner, which follows `_json.c`, which `json.loads` uses,
//! and `json/decoder.py` where the two agree. The scanner's messages and positions are the ones `json.loads` raises.
//! A `JSONDecodeError` renders as `<message>: line L column C (char P)`, and a body that is not UTF-8 gives the
//! `UnicodeDecodeError` text. `json/__init__.py`'s `detect_encoding` picks the codec of a body, so a body with null
//! bytes decodes as UTF-16 or UTF-32, as Python decodes it.
//!
//! Positions count code points, as Python `str` indexes do. A document Python loads but a `serde_json::Value` cannot
//! hold (a non-finite float, an integer beyond 64 bits, a lone surrogate, nesting past [`MAX_DEPTH`]) is parsed to its
//! end. The syntax errors before it therefore win as in Python, and the document is then refused as
//! [`LoadError::Unrepresentable`].

use std::fmt;
use std::sync::LazyLock;

use regex::bytes::Regex;
use serde_json::{Map, Number, Value};

/// The integer forms that serde_json reads as a different value than `json.loads`, so a body holding one goes to the
/// scanner. A run of 19 digits: serde_json reads an integer past 64 bits as a float, where `json.loads` gives an int
/// that [`Value`] cannot hold. A negative one past `i64::MIN` likewise gives an int that [`Value`] cannot hold. An
/// integer `-0`: serde_json reads the float -0.0, where `json.loads` gives the int 0. Other digit runs and the float
/// forms read the same in both.
static INTEGER_FORM_OF_SCANNER: LazyLock<Regex> =
  LazyLock::new(|| Regex::new("[0-9]{19}|-0([^.0-9eE]|$)").expect("a literal pattern compiles"));

/// The nesting depth the scanner accepts. CPython's recursion limit is 1000 frames minus the caller's depth, so the
/// exact depth where Python fails varies; the port refuses past this bound instead of overflowing its stack.
pub const MAX_DEPTH: usize = 500;

/// Why a document did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
  /// Python raises `ValueError` with this text: a `JSONDecodeError` or a `UnicodeDecodeError`.
  Value(String),
  /// Python loads the document, and this port cannot hold it in a `serde_json::Value`. The text names the value.
  Unrepresentable(String),
}

impl fmt::Display for LoadError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      LoadError::Value(text) | LoadError::Unrepresentable(text) => f.write_str(text),
    }
  }
}

impl std::error::Error for LoadError {}

/// The source of a failed `json.loads(body)` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadFailure {
  /// The scanner rejected JSON at a code-point offset.
  JsonDecode { message: String, position: usize, text: String },
  /// Python could not decode the body's detected character encoding.
  Decode(String),
  /// Python loaded a value that this port cannot represent.
  Unrepresentable(String),
}

impl LoadFailure {
  fn into_load_error(self) -> LoadError {
    match self {
      LoadFailure::JsonDecode { text, .. } | LoadFailure::Decode(text) => LoadError::Value(text),
      LoadFailure::Unrepresentable(text) => LoadError::Unrepresentable(text),
    }
  }
}

/// `json.loads(body)` for a body of bytes: the codec `detect_encoding` picks, then the scanner.
pub fn loads(body: &[u8]) -> Result<Value, LoadError> {
  loads_classified(body).map_err(LoadFailure::into_load_error)
}

/// `json.loads(body)` with the distinction FastAPI uses between JSON and body-decoding failures.
///
/// A body serde_json reads needs no codec. A body that `detect_encoding` would decode as UTF-16 or UTF-32 or with a
/// byte-order mark is not valid JSON text. serde_json refuses it and the scanner decodes it.
pub fn loads_classified(body: &[u8]) -> Result<Value, LoadFailure> {
  if let Some(value) = loads_serde(body) {
    return Ok(value);
  }
  let code_points = decode(body).map_err(|error| match error {
    LoadError::Value(text) => LoadFailure::Decode(text),
    LoadError::Unrepresentable(text) => LoadFailure::Unrepresentable(text),
  })?;
  loads_code_points(&code_points)
}

/// `json.loads(text)` for a `str`. Python refuses a leading byte-order mark here, where the bytes path has already
/// removed one.
pub fn loads_text(text: &str) -> Result<Value, LoadError> {
  if text.starts_with('\u{feff}') {
    return Err(LoadError::Value(format_error("Unexpected UTF-8 BOM (decode using utf-8-sig)", &[], 0)));
  }
  if let Some(value) = loads_serde(text.as_bytes()) {
    return Ok(value);
  }
  let code_points: Vec<u32> = text.chars().map(u32::from).collect();
  loads_code_points(&code_points).map_err(LoadFailure::into_load_error)
}

/// The success path: the value serde_json reads from `body`, when it reads one that `json.loads` also reads to the same
/// value. Any other body gives `None`, and the scanner then raises the error `json.loads` raises, so serde_json's own
/// error text is not reported.
fn loads_serde(body: &[u8]) -> Option<Value> {
  if INTEGER_FORM_OF_SCANNER.is_match(body) {
    return None;
  }
  serde_json::from_slice(body).ok()
}

/// A failed scan. `Stop` is `StopIteration(pos)`, which `raw_decode` reports as "Expecting value" at `pos`.
#[derive(Debug)]
enum Fail {
  Stop(usize),
  Decode(&'static str, usize),
  Nested,
}

type Scan<T> = Result<T, Fail>;

const QUOTE: char = '"';
const BACKSLASH: char = '\\';

struct Scanner<'a> {
  text: &'a [u32],
  depth: usize,
  unrepresentable: Option<String>,
}

fn loads_code_points(text: &[u32]) -> Result<Value, LoadFailure> {
  let mut scanner = Scanner { text, depth: 0, unrepresentable: None };
  let start = scanner.skip_ws(0);
  let (value, end) = match scanner.scan_once(start) {
    Ok(found) => found,
    Err(Fail::Stop(pos)) => return Err(json_decode_failure("Expecting value", text, pos)),
    Err(Fail::Decode(message, pos)) => return Err(json_decode_failure(message, text, pos)),
    Err(Fail::Nested) => return Err(nested_refusal()),
  };
  let end = scanner.skip_ws(end);
  if end != text.len() {
    return Err(json_decode_failure("Extra data", text, end));
  }
  match scanner.unrepresentable {
    Some(what) => Err(LoadFailure::Unrepresentable(what)),
    None => Ok(value),
  }
}

fn nested_refusal() -> LoadFailure {
  LoadFailure::Unrepresentable(format!("JSON nested deeper than {MAX_DEPTH} levels"))
}

fn json_decode_failure(message: &str, text: &[u32], position: usize) -> LoadFailure {
  LoadFailure::JsonDecode { message: message.to_string(), position, text: format_error(message, text, position) }
}

/// `JSONDecodeError.__init__`: the message, then the 1-based line and column and the code point index of `pos`.
fn format_error(message: &str, text: &[u32], pos: usize) -> String {
  let before = &text[..pos];
  let line = before.iter().filter(|&&c| c == u32::from('\n')).count() + 1;
  let column = match before.iter().rposition(|&c| c == u32::from('\n')) {
    Some(newline) => pos - newline,
    None => pos + 1,
  };
  format!("{message}: line {line} column {column} (char {pos})")
}

fn hex4(digits: &[u32]) -> Option<u32> {
  digits.iter().try_fold(0u32, |code, &digit| {
    let value = char::from_u32(digit)?.to_digit(16)?;
    Some((code << 4) | value)
  })
}

impl Scanner<'_> {
  fn at(&self, idx: usize) -> Option<char> {
    self.text.get(idx).and_then(|&c| char::from_u32(c))
  }

  /// The code point at `idx`, a surrogate included, which `at` cannot read as a `char`.
  fn code(&self, idx: usize) -> Option<u32> {
    self.text.get(idx).copied()
  }

  fn is(&self, idx: usize, expected: char) -> bool {
    self.at(idx) == Some(expected)
  }

  fn word(&self, idx: usize, word: &str) -> bool {
    word.chars().enumerate().all(|(offset, expected)| self.is(idx + offset, expected))
  }

  fn skip_ws(&self, mut idx: usize) -> usize {
    while matches!(self.at(idx), Some(' ' | '\t' | '\n' | '\r')) {
      idx += 1;
    }
    idx
  }

  fn refuse(&mut self, what: &str) {
    self.unrepresentable.get_or_insert_with(|| format!("JSON value {what} is not representable"));
  }

  /// The `str` a code point sequence makes. A lone surrogate is refused and reads as an empty string, which no
  /// caller keeps once the refusal is set.
  fn text_of(&mut self, code_points: &[u32]) -> String {
    let mut text = String::with_capacity(code_points.len());
    for &code in code_points {
      match char::from_u32(code) {
        Some(c) => text.push(c),
        None => {
          self.refuse(&format!("string with the lone surrogate U+{code:04X}"));
          return String::new();
        }
      }
    }
    text
  }

  fn scan_once(&mut self, idx: usize) -> Scan<(Value, usize)> {
    match self.at(idx) {
      Some(QUOTE) => {
        let (code_points, end) = self.scan_string(idx, idx + 1)?;
        Ok((Value::String(self.text_of(&code_points)), end))
      }
      Some('{') => self.nested(idx + 1, Self::parse_object),
      Some('[') => self.nested(idx + 1, Self::parse_array),
      Some('n') if self.word(idx, "null") => Ok((Value::Null, idx + 4)),
      Some('t') if self.word(idx, "true") => Ok((Value::Bool(true), idx + 4)),
      Some('f') if self.word(idx, "false") => Ok((Value::Bool(false), idx + 5)),
      Some('N') if self.word(idx, "NaN") => {
        self.refuse("NaN");
        Ok((Value::Null, idx + 3))
      }
      Some('I') if self.word(idx, "Infinity") => {
        self.refuse("Infinity");
        Ok((Value::Null, idx + 8))
      }
      Some('-') if self.word(idx, "-Infinity") => {
        self.refuse("-Infinity");
        Ok((Value::Null, idx + 9))
      }
      _ => self.number(idx).ok_or(Fail::Stop(idx)),
    }
  }

  fn nested(&mut self, idx: usize, parse: fn(&mut Self, usize) -> Scan<(Value, usize)>) -> Scan<(Value, usize)> {
    if self.depth == MAX_DEPTH {
      return Err(Fail::Nested);
    }
    self.depth += 1;
    let found = parse(self, idx);
    self.depth -= 1;
    found
  }

  fn digits(&self, mut idx: usize) -> usize {
    while matches!(self.at(idx), Some('0'..='9')) {
      idx += 1;
    }
    idx
  }

  /// `_match_number_unicode`: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`, or `None` when no number starts at
  /// `start`. An exponent without digits is not part of the number.
  fn number(&mut self, start: usize) -> Option<(Value, usize)> {
    let mut idx = start;
    if self.is(idx, '-') {
      idx += 1;
    }
    match self.at(idx) {
      Some('1'..='9') => idx = self.digits(idx),
      Some('0') => idx += 1,
      _ => return None,
    }
    let mut float = false;
    if self.is(idx, '.') && matches!(self.at(idx + 1), Some('0'..='9')) {
      idx = self.digits(idx + 1);
      float = true;
    }
    if self.is(idx, 'e') || self.is(idx, 'E') {
      let mut exponent = idx + 1;
      if self.is(exponent, '+') || self.is(exponent, '-') {
        exponent += 1;
      }
      if matches!(self.at(exponent), Some('0'..='9')) {
        idx = self.digits(exponent);
        float = true;
      }
    }
    let literal: String = self.text[start..idx].iter().filter_map(|&c| char::from_u32(c)).collect();
    let value = if float {
      let parsed: f64 = literal.parse().expect("the scanned float literal parses as f64");
      match Number::from_f64(parsed) {
        Some(number) => Value::Number(number),
        None => {
          self.refuse(&literal);
          Value::Null
        }
      }
    } else if let Ok(integer) = literal.parse::<i64>() {
      Value::from(integer)
    } else if let Ok(integer) = literal.parse::<u64>() {
      Value::from(integer)
    } else {
      self.refuse(&literal);
      Value::Null
    };
    Some((value, idx))
  }

  /// `scanstring_unicode`: the string whose body starts at `end`, the index after its opening quote at `begin`.
  /// Returns the code points and the index after the closing quote.
  fn scan_string(&self, begin: usize, mut end: usize) -> Scan<(Vec<u32>, usize)> {
    let mut out = Vec::new();
    loop {
      let mut next = end;
      while let Some(&c) = self.text.get(next) {
        if c == u32::from(QUOTE) || c == u32::from(BACKSLASH) {
          break;
        }
        if c <= 0x1f {
          return Err(Fail::Decode("Invalid control character at", next));
        }
        next += 1;
      }
      let stop = self.code(next).ok_or(Fail::Decode("Unterminated string starting at", begin))?;
      out.extend_from_slice(&self.text[end..next]);
      if stop == u32::from(QUOTE) {
        return Ok((out, next + 1));
      }
      next += 1;
      let escape = self.code(next).ok_or(Fail::Decode("Unterminated string starting at", begin))?;
      if escape != u32::from('u') {
        end = next + 1;
        let decoded = match char::from_u32(escape) {
          Some(QUOTE) => QUOTE,
          Some(BACKSLASH) => BACKSLASH,
          Some('/') => '/',
          Some('b') => '\u{8}',
          Some('f') => '\u{c}',
          Some('n') => '\n',
          Some('r') => '\r',
          Some('t') => '\t',
          _ => return Err(Fail::Decode("Invalid \\escape", end - 2)),
        };
        out.push(u32::from(decoded));
        continue;
      }
      next += 1;
      end = next + 4;
      if end > self.text.len() {
        return Err(Fail::Decode("Invalid \\uXXXX escape", next - 1));
      }
      let mut code = hex4(&self.text[next..end]).ok_or(Fail::Decode("Invalid \\uXXXX escape", end - 5))?;
      let pair_follows = end + 6 < self.text.len() && self.is(end, BACKSLASH) && self.is(end + 1, 'u');
      if (0xD800..0xDC00).contains(&code) && pair_follows {
        let low = hex4(&self.text[end + 2..end + 6]).ok_or(Fail::Decode("Invalid \\uXXXX escape", end + 1))?;
        if (0xDC00..0xE000).contains(&low) {
          code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
          end += 6;
        }
      }
      out.push(code);
    }
  }

  /// `_parse_object_unicode`, from the index after `{`.
  fn parse_object(&mut self, mut idx: usize) -> Scan<(Value, usize)> {
    let mut members = Map::new();
    idx = self.skip_ws(idx);
    if !self.is(idx, '}') {
      loop {
        if !self.is(idx, QUOTE) {
          return Err(Fail::Decode("Expecting property name enclosed in double quotes", idx));
        }
        let (key, after_key) = self.scan_string(idx, idx + 1)?;
        idx = self.skip_ws(after_key);
        if !self.is(idx, ':') {
          return Err(Fail::Decode("Expecting ':' delimiter", idx));
        }
        idx = self.skip_ws(idx + 1);
        let (value, after_value) = self.scan_once(idx)?;
        members.insert(self.text_of(&key), value);
        idx = self.skip_ws(after_value);
        if self.is(idx, '}') {
          break;
        }
        if !self.is(idx, ',') {
          return Err(Fail::Decode("Expecting ',' delimiter", idx));
        }
        let comma = idx;
        idx = self.skip_ws(idx + 1);
        if self.is(idx, '}') {
          return Err(Fail::Decode("Illegal trailing comma before end of object", comma));
        }
      }
    }
    Ok((Value::Object(members), idx + 1))
  }

  /// `_parse_array_unicode`, from the index after `[`.
  fn parse_array(&mut self, mut idx: usize) -> Scan<(Value, usize)> {
    let mut items = Vec::new();
    idx = self.skip_ws(idx);
    if !self.is(idx, ']') {
      loop {
        let (item, after_item) = self.scan_once(idx)?;
        items.push(item);
        idx = self.skip_ws(after_item);
        if self.is(idx, ']') {
          break;
        }
        if !self.is(idx, ',') {
          return Err(Fail::Decode("Expecting ',' delimiter", idx));
        }
        let comma = idx;
        idx = self.skip_ws(idx + 1);
        if self.is(idx, ']') {
          return Err(Fail::Decode("Illegal trailing comma before end of array", comma));
        }
      }
    }
    Ok((Value::Array(items), idx + 1))
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endian {
  Little,
  Big,
}

/// The codec `json/__init__.py`'s `detect_encoding` names, with the BOM the codec consumes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
  Utf8 { skip: usize },
  Utf16 { endian: Endian, skip: usize },
  Utf32 { endian: Endian, skip: usize },
}

fn detect_codec(body: &[u8]) -> Codec {
  if body.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
    return Codec::Utf32 { endian: Endian::Big, skip: 4 };
  }
  if body.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
    return Codec::Utf32 { endian: Endian::Little, skip: 4 };
  }
  if body.starts_with(&[0xFE, 0xFF]) {
    return Codec::Utf16 { endian: Endian::Big, skip: 2 };
  }
  if body.starts_with(&[0xFF, 0xFE]) {
    return Codec::Utf16 { endian: Endian::Little, skip: 2 };
  }
  if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
    return Codec::Utf8 { skip: 3 };
  }
  if body.len() >= 4 {
    if body[0] == 0 {
      return if body[1] != 0 {
        Codec::Utf16 { endian: Endian::Big, skip: 0 }
      } else {
        Codec::Utf32 { endian: Endian::Big, skip: 0 }
      };
    }
    if body[1] == 0 {
      return if body[2] != 0 || body[3] != 0 {
        Codec::Utf16 { endian: Endian::Little, skip: 0 }
      } else {
        Codec::Utf32 { endian: Endian::Little, skip: 0 }
      };
    }
  } else if body.len() == 2 {
    if body[0] == 0 {
      return Codec::Utf16 { endian: Endian::Big, skip: 0 };
    }
    if body[1] == 0 {
      return Codec::Utf16 { endian: Endian::Little, skip: 0 };
    }
  }
  Codec::Utf8 { skip: 0 }
}

/// `body.decode(detect_encoding(body), 'surrogatepass')`: the code points of the document.
fn decode(body: &[u8]) -> Result<Vec<u32>, LoadError> {
  match detect_codec(body) {
    Codec::Utf8 { skip } => decode_utf8(body, skip),
    Codec::Utf16 { endian, skip } => decode_utf16(body, skip, endian),
    Codec::Utf32 { endian, skip } => decode_utf32(body, skip, endian),
  }
}

/// `UnicodeDecodeError.__str__`: a one-byte range names the byte, a longer range names `start-end` (end inclusive).
fn codec_error(codec: &str, body: &[u8], start: usize, end: usize, reason: &str) -> LoadError {
  let text = if end == start + 1 {
    format!("'{codec}' codec can't decode byte 0x{:02x} in position {start}: {reason}", body[start])
  } else {
    format!("'{codec}' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
  };
  LoadError::Value(text)
}

/// UTF-8 as CPython's decoder reads it under `surrogatepass`: a surrogate encoded in three bytes is a code point.
fn decode_utf8(body: &[u8], skip: usize) -> Result<Vec<u32>, LoadError> {
  let mut out = Vec::with_capacity(body.len());
  let mut i = skip;
  while i < body.len() {
    let lead = body[i];
    if lead < 0x80 {
      out.push(u32::from(lead));
      i += 1;
      continue;
    }
    let (width, low, high) = match lead {
      0xC2..=0xDF => (2, 0x80, 0xBF),
      0xE0 => (3, 0xA0, 0xBF),
      0xE1..=0xEF => (3, 0x80, 0xBF),
      0xF0 => (4, 0x90, 0xBF),
      0xF1..=0xF3 => (4, 0x80, 0xBF),
      0xF4 => (4, 0x80, 0x8F),
      _ => return Err(codec_error("utf-8", body, i, i + 1, "invalid start byte")),
    };
    let mut code = u32::from(lead) & (0x7F >> width);
    for offset in 1..width {
      let position = i + offset;
      let Some(&byte) = body.get(position) else {
        return Err(codec_error("utf-8", body, i, position, "unexpected end of data"));
      };
      let (lower, upper) = if offset == 1 { (low, high) } else { (0x80, 0xBF) };
      if !(lower..=upper).contains(&byte) {
        return Err(codec_error("utf-8", body, i, position, "invalid continuation byte"));
      }
      code = (code << 6) | u32::from(byte & 0x3F);
    }
    out.push(code);
    i += width;
  }
  Ok(out)
}

fn unit_at(body: &[u8], at: usize, endian: Endian) -> u32 {
  let pair = [body[at], body[at + 1]];
  u32::from(match endian {
    Endian::Little => u16::from_le_bytes(pair),
    Endian::Big => u16::from_be_bytes(pair),
  })
}

/// UTF-16 under `surrogatepass`: a surrogate pair joins into one code point, and a lone surrogate stands alone.
fn decode_utf16(body: &[u8], skip: usize, endian: Endian) -> Result<Vec<u32>, LoadError> {
  let codec = match endian {
    Endian::Little => "utf-16-le",
    Endian::Big => "utf-16-be",
  };
  if (body.len() - skip) % 2 == 1 {
    return Err(codec_error(codec, body, body.len() - 1, body.len(), "truncated data"));
  }
  let units: Vec<u32> = (skip..body.len()).step_by(2).map(|at| unit_at(body, at, endian)).collect();
  let mut out = Vec::with_capacity(units.len());
  let mut i = 0;
  while i < units.len() {
    let unit = units[i];
    match units.get(i + 1) {
      Some(&low) if (0xD800..0xDC00).contains(&unit) && (0xDC00..0xE000).contains(&low) => {
        out.push(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00));
        i += 2;
      }
      _ => {
        out.push(unit);
        i += 1;
      }
    }
  }
  Ok(out)
}

/// UTF-32 under `surrogatepass`: each four bytes are one code point, which must not pass U+10FFFF.
fn decode_utf32(body: &[u8], skip: usize, endian: Endian) -> Result<Vec<u32>, LoadError> {
  let codec = match endian {
    Endian::Little => "utf-32-le",
    Endian::Big => "utf-32-be",
  };
  let remainder = (body.len() - skip) % 4;
  if remainder != 0 {
    return Err(codec_error(codec, body, body.len() - remainder, body.len(), "truncated data"));
  }
  let mut out = Vec::with_capacity((body.len() - skip) / 4);
  for at in (skip..body.len()).step_by(4) {
    let quad = [body[at], body[at + 1], body[at + 2], body[at + 3]];
    let code = match endian {
      Endian::Little => u32::from_le_bytes(quad),
      Endian::Big => u32::from_be_bytes(quad),
    };
    if code > 0x10FFFF {
      return Err(codec_error(codec, body, at, at + 4, "code point not in range(0x110000)"));
    }
    out.push(code);
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn message(body: &[u8]) -> String {
    match loads(body) {
      Err(LoadError::Value(text)) => text,
      other => panic!("expected a ValueError text, got {other:?}"),
    }
  }

  #[test]
  fn decode_errors_match_the_scanner_positions() {
    assert_eq!(message(b""), "Expecting value: line 1 column 1 (char 0)");
    assert_eq!(message(b"[1,]"), "Illegal trailing comma before end of array: line 1 column 3 (char 2)");
    assert_eq!(message(b"\"\\u12\""), "Invalid \\uXXXX escape: line 1 column 3 (char 2)");
    assert_eq!(message(b"\"\\u0041"), "Unterminated string starting at: line 1 column 1 (char 0)");
    assert_eq!(message(b"[1,\n2,\n]"), "Illegal trailing comma before end of array: line 2 column 2 (char 5)");
  }

  #[test]
  fn utf8_errors_name_the_range_as_unicodedecodeerror_does() {
    assert_eq!(message(b"\xff"), "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
    assert_eq!(message(b"\"\xe2\x82\""), "'utf-8' codec can't decode bytes in position 1-2: invalid continuation byte");
    assert_eq!(message(b"\xe2\x82"), "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data");
  }

  #[test]
  fn a_document_python_loads_but_serde_cannot_hold_is_refused() {
    assert!(matches!(loads(b"NaN"), Err(LoadError::Unrepresentable(_))));
    assert!(matches!(loads(b"1e400"), Err(LoadError::Unrepresentable(_))));
    assert!(matches!(loads(b"\"\\ud800\""), Err(LoadError::Unrepresentable(_))));
    assert_eq!(message(b"[NaN, }"), "Expecting value: line 1 column 7 (char 6)");
  }

  #[test]
  fn text_refuses_a_leading_byte_order_mark_only() {
    assert_eq!(
      loads_text("\u{feff}{}").unwrap_err().to_string(),
      "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)"
    );
    assert_eq!(loads(b"\xef\xbb\xbf{}").unwrap(), serde_json::json!({}));
  }

  #[test]
  fn null_bytes_select_the_wide_codecs_as_detect_encoding_does() {
    assert_eq!(message(b"\x00{"), "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)");
    assert_eq!(loads(b"\xff\xfe{\x00}\x00").unwrap(), serde_json::json!({}));
    assert_eq!(message(b"{\x00\x00\x00}"), "'utf-32-le' codec can't decode byte 0x7d in position 4: truncated data");
  }

  #[test]
  fn integers_keep_their_64_bit_kind_and_refuse_wider_ones() {
    assert_eq!(loads(b"18446744073709551615").unwrap(), serde_json::json!(18446744073709551615u64));
    assert_eq!(loads(b"-9223372036854775808").unwrap(), serde_json::json!(-9223372036854775808i64));
    assert!(matches!(loads(b"18446744073709551616"), Err(LoadError::Unrepresentable(_))));
    assert!(matches!(loads(b"-9223372036854775809"), Err(LoadError::Unrepresentable(_))));
  }

  #[test]
  fn an_integer_negative_zero_is_the_int_zero_and_a_float_stays_negative() {
    assert_eq!(loads(b"[-0,-0.0,0]").unwrap().to_string(), "[0,-0.0,0]");
  }

  #[test]
  fn a_duplicate_key_keeps_the_first_position_and_the_last_value() {
    assert_eq!(loads(b"{\"b\":1,\"a\":2,\"b\":3}").unwrap().to_string(), "{\"b\":3,\"a\":2}");
  }

  #[test]
  fn nesting_is_accepted_to_the_depth_bound_and_refused_past_it() {
    let at_bound = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    assert!(loads(at_bound.as_bytes()).is_ok());
    let past_bound = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
    assert!(matches!(loads(past_bound.as_bytes()), Err(LoadError::Unrepresentable(_))));
  }
}
