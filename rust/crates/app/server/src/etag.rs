//! Entity tags for the JSON API's reads.
//!
//! A `GET` under `/api/` that answers `200` with a JSON body carries `ETag: "<hash of the body>"` and
//! `Cache-Control: no-cache`. A request whose `If-None-Match` names that tag gets `304` with no body. The tag is hashed
//! from the body, and a body a handler answers from a memo keeps the tag of its first hash, so a repeat request whose
//! handler reuses one body does not pay the hash again. The gzip layer, outside this one, memoizes the compressed body
//! of each tag, which is the same identity a client's cache uses.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
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
  let tag_value = TAG_MEMO.lock().expect("the tag memo mutex is not poisoned").tag(&bytes);
  let tag = tag_value.to_str().expect("a quoted hex entity tag is a valid header value");
  parts.headers.insert(header::ETAG, tag_value.clone());
  parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  if names(&if_none_match, tag) {
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

/// The tags of the bodies this layer hashed, keyed by the body's identity: its address and its length.
///
/// The compact listing and a session's bootstrap answer every request from the same `Bytes`, and hashing a body of a
/// few hundred kilobytes costs more than the rest of the request. An entry holds the body it hashed, so the address it
/// keys on stays that body's alone: the allocator cannot hand the same range to another body while the entry lives.
struct TagMemo {
  entries: VecDeque<TagEntry>,
}

struct TagEntry {
  body: Bytes,
  tag: HeaderValue,
}

impl Default for TagMemo {
  fn default() -> Self {
    Self { entries: VecDeque::new() }
  }
}

/// How many hashed bodies the memo holds, the most bytes they may keep alive together, and the longest body it keeps.
const MEMO_ENTRIES: usize = 4;
const MEMO_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MEMO_MAX_BODY_BYTES: usize = 1024 * 1024;

impl TagMemo {
  /// The tag of `body`: the one this memo hashed for the same body, or a fresh hash kept for the next request.
  fn tag(&mut self, body: &Bytes) -> HeaderValue {
    if let Some(entry) = self.entries.iter().find(|entry| same_body(&entry.body, body)) {
      return entry.tag.clone();
    }
    let tag = HeaderValue::from_str(&entity_tag(body)).expect("a quoted hex entity tag is a valid header value");
    if body.len() <= MEMO_MAX_BODY_BYTES {
      self.entries.push_back(TagEntry { body: body.clone(), tag: tag.clone() });
      while self.entries.len() > MEMO_ENTRIES || self.held_bytes() > MEMO_TOTAL_BYTES {
        self.entries.pop_front();
      }
    }
    tag
  }

  fn held_bytes(&self) -> usize {
    self.entries.iter().map(|entry| entry.body.len()).sum()
  }
}

/// Whether `current` is the body `held`: one nonempty range at one address. The memo holds `held`, so the address
/// cannot belong to a later body, and an equal address with an equal length is the same bytes.
fn same_body(held: &Bytes, current: &Bytes) -> bool {
  !current.is_empty() && held.len() == current.len() && held.as_ptr() == current.as_ptr()
}

static TAG_MEMO: LazyLock<Mutex<TagMemo>> = LazyLock::new(|| Mutex::new(TagMemo::default()));

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

  fn json_bytes(body: Bytes) -> Response {
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

  #[test]
  fn the_memo_answers_the_tag_it_hashed_for_the_body_it_holds() {
    let mut memo = TagMemo::default();
    let body = Bytes::from(vec![b'x'; 2048]);
    let first = memo.tag(&body);
    assert_eq!(memo.tag(&body), first, "the held body answers the tag its first hash gave");
    let other = Bytes::from(vec![b'y'; body.len()]);
    assert_ne!(memo.tag(&other), first, "another body hashes to its own tag");
  }

  #[test]
  fn a_body_past_the_memo_cap_keeps_no_entry() {
    let mut memo = TagMemo::default();
    let big = Bytes::from(vec![0u8; MEMO_MAX_BODY_BYTES + 1]);
    let first = memo.tag(&big);
    assert_eq!(memo.tag(&big), first, "equal bytes answer an equal tag");
    assert!(memo.entries.is_empty(), "a body past the cap is hashed on every request");
  }

  #[test]
  fn the_memo_drops_the_entry_stored_longest_ago_past_its_count() {
    let mut memo = TagMemo::default();
    let bodies: Vec<Bytes> = (0..=MEMO_ENTRIES).map(|index| Bytes::from(vec![index as u8; 64])).collect();
    for body in &bodies {
      memo.tag(body);
    }
    assert_eq!(memo.entries.len(), MEMO_ENTRIES);
    assert!(!memo.entries.iter().any(|entry| same_body(&entry.body, &bodies[0])), "the oldest entry is dropped");
    assert!(memo.entries.iter().any(|entry| same_body(&entry.body, &bodies[MEMO_ENTRIES])));
  }

  #[tokio::test]
  async fn a_repeat_response_over_one_held_body_carries_one_tag() {
    // A handler that answers from a memo returns the same `Bytes` every request. The layer holds the body it hashed,
    // so the repeat's tag comes from that body and no other allocation can claim its address.
    let held = Bytes::from(vec![b'z'; 2048]);
    let body = held.clone();
    let app = Router::new()
      .route(
        "/api/held",
        get(move || {
          let body = body.clone();
          async move { json_bytes(body) }
        }),
      )
      .layer(middleware::from_fn(layer));
    let (_, first, _) = get_with(&app, "/api/held", None).await;
    let (_, second, _) = get_with(&app, "/api/held", None).await;
    assert_eq!(first[header::ETAG], second[header::ETAG]);
    let memo = TAG_MEMO.lock().expect("the tag memo mutex is not poisoned");
    assert!(memo.entries.iter().any(|entry| same_body(&entry.body, &held)), "the layer keeps the body it hashed");
  }
}
