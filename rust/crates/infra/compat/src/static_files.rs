//! Starlette's `StaticFiles` and `FileResponse`, as the Python app serves a directory of files.
//!
//! A served file carries the content type `mimetypes` guesses, `accept-ranges`, `content-length`,
//! `last-modified` and an `etag` of `md5("<st_mtime>-<st_size>")`; a matching `If-None-Match` or a newer
//! `If-Modified-Since` answers 304, and a `Range` header answers 206, 400 or 416. The directory is checked
//! per request until the check first passes, so a directory that appears later serves without a restart,
//! and a missing one answers 500 as the Python mount's `RuntimeError` does.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use charliebot_core::{paths, pyfmt, timefmt};
use futures_util::stream;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use md5::{Digest, Md5};

use crate::headers::header_text;
use crate::mimetypes::MimeTypes;
use crate::responses;

/// `FileResponse.chunk_size`: a body at least this long goes out in more than one message.
const CHUNK_SIZE: usize = 64 * 1024;

/// One served directory: the `StaticFiles(directory=...)` a Python mount holds.
pub struct StaticDirectory {
  /// The served directory as the Python app names it (unresolved).
  path: PathBuf,
  /// Whether the directory check has passed once (`StaticFiles.config_checked`).
  checked: AtomicBool,
}

impl StaticDirectory {
  pub fn new(path: PathBuf) -> StaticDirectory {
    StaticDirectory { path, checked: AtomicBool::new(false) }
  }
}

/// Serve `route_path` (the decoded path below the directory, starting with `/`).
pub fn serve(
  served: &StaticDirectory,
  mime_types: &MimeTypes,
  route_path: &str,
  method: &Method,
  headers: &HeaderMap,
) -> Response {
  if !served.checked.load(Ordering::Acquire) {
    if let Some(failure) = directory_failure(&served.path) {
      return failure;
    }
    served.checked.store(true, Ordering::Release);
  }
  if method != Method::GET && method != Method::HEAD {
    return responses::http_exception(StatusCode::METHOD_NOT_ALLOWED);
  }
  let relative = normpath(route_path.trim_start_matches('/'));
  let directory = paths::realpath(&served.path);
  let full_path = paths::realpath(&served.path.join(&relative));
  if !paths::is_within(&full_path, &directory) {
    return responses::http_exception(StatusCode::NOT_FOUND);
  }
  let metadata = match std::fs::metadata(&full_path) {
    Ok(metadata) => metadata,
    Err(error) => {
      return match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidFilename => {
          responses::http_exception(StatusCode::NOT_FOUND)
        }
        std::io::ErrorKind::InvalidInput => responses::http_exception(StatusCode::NOT_FOUND),
        std::io::ErrorKind::PermissionDenied => responses::http_exception(StatusCode::UNAUTHORIZED),
        _ => responses::internal_error("OSError", format!("{}: {error}", full_path.display())),
      };
    }
  };
  if !metadata.is_file() {
    return responses::http_exception(StatusCode::NOT_FOUND);
  }
  file_response(&full_path, &metadata, mime_types, None, method, headers)
}

/// Serve one known file with Starlette `FileResponse` validators and a caller-selected media type.
pub fn serve_file(path: &Path, content_type: &str, method: &Method, headers: &HeaderMap) -> Response {
  if method != Method::GET && method != Method::HEAD {
    return responses::http_exception(StatusCode::METHOD_NOT_ALLOWED);
  }
  let metadata = match std::fs::metadata(path) {
    Ok(metadata) => metadata,
    Err(error) => {
      return match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidFilename => {
          responses::http_exception(StatusCode::NOT_FOUND)
        }
        std::io::ErrorKind::InvalidInput => responses::http_exception(StatusCode::NOT_FOUND),
        std::io::ErrorKind::PermissionDenied => responses::http_exception(StatusCode::UNAUTHORIZED),
        _ => responses::internal_error("OSError", format!("{}: {error}", path.display())),
      };
    }
  };
  if !metadata.is_file() {
    return responses::http_exception(StatusCode::NOT_FOUND);
  }
  file_response(path, &metadata, &MimeTypes::builtin(), Some(content_type), method, headers)
}

/// `StaticFiles.check_config`: the 500 a mount answers while its directory is missing or not a directory.
fn directory_failure(directory: &Path) -> Option<Response> {
  match std::fs::metadata(directory) {
    Ok(metadata) if metadata.is_dir() => None,
    Ok(_) => Some(responses::internal_error(
      "RuntimeError",
      format!("StaticFiles path '{}' is not a directory.", directory.display()),
    )),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(responses::internal_error(
      "RuntimeError",
      format!("StaticFiles directory '{}' does not exist.", directory.display()),
    )),
    Err(error) => Some(responses::internal_error("OSError", format!("{}: {error}", directory.display()))),
  }
}

/// `posixpath.normpath` of a relative path: `.` and empty parts drop, `..` cancels a preceding name.
fn normpath(path: &str) -> String {
  let mut parts: Vec<&str> = Vec::new();
  for part in path.split('/') {
    match part {
      "" | "." => {}
      ".." if parts.last().is_some_and(|last| *last != "..") => {
        parts.pop();
      }
      other => parts.push(other),
    }
  }
  if parts.is_empty() { ".".to_string() } else { parts.join("/") }
}

/// The validators and type headers of one file, in `FileResponse`'s order.
struct FileHeaders {
  content_type: String,
  size: u64,
  last_modified: String,
  etag: String,
}

impl FileHeaders {
  fn of(
    path: &Path,
    metadata: &std::fs::Metadata,
    mime_types: &MimeTypes,
    override_content_type: Option<&str>,
  ) -> FileHeaders {
    let content_type = override_content_type.map_or_else(
      || {
        let guessed = mime_types.guess_type(&paths::display(path)).unwrap_or("text/plain");
        if guessed.starts_with("text/") { format!("{guessed}; charset=utf-8") } else { guessed.to_string() }
      },
      str::to_string,
    );
    let mtime = timefmt::python_float_seconds(metadata.mtime(), metadata.mtime_nsec());
    let etag_base = format!("{}-{}", pyfmt::float_repr(mtime), metadata.size());
    FileHeaders {
      content_type,
      size: metadata.size(),
      last_modified: timefmt::http_date(mtime.floor() as i64),
      etag: format!("\"{:x}\"", Md5::digest(etag_base.as_bytes())),
    }
  }

  fn apply(&self, builder: http::response::Builder) -> http::response::Builder {
    builder
      .header(header::CONTENT_TYPE, &self.content_type)
      .header(header::ACCEPT_RANGES, "bytes")
      .header(header::CONTENT_LENGTH, self.size)
      .header(header::LAST_MODIFIED, &self.last_modified)
      .header(header::ETAG, &self.etag)
  }
}

fn file_response(
  path: &Path,
  metadata: &std::fs::Metadata,
  mime_types: &MimeTypes,
  override_content_type: Option<&str>,
  method: &Method,
  headers: &HeaderMap,
) -> Response {
  let file_headers = FileHeaders::of(path, metadata, mime_types, override_content_type);
  if is_not_modified(&file_headers, headers) {
    return Response::builder()
      .status(StatusCode::NOT_MODIFIED)
      .header(header::ETAG, &file_headers.etag)
      .body(Body::empty())
      .expect("a 304 response builds");
  }
  let head_only = method == Method::HEAD;
  let range = headers.get(header::RANGE).map(|_| header_text(headers, &header::RANGE));
  let if_range = headers.get(header::IF_RANGE).map(|_| header_text(headers, &header::IF_RANGE));
  let use_range = match (&range, &if_range) {
    (None, _) => false,
    (Some(_), None) => true,
    (Some(_), Some(if_range)) => *if_range == file_headers.last_modified || *if_range == file_headers.etag,
  };
  if !use_range {
    let body = if head_only {
      Body::empty()
    } else {
      match read_body(path, 0, file_headers.size, true) {
        Ok(body) => body,
        Err(error) => return responses::internal_error("OSError", format!("{}: {error}", path.display())),
      }
    };
    return file_headers.apply(Response::builder().status(StatusCode::OK)).body(body).expect("a file response builds");
  }
  let ranges = match parse_range_header(range.as_deref().expect("use_range implies a Range header"), file_headers.size)
  {
    Ok(ranges) => ranges,
    Err(RangeError::Malformed(message)) => return responses::plain_text(StatusCode::BAD_REQUEST, message),
    Err(RangeError::NotSatisfiable) => {
      return Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{}", file_headers.size))
        .header(header::CONTENT_LENGTH, 0)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::empty())
        .expect("a 416 response builds");
    }
  };
  if let [(start, end)] = ranges[..] {
    let builder = file_headers
      .apply(Response::builder().status(StatusCode::PARTIAL_CONTENT))
      .header(header::CONTENT_RANGE, format!("bytes {start}-{}/{}", end - 1, file_headers.size));
    let body = if head_only {
      Body::empty()
    } else {
      match read_body(path, start, end, false) {
        Ok(body) => body,
        Err(error) => return responses::internal_error("OSError", format!("{}: {error}", path.display())),
      }
    };
    let mut response = builder.body(body).expect("a 206 response builds");
    response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(end - start));
    return response;
  }
  multipart_response(path, &file_headers, &ranges, head_only)
}

/// `StaticFiles.is_not_modified`.
fn is_not_modified(file_headers: &FileHeaders, request: &HeaderMap) -> bool {
  let if_none_match = header_text(request, &header::IF_NONE_MATCH);
  if !if_none_match.is_empty() {
    return if_none_match
      .split(',')
      .any(|tag| tag.trim_matches(|c| c == ' ' || c == 'W' || c == '/') == file_headers.etag);
  }
  if request.get(header::IF_MODIFIED_SINCE).is_none() {
    return false;
  }
  match (
    parse_http_date(&header_text(request, &header::IF_MODIFIED_SINCE)),
    parse_http_date(&file_headers.last_modified),
  ) {
    (Some(since), Some(modified)) => since >= modified,
    _ => false,
  }
}

/// `email.utils.parsedate` for the forms HTTP clients send (`Sun, 18 Oct 2026 07:15:40 GMT`, with or
/// without the weekday), as the `(year, month, day, hour, minute, second)` tuple CPython compares; the
/// zone is ignored as `parsedate` ignores it.
fn parse_http_date(text: &str) -> Option<(i64, i64, i64, i64, i64, i64)> {
  const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
  let mut words: Vec<&str> = text.split_whitespace().collect();
  if words.first().is_some_and(|word| word.ends_with(',') || word.len() == 3 && word.chars().all(char::is_alphabetic)) {
    words.remove(0);
  }
  if words.len() < 4 {
    return None;
  }
  let day: i64 = words[0].parse().ok()?;
  let month = MONTHS.iter().position(|name| words[1].to_ascii_lowercase().starts_with(name))? as i64 + 1;
  let mut year: i64 = words[2].parse().ok()?;
  if year < 100 {
    year += if year > 68 { 1900 } else { 2000 };
  }
  let clock: Vec<i64> = words[3].split(':').map(str::parse).collect::<Result<_, _>>().ok()?;
  let (hour, minute, second) = match clock[..] {
    [hour, minute, second] => (hour, minute, second),
    [hour, minute] => (hour, minute, 0),
    _ => return None,
  };
  Some((year, month, day, hour, minute, second))
}

enum RangeError {
  Malformed(&'static str),
  NotSatisfiable,
}

/// `FileResponse._parse_range_header`: the requested `[start, end)` ranges, merged when several.
fn parse_range_header(range: &str, file_size: u64) -> Result<Vec<(u64, u64)>, RangeError> {
  let (units, spec) = range.split_once('=').ok_or(RangeError::Malformed("Malformed range header."))?;
  if units.trim().to_lowercase() != "bytes" {
    return Err(RangeError::Malformed("Only support bytes range"));
  }
  let size = file_size as i128;
  let mut ranges: Vec<(i128, i128)> = Vec::new();
  for part in spec.split(',') {
    let part = part.trim();
    if part.is_empty() || part == "-" || !part.contains('-') {
      continue;
    }
    let (start_text, end_text) = part.split_once('-').expect("the part holds a dash");
    let (start_text, end_text) = (start_text.trim(), end_text.trim());
    let start = if start_text.is_empty() { python_int(end_text).map(|end| size - end) } else { python_int(start_text) };
    let end = if !start_text.is_empty() && !end_text.is_empty() {
      python_int(end_text).map(|end| if end < size { end + 1 } else { size })
    } else {
      Some(size)
    };
    if let (Some(start), Some(end)) = (start, end) {
      ranges.push((start, end));
    }
  }
  if ranges.is_empty() {
    return Err(RangeError::Malformed("Range header: range must be requested"));
  }
  if ranges.iter().any(|(start, _)| !(0 <= *start && *start < size)) {
    return Err(RangeError::NotSatisfiable);
  }
  if ranges.iter().any(|(start, end)| start > end) {
    return Err(RangeError::Malformed("Range header: start must be less than end"));
  }
  if ranges.len() > 1 {
    ranges.sort();
    let mut merged: Vec<(i128, i128)> = vec![ranges[0]];
    for (start, end) in ranges.into_iter().skip(1) {
      let last = merged.last_mut().expect("merged starts non-empty");
      if start <= last.1 {
        last.1 = last.1.max(end);
      } else {
        merged.push((start, end));
      }
    }
    ranges = merged;
  }
  Ok(ranges.into_iter().map(|(start, end)| (start as u64, end as u64)).collect())
}

/// CPython `int(text)` for decimal text: optional sign, digits with single underscores between them.
fn python_int(text: &str) -> Option<i128> {
  let text = text.trim();
  let (negative, digits) = match text.as_bytes().first()? {
    b'-' => (true, &text[1..]),
    b'+' => (false, &text[1..]),
    _ => (false, text),
  };
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
    return None;
  }
  let value: i128 = digits.replace('_', "").parse().ok()?;
  Some(if negative { -value } else { value })
}

fn multipart_response(path: &Path, file_headers: &FileHeaders, ranges: &[(u64, u64)], head_only: bool) -> Response {
  let boundary = match random_hex(13) {
    Ok(boundary) => boundary,
    Err(error) => return responses::internal_error("OSError", format!("/dev/urandom: {error}")),
  };
  let mut messages: Vec<Bytes> = Vec::new();
  for (start, end) in ranges {
    messages.push(Bytes::from(format!(
      "--{boundary}\r\nContent-Type: {}\r\nContent-Range: bytes {start}-{}/{}\r\n\r\n",
      file_headers.content_type,
      end - 1,
      file_headers.size
    )));
    let content = match read_file_range(path, *start, *end) {
      Ok(content) => content,
      Err(error) => return responses::internal_error("OSError", format!("{}: {error}", path.display())),
    };
    for chunk in content.chunks(CHUNK_SIZE) {
      messages.push(Bytes::copy_from_slice(chunk));
    }
    messages.push(Bytes::from_static(b"\r\n"));
  }
  messages.push(Bytes::from(format!("--{boundary}--")));
  let length: usize = messages.iter().map(Bytes::len).sum();
  let builder = file_headers.apply(Response::builder().status(StatusCode::PARTIAL_CONTENT));
  let body = if head_only {
    Body::empty()
  } else {
    Body::from_stream(stream::iter(messages.into_iter().map(Ok::<_, std::io::Error>)))
  };
  let mut response = builder.body(body).expect("a multipart response builds");
  response.headers_mut().insert(
    header::CONTENT_TYPE,
    HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}"))
      .expect("a hex boundary is header-safe"),
  );
  response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(length));
  response
}

/// `secrets.token_hex(count)`: the multipart boundary.
fn random_hex(count: usize) -> std::io::Result<String> {
  let mut bytes = vec![0u8; count];
  std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
  Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The bytes `[start, end)` of `path` as starlette sends them: one message when the first chunk ends
/// the body, a stream of `CHUNK_SIZE` messages otherwise. `whole_file` selects `_handle_simple`'s rule
/// (a full first chunk always announces more) over the range rule (more only while bytes remain).
fn read_body(path: &Path, start: u64, end: u64, whole_file: bool) -> std::io::Result<Body> {
  let slice = if whole_file { Bytes::from(std::fs::read(path)?) } else { read_file_range(path, start, end)? };
  let single_message = if whole_file { slice.len() < CHUNK_SIZE } else { slice.len() <= CHUNK_SIZE };
  if single_message {
    return Ok(Body::from(slice));
  }
  let mut messages: Vec<Bytes> = (0..slice.len())
    .step_by(CHUNK_SIZE)
    .map(|offset| slice.slice(offset..(offset + CHUNK_SIZE).min(slice.len())))
    .collect();
  if whole_file && slice.len().is_multiple_of(CHUNK_SIZE) {
    messages.push(Bytes::new());
  }
  Ok(Body::from_stream(stream::iter(messages.into_iter().map(Ok::<_, std::io::Error>))))
}

/// Seek to `[start, end)` and read only that range, as Starlette's `FileResponse` does for range requests.
fn read_file_range(path: &Path, start: u64, end: u64) -> std::io::Result<Bytes> {
  let mut file = std::fs::File::open(path)?;
  read_range(&mut file, start, end)
}

fn read_range(reader: &mut (impl Read + Seek), start: u64, end: u64) -> std::io::Result<Bytes> {
  reader.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::with_capacity(end.saturating_sub(start) as usize);
  reader.take(end.saturating_sub(start)).read_to_end(&mut bytes)?;
  Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
  use super::*;

  struct CountingReader {
    inner: std::io::Cursor<Vec<u8>>,
    bytes_read: usize,
  }

  impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
      let count = self.inner.read(buffer)?;
      self.bytes_read += count;
      Ok(count)
    }
  }

  impl Seek for CountingReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
      self.inner.seek(position)
    }
  }

  #[test]
  fn a_range_reads_only_the_requested_bytes() {
    let mut reader = CountingReader { inner: std::io::Cursor::new(vec![b'x'; 2 * 1024 * 1024]), bytes_read: 0 };
    let bytes = read_range(&mut reader, 1024 * 1024, 1024 * 1024 + 64 * 1024).unwrap();
    assert_eq!(bytes.len(), 64 * 1024);
    assert_eq!(reader.bytes_read, 64 * 1024);
  }

  #[test]
  fn normpath_matches_posixpath() {
    assert_eq!(normpath("css/styles.css"), "css/styles.css");
    assert_eq!(normpath("a//b/./c/../d"), "a/b/d");
    assert_eq!(normpath("../server.py"), "../server.py");
    assert_eq!(normpath(""), ".");
  }

  #[test]
  fn ranges_parse_like_file_response() {
    assert!(matches!(parse_range_header("bytes=0-9", 100), Ok(ranges) if ranges == vec![(0, 10)]));
    assert!(matches!(parse_range_header("bytes=-10", 100), Ok(ranges) if ranges == vec![(90, 100)]));
    assert!(matches!(parse_range_header("bytes=90-", 100), Ok(ranges) if ranges == vec![(90, 100)]));
    assert!(matches!(parse_range_header("bytes=0-5,3-9,20-29", 100), Ok(ranges) if ranges == vec![(0, 10), (20, 30)]));
    assert!(matches!(parse_range_header("bytes=200-300", 100), Err(RangeError::NotSatisfiable)));
    assert!(matches!(parse_range_header("items=0-1", 100), Err(RangeError::Malformed("Only support bytes range"))));
    assert!(matches!(
      parse_range_header("bytes=x-y", 100),
      Err(RangeError::Malformed("Range header: range must be requested"))
    ));
    assert!(matches!(parse_range_header("bytes", 100), Err(RangeError::Malformed("Malformed range header."))));
  }

  #[test]
  fn http_dates_compare_as_parsedate_tuples() {
    assert_eq!(parse_http_date("Sun, 18 Oct 2026 07:15:40 GMT"), Some((2026, 10, 18, 7, 15, 40)));
    assert_eq!(parse_http_date("18 Oct 2026 07:15:40 GMT"), Some((2026, 10, 18, 7, 15, 40)));
    assert_eq!(parse_http_date("garbage"), None);
  }
}
