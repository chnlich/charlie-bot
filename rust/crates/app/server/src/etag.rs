//! Entity tags for the JSON API's reads and for the pages.
//!
//! A `GET` under `/api/` that answers `200` with a JSON body carries `ETag: "<hash of the body>"` and
//! `Cache-Control: no-cache`. A request whose `If-None-Match` names that tag gets `304` with no body. The tag is hashed
//! from the body, and a body a handler answers from a memo on every request keeps the tag of its first hash, so such a
//! repeat request does not pay the hash again. The gzip layer, outside this one, memoizes the compressed body of each
//! tag, which is the same identity a client's cache uses.
//!
//! A page marks its own body `no-store`, because a page must never come out of a cache: it names the static asset
//! version it was built against, and an older body runs an older script. The layer replaces that marker with
//! `no-cache` and the body's tag, so every navigation still reaches the server — a back/forward-cache restore reloads
//! from the page prologue — and a body the browser already holds unchanged costs a `304` instead of a transfer.
//!
//! A handler that answers its own `ETag` has named the body's version, so the layer keeps that tag and skips the hash.
//! The tag makes the same promise either way: two bodies share it only when they are the same body.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use charliebot_compat::headers::header_text;
use charliebot_compat::mimetypes_table::TEXT_HTML;
use charliebot_compat::responses;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use sha2::{Digest, Sha256};

/// The largest body the layer reads whole to hash it. Every JSON and page handler builds its body in memory first, so
/// the bound stops a runaway body rather than shaping a normal response.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The middleware as an axum layer.
pub async fn layer(request: Request, next: Next) -> Response {
  let get = request.method() == Method::GET;
  let api = request.uri().path().starts_with("/api/");
  let if_none_match = header_text(request.headers(), &header::IF_NONE_MATCH);
  let response = next.run(request).await;
  let taggable = (api && is_json(response.headers())) || is_page(response.headers());
  if !get || response.status() != StatusCode::OK || !taggable {
    return response;
  }
  let (mut parts, body) = response.into_parts();
  // A handler that names its body's version has already paid for that identity, and its body may be cheaper to name
  // than to hash. The supplied tag answers `If-None-Match` like a hashed one.
  let (tag_value, body) = match parts.headers.get(header::ETAG).cloned() {
    Some(tag) => (tag, body),
    None => {
      let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
      };
      let tag = TAG_MEMO.lock().expect("the tag memo mutex is not poisoned").tag(&bytes);
      (tag, Body::from(bytes))
    }
  };
  let tag = tag_value.to_str().expect("an entity tag is visible ASCII");
  parts.headers.insert(header::ETAG, tag_value.clone());
  parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  if names(&if_none_match, tag) {
    return not_modified(tag_value, &parts.headers);
  }
  Response::from_parts(parts, body)
}

fn is_json(headers: &HeaderMap) -> bool {
  header_text(headers, &header::CONTENT_TYPE).starts_with("application/json")
}

/// Whether the response is a page: an HTML body the page layer marked `no-store`.
fn is_page(headers: &HeaderMap) -> bool {
  header_text(headers, &header::CONTENT_TYPE).starts_with(TEXT_HTML)
    && header_text(headers, &header::CACHE_CONTROL) == "no-store"
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
/// The compact listing's gzip form and a session's bootstrap answer every request from the same `Bytes`, and hashing a
/// body of a few hundred kilobytes costs more than the rest of the request. A handler that builds its body again per
/// request allocates a new one, so its body hashes again. An entry holds the body it hashed, so the address it keys on
/// stays that body's alone: the allocator cannot hand the same range to another body while the entry lives.
struct TagMemo {
  entries: VecDeque<TagEntry>,
  max_body_bytes: usize,
}

struct TagEntry {
  body: Bytes,
  tag: HeaderValue,
}

/// How many hashed bodies the memo holds, the most bytes they may keep alive together, and the longest body it keeps.
const MEMO_ENTRIES: usize = 4;
const MEMO_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MEMO_MAX_BODY_BYTES: usize = 1024 * 1024;

impl Default for TagMemo {
  fn default() -> Self {
    TagMemo::with_max_body_bytes(MEMO_MAX_BODY_BYTES)
  }
}

impl TagMemo {
  fn with_max_body_bytes(max_body_bytes: usize) -> TagMemo {
    TagMemo { entries: VecDeque::new(), max_body_bytes }
  }

  /// The tag of `body`: the one this memo hashed for the same body, or a fresh hash kept for the next request.
  fn tag(&mut self, body: &Bytes) -> HeaderValue {
    if let Some(entry) = self.entries.iter().find(|entry| same_body(&entry.body, body)) {
      return entry.tag.clone();
    }
    let tag = HeaderValue::from_str(&entity_tag(body)).expect("a quoted hex entity tag is a valid header value");
    if body.len() <= self.max_body_bytes {
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
/// specifies for this header, so a `W/` prefix on either side does not change the match.
fn names(if_none_match: &str, tag: &str) -> bool {
  let tag = tag.trim().trim_start_matches("W/");
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
      .route(
        "/no-store-page",
        get(|| async {
          Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::VARY, "Accept-Encoding")
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

  #[tokio::test]
  async fn a_page_marked_no_store_revalidates_and_no_longer_forbids_storing() {
    let app = app(Arc::new(AtomicUsize::new(0)));
    let (status, headers, body) = get_with(&app, "/no-store-page", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"<p>page</p>");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache", "the marker gives way to the body's tag");
    let tag = headers[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(tag, entity_tag(&body));

    let (status, headers, body) = get_with(&app, "/no-store-page", Some(&tag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(headers[header::ETAG], tag.as_str());
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers[header::VARY], "Accept-Encoding", "the 304 keeps the stored response's representation list");
  }

  #[tokio::test]
  async fn a_handler_that_names_its_body_keeps_its_tag_and_revalidates_with_it() {
    let app = Router::new()
      .route(
        "/api/named",
        get(|| async {
          Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, "\"handler-1\"")
            .body(Body::from(r#"{"n":1}"#))
            .expect("a JSON response builds")
        }),
      )
      .layer(middleware::from_fn(layer));
    let (status, headers, body) = get_with(&app, "/api/named", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], "\"handler-1\"", "the handler's own tag stays");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(body, br#"{"n":1}"#);

    let (status, headers, body) = get_with(&app, "/api/named", Some("\"handler-1\"")).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "a client that names the handler's tag revalidates");
    assert_eq!(headers[header::ETAG], "\"handler-1\"");
    assert!(body.is_empty());
  }

  #[tokio::test]
  async fn a_weak_tag_matches_a_weak_or_strong_client_candidate() {
    let app = Router::new()
      .route(
        "/api/weak",
        get(|| async {
          Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, "W/\"weak-1\"")
            .body(Body::from(r#"{"n":1}"#))
            .expect("a JSON response builds")
        }),
      )
      .layer(middleware::from_fn(layer));
    for candidate in ["W/\"weak-1\"", "\"weak-1\""] {
      let (status, headers, body) = get_with(&app, "/api/weak", Some(candidate)).await;
      assert_eq!(
        status,
        StatusCode::NOT_MODIFIED,
        "the weak comparison ignores the prefix on either side: {candidate}"
      );
      assert_eq!(headers[header::ETAG], "W/\"weak-1\"", "the stored tag comes back unchanged");
      assert!(body.is_empty());
    }
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
    let mut memo = TagMemo::with_max_body_bytes(64);
    let big = Bytes::from(vec![0u8; 65]);
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
  async fn a_handler_that_reuses_one_body_answers_one_tag() {
    // A handler that answers from a memo returns the same `Bytes` every request, so both answers carry one tag.
    let body = Bytes::from(vec![b'z'; 2048]);
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
  }
}
