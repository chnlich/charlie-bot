//! FastAPI's multipart body parsing and `File`/`Form` validation.
//!
//! `read` runs before route dependencies and streams file parts into temporary files. `FormBody::validate` runs after
//! dependencies and reports required or mistyped fields at `body.<field>`, like `fastapi_body::RequestBody`.

use crate::responses;
use crate::validation::{self, FieldError, Kind, Loc};
use axum::body::{Body, BodyDataStream, Bytes};
use axum::response::Response;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};
use tempfile::TempPath;
use tokio::io::AsyncWriteExt;

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

  /// Declare an optional `File` field.
  pub const fn optional_file(name: &'static str) -> Self {
    Self { name, field_type: FieldType::File, required: false }
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

  /// The number of bytes written to the temporary file.
  pub fn size(&self) -> u64 {
    self.size
  }
}

enum PartValue {
  File(UploadedFile),
  Form(String),
}

struct Part {
  name: String,
  value: PartValue,
}

/// A parsed multipart form, with file parts held in temporary files.
#[derive(Default)]
pub struct FormBody {
  parts: Vec<Part>,
}

impl FormBody {
  /// Validate this form in field declaration order.
  pub fn validate(&self, model: &FormModel) -> Result<(), Box<Response>> {
    let mut errors = Vec::new();
    for field in model.fields {
      match (field.field_type, self.value(field.name)) {
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

/// Read a multipart request before dependencies, returning an empty form for other content types.
///
/// A body whose opening does not match its boundary is a parse error, as python-multipart reports it.
pub async fn read(headers: &HeaderMap, body: Body) -> Result<FormBody, Box<Response>> {
  let Some(content_type) = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) else {
    return Ok(FormBody::default());
  };
  if !is_multipart_form(content_type) {
    return Ok(FormBody::default());
  }
  let boundary = match multer::parse_boundary(content_type) {
    Ok(boundary) => boundary.to_string(),
    Err(_) => return Err(multipart_error("Missing boundary in multipart.")),
  };

  let (opening, rest) = read_opening(body.into_data_stream(), &boundary).await?;
  let stream = stream::iter([Ok::<_, axum::Error>(Bytes::from(opening))]).chain(rest);
  let mut multipart = multer::Multipart::new(stream, boundary);
  let mut form = FormBody::default();
  let mut file_count = 0;
  let mut field_count = 0;
  'parts: loop {
    let next_field = match multipart.next_field().await {
      Ok(next_field) => next_field,
      Err(error) if is_incomplete(&error) => break,
      Err(_) => return Err(multipart_error("There was an error parsing the body")),
    };
    let Some(mut field) = next_field else {
      break;
    };
    let name = field
      .name()
      .ok_or_else(|| multipart_error("The Content-Disposition header field \"name\" must be provided."))?
      .to_string();
    if let Some(filename) = field.file_name().map(str::to_string) {
      file_count += 1;
      if file_count > MAX_FILES {
        return Err(multipart_error("Too many files. Maximum number of files is 1000."));
      }
      let named_file =
        tempfile::NamedTempFile::new().map_err(|_| multipart_error("There was an error parsing the body"))?;
      let (file, path) = named_file.into_parts();
      let mut output = tokio::fs::File::from_std(file);
      let mut size = 0_u64;
      while let Some(chunk) = match field.chunk().await {
        Ok(chunk) => chunk,
        Err(error) if is_incomplete(&error) => break 'parts,
        Err(_) => return Err(multipart_error("There was an error parsing the body")),
      } {
        output.write_all(&chunk).await.map_err(|_| multipart_error("There was an error parsing the body"))?;
        size += chunk.len() as u64;
      }
      output.flush().await.map_err(|_| multipart_error("There was an error parsing the body"))?;
      drop(output);
      form.parts.push(Part { name, value: PartValue::File(UploadedFile { filename: Some(filename), path, size }) });
    } else {
      field_count += 1;
      if field_count > MAX_FIELDS {
        return Err(multipart_error("Too many fields. Maximum number of fields is 1000."));
      }
      let mut data = Vec::new();
      while let Some(chunk) = match field.chunk().await {
        Ok(chunk) => chunk,
        Err(error) if is_incomplete(&error) => break 'parts,
        Err(_) => return Err(multipart_error("There was an error parsing the body")),
      } {
        if data.len().saturating_add(chunk.len()) > MAX_PART_SIZE {
          return Err(multipart_error("Part exceeded maximum size of 1024KB."));
        }
        data.extend_from_slice(&chunk);
      }
      form.parts.push(Part { name, value: PartValue::Form(decode_text(&data)) });
    }
  }
  Ok(form)
}

fn is_multipart_form(content_type: &str) -> bool {
  content_type.split(';').next().is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("multipart/form-data"))
}

fn is_incomplete(error: &multer::Error) -> bool {
  matches!(
    error,
    multer::Error::IncompleteFieldData { .. } | multer::Error::IncompleteHeaders | multer::Error::IncompleteStream
  )
}

/// Read the bytes python-multipart checks before the first part, and return them with the rest of the body.
///
/// python-multipart skips leading CR and LF bytes, then requires `--<boundary>` followed by CRLF or `--`. A body that
/// ends inside that opening is not an error for python-multipart, so it goes on to multer, which reads it as truncated.
async fn read_opening(mut stream: BodyDataStream, boundary: &str) -> Result<(Vec<u8>, BodyDataStream), Box<Response>> {
  let marker = format!("--{boundary}");
  let mut opening = Vec::new();
  while opening.len() < marker.len() + 2 && !opening_mismatch(&opening, marker.as_bytes()) {
    let Some(chunk) = stream.next().await else {
      break;
    };
    let chunk = chunk.map_err(|_| multipart_error("There was an error parsing the body"))?;
    let skipped =
      if opening.is_empty() { chunk.iter().take_while(|byte| matches!(byte, b'\r' | b'\n')).count() } else { 0 };
    opening.extend_from_slice(&chunk[skipped..]);
  }
  if opening_mismatch(&opening, marker.as_bytes()) {
    return Err(multipart_error("There was an error parsing the body"));
  }
  Ok((opening, stream))
}

/// Whether `opening` already differs from `marker` followed by CRLF or `--`. Bytes not read yet never differ.
fn opening_mismatch(opening: &[u8], marker: &[u8]) -> bool {
  if opening.len() <= marker.len() {
    return opening != &marker[..opening.len()];
  }
  if &opening[..marker.len()] != marker {
    return true;
  }
  !matches!(
    (opening[marker.len()], opening.get(marker.len() + 1)),
    (b'\r', Some(b'\n')) | (b'-', Some(b'-')) | (b'\r' | b'-', None)
  )
}

fn decode_text(bytes: &[u8]) -> String {
  match std::str::from_utf8(bytes) {
    Ok(value) => value.to_string(),
    Err(_) => bytes.iter().map(|byte| char::from(*byte)).collect(),
  }
}

fn field_error(name: &str, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: vec![Loc::Field("body".to_string()), Loc::Field(name.to_string())], input, kind }
}

fn multipart_error(detail: &str) -> Box<Response> {
  Box::new(responses::json(StatusCode::BAD_REQUEST, &json!({ "detail": detail })))
}
