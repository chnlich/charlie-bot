//! Entity tags for the JSON API's reads.
//!
//! A `GET` under `/api/` that answers `200` with a JSON body carries `ETag: "<hash of the body>"` and
//! `Cache-Control: no-cache`. A request whose `If-None-Match` names that tag gets `304` with no body. The tag is hashed
//! from the body on every request, so the server keeps no state for it. The gzip layer, outside this one, memoizes the
//! compressed body of each tag, which is the same identity a client's cache uses.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use charliebot_compat::headers::header_text;
use charliebot_compat::responses;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use sha2::{Digest, Sha256};

/// The largest JSON body the layer reads whole to hash it. Every JSON handler builds its body in memory first, so the
/// bound stops a runaway body rather than shaping a normal response.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The middleware as an axum layer.
pub async fn layer(request: Request, next: Next) -> Response {
  let conditional = request.method() == Method::GET && request.uri().path().starts_with("/api/");
  let if_none_match = header_text(request.headers(), &header::IF_NONE_MATCH);
  let response = next.run(request).await;
  if !conditional || response.status() != StatusCode::OK || !is_json(response.headers()) {
    return response;
  }
  let (mut parts, body) = response.into_parts();
  let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
    Ok(bytes) => bytes,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let tag = entity_tag(&bytes);
  let tag_value = HeaderValue::from_str(&tag).expect("a quoted hex entity tag is a valid header value");
  parts.headers.insert(header::ETAG, tag_value.clone());
  parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  if names(&if_none_match, &tag) {
    return not_modified(tag_value, &parts.headers);
  }
  Response::from_parts(parts, Body::from(bytes))
}

fn is_json(headers: &HeaderMap) -> bool {
  header_text(headers, &header::CONTENT_TYPE).starts_with("application/json")
}

/// The `304` answer to a request whose `If-None-Match` names the body the server holds: the validators, no body. The
/// stored response keeps its `Vary`, so the tag's representation list stays with it.
fn not_modified(tag: HeaderValue, headers: &HeaderMap) -> Response {
  let mut response = Response::new(Body::empty());
  *response.status_mut() = StatusCode::NOT_MODIFIED;
  let out = response.headers_mut();
  out.insert(header::ETAG, tag);
  out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  if let Some(vary) = headers.get(header::VARY) {
    out.insert(header::VARY, vary.clone());
  }
  response
}

/// The entity tag of a body: the first 128 bits of its SHA-256, in the quoted form an `ETag` header carries.
fn entity_tag(body: &[u8]) -> String {
  let digest = Sha256::digest(body);
  let hex: String = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect();
  format!("\"{hex}\"")
}

/// Whether an `If-None-Match` value names `tag`: a list of entity tags, or `*`. The comparison is weak, as RFC 9110
/// specifies for this header, so a `W/` prefix does not change the match.
fn names(if_none_match: &str, tag: &str) -> bool {
  if_none_match.trim() == "*"
    || if_none_match.split(',').any(|candidate| candidate.trim().trim_start_matches("W/") == tag)
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use axum::Router;
  use axum::middleware;
  use axum::routing::get;
  use http::Request as HttpRequest;
  use tower::ServiceExt;

  use super::*;

  fn json(body: String) -> Response {
    Response::builder()
      .status(StatusCode::OK)
      .header(header::CONTENT_TYPE, "application/json")
      .header(header::VARY, "Accept-Encoding")
      .body(Body::from(body))
      .expect("a JSON response builds")
  }

  /// A router whose `/api/items` body counts its own reads, so a test can change the body between requests.
  fn app(reads: Arc<AtomicUsize>) -> Router {
    Router::new()
      .route(
        "/api/items",
        get(move || {
          let reads = Arc::clone(&reads);
          async move {
            let read = reads.fetch_add(1, Ordering::SeqCst);
            json(format!(r#"{{"read":{read}}}"#))
          }
        }),
      )
      .route(
        "/page",
        get(|| async {
          Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from("<p>page</p>"))
            .expect("an HTML response builds")
        }),
      )
      .layer(middleware::from_fn(layer))
  }

  async fn get_with(app: &Router, uri: &str, if_none_match: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = HttpRequest::builder().method(Method::GET).uri(uri);
    if let Some(tag) = if_none_match {
      builder = builder.header(header::IF_NONE_MATCH, tag);
    }
    let response = app.clone().oneshot(builder.body(Body::empty()).unwrap()).await.expect("the router answers");
    let (parts, body) = response.into_parts();
    (parts.status, parts.headers, to_bytes(body, usize::MAX).await.unwrap().to_vec())
  }

  #[tokio::test]
  async fn a_json_get_is_tagged_by_its_body_and_revalidates_to_304() {
    let fixed = Router::new()
      .route("/api/items", get(|| async { json(r#"{"a":1}"#.to_string()) }))
      .layer(middleware::from_fn(layer));
    let (status, headers, body) = get_with(&fixed, "/api/items", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, br#"{"a":1}"#);
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    let tag = headers[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(tag, entity_tag(&body));

    let (status, headers, body) = get_with(&fixed, "/api/items", Some(&tag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(headers[header::ETAG], tag.as_str());
    assert_eq!(headers[header::VARY], "Accept-Encoding", "the 304 keeps the stored response's representation list");
  }

  #[tokio::test]
  async fn a_changed_body_answers_its_new_tag_and_a_stale_tag_gets_the_body() {
    let app = app(Arc::new(AtomicUsize::new(0)));
    let (_, first, _) = get_with(&app, "/api/items", None).await;
    let stale = first[header::ETAG].to_str().unwrap().to_string();
    let (status, headers, body) = get_with(&app, "/api/items", Some(&stale)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, br#"{"read":1}"#);
    assert_ne!(headers[header::ETAG], stale.as_str());
  }

  #[tokio::test]
  async fn responses_outside_a_json_api_get_carry_no_entity_tag() {
    let app = app(Arc::new(AtomicUsize::new(0)));
    let (status, headers, _) = get_with(&app, "/page", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::ETAG));
    assert!(!headers.contains_key(header::CACHE_CONTROL));
  }
}
