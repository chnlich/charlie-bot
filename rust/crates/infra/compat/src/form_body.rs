//! FastAPI's form body parsing and `File`/`Form` validation.
//!
//! `read` runs before route dependencies. A multipart body is parsed by a port of python-multipart's state machine, so
//! that every truncated or malformed body reaches the same error as Starlette does. File parts stream into temporary
//! files. A urlencoded body is parsed the way Starlette's `FormParser` does. `FormBody::validate` runs after
//! dependencies and reports required or mistyped fields at `body.<field>`, like `request_body::RequestBody`.

use crate::responses;
use crate::validation::{self, FieldError, Kind, Loc};
use axum::body::Body;
use axum::response::Response;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};
use std::io::Write;
use tempfile::{NamedTempFile, TempPath};

const MAX_FILES: usize = 1000;
const MAX_FIELDS: usize = 1000;
const MAX_PART_SIZE: usize = 1024 * 1024;

/// A multipart field declared by a FastAPI endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
  File,
  Form,
}

/// One required or optional field in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
  name: &'static str,
  field_type: FieldType,
  required: bool,
}

impl Field {
  /// Declare a required `File` field.
  pub const fn file(name: &'static str) -> Self {
    Self { name, field_type: FieldType::File, required: true }
  }

  /// Declare a required string `Form` field.
  pub const fn form(name: &'static str) -> Self {
    Self { name, field_type: FieldType::Form, required: true }
  }

  /// Declare an optional string `Form` field.
  pub const fn optional_form(name: &'static str) -> Self {
    Self { name, field_type: FieldType::Form, required: false }
  }
}

/// A multipart body model whose fields are checked after route dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormModel {
  fields: &'static [Field],
}

impl FormModel {
  /// Declare the fields accepted by a route, in FastAPI parameter order.
  pub const fn new(fields: &'static [Field]) -> Self {
    Self { fields }
  }
}

/// A parsed file part, held in a temporary file until the request finishes.
pub struct UploadedFile {
  filename: Option<String>,
  path: TempPath,
  size: u64,
}

impl UploadedFile {
  /// The name supplied by the multipart client, including any directory components.
  pub fn filename(&self) -> Option<&str> {
    self.filename.as_deref()
  }

  /// The temporary file containing the streamed part.
  pub fn path(&self) -> &std::path::Path {
    &self.path
  }

  /// The number of body bytes written into the temporary file.
  pub fn size(&self) -> u64 {
    self.size
  }
}

/// One parsed part: a streamed file or a decoded text field.
struct Part {
  name: String,
  value: PartValue,
}

enum PartValue {
  File(UploadedFile),
  Form(String),
}

/// A parsed form, with file parts held in temporary files.
#[derive(Default)]
pub struct FormBody {
  parts: Vec<Part>,
}

impl FormBody {
  /// Validate this form in field declaration order.
  pub fn validate(&self, model: &FormModel) -> Result<(), Box<Response>> {
    let mut errors = Vec::new();
    for field in model.fields {
      // FastAPI's `_get_multidict_value` reads an empty string as an absent value for every `Form` field, and `File` is
      // a `Form` field.
      let value = self.value(field.name).filter(|value| !matches!(value, PartValue::Form(text) if text.is_empty()));
      match (field.field_type, value) {
        (_, None) if field.required => errors.push(field_error(field.name, Value::Null, Kind::Missing)),
        (FieldType::File, Some(PartValue::File(_))) | (FieldType::Form, Some(PartValue::Form(_))) => {}
        (FieldType::File, Some(PartValue::Form(value))) => errors.push(field_error(
          field.name,
          Value::String(value.clone()),
          Kind::ValueError { message: "Expected UploadFile, received: <class 'str'>".to_string() },
        )),
        (FieldType::Form, Some(PartValue::File(_))) => {
          errors.push(field_error(field.name, json!({}), Kind::StringType));
        }
        (_, None) => {}
      }
    }
    if errors.is_empty() { Ok(()) } else { Err(Box::new(validation::response(&errors))) }
  }

  /// Return the last file value for `name`, as Starlette's `FormData.get` does.
  pub fn file(&self, name: &str) -> Option<&UploadedFile> {
    match self.value(name)? {
      PartValue::File(file) => Some(file),
      PartValue::Form(_) => None,
    }
  }

  /// Return the last text value for `name`, as Starlette's `FormData.get` does.
  pub fn form(&self, name: &str) -> Option<&str> {
    match self.value(name)? {
      PartValue::File(_) => None,
      PartValue::Form(value) => Some(value),
    }
  }

  fn value(&self, name: &str) -> Option<&PartValue> {
    self.parts.iter().rev().find(|part| part.name == name).map(|part| &part.value)
  }
}

/// Read a form request before dependencies, returning an empty form for other content types.
pub async fn read(headers: &HeaderMap, body: Body) -> Result<FormBody, Box<Response>> {
  let Some(content_type) = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) else {
    return Ok(FormBody::default());
  };
  let (media_type, params) = parse_options_header(content_type);
  if media_type == "multipart/form-data" {
    let boundary = last_param(&params, "boundary").ok_or_else(|| starlette_error("Missing boundary in multipart."))?;
    let charset = last_param(&params, "charset").unwrap_or_else(|| "utf-8".to_string());
    read_multipart(&boundary, &charset, body).await
  } else if media_type == "application/x-www-form-urlencoded" {
    read_urlencoded(body).await
  } else {
    Ok(FormBody::default())
  }
}

/// Read a urlencoded body the way Starlette's `FormParser` does: each name and value is latin-1 decoded, then
/// percent-decoded with `urllib.parse.unquote_plus`.
async fn read_urlencoded(body: Body) -> Result<FormBody, Box<Response>> {
  let mut bytes = Vec::new();
  let mut stream = body.into_data_stream();
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|_| multipart_error())?;
    bytes.extend_from_slice(&chunk);
  }
  let mut form = FormBody::default();
  for (name, value) in querystring_fields(&bytes) {
    form.parts.push(Part { name: unquote_plus(name), value: PartValue::Form(unquote_plus(value)) });
  }
  Ok(form)
}

/// Split a urlencoded body into raw name and value pairs the way python-multipart's `QuerystringParser` does, which
/// Starlette's `FormParser` uses. Both `&` and `;` separate fields, and repeated separators are skipped. A name ends at
/// the first `=` before the next separator. A value ends at the next separator, where `&` is looked for before `;`. A
/// bare name is kept only when a separator follows it, because python-multipart finishes a field at the end of the
/// body only once its value has started.
fn querystring_fields(data: &[u8]) -> Vec<(&[u8], &[u8])> {
  let ampersands = positions_of(data, b'&');
  let semicolons = positions_of(data, b';');
  let separator_from = |from: usize| next_position(&ampersands, from).or_else(|| next_position(&semicolons, from));
  let mut fields = Vec::new();
  let mut index = 0;
  while index < data.len() {
    if matches!(data[index], b'&' | b';') {
      index += 1;
      continue;
    }
    let separator = separator_from(index);
    let name_end = separator.unwrap_or(data.len());
    match data[index..name_end].iter().position(|byte| *byte == b'=').map(|offset| index + offset) {
      Some(equals) => {
        let value_end = separator_from(equals + 1).unwrap_or(data.len());
        fields.push((&data[index..equals], &data[equals + 1..value_end]));
        index = value_end;
      }
      None => {
        let Some(end) = separator else { break };
        fields.push((&data[index..end], &data[end..end]));
        index = end;
      }
    }
  }
  fields
}

fn positions_of(data: &[u8], byte: u8) -> Vec<usize> {
  data.iter().enumerate().filter(|(_, candidate)| **candidate == byte).map(|(index, _)| index).collect()
}

/// The first position at or after `from` in the ascending `positions`.
fn next_position(positions: &[usize], from: usize) -> Option<usize> {
  positions.get(positions.partition_point(|position| *position < from)).copied()
}

/// Decode a name or value the way Starlette's `FormParser` does: each byte is a latin-1 character, and the characters
/// are percent-decoded with `urllib.parse.unquote_plus`. Escapes in a run of ASCII characters decode as UTF-8 with
/// replacement characters, and every other character stays as it is.
fn unquote_plus(bytes: &[u8]) -> String {
  let mut text = String::with_capacity(bytes.len());
  let mut ascii_run = Vec::new();
  for &byte in bytes {
    if byte.is_ascii() {
      ascii_run.push(byte);
    } else {
      text.push_str(&unquote_ascii(&ascii_run));
      ascii_run.clear();
      text.push(char::from(byte));
    }
  }
  text.push_str(&unquote_ascii(&ascii_run));
  text
}

fn unquote_ascii(run: &[u8]) -> String {
  let mut decoded = Vec::with_capacity(run.len());
  let mut index = 0;
  while index < run.len() {
    match run[index] {
      b'+' => decoded.push(b' '),
      b'%' => match run.get(index + 1..index + 3).and_then(hex_pair) {
        Some(byte) => {
          decoded.push(byte);
          index += 2;
        }
        None => decoded.push(b'%'),
      },
      byte => decoded.push(byte),
    }
    index += 1;
  }
  String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
  let high = char::from(pair[0]).to_digit(16)?;
  let low = char::from(pair[1]).to_digit(16)?;
  Some((high * 16 + low) as u8)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(data: &[u8]) -> Vec<(String, String)> {
    querystring_fields(data).into_iter().map(|(name, value)| (unquote_plus(name), unquote_plus(value))).collect()
  }

  fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
  }

  #[test]
  fn querystring_splits_fields_as_python_multipart_does() {
    assert_eq!(fields(b"a;b=1&c=2"), [pair("a;b", "1"), pair("c", "2")]);
    assert_eq!(fields(b"a=x;y&b=2"), [pair("a", "x;y"), pair("b", "2")]);
    assert_eq!(fields(b"a=x;y"), [pair("a", "x")]);
    assert_eq!(fields(b"a;b"), [pair("a", "")]);
    assert_eq!(fields(b"a&b&c=3"), [pair("a", ""), pair("b", ""), pair("c", "3")]);
    assert_eq!(fields(b"a==b"), [pair("a", "=b")]);
    assert_eq!(fields(b"x=1;;y=2"), [pair("x", "1"), pair("y", "2")]);
    assert_eq!(fields(b"=x&&note=2"), [pair("", "x"), pair("note", "2")]);
    assert_eq!(fields(b"&&"), []);
  }

  #[test]
  fn unquote_plus_decodes_latin_1_bytes_as_starlette_does() {
    assert_eq!(unquote_plus(b"caf\xc3\xa9"), "caf\u{c3}\u{a9}");
    assert_eq!(unquote_plus(b"caf%C3%A9"), "caf\u{e9}");
    assert_eq!(unquote_plus(b"%C3\xa9"), "\u{fffd}\u{a9}");
    assert_eq!(unquote_plus(b"a+b%2Bc"), "a b+c");
    assert_eq!(unquote_plus(b"%zz%4"), "%zz%4");
  }

  #[test]
  fn header_options_split_outside_quotes_as_python_multipart_does() {
    let (media_type, params) = parse_options_header(r#"form-data; name="a;b"; filename="x.txt""#);
    assert_eq!(media_type, "form-data");
    assert_eq!(params, [pair("name", "a;b"), pair("filename", "x.txt")]);
    let (media_type, params) = parse_options_header(r#"form-data; name="a\";b"; x=1"#);
    assert_eq!(media_type, "form-data");
    assert_eq!(params, [pair("name", "a\";b"), pair("x", "1")]);
    assert_eq!(parse_options_header("Form-Data; name=q").0, "Form-Data");
    assert_eq!(parse_options_header("Form-Data").0, "form-data");
    assert_eq!(parse_options_header("form-data; name").1, [pair("name", "")]);
    assert_eq!(parse_options_header("form-data; name=<x>").1, [pair("name", "x")]);
    assert_eq!(parse_options_header("form-data; name=\"\\\\\"\"").1, [pair("name", "\"")]);
    assert_eq!(parse_options_header(r#"form-data; filename="C:\dir\a.txt""#).1, [pair("filename", "a.txt")]);
  }

  #[test]
  fn user_safe_decode_normalizes_codec_names_and_falls_back_to_latin_1() {
    assert_eq!(user_safe_decode("caf\u{e9}".as_bytes(), "UTF-8"), "caf\u{e9}");
    assert_eq!(user_safe_decode("caf\u{e9}".as_bytes(), "utf_8"), "caf\u{e9}");
    assert_eq!(user_safe_decode(b"caf\xe9", "latin-1"), "caf\u{e9}");
    assert_eq!(user_safe_decode(b"\xff", "utf-8"), "\u{ff}");
  }
}

/// Decode a string with Starlette's `_user_safe_decode`: an unknown codec or an invalid sequence falls back to
/// latin-1, which never fails. Only the codecs that need no table are known here, so any other charset also falls back
/// to latin-1.
fn user_safe_decode(bytes: &[u8], charset: &str) -> String {
  let decoded = match normalize_codec_name(charset).as_str() {
    "utf_8" | "utf8" | "u8" | "utf" | "cp65001" => std::str::from_utf8(bytes).ok().map(str::to_string),
    "ascii" | "us_ascii" | "646" | "cp367" => bytes.is_ascii().then(|| String::from_utf8_lossy(bytes).into_owned()),
    "latin_1" | "latin1" | "iso8859_1" | "iso_8859_1" | "iso_8859_1_1987" | "iso_ir_100" | "l1" | "latin" | "cp819"
    | "8859" => Some(latin1(bytes)),
    _ => None,
  };
  decoded.unwrap_or_else(|| latin1(bytes))
}

/// Normalize a codec name the way Python's `encodings.normalize_encoding` does before its alias lookup. Letters and
/// digits and dots are kept in lower case, and each run of other characters becomes one underscore between names.
fn normalize_codec_name(name: &str) -> String {
  let mut normalized = String::new();
  let mut separated = false;
  for character in name.chars() {
    if character.is_alphanumeric() || character == '.' {
      if separated && !normalized.is_empty() {
        normalized.push('_');
      }
      if character.is_ascii() {
        normalized.push(character.to_ascii_lowercase());
      }
      separated = false;
    } else {
      separated = true;
    }
  }
  normalized
}

fn latin1(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn latin1_bytes(value: &str) -> Vec<u8> {
  value.chars().map(|character| character as u32 as u8).collect()
}

/// Parse a header the way python-multipart's `parse_options_header` does through `email.message`: a value without a
/// semicolon is lowered, and otherwise the first segment is the media type as written. A parameter with a value has
/// its name lowered, and a later parameter with the same name wins.
fn parse_options_header(value: &str) -> (String, Vec<(String, String)>) {
  if !value.contains(';') {
    return (value.trim().to_lowercase(), Vec::new());
  }
  let mut segments = split_parameters(value).into_iter().map(|segment| match segment.split_once('=') {
    Some((raw_key, raw_value)) => (raw_key.trim().to_lowercase(), unquote_header_value(raw_value.trim())),
    None => (segment.trim().to_string(), String::new()),
  });
  let media_type = segments.next().map(|(key, _)| key).unwrap_or_default();
  let mut params: Vec<(String, String)> = Vec::new();
  for (key, value) in segments {
    let mut value = value;
    // python-multipart fixes the IE6 bug of sending the full file path as the filename.
    if key == "filename"
      && (value.chars().nth(1) == Some(':') && value.chars().nth(2) == Some('\\') || value.starts_with("\\\\"))
    {
      value = value.rsplit('\\').next().unwrap_or_default().to_string();
    }
    params.retain(|(name, _)| *name != key);
    params.push((key, value));
  }
  (media_type, params)
}

/// Split a header value at the semicolons outside quoted strings. python-multipart's `_parseparam` takes a semicolon as
/// a separator when the quotes before it, not counting escaped quotes, are balanced.
fn split_parameters(value: &str) -> Vec<&str> {
  let mut segments = Vec::new();
  let mut start = 0;
  let mut in_quotes = false;
  let mut previous = None;
  for (index, character) in value.char_indices() {
    match character {
      '"' if previous != Some('\\') => in_quotes = !in_quotes,
      ';' if !in_quotes => {
        segments.push(&value[start..index]);
        start = index + 1;
      }
      _ => {}
    }
    previous = Some(character);
  }
  segments.push(&value[start..]);
  segments
}

/// Unquote a header value the way `email.utils.unquote` does. A quoted value loses its quotes, with `\\` read as `\`
/// before `\"` is read as `"`. An angle-bracketed value loses its brackets.
fn unquote_header_value(value: &str) -> String {
  if value.len() > 1 && value.starts_with('"') && value.ends_with('"') {
    return value[1..value.len() - 1].replace("\\\\", "\\").replace("\\\"", "\"");
  }
  if value.len() > 1 && value.starts_with('<') && value.ends_with('>') {
    return value[1..value.len() - 1].to_string();
  }
  value.to_string()
}

/// Return the value of the last parameter named `key`, the way a Python dict of parameters does.
fn last_param(params: &[(String, String)], key: &str) -> Option<String> {
  params.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.clone())
}

fn field_error(name: &str, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: vec![Loc::Field("body".to_string()), Loc::Field(name.to_string())], input, kind }
}

/// A Starlette `MultiPartException` becomes a 400 whose detail is the message.
fn starlette_error(message: &str) -> Box<Response> {
  multipart_error_with(message)
}

/// A python-multipart `MultipartParseError` becomes FastAPI's catch-all body parsing error.
fn multipart_error() -> Box<Response> {
  multipart_error_with("There was an error parsing the body")
}

fn multipart_error_with(detail: &str) -> Box<Response> {
  Box::new(responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": detail })))
}

/// The states of python-multipart's `MultipartParser`, ported byte for byte so that every malformed body reaches the
/// same error and every truncated body leaves the same parts behind.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MultipartState {
  Start,
  StartBoundary,
  HeaderFieldStart,
  HeaderField,
  HeaderValueStart,
  HeaderValue,
  HeaderValueAlmostDone,
  HeadersAlmostDone,
  PartDataStart,
  PartData,
  PartBoundaryMatched,
  PartBoundaryCr,
  PartBoundaryHyphen,
  OpeningHyphen,
  End,
}

/// Where the bytes of the part being read go.
enum Sink {
  None,
  Field(Vec<u8>),
  File { filename: Option<String>, writer: std::io::BufWriter<std::fs::File>, path: TempPath, size: u64 },
}

struct MultipartMachine {
  opening: Vec<u8>,
  boundary: Vec<u8>,
  charset: String,
  state: MultipartState,
  opening_index: usize,
  match_len: usize,
  header_name: Vec<u8>,
  header_value: Vec<u8>,
  content_disposition: Option<Vec<u8>>,
  part_name: Option<String>,
  sink: Sink,
  file_count: usize,
  field_count: usize,
  parts: Vec<Part>,
}

impl MultipartMachine {
  fn new(boundary: &str, charset: &str) -> Self {
    Self {
      opening: format!("--{boundary}").into_bytes(),
      boundary: format!("\r\n--{boundary}").into_bytes(),
      charset: charset.to_string(),
      state: MultipartState::Start,
      opening_index: 0,
      match_len: 0,
      header_name: Vec::new(),
      header_value: Vec::new(),
      content_disposition: None,
      part_name: None,
      sink: Sink::None,
      file_count: 0,
      field_count: 0,
      parts: Vec::new(),
    }
  }

  fn step(&mut self, byte: u8) -> Result<(), Box<Response>> {
    match self.state {
      MultipartState::Start => {
        if byte == b'\r' || byte == b'\n' {
          return Ok(());
        }
        self.state = MultipartState::StartBoundary;
        self.step(byte)
      }
      MultipartState::StartBoundary => {
        // `opening_index` counts comparison steps against `boundary[index + 2]`, so the name ends at `opening.len()`
        // and the CRLF that must follow it sits at `opening.len()` and `opening.len() + 1`.
        if self.opening_index == self.opening.len() {
          if byte == b'-' {
            self.opening_index += 1;
            self.state = MultipartState::OpeningHyphen;
          } else if byte != b'\r' {
            return Err(multipart_error());
          } else {
            self.opening_index += 1;
          }
        } else if self.opening_index == self.opening.len() + 1 {
          if byte != b'\n' {
            return Err(multipart_error());
          }
          self.opening_index = 0;
          self.begin_part();
          self.state = MultipartState::HeaderFieldStart;
        } else if byte != self.opening[self.opening_index] {
          return Err(multipart_error());
        } else {
          self.opening_index += 1;
        }
        Ok(())
      }
      MultipartState::OpeningHyphen => {
        if byte != b'-' {
          return Err(multipart_error());
        }
        self.state = MultipartState::End;
        Ok(())
      }
      MultipartState::HeaderFieldStart => {
        self.header_name.clear();
        self.state = MultipartState::HeaderField;
        self.step(byte)
      }
      MultipartState::HeaderField => {
        if byte == b'\r' && self.header_name.is_empty() {
          self.state = MultipartState::HeadersAlmostDone;
          return Ok(());
        }
        if byte == b':' {
          if self.header_name.is_empty() {
            return Err(multipart_error());
          }
          self.state = MultipartState::HeaderValueStart;
          return Ok(());
        }
        if !is_token_byte(byte) {
          return Err(multipart_error());
        }
        self.header_name.push(byte);
        Ok(())
      }
      MultipartState::HeaderValueStart => {
        if byte == b' ' {
          return Ok(());
        }
        self.header_value.clear();
        self.state = MultipartState::HeaderValue;
        self.step(byte)
      }
      MultipartState::HeaderValue => {
        if byte == b'\r' {
          self.header_end();
          self.state = MultipartState::HeaderValueAlmostDone;
          return Ok(());
        }
        self.header_value.push(byte);
        Ok(())
      }
      MultipartState::HeaderValueAlmostDone => {
        if byte != b'\n' {
          return Err(multipart_error());
        }
        self.state = MultipartState::HeaderFieldStart;
        Ok(())
      }
      MultipartState::HeadersAlmostDone => {
        if byte != b'\n' {
          return Err(multipart_error());
        }
        self.headers_finished()?;
        self.state = MultipartState::PartDataStart;
        Ok(())
      }
      MultipartState::PartDataStart => {
        self.state = MultipartState::PartData;
        self.step(byte)
      }
      MultipartState::PartData => self.part_data_byte(byte),
      MultipartState::PartBoundaryMatched => {
        if byte == b'\r' {
          self.state = MultipartState::PartBoundaryCr;
        } else if byte == b'-' {
          self.state = MultipartState::PartBoundaryHyphen;
        } else {
          self.discard_boundary_match()?;
          self.state = MultipartState::PartData;
          self.part_data_byte(byte)?;
        }
        Ok(())
      }
      MultipartState::PartBoundaryCr => {
        if byte == b'\n' {
          self.end_part()?;
          self.begin_part();
          self.match_len = 0;
          self.state = MultipartState::HeaderFieldStart;
        } else {
          self.match_len = self.boundary.len();
          self.discard_boundary_match()?;
          self.push_part_byte(b'\r')?;
          self.state = MultipartState::PartData;
          self.part_data_byte(byte)?;
        }
        Ok(())
      }
      MultipartState::PartBoundaryHyphen => {
        if byte == b'-' {
          self.end_part()?;
          self.match_len = 0;
          self.state = MultipartState::End;
        } else {
          self.match_len = self.boundary.len();
          self.discard_boundary_match()?;
          self.push_part_byte(b'-')?;
          self.state = MultipartState::PartData;
          self.part_data_byte(byte)?;
        }
        Ok(())
      }
      MultipartState::End => Ok(()),
    }
  }

  /// Match part data against the boundary, byte for byte, the way python-multipart does. A byte that starts a
  /// potential boundary is withheld in `match_len` until the match fails or a part boundary confirms.
  fn part_data_byte(&mut self, byte: u8) -> Result<(), Box<Response>> {
    if self.match_len == 0 {
      if byte == self.boundary[0] {
        self.match_len = 1;
      } else {
        self.push_part_byte(byte)?;
      }
      return Ok(());
    }
    if byte == self.boundary[self.match_len] {
      self.match_len += 1;
      if self.match_len == self.boundary.len() {
        self.state = MultipartState::PartBoundaryMatched;
      }
      return Ok(());
    }
    self.discard_boundary_match()?;
    self.part_data_byte(byte)
  }

  /// Turn the withheld bytes of a failed boundary match back into part data, then restart the match.
  fn discard_boundary_match(&mut self) -> Result<(), Box<Response>> {
    let matched = self.match_len;
    self.match_len = 0;
    for index in 0..matched {
      self.push_part_byte(self.boundary[index])?;
    }
    Ok(())
  }

  fn push_part_byte(&mut self, byte: u8) -> Result<(), Box<Response>> {
    match &mut self.sink {
      Sink::Field(data) => {
        if data.len() + 1 > MAX_PART_SIZE {
          return Err(starlette_error("Part exceeded maximum size of 1024KB."));
        }
        data.push(byte);
      }
      Sink::File { writer, size, .. } => {
        writer.write_all(&[byte]).map_err(|_| multipart_error())?;
        *size += 1;
      }
      Sink::None => unreachable!("part data arrives only after the part headers finished"),
    }
    Ok(())
  }

  fn begin_part(&mut self) {
    self.sink = Sink::None;
    self.part_name = None;
    self.content_disposition = None;
    self.header_name.clear();
    self.header_value.clear();
  }

  fn header_end(&mut self) {
    let name = latin1(&self.header_name).to_lowercase();
    if name == "content-disposition" {
      self.content_disposition = Some(std::mem::take(&mut self.header_value));
    }
    self.header_value.clear();
  }

  /// Finish the headers of a part the way Starlette's `on_headers_finished` does: read the disposition, count the
  /// part, and open the temporary file for a file part.
  fn headers_finished(&mut self) -> Result<(), Box<Response>> {
    let disposition = self.content_disposition.take().unwrap_or_default();
    let (_, options) = parse_options_header(&latin1(&disposition));
    let Some(name) = last_param(&options, "name") else {
      return Err(starlette_error("The Content-Disposition header field \"name\" must be provided."));
    };
    self.part_name = Some(user_safe_decode(&latin1_bytes(&name), &self.charset));
    if let Some(filename) = last_param(&options, "filename") {
      self.file_count += 1;
      if self.file_count > MAX_FILES {
        return Err(starlette_error("Too many files. Maximum number of files is 1000."));
      }
      let named_file = NamedTempFile::new().map_err(|_| multipart_error())?;
      let (file, path) = named_file.into_parts();
      self.sink = Sink::File {
        filename: Some(user_safe_decode(&latin1_bytes(&filename), &self.charset)),
        writer: std::io::BufWriter::new(file),
        path,
        size: 0,
      };
    } else {
      self.field_count += 1;
      if self.field_count > MAX_FIELDS {
        return Err(starlette_error("Too many fields. Maximum number of fields is 1000."));
      }
      self.sink = Sink::Field(Vec::new());
    }
    Ok(())
  }

  /// Close the part being read, the way Starlette's `on_part_end` does.
  fn end_part(&mut self) -> Result<(), Box<Response>> {
    let name = self.part_name.take().unwrap_or_default();
    match std::mem::replace(&mut self.sink, Sink::None) {
      Sink::Field(data) => {
        self.parts.push(Part { name, value: PartValue::Form(user_safe_decode(&data, &self.charset)) });
      }
      Sink::File { filename, mut writer, path, size } => {
        writer.flush().map_err(|_| multipart_error())?;
        self.parts.push(Part { name, value: PartValue::File(UploadedFile { filename, path, size }) });
      }
      Sink::None => unreachable!("part data arrives only after the part headers finished"),
    }
    Ok(())
  }
}

fn is_token_byte(byte: u8) -> bool {
  byte.is_ascii_alphanumeric()
    || matches!(
      byte,
      b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
    )
}

/// Read a multipart body with the ported python-multipart state machine.
async fn read_multipart(boundary: &str, charset: &str, body: Body) -> Result<FormBody, Box<Response>> {
  let mut machine = MultipartMachine::new(boundary, charset);
  let mut stream = body.into_data_stream();
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|_| multipart_error())?;
    for byte in chunk.iter() {
      machine.step(*byte)?;
    }
  }
  // A body that ends before the current part closes leaves that part behind, as Starlette does when the stream ends.
  Ok(FormBody { parts: std::mem::take(&mut machine.parts) })
}
