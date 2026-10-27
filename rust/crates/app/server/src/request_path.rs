//! The decoded request path: the ASGI `scope["path"]` that every Python routing and auth decision reads.
//!
//! uvicorn percent-decodes the raw path as UTF-8 with replacement characters, and starlette matches
//! routes against that decoded text, so `/api/auth/%73tatus` is `/api/auth/status`. The outermost
//! layer records the decoded path as a [`RoutePath`] extension and rewrites the URI so axum's router
//! matches the same text: the decoded path re-encoded with only the bytes a URI path cannot carry.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::Uri;
use http::uri::PathAndQuery;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// The bytes the re-encoded path escapes: what a URI path cannot hold, plus `%` itself so that decoding
/// the rewritten path gives back the decoded text unchanged.
const PATH_UNSAFE: &AsciiSet = &CONTROLS
  .add(b' ')
  .add(b'"')
  .add(b'#')
  .add(b'%')
  .add(b'<')
  .add(b'>')
  .add(b'?')
  .add(b'`')
  .add(b'{')
  .add(b'}')
  .add(b'\\')
  .add(b'^')
  .add(b'|')
  .add(b'[')
  .add(b']');

/// The request's decoded path (`scope["path"]`).
#[derive(Debug, Clone)]
pub struct RoutePath(pub String);

/// The decoded form of a raw request path.
pub fn decode(raw_path: &str) -> String {
  percent_decode_str(raw_path).decode_utf8_lossy().into_owned()
}

/// The decoded path of a request that passed the outermost layer.
pub fn of(request: &Request) -> &str {
  &request.extensions().get::<RoutePath>().expect("the path layer runs before every handler").0
}

/// The outermost layer: record the decoded path and route on it.
pub async fn layer(mut request: Request, next: Next) -> Response {
  let decoded = decode(request.uri().path());
  let encoded = utf8_percent_encode(&decoded, PATH_UNSAFE).to_string();
  if encoded != request.uri().path() {
    let mut parts = request.uri().clone().into_parts();
    let path_and_query = match request.uri().query() {
      Some(query) => format!("{encoded}?{query}"),
      None => encoded,
    };
    parts.path_and_query = Some(PathAndQuery::try_from(path_and_query).expect("an escaped path is a valid URI path"));
    *request.uri_mut() = Uri::from_parts(parts).expect("replacing the path keeps a valid URI");
  }
  request.extensions_mut().insert(RoutePath(decoded));
  next.run(request).await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decoding_matches_uvicorn() {
    assert_eq!(decode("/api/auth/%73tatus"), "/api/auth/status");
    assert_eq!(decode("/static/a%20b.css"), "/static/a b.css");
    assert_eq!(decode("/x/%ff"), "/x/\u{fffd}");
  }

  #[test]
  fn re_encoding_round_trips() {
    for raw in ["/static/a%20b.css", "/a%25b", "/plain/path"] {
      let decoded = decode(raw);
      assert_eq!(decode(&utf8_percent_encode(&decoded, PATH_UNSAFE).to_string()), decoded);
    }
  }
}
