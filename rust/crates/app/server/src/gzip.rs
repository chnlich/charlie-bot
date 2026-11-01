//! Transport gzip (`server._CharlieBotGZipMiddleware` over starlette's `GZipMiddleware`).
//!
//! A request whose first `Accept-Encoding` value contains `gzip` takes the gzip responder; any other
//! takes the identity responder. Both pass a response through untouched when it already sets
//! `Content-Encoding` or its type is excluded: the gzip responder excludes the media types of
//! [`SKIP_MEDIA_PREFIXES`], the identity responder only `text/event-stream`. Otherwise the first body
//! message decides: shorter than [`MINIMUM_SIZE`] and complete passes untouched; complete and longer
//! gains `Vary: Accept-Encoding` and, under gzip, a level-1 deflate with a new `Content-Length`; a
//! message with more to follow gains `Vary` and, under gzip, streams deflated without `Content-Length`.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{LazyLock, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use http_body::{Body as _, Frame};
use http_body_util::{BodyExt, BodyStream, StreamBody};

use charliebot_compat::headers::header_text;

/// `GZipMiddleware(minimum_size=1000)`.
pub const MINIMUM_SIZE: usize = 1000;
/// `GZipMiddleware(compresslevel=1)`.
pub const COMPRESS_LEVEL: u32 = 1;

/// `_TRANSPORT_GZIP_SKIP_MEDIA_PREFIXES`: content types already entropy-coded, matched by prefix.
pub const SKIP_MEDIA_PREFIXES: &[&str] = &[
  "text/event-stream",
  "image/gif",
  "image/heic",
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/x-icon",
  "image/vnd.microsoft.icon",
  "video/",
  "audio/",
  "application/pdf",
  "application/zip",
  "application/x-zip-compressed",
  "application/x-7z-compressed",
  "application/gzip",
  "application/x-gzip",
  "application/vnd.openxmlformats-officedocument",
  "application/vnd.ms-powerpoint",
  "application/msword",
  "font/woff",
  "application/font-woff",
];

/// The identity responder's own exclusion (`DEFAULT_EXCLUDED_CONTENT_TYPES`).
const IDENTITY_EXCLUDED_PREFIX: &str = "text/event-stream";

/// Whether the gzip responder passes a response of this content type through untouched.
pub fn skips_transport_gzip(content_type: &str) -> bool {
  SKIP_MEDIA_PREFIXES.iter().any(|prefix| content_type.starts_with(prefix))
}

/// The responder a request takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responder {
  Gzip,
  Identity,
}

impl Responder {
  /// The responder for a request with these headers.
  pub fn for_request(headers: &HeaderMap) -> Responder {
    if header_text(headers, &header::ACCEPT_ENCODING).contains("gzip") { Responder::Gzip } else { Responder::Identity }
  }

  /// Whether this responder leaves a response with these headers untouched before reading its body.
  pub fn passes_through(self, headers: &HeaderMap) -> bool {
    let content_type = header_text(headers, &header::CONTENT_TYPE);
    let excluded = match self {
      Responder::Gzip => skips_transport_gzip(&content_type),
      Responder::Identity => content_type.starts_with(IDENTITY_EXCLUDED_PREFIX),
    };
    headers.contains_key(header::CONTENT_ENCODING) || excluded
  }
}

/// The middleware as an axum layer.
pub async fn layer(request: Request, next: Next) -> Response {
  let responder = Responder::for_request(request.headers());
  let uri = request.uri().clone();
  let response = next.run(request).await;
  apply(responder, &uri, response).await
}

async fn apply(responder: Responder, uri: &Uri, response: Response) -> Response {
  let (mut parts, mut body) = response.into_parts();
  if responder.passes_through(&parts.headers) {
    return Response::from_parts(parts, body);
  }
  let first = match body.frame().await {
    None => return Response::from_parts(parts, Body::empty()),
    Some(Err(error)) => {
      return Response::from_parts(parts, Body::from_stream(stream::once(async { Err::<Bytes, _>(error) })));
    }
    Some(Ok(frame)) => frame,
  };
  let first = match first.into_data() {
    Ok(data) => data,
    Err(frame) => return Response::from_parts(parts, rejoin(frame, body)),
  };
  let more_body = !body.is_end_stream();
  if first.len() < MINIMUM_SIZE && !more_body {
    return Response::from_parts(parts, Body::from(first));
  }
  add_vary_accept_encoding(&mut parts.headers);
  match (responder, more_body) {
    (Responder::Identity, false) => Response::from_parts(parts, Body::from(first)),
    (Responder::Identity, true) => Response::from_parts(parts, rejoin(Frame::data(first), body)),
    (Responder::Gzip, false) => {
      let compressed = compressed_body(uri, &parts, &first);
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
      parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
      Response::from_parts(parts, Body::from(compressed))
    }
    // A response that names its whole body by ETag and length is one version of one file even when it arrives in
    // several messages, so the memo can answer it: collect the body the length bounds and send it as one compressed
    // message. A body past the memo's cap, and any body without both headers, keeps streaming.
    (Responder::Gzip, true) if collectable(uri, &parts) => match collect_body(first, body).await {
      Ok(whole) => {
        let compressed = compressed_body(uri, &parts, &whole);
        parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
        Response::from_parts(parts, Body::from(compressed))
      }
      Err(error) => Response::from_parts(parts, Body::from_stream(stream::once(async { Err::<Bytes, _>(error) }))),
    },
    (Responder::Gzip, true) => {
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
      parts.headers.remove(header::CONTENT_LENGTH);
      Response::from_parts(parts, gzip_stream(first, body))
    }
  }
}

/// Whether the memo can answer this response's body: a whole `200` version named by its ETag, whose declared length
/// fits the memo.
fn collectable(uri: &Uri, parts: &http::response::Parts) -> bool {
  body_key(uri, parts).is_some() && declared_length(parts).is_some_and(|length| length <= MEMO_MAX_BODY_BYTES)
}

/// The body length the response declares, which bounds how much the memo buffers.
fn declared_length(parts: &http::response::Parts) -> Option<usize> {
  parts.headers.get(header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// One body's data messages joined: the first, then every frame left in `body`.
async fn collect_body(first: Bytes, mut body: Body) -> Result<Vec<u8>, axum::Error> {
  let mut whole = Vec::with_capacity(first.len());
  whole.extend_from_slice(&first);
  while let Some(frame) = body.frame().await {
    if let Ok(data) = frame?.into_data() {
      whole.extend_from_slice(&data);
    }
  }
  Ok(whole)
}

/// `MutableHeaders.add_vary_header("Accept-Encoding")`: an existing `Vary` gains `, Accept-Encoding`.
fn add_vary_accept_encoding(headers: &mut HeaderMap) {
  let value = match headers.get(header::VARY) {
    Some(existing) => {
      let joined = [existing.as_bytes(), b", Accept-Encoding"].concat();
      HeaderValue::from_bytes(&joined).expect("appending visible ASCII keeps a header value valid")
    }
    None => HeaderValue::from_static("Accept-Encoding"),
  };
  headers.insert(header::VARY, value);
}

/// A body that yields `first`, then the rest of `body`.
fn rejoin(first: Frame<Bytes>, body: Body) -> Body {
  Body::new(StreamBody::new(stream::once(async { Ok::<_, axum::Error>(first) }).chain(BodyStream::new(body))))
}

fn new_encoder() -> GzEncoder<Vec<u8>> {
  GzEncoder::new(Vec::new(), Compression::new(COMPRESS_LEVEL))
}

/// The whole body deflated at once (the single-message case).
pub fn gzip_whole(data: &[u8]) -> Vec<u8> {
  charliebot_compat::compression::gzip_level_one(data)
}

/// How many compressed bodies the memo holds.
const MEMO_ENTRIES: usize = 64;

/// The largest compressed body the memo holds; a larger one is compressed and sent without being kept.
const MEMO_MAX_BODY_BYTES: usize = 256 * 1024;

/// One memo key: the request's path and query, and the ETag of the response it answered.
#[derive(Clone, PartialEq, Eq, Hash)]
struct BodyKey {
  uri: Uri,
  etag: String,
}

/// The complete gzip bodies one request answered with, reused while the response keeps its ETag.
///
/// Deflating a body costs far more than looking it up, and a page load asks for dozens of static files whose bytes
/// hold across requests. Static files are the responses that carry an ETag, which is the file version's identity: the
/// client's own cache treats the same ETag as the same bytes. A response without an ETag, or one that is not a whole
/// 200, is compressed fresh and never cached.
#[derive(Default)]
struct BodyMemo {
  bodies: HashMap<BodyKey, Bytes>,
  /// The keys in insertion order, so a full memo drops the entry stored longest ago.
  order: VecDeque<BodyKey>,
}

impl BodyMemo {
  fn get(&self, key: &BodyKey) -> Option<Bytes> {
    self.bodies.get(key).cloned()
  }

  fn store(&mut self, key: BodyKey, body: Bytes) {
    if body.len() > MEMO_MAX_BODY_BYTES {
      return;
    }
    if self.bodies.insert(key.clone(), body).is_none() {
      self.order.push_back(key);
      while self.order.len() > MEMO_ENTRIES {
        if let Some(oldest) = self.order.pop_front() {
          self.bodies.remove(&oldest);
        }
      }
    }
  }
}

static BODY_MEMO: LazyLock<Mutex<BodyMemo>> = LazyLock::new(|| Mutex::new(BodyMemo::default()));

/// The deflated body the gzip responder sends for one complete response: the bytes memoized for this request's
/// resource and ETag, or a fresh deflate that the memo keeps for the next request.
fn compressed_body(uri: &Uri, parts: &http::response::Parts, body: &[u8]) -> Bytes {
  let key = body_key(uri, parts);
  if let Some(key) = &key
    && let Some(cached) = BODY_MEMO.lock().expect("the gzip body memo mutex is not poisoned").get(key)
  {
    return cached;
  }
  let compressed = Bytes::from(gzip_whole(body));
  if let Some(key) = key {
    BODY_MEMO.lock().expect("the gzip body memo mutex is not poisoned").store(key, compressed.clone());
  }
  compressed
}

/// The identity a complete response's bytes answer under, or `None` when the response names no version of them.
fn body_key(uri: &Uri, parts: &http::response::Parts) -> Option<BodyKey> {
  if parts.status != StatusCode::OK {
    return None;
  }
  let etag = parts.headers.get(header::ETAG)?.to_str().ok()?;
  Some(BodyKey { uri: uri.clone(), etag: etag.to_string() })
}

/// `first` and the rest of `body` deflated as one gzip stream, each message emitting what it produced.
fn gzip_stream(first: Bytes, body: Body) -> Body {
  let mut encoder = new_encoder();
  encoder.write_all(&first).expect("writing to a Vec cannot fail");
  let head = Bytes::from(std::mem::take(encoder.get_mut()));
  let tail = stream::unfold(Some((encoder, BodyStream::new(body))), |state| async move {
    let (mut encoder, mut rest) = state?;
    loop {
      match rest.next().await {
        Some(Ok(frame)) => {
          if let Ok(data) = frame.into_data() {
            encoder.write_all(&data).expect("writing to a Vec cannot fail");
            let produced = std::mem::take(encoder.get_mut());
            if !produced.is_empty() {
              return Some((Ok(Bytes::from(produced)), Some((encoder, rest))));
            }
          }
        }
        Some(Err(error)) => return Some((Err(error), None)),
        None => {
          return Some((Ok(Bytes::from(encoder.finish().expect("finishing into a Vec cannot fail"))), None));
        }
      }
    }
  });
  Body::from_stream(stream::once(async { Ok::<_, axum::Error>(head) }).chain(tail))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Read;

  #[test]
  fn the_skip_list_matches_by_prefix() {
    for skipped in [
      "image/png",
      "font/woff2",
      "video/mp4",
      "audio/ogg",
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
      "text/event-stream; charset=utf-8",
    ] {
      assert!(skips_transport_gzip(skipped), "{skipped}");
    }
    for compressed in
      ["text/html; charset=utf-8", "application/json", "image/svg+xml", "text/css; charset=utf-8", "text/csv", ""]
    {
      assert!(!skips_transport_gzip(compressed), "{compressed}");
    }
  }

  #[test]
  fn the_identity_responder_only_excludes_event_streams() {
    let mut png = HeaderMap::new();
    png.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    assert!(!Responder::Identity.passes_through(&png));
    assert!(Responder::Gzip.passes_through(&png));
    let mut encoded = HeaderMap::new();
    encoded.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
    assert!(Responder::Identity.passes_through(&encoded) && Responder::Gzip.passes_through(&encoded));
  }

  #[test]
  fn the_responder_follows_the_first_accept_encoding_value() {
    let mut headers = HeaderMap::new();
    headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    assert_eq!(Responder::for_request(&headers), Responder::Identity);
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
    assert_eq!(Responder::for_request(&headers), Responder::Gzip);
  }

  async fn run(responder: Responder, content_type: &str, body: Body) -> (HeaderMap, Vec<u8>) {
    run_at("/", None, responder, content_type, body).await
  }

  async fn run_at(
    uri: &str,
    etag: Option<&str>,
    responder: Responder,
    content_type: &str,
    body: Body,
  ) -> (HeaderMap, Vec<u8>) {
    let mut builder = Response::builder();
    if let Some(etag) = etag {
      builder = builder.header(header::ETAG, etag);
    }
    let response = builder.header(header::CONTENT_TYPE, content_type).body(body).unwrap();
    let response = apply(responder, &uri.parse().expect("the test URI parses"), response).await;
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (headers, bytes)
  }

  /// The bytes a gzip response carried before the deflate.
  fn inflated(body: &[u8]) -> String {
    let mut text = String::new();
    flate2::read::GzDecoder::new(body).read_to_string(&mut text).unwrap();
    text
  }

  #[tokio::test]
  async fn a_repeat_response_serves_the_body_its_etag_names() {
    // The ETag names one version of a resource, so the memo answers that version instead of deflating again: the same
    // URI and ETag carry the same body, and a moved ETag compresses the new one.
    let first = "a".repeat(MINIMUM_SIZE * 2);
    let (_, compressed) =
      run_at("/static/a.js", Some("\"v1\""), Responder::Gzip, "text/plain", Body::from(first)).await;
    assert_eq!(inflated(&compressed), "a".repeat(MINIMUM_SIZE * 2));

    let changed = "b".repeat(MINIMUM_SIZE * 2);
    let (_, repeat) =
      run_at("/static/a.js", Some("\"v1\""), Responder::Gzip, "text/plain", Body::from(changed.clone())).await;
    assert_eq!(repeat, compressed, "the ETag's version answers a repeat request");

    let (_, moved) =
      run_at("/static/a.js", Some("\"v2\""), Responder::Gzip, "text/plain", Body::from(changed.clone())).await;
    assert_eq!(inflated(&moved), "b".repeat(MINIMUM_SIZE * 2), "a moved ETag compresses its own bytes");

    let (_, other) = run_at("/static/b.js", Some("\"v1\""), Responder::Gzip, "text/plain", Body::from(changed)).await;
    assert_eq!(inflated(&other), "b".repeat(MINIMUM_SIZE * 2), "another path is another resource");
  }

  /// Drive one response whose body arrives in several messages and whose length it declares.
  async fn run_stream(uri: &str, etag: &str, declared: usize, body: Body) -> (HeaderMap, Vec<u8>) {
    let response = Response::builder()
      .header(header::ETAG, etag)
      .header(header::CONTENT_LENGTH, declared)
      .header(header::CONTENT_TYPE, "text/plain")
      .body(body)
      .unwrap();
    let response = apply(Responder::Gzip, &uri.parse().expect("the test URI parses"), response).await;
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (headers, bytes)
  }

  /// One body split into two messages, as a file past `CHUNK_SIZE` arrives.
  fn split_body(text: String) -> Body {
    let (left, right) = text.split_at(text.len() / 2);
    let messages = vec![Ok::<_, std::io::Error>(Bytes::from(left.to_string())), Ok(Bytes::from(right.to_string()))];
    Body::from_stream(stream::iter(messages))
  }

  #[tokio::test]
  async fn a_multi_message_file_is_memoized_by_its_etag_and_length() {
    let length = MINIMUM_SIZE * 2;
    let first = "a".repeat(length);
    let (headers, compressed) = run_stream("/static/big.js", "\"v1\"", length, split_body(first.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), &compressed.len().to_string());
    assert_eq!(inflated(&compressed), first);

    let (_, repeat) = run_stream("/static/big.js", "\"v1\"", length, split_body("b".repeat(length))).await;
    assert_eq!(repeat, compressed, "the ETag's version answers a repeat request that arrived in pieces");
  }

  #[tokio::test]
  async fn a_body_past_the_memo_cap_keeps_streaming() {
    let length = MEMO_MAX_BODY_BYTES + 1;
    let text = "a".repeat(length);
    let (headers, body) = run_stream("/static/huge.js", "\"v1\"", length, split_body(text.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(headers.get(header::CONTENT_LENGTH).is_none(), "a streamed body carries no length");
    assert_eq!(inflated(&body), text);
  }

  #[tokio::test]
  async fn an_empty_response_never_answers_a_later_body() {
    // A `HEAD` of a static file answers its headers with no body and the same ETag, so the memo must not keep that
    // empty body under the version's key.
    let (headers, head) = run_at("/static/c.js", Some("\"v1\""), Responder::Gzip, "text/plain", Body::empty()).await;
    assert!(head.is_empty());
    assert!(headers.get(header::CONTENT_ENCODING).is_none());

    let text = "c".repeat(MINIMUM_SIZE * 2);
    let (_, get) =
      run_at("/static/c.js", Some("\"v1\""), Responder::Gzip, "text/plain", Body::from(text.clone())).await;
    assert_eq!(inflated(&get), text, "the version's body is computed, not the empty one");
  }

  #[tokio::test]
  async fn a_response_without_an_etag_is_never_memoized() {
    let first = "a".repeat(MINIMUM_SIZE * 2);
    let (_, compressed) = run_at("/api/thing", None, Responder::Gzip, "text/plain", Body::from(first)).await;
    let changed = "b".repeat(MINIMUM_SIZE * 2);
    let (_, repeat) = run_at("/api/thing", None, Responder::Gzip, "text/plain", Body::from(changed)).await;
    assert_eq!(inflated(&compressed), "a".repeat(MINIMUM_SIZE * 2));
    assert_eq!(inflated(&repeat), "b".repeat(MINIMUM_SIZE * 2));
  }

  #[tokio::test]
  async fn the_first_message_decides_vary_and_compression() {
    let small = "x".repeat(MINIMUM_SIZE - 1);
    let (headers, body) = run(Responder::Gzip, "text/plain", Body::from(small.clone())).await;
    assert!(headers.get(header::VARY).is_none() && headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body, small.as_bytes());

    let large = "y".repeat(MINIMUM_SIZE);
    let (headers, body) = run(Responder::Identity, "text/plain", Body::from(large.clone())).await;
    assert_eq!(headers.get(header::VARY).unwrap(), "Accept-Encoding");
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body, large.as_bytes());

    let (headers, body) = run(Responder::Gzip, "text/plain", Body::from(large.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), &body.len().to_string());
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut inflated).unwrap();
    assert_eq!(inflated, large);

    let chunks = stream::iter(["a".repeat(10), "b".repeat(10)].map(|text| Ok::<_, std::io::Error>(Bytes::from(text))));
    let (headers, body) = run(Responder::Gzip, "text/plain", Body::from_stream(chunks)).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(headers.get(header::CONTENT_LENGTH).is_none());
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut inflated).unwrap();
    assert_eq!(inflated, format!("{}{}", "a".repeat(10), "b".repeat(10)));

    let (headers, _) = run(Responder::Gzip, "image/png", Body::from(large.clone())).await;
    assert!(headers.get(header::VARY).is_none() && headers.get(header::CONTENT_ENCODING).is_none());
  }
}
