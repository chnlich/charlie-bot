//! Transport gzip (`server._CharlieBotGZipMiddleware` over starlette's `GZipMiddleware`).
//!
//! A request whose first `Accept-Encoding` value contains `gzip` takes the gzip responder; any other
//! takes the identity responder. Both pass a response through untouched when it already sets
//! `Content-Encoding` or its type is excluded: the gzip responder excludes the media types of
//! [`SKIP_MEDIA_PREFIXES`], the identity responder only `text/event-stream`. Otherwise the first body
//! message decides: shorter than [`MINIMUM_SIZE`] and complete passes untouched; complete and longer
//! gains `Vary: Accept-Encoding` and, under gzip, a level-1 deflate with a new `Content-Length`; a
//! message with more to follow gains `Vary` and, under gzip, streams deflated without `Content-Length`.

use std::io::Write;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, header};
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
  let response = next.run(request).await;
  apply(responder, response).await
}

async fn apply(responder: Responder, response: Response) -> Response {
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
      let compressed = gzip_whole(&first);
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
      parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(compressed.len()));
      Response::from_parts(parts, Body::from(compressed))
    }
    (Responder::Gzip, true) => {
      parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
      parts.headers.remove(header::CONTENT_LENGTH);
      Response::from_parts(parts, gzip_stream(first, body))
    }
  }
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
    let response = Response::builder().header(header::CONTENT_TYPE, content_type).body(body).unwrap();
    let response = apply(responder, response).await;
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (headers, bytes)
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
