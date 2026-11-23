//! Transport compression.
//!
//! A request negotiates `zstd`, `gzip`, or `identity` via [`ResponseEncoding::for_request`]. Both compressed encodings
//! pass a response through untouched when it already sets `Content-Encoding` or its type is excluded: compressed
//! responders exclude the media types of [`SKIP_MEDIA_PREFIXES`], the identity responder only `text/event-stream`.
//! Otherwise the first body message decides: shorter than [`MINIMUM_SIZE`] and complete passes untouched; complete and
//! longer gains `Vary: Accept-Encoding` and, under compression, a compressed body with a new `Content-Length`; a
//! message with more to follow gains `Vary` and, under compression, streams compressed without `Content-Length`.
//!
//! A complete body that carries an ETag has one version named by that ETag, so the memo keeps its compressed bytes and
//! serves them to every repeat request. A body the memo can hold compresses once per version and encoding
//! (`gzip_memoized_body` or `zstd_memoized_body`); a body too long for the memo's cap, and a response compressed per
//! request, keep the fast level (`gzip_level_one` or `zstd_fast_body`).

use std::sync::{LazyLock, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use charliebot_compat::body_memo::BodyMemo;
use charliebot_compat::compression::{
  CompressedEncoding, ResponseEncoding, StreamCompressor, gzip_level_one, gzip_memoized_body, zstd_fast_body,
  zstd_memoized_body,
};
use charliebot_compat::headers::header_text;
use charliebot_core::headers::ACCEPT_ENCODING;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use http_body::{Body as _, Frame};
use http_body_util::{BodyExt, BodyStream, StreamBody};

/// `GZipMiddleware(minimum_size=1000)`.
pub const MINIMUM_SIZE: usize = charliebot_compat::compression::MINIMUM_DEFLATE_BYTES;

/// `_TRANSPORT_GZIP_SKIP_MEDIA_PREFIXES`: content types already entropy-coded, matched by prefix.
pub use charliebot_compat::compression::SKIP_MEDIA_PREFIXES;

/// The identity responder's own exclusion (`DEFAULT_EXCLUDED_CONTENT_TYPES`).
const IDENTITY_EXCLUDED_PREFIX: &str = charliebot_compat::compression::TEXT_EVENT_STREAM;

/// Whether a compressed responder passes a response of this content type through untouched.
pub fn skips_transport_gzip(content_type: &str) -> bool {
  SKIP_MEDIA_PREFIXES.iter().any(|prefix| content_type.starts_with(prefix))
}

/// Whether `encoding` leaves a response with these headers untouched before reading its body.
fn passes_through(encoding: ResponseEncoding, headers: &HeaderMap) -> bool {
  let content_type = header_text(headers, &header::CONTENT_TYPE);
  let excluded = match encoding {
    ResponseEncoding::Gzip | ResponseEncoding::Zstd => skips_transport_gzip(&content_type),
    ResponseEncoding::Identity => content_type.starts_with(IDENTITY_EXCLUDED_PREFIX),
  };
  headers.contains_key(header::CONTENT_ENCODING) || excluded
}

/// The middleware as an axum layer.
pub async fn layer(request: Request, next: Next) -> Response {
  let encoding = ResponseEncoding::for_request(request.headers());
  let uri = request.uri().clone();
  let response = next.run(request).await;
  apply(encoding, &uri, response).await
}

async fn apply(encoding: ResponseEncoding, uri: &Uri, response: Response) -> Response {
  let (mut parts, mut body) = response.into_parts();
  if passes_through(encoding, &parts.headers) {
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
  match (encoding.compressed(), more_body) {
    (None, false) => Response::from_parts(parts, Body::from(first)),
    (None, true) => Response::from_parts(parts, rejoin(Frame::data(first), body)),
    (Some(coding), false) => {
      let compressed = compressed_body(uri, &parts, &first, coding);
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding.content_encoding()));
      parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
      Response::from_parts(parts, Body::from(compressed))
    }
    // A response that names its whole body by ETag and length is one version of one file even when it arrives in
    // several messages, so the memo can answer it: collect the body the length bounds and send it as one compressed
    // message. A body past the memo's cap, and any body without both headers, keeps streaming.
    (Some(coding), true) if collectable(uri, &parts, coding) => match collect_body(first, body).await {
      Ok(whole) => {
        let compressed = compressed_body(uri, &parts, &whole, coding);
        parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding.content_encoding()));
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
        Response::from_parts(parts, Body::from(compressed))
      }
      Err(error) => Response::from_parts(parts, Body::from_stream(stream::once(async { Err::<Bytes, _>(error) }))),
    },
    (Some(coding), true) => {
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding.content_encoding()));
      parts.headers.remove(header::CONTENT_LENGTH);
      Response::from_parts(parts, compressed_stream(coding, first, body))
    }
  }
}

/// Whether the memo can answer this response's body: a whole `200` version named by its ETag, whose declared length
/// fits the memo.
fn collectable(uri: &Uri, parts: &http::response::Parts, encoding: CompressedEncoding) -> bool {
  body_key(uri, parts, encoding).is_some() && declared_length(parts).is_some_and(|length| length <= MEMO_MAX_BODY_BYTES)
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

/// `MutableHeaders.add_vary_header`: an existing `Vary` list gains the `Accept-Encoding` name.
fn add_vary_accept_encoding(headers: &mut HeaderMap) {
  let value = match headers.get(header::VARY) {
    Some(existing) => {
      let joined = [existing.as_bytes(), b", ", ACCEPT_ENCODING.as_bytes()].concat();
      HeaderValue::from_bytes(&joined).expect("appending visible ASCII keeps a header value valid")
    }
    None => HeaderValue::from_static(ACCEPT_ENCODING),
  };
  headers.insert(header::VARY, value);
}

/// A body that yields `first`, then the rest of `body`.
fn rejoin(first: Frame<Bytes>, body: Body) -> Body {
  Body::new(StreamBody::new(stream::once(async { Ok::<_, axum::Error>(first) }).chain(BodyStream::new(body))))
}

/// How many compressed bodies the memo holds.
const MEMO_ENTRIES: usize = 64;

/// The largest compressed body the memo holds; a larger one is compressed and sent without being kept.
const MEMO_MAX_BODY_BYTES: usize = 256 * 1024;

/// The longest body whose memoized form the memo still holds: [`MEMO_MAX_BODY_BYTES`] less a margin for the header,
/// trailer and worst-case stored-block overhead. A body at this length compresses to at most the cap, so a memoized
/// form of it is kept, and a longer body keeps the per-response fast form of the cap's own case.
const MEMO_MAX_INPUT_BYTES: usize = MEMO_MAX_BODY_BYTES - 512;

/// One memo key: the request's path and query, the ETag of the response it answered, and its encoding.
#[derive(Clone, PartialEq, Eq, Hash)]
struct BodyKey {
  uri: Uri,
  etag: String,
  encoding: CompressedEncoding,
}

/// The complete compressed bodies one request answered with, reused while the response keeps its ETag.
///
/// Compressing a body costs far more than looking it up, and a page load asks for dozens of static files whose bytes
/// hold across requests. Static files are the responses that carry an ETag, which is the file version's identity: the
/// client's own cache treats the same ETag as the same bytes. A response without an ETag, or one that is not a whole
/// 200, is compressed fresh and never cached.
static BODY_MEMO: LazyLock<Mutex<BodyMemo<BodyKey>>> =
  LazyLock::new(|| Mutex::new(BodyMemo::new(MEMO_ENTRIES, MEMO_MAX_BODY_BYTES)));

/// The compressed body the transport sends for one complete response: the bytes memoized for this request's
/// resource, ETag and encoding, or a fresh compression that the memo keeps for the next request.
///
/// A body the memo can hold compresses once per version and encoding (`gzip_memoized_body` or `zstd_memoized_body`),
/// and every repeat request reads the smaller bytes. A longer body, which the memo's cap would reject, keeps the fast
/// compression it pays once per response; a response with no ETag to key on takes the same per-response form.
fn compressed_body(uri: &Uri, parts: &http::response::Parts, body: &[u8], encoding: CompressedEncoding) -> Bytes {
  compressed_body_with_input_limit(uri, parts, body, encoding, MEMO_MAX_INPUT_BYTES)
}

fn compressed_body_with_input_limit(
  uri: &Uri,
  parts: &http::response::Parts,
  body: &[u8],
  encoding: CompressedEncoding,
  memo_max_input_bytes: usize,
) -> Bytes {
  let key = body_key(uri, parts, encoding);
  if let Some(key) = &key
    && let Some(cached) = BODY_MEMO.lock().expect("the gzip body memo mutex is not poisoned").get(key)
  {
    return cached;
  }
  let memoized = key.is_some() && body.len() <= memo_max_input_bytes;
  let compressed = Bytes::from(match (encoding, memoized) {
    (CompressedEncoding::Gzip, true) => gzip_memoized_body(body),
    (CompressedEncoding::Gzip, false) => gzip_level_one(body),
    (CompressedEncoding::Zstd, true) => zstd_memoized_body(body),
    (CompressedEncoding::Zstd, false) => zstd_fast_body(body),
  });
  if let Some(key) = key {
    BODY_MEMO.lock().expect("the gzip body memo mutex is not poisoned").store(key, compressed.clone());
  }
  compressed
}

/// The identity a complete response's bytes answer under, or `None` when the response names no version of them.
fn body_key(uri: &Uri, parts: &http::response::Parts, encoding: CompressedEncoding) -> Option<BodyKey> {
  if parts.status != StatusCode::OK {
    return None;
  }
  let etag = parts.headers.get(header::ETAG)?.to_str().ok()?;
  Some(BodyKey { uri: uri.clone(), etag: etag.to_string(), encoding })
}

/// `first` and the rest of `body` compressed as one stream, each message emitting what it produced.
fn compressed_stream(encoding: CompressedEncoding, first: Bytes, body: Body) -> Body {
  let mut compressor = StreamCompressor::new(encoding);
  let head = Bytes::from(compressor.push(&first));
  let tail = stream::unfold(Some((compressor, BodyStream::new(body))), |state| async move {
    let (mut compressor, mut rest) = state?;
    loop {
      match rest.next().await {
        Some(Ok(frame)) => {
          if let Ok(data) = frame.into_data() {
            let produced = compressor.push(&data);
            if !produced.is_empty() {
              return Some((Ok(Bytes::from(produced)), Some((compressor, rest))));
            }
          }
        }
        Some(Err(error)) => return Some((Err(error), None)),
        None => return Some((Ok(Bytes::from(compressor.finish())), None)),
      }
    }
  });
  Body::from_stream(stream::once(async { Ok::<_, axum::Error>(head) }).chain(tail))
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_compat::compression::accepted_weight;
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
    assert!(!passes_through(ResponseEncoding::Identity, &png));
    assert!(passes_through(ResponseEncoding::Gzip, &png));
    assert!(passes_through(ResponseEncoding::Zstd, &png));
    let mut encoded = HeaderMap::new();
    encoded.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
    assert!(passes_through(ResponseEncoding::Identity, &encoded));
    assert!(passes_through(ResponseEncoding::Gzip, &encoded));
    assert!(passes_through(ResponseEncoding::Zstd, &encoded));
  }

  #[test]
  fn the_responder_follows_the_first_accept_encoding_value() {
    let mut headers = HeaderMap::new();
    headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    assert_eq!(ResponseEncoding::for_request(&headers), ResponseEncoding::Identity);
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
    assert_eq!(ResponseEncoding::for_request(&headers), ResponseEncoding::Gzip);
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate, br, zstd"));
    assert_eq!(ResponseEncoding::for_request(&headers), ResponseEncoding::Zstd);
  }

  #[test]
  fn a_zero_weight_forbids_a_coding_the_header_still_names() {
    let headers = |value: &'static str| {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
      headers
    };
    let weight =
      |value: &'static str| (accepted_weight(&headers(value), "gzip"), accepted_weight(&headers(value), "zstd"));
    assert_eq!(weight("gzip"), (Some(1.0), None));
    assert_eq!(weight("GZIP;q=0.5"), (Some(0.5), None));
    assert_eq!(weight("gzip;q=0"), (None, None));
    assert_eq!(weight("gzip;q=0, zstd;q=1"), (None, Some(1.0)));
    assert_eq!(weight("br"), (None, None));
    assert_eq!(ResponseEncoding::for_request(&headers("gzip;q=0")), ResponseEncoding::Identity);
    assert_eq!(ResponseEncoding::for_request(&headers("gzip;q=0.2")), ResponseEncoding::Gzip);
  }

  async fn run(encoding: ResponseEncoding, content_type: &str, body: Body) -> (HeaderMap, Vec<u8>) {
    run_at("/", None, encoding, content_type, body).await
  }

  async fn run_at(
    uri: &str,
    etag: Option<&str>,
    encoding: ResponseEncoding,
    content_type: &str,
    body: Body,
  ) -> (HeaderMap, Vec<u8>) {
    let mut builder = Response::builder();
    if let Some(etag) = etag {
      builder = builder.header(header::ETAG, etag);
    }
    let response = builder.header(header::CONTENT_TYPE, content_type).body(body).unwrap();
    let response = apply(encoding, &uri.parse().expect("the test URI parses"), response).await;
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

  fn zstd_decoded(body: &[u8]) -> String {
    String::from_utf8(zstd::decode_all(body).unwrap()).unwrap()
  }

  #[tokio::test]
  async fn a_repeat_response_serves_the_body_its_etag_names() {
    // The ETag names one version of a resource, so the memo answers that version instead of deflating again: the same
    // URI and ETag carry the same body, and a moved ETag compresses the new one.
    let first = "a".repeat(MINIMUM_SIZE * 2);
    let (_, compressed) =
      run_at("/static/a.js", Some("\"v1\""), ResponseEncoding::Gzip, "text/plain", Body::from(first)).await;
    assert_eq!(inflated(&compressed), "a".repeat(MINIMUM_SIZE * 2));

    let changed = "b".repeat(MINIMUM_SIZE * 2);
    let (_, repeat) =
      run_at("/static/a.js", Some("\"v1\""), ResponseEncoding::Gzip, "text/plain", Body::from(changed.clone())).await;
    assert_eq!(repeat, compressed, "the ETag's version answers a repeat request");

    let (zstd_headers, zstd_first) =
      run_at("/static/a.js", Some("\"v1\""), ResponseEncoding::Zstd, "text/plain", Body::from(changed.clone())).await;
    assert_eq!(zstd_headers[header::CONTENT_ENCODING], "zstd");
    assert_eq!(zstd_decoded(&zstd_first), changed, "zstd keeps its own memo slot for the same ETag");

    let (_, moved) =
      run_at("/static/a.js", Some("\"v2\""), ResponseEncoding::Gzip, "text/plain", Body::from(changed.clone())).await;
    assert_eq!(inflated(&moved), "b".repeat(MINIMUM_SIZE * 2), "a moved ETag compresses its own bytes");

    let (_, other) =
      run_at("/static/b.js", Some("\"v1\""), ResponseEncoding::Gzip, "text/plain", Body::from(changed)).await;
    assert_eq!(inflated(&other), "b".repeat(MINIMUM_SIZE * 2), "another path is another resource");
  }

  #[test]
  fn a_body_past_the_memo_input_bound_keeps_its_level_one_deflate() {
    // A body above the memo input bound uses level 1 instead of a memoized form, which a kept version's repeats pay
    // once.
    let body = "a".repeat(64);
    let response = Response::builder().header(header::ETAG, "\"v1\"").body(Body::empty()).unwrap();
    let (parts, _) = response.into_parts();
    let uri = "/static/past-the-cap.js".parse().unwrap();
    let compressed =
      compressed_body_with_input_limit(&uri, &parts, body.as_bytes(), CompressedEncoding::Gzip, body.len() - 1);
    // The gzip header's XFL byte records 4 for a fastest-level deflate and 2 for the memo's maximum-compression form.
    assert_eq!(compressed[8], 4);
    assert_eq!(inflated(&compressed), body);
  }

  /// Drive one response whose body arrives in several messages and whose length it declares.
  async fn run_stream(
    uri: &str,
    etag: &str,
    declared: usize,
    encoding: ResponseEncoding,
    body: Body,
  ) -> (HeaderMap, Vec<u8>) {
    let response = Response::builder()
      .header(header::ETAG, etag)
      .header(header::CONTENT_LENGTH, declared)
      .header(header::CONTENT_TYPE, "text/plain")
      .body(body)
      .unwrap();
    let response = apply(encoding, &uri.parse().expect("the test URI parses"), response).await;
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
    let (headers, compressed) =
      run_stream("/static/big.js", "\"v1\"", length, ResponseEncoding::Gzip, split_body(first.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), &compressed.len().to_string());
    assert_eq!(inflated(&compressed), first);

    let (_, repeat) =
      run_stream("/static/big.js", "\"v1\"", length, ResponseEncoding::Gzip, split_body("b".repeat(length))).await;
    assert_eq!(repeat, compressed, "the ETag's version answers a repeat request that arrived in pieces");
  }

  #[tokio::test]
  async fn a_body_past_the_memo_cap_keeps_streaming() {
    // The memo's collect decision reads the declared Content-Length alone, so a declared length one
    // byte past the cap keeps a small body on the streamed path a real 256 KiB body takes.
    let text = "a".repeat(MINIMUM_SIZE + 1);
    let (headers, body) = run_stream(
      "/static/huge.js",
      "\"v1\"",
      MEMO_MAX_BODY_BYTES + 1,
      ResponseEncoding::Gzip,
      split_body(text.clone()),
    )
    .await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(headers.get(header::CONTENT_LENGTH).is_none(), "a streamed body carries no length");
    assert_eq!(inflated(&body), text);

    let (zstd_headers, zstd_body) = run_stream(
      "/static/huge.js",
      "\"v1\"",
      MEMO_MAX_BODY_BYTES + 1,
      ResponseEncoding::Zstd,
      split_body(text.clone()),
    )
    .await;
    assert_eq!(zstd_headers.get(header::CONTENT_ENCODING).unwrap(), "zstd");
    assert!(zstd_headers.get(header::CONTENT_LENGTH).is_none(), "a streamed body carries no length");
    assert_eq!(zstd_decoded(&zstd_body), text);
  }

  #[tokio::test]
  async fn an_empty_response_never_answers_a_later_body() {
    // A `HEAD` of a static file answers its headers with no body and the same ETag, so the memo must not keep that
    // empty body under the version's key.
    let (headers, head) =
      run_at("/static/c.js", Some("\"v1\""), ResponseEncoding::Gzip, "text/plain", Body::empty()).await;
    assert!(head.is_empty());
    assert!(headers.get(header::CONTENT_ENCODING).is_none());

    let text = "c".repeat(MINIMUM_SIZE * 2);
    let (_, get) =
      run_at("/static/c.js", Some("\"v1\""), ResponseEncoding::Gzip, "text/plain", Body::from(text.clone())).await;
    assert_eq!(inflated(&get), text, "the version's body is computed, not the empty one");
  }

  #[tokio::test]
  async fn a_response_without_an_etag_is_never_memoized() {
    let first = "a".repeat(MINIMUM_SIZE * 2);
    let (_, compressed) = run_at("/api/thing", None, ResponseEncoding::Gzip, "text/plain", Body::from(first)).await;
    let changed = "b".repeat(MINIMUM_SIZE * 2);
    let (_, repeat) = run_at("/api/thing", None, ResponseEncoding::Gzip, "text/plain", Body::from(changed)).await;
    assert_eq!(inflated(&compressed), "a".repeat(MINIMUM_SIZE * 2));
    assert_eq!(inflated(&repeat), "b".repeat(MINIMUM_SIZE * 2));
  }

  #[tokio::test]
  async fn the_first_message_decides_vary_and_compression() {
    let small = "x".repeat(MINIMUM_SIZE - 1);
    let (headers, body) = run(ResponseEncoding::Gzip, "text/plain", Body::from(small.clone())).await;
    assert!(headers.get(header::VARY).is_none() && headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body, small.as_bytes());

    let large = "y".repeat(MINIMUM_SIZE);
    let (headers, body) = run(ResponseEncoding::Identity, "text/plain", Body::from(large.clone())).await;
    assert_eq!(headers.get(header::VARY).unwrap(), "Accept-Encoding");
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body, large.as_bytes());

    let (headers, body) = run(ResponseEncoding::Gzip, "text/plain", Body::from(large.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), &body.len().to_string());
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut inflated).unwrap();
    assert_eq!(inflated, large);

    let (headers, body) = run(ResponseEncoding::Zstd, "text/plain", Body::from(large.clone())).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "zstd");
    assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), &body.len().to_string());
    assert_eq!(zstd_decoded(&body), large);

    let chunks = stream::iter(["a".repeat(10), "b".repeat(10)].map(|text| Ok::<_, std::io::Error>(Bytes::from(text))));
    let (headers, body) = run(ResponseEncoding::Gzip, "text/plain", Body::from_stream(chunks)).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "gzip");
    assert!(headers.get(header::CONTENT_LENGTH).is_none());
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&body[..]).read_to_string(&mut inflated).unwrap();
    assert_eq!(inflated, format!("{}{}", "a".repeat(10), "b".repeat(10)));

    let chunks = stream::iter(["a".repeat(10), "b".repeat(10)].map(|text| Ok::<_, std::io::Error>(Bytes::from(text))));
    let (headers, body) = run(ResponseEncoding::Zstd, "text/plain", Body::from_stream(chunks)).await;
    assert_eq!(headers.get(header::CONTENT_ENCODING).unwrap(), "zstd");
    assert!(headers.get(header::CONTENT_LENGTH).is_none());
    assert_eq!(zstd_decoded(&body), format!("{}{}", "a".repeat(10), "b".repeat(10)));

    let (headers, _) = run(ResponseEncoding::Gzip, "image/png", Body::from(large.clone())).await;
    assert!(headers.get(header::VARY).is_none() && headers.get(header::CONTENT_ENCODING).is_none());
  }
}
