//! The chat page, home page and per-session events viewer (`src/app/pages.py`).

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, StatusCode, Uri, header, request::Parts};
use axum::response::Response;
use bytes::Bytes;
use charliebot_cli_common::route;
use charliebot_compat::compression::gzip_level_one;
use charliebot_compat::responses::{self, Failure};
use charliebot_compat::routing::get_only;
use charliebot_core::config::{CharlieBotConfig, HomeService};
use charliebot_core::paths;
use charliebot_registry::Registry;
use charliebot_registry::templating::Templating;
use charliebot_session_api::api as session_routes;
use charliebot_session_api::{detail, listing};
use charliebot_store::{StoreError, StoreResult};
use futures_util::future::join_all;
use percent_encoding::percent_decode_str;
use serde_json::{Map, Value, json};

use super::family::Family;
use crate::gzip::{self, Responder};
use crate::state::AppState;

pub const FAMILY: Family = Family { routes, matches: |_, path| is_page_path(path) };

fn routes() -> Router<Arc<AppState>> {
  Router::new()
    .route("/", get_only(index))
    .route("/home", get_only(home_page))
    .route("/sessions/{session_id}/events", get_only(events_viewer))
}

fn is_page_path(path: &str) -> bool {
  if path == "/" || path == "/home" {
    return true;
  }
  let Some(rest) = path.strip_prefix("/sessions/") else { return false };
  let Some((session_id, page)) = rest.split_once('/') else { return false };
  !session_id.is_empty() && page == "events"
}

/// The chat page's sidebar outcome: the redirect of a page opened without a session, or the rendered rows as text.
enum PageListing {
  Redirect(String),
  Text(Arc<String>),
}

/// Stands in for the chat page's sidebar JSON inside the template.
///
/// The sidebar text is far larger than its template, and the template engine copies every value it writes, so the
/// template writes this placeholder and the finished page substitutes the text once. JSON escapes control characters,
/// so the sidebar text itself never carries this placeholder.
const INITIAL_SESSIONS_PLACEHOLDER: &str = "\u{1}charliebot-initial-sessions\u{1}";

/// The chat page's rendered row texts, reused across requests while a row's inputs are unchanged.
static PAGE_ROWS_MEMO: LazyLock<listing::PageRowsMemo> = LazyLock::new(listing::PageRowsMemo::default);

/// How many finished page bodies the gzip memo keeps.
///
/// One page body is about as large as the sidebar it carries, so the limit bounds the memo to a few times one page.
const PAGE_GZIP_MEMO_LIMIT: usize = 2;

/// The body size from which keeping the compressed form is worth the memory.
///
/// The chat page is about 9 MB and pays for its compression on every request. The home page and the events viewer
/// are a few kilobytes, so compressing them again costs less than holding their bodies would.
const PAGE_GZIP_MEMO_MINIMUM_BYTES: usize = 1024 * 1024;

/// The compressed form of the page bodies this server served recently.
///
/// The chat page is about 9 MB and the transport gzip compresses each response on its own request, so a repeat
/// request pays the whole compression again for a page that did not change. The key is the finished body itself: a
/// page that changed never matches, so the memo needs no invalidation and a template, page part or template
/// directory edited on disk still takes effect at the next request.
#[derive(Default)]
struct PageGzipMemo {
  entries: Vec<(Bytes, Bytes)>,
}

impl PageGzipMemo {
  /// The gzip form of `body`, compressing it when no entry matches and remembering the bodies large enough to keep.
  fn gzip_body(&mut self, body: &Bytes) -> Bytes {
    if let Some((_, compressed)) = self.entries.iter().find(|(cached, _)| cached == body) {
      return compressed.clone();
    }
    let compressed = Bytes::from(gzip_level_one(body));
    if body.len() >= PAGE_GZIP_MEMO_MINIMUM_BYTES {
      self.entries.push((body.clone(), compressed.clone()));
      if self.entries.len() > PAGE_GZIP_MEMO_LIMIT {
        self.entries.remove(0);
      }
    }
    compressed
  }
}

static PAGE_GZIP_MEMO: LazyLock<Mutex<PageGzipMemo>> = LazyLock::new(|| Mutex::new(PageGzipMemo::default()));

async fn index(State(state): State<Arc<AppState>>, request: Parts) -> Response {
  let cfg = match state.config() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  let credentials = match state.credentials() {
    Ok(credentials) => credentials,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  let session = session_query(&request);
  let mut load_errors = Vec::new();
  let mut active_session = Value::Null;
  let mut pending_draft = Value::Null;
  let mut event_count = Value::from(0);
  let mut session_bootstrap = Value::Null;
  let mut clear_unread = None;
  if let Some(session_id) = session.as_deref().filter(|session_id| !session_id.is_empty()) {
    match state.features.store.get_session(session_id).await {
      Ok(Some(meta)) => {
        active_session = state.features.store.session_metadata_value(&meta);
        let store = Arc::clone(&state.features.store);
        let registry = Arc::clone(&state.features.registry);
        let bootstrap_cfg = Arc::clone(&cfg);
        let bootstrap = tokio::task::spawn_blocking(move || {
          detail::build_bootstrap_payload(&store, &registry, &bootstrap_cfg, &meta)
        })
        .await
        .map_err(|error| charliebot_store::StoreError::new(error.to_string()))
        .and_then(|result| result);
        match bootstrap {
          Ok(bootstrap) => {
            let bootstrap_session = bootstrap.get("session").expect("a bootstrap payload has a session");
            let bootstrap_session = bootstrap_session.as_object().expect("a bootstrap session is an object");
            let active = active_session.as_object_mut().expect("session metadata serializes to an object");
            for name in ["run_backend", "thinking_since"] {
              if let Some(value) = bootstrap_session.get(name) {
                active.insert(name.to_string(), value.clone());
              }
            }
            pending_draft = bootstrap.get("pending_draft").expect("a bootstrap payload has a pending draft").clone();
            event_count = bootstrap.get("event_count").expect("a bootstrap payload has an event count").clone();
            session_bootstrap = bootstrap;
            // As Python's index: the active row is marked read only when its bootstrap loaded; a failed load keeps
            // the unread mark.
            clear_unread = Some(session_id.to_string());
          }
          Err(error) => {
            tracing::error!(session_id, error = %error, "load_session_data_failed");
            load_errors.push("Failed to load session data. Check server logs for details.".to_string());
          }
        }
      }
      Ok(None) => {}
      Err(error) => tracing::error!(session_id, error = %error, "get_session_failed"),
    }
  }

  let features = Arc::clone(&state.features);
  let redirect_to_first = session.is_none();
  let listing_result = tokio::task::spawn_blocking(move || -> StoreResult<PageListing> {
    // As Python's index: a page opened without a session redirects to the first row before any row renders.
    if redirect_to_first && let Some(first) = listing::first_page_row_id(&features.store)? {
      return Ok(PageListing::Redirect(first));
    }
    let page = listing::active_page_rows(&features.store, &features.registry)?;
    listing::active_page_json_text(&features.store, &page, clear_unread.as_deref(), &PAGE_ROWS_MEMO)
      .map(PageListing::Text)
  })
  .await
  .expect("the chat page listing task does not panic");
  let initial_sessions_json = match listing_result {
    Ok(PageListing::Redirect(session_id)) => return responses::redirect(&format!("/?session={session_id}")),
    Ok(PageListing::Text(text)) => text,
    Err(error) => {
      tracing::error!(error = %error, "list_sessions_failed");
      load_errors.push("Failed to load sessions. Check server logs for details.".to_string());
      Arc::new("[]".to_string())
    }
  };

  let default_backend_id = cfg.backends.options.first().map_or("claude", |option| option.id.as_str());
  let active_backend = active_session
    .get("run_backend")
    .and_then(Value::as_str)
    .filter(|backend| !backend.is_empty())
    .or_else(|| active_session.get("backend").and_then(Value::as_str).filter(|backend| !backend.is_empty()))
    .unwrap_or(default_backend_id);
  let active_option = cfg.get_backend_option(active_backend);
  let context = json!({
    "initial_sessions_json": INITIAL_SESSIONS_PLACEHOLDER,
    "active_session": active_session,
    "pending_draft": pending_draft,
    "event_count": event_count,
    "session_bootstrap": session_bootstrap,
    "backend_options": cfg.backends.options
      .iter()
      .map(|option| json!({"id": option.id, "label": option.label, "type": option.backend_type}))
      .collect::<Vec<_>>(),
    "backend_compact": match compact_facts(&cfg, &state.features.registry) {
      Ok(facts) => Value::Object(facts),
      Err(error) => return responses::internal_error(&error.kind, error.detail),
    },
    "default_backend_id": default_backend_id,
    "active_backend": active_backend,
    "active_backend_label": active_option.map_or(active_backend, |option| option.label.as_str()),
    "active_backend_type": active_option.map_or("", |option| option.backend_type.as_str()),
    "load_errors": load_errors,
  });
  render_page(
    state,
    request,
    "index.html",
    context,
    PageRuntime::Index { cfg, auth_enabled: credentials.access_key().is_some(), sidebar_text: initial_sessions_json },
  )
  .await
}

async fn home_page(State(state): State<Arc<AppState>>, request: Parts) -> Response {
  let cfg = match state.config() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  let probes = cfg.ui.home_services.iter().map(|service| {
    let url = service.url.clone();
    tokio::task::spawn_blocking(move || probe_home_service(&url))
  });
  let statuses = join_all(probes)
    .await
    .into_iter()
    .map(|probe| probe.expect("a home service probe does not panic"))
    .collect::<Vec<_>>();
  let services =
    cfg.ui.home_services.iter().zip(statuses).map(|(service, up)| home_service_value(service, up)).collect::<Vec<_>>();
  let mut destinations = vec![json!({
    "name": "Chat",
    "url": "/",
    "description": "The CharlieBot chat and session UI."
  })];
  destinations.extend(
    state
      .features
      .registry
      .home_cards()
      .iter()
      .map(|card| json!({"name": card.name, "url": card.url, "description": card.description})),
  );
  render_page(
    state,
    request,
    "home.html",
    json!({"destinations": destinations, "services": services}),
    PageRuntime::Home,
  )
  .await
}

async fn events_viewer(
  State(state): State<Arc<AppState>>,
  AxumPath(session_id): AxumPath<String>,
  request: Parts,
) -> Response {
  let session = match state.features.store.get_session(&session_id).await {
    Ok(Some(session)) => session,
    Ok(None) | Err(StoreError::NotFound(_)) => return session_not_found(),
    Err(error) => {
      tracing::error!(session_id, error = %error, "get_session_failed");
      return responses::json(StatusCode::INTERNAL_SERVER_ERROR, &json!({"detail": "Failed to load session"}));
    }
  };
  let session = state.features.store.session_metadata_value(&session);
  let context = json!({
    "session": session,
    "session_id": session_id,
    "events_url": route::fill(session_routes::SESSION_EVENTS_JSONL_PATH, &[session_id.as_str()]),
  });
  render_page(state, request, "events_viewer.html", context, PageRuntime::Events).await
}

enum PageRuntime {
  Index { cfg: Arc<CharlieBotConfig>, auth_enabled: bool, sidebar_text: Arc<String> },
  Home,
  Events,
}

impl PageRuntime {
  /// The values the page carries as placeholders, as `(placeholder, text)` pairs for the finished page.
  fn substitutions(&self) -> Vec<(&'static str, &str)> {
    match self {
      Self::Index { sidebar_text, .. } => vec![(INITIAL_SESSIONS_PLACEHOLDER, sidebar_text.as_str())],
      Self::Home | Self::Events => Vec::new(),
    }
  }
}

async fn render_page(
  state: Arc<AppState>,
  request: Parts,
  name: &'static str,
  mut context: Value,
  runtime: PageRuntime,
) -> Response {
  let accepts_gzip = Responder::for_request(&request.headers) == Responder::Gzip;
  let rendered = tokio::task::spawn_blocking(move || {
    let templating =
      state.features.startup.require::<Templating>().expect("run() inserts the Templating before the server binds");
    let hostname = hostname()?;
    let object = context.as_object_mut().expect("a page context is a JSON object");
    object.insert("hostname".to_string(), Value::String(hostname));
    match &runtime {
      PageRuntime::Index { cfg, auth_enabled, .. } => {
        let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
        object.insert("sessions_root".to_string(), Value::String(paths::display(&cfg.sessions_dir())));
        object.insert("version".to_string(), Value::String(templating.git_version().to_string()));
        object.insert("static_asset_version".to_string(), Value::String(asset_version));
        object.insert("auth_enabled".to_string(), Value::Bool(*auth_enabled));
      }
      PageRuntime::Home => {}
      PageRuntime::Events => {
        let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
        object.insert("static_asset_version".to_string(), Value::String(asset_version));
      }
    }
    templating
      .render_page_html(&state.features, name, &context, &request, &runtime.substitutions())
      .map_err(|error| failure(error.kind(), error))
  })
  .await
  .expect("rendering a page does not panic");
  match rendered {
    Ok(html) => page_response(html, accepts_gzip),
    Err(error) => responses::internal_error(&error.kind, error.detail),
  }
}

/// Wrap finished page HTML in the response, answering a gzip client from the memoized compressed body.
///
/// For a body at or above the transport's minimum size, the compressed form travels with its own headers here
/// instead of under the transport gzip; the transport passes a response that already sets `Content-Encoding`
/// through untouched.
fn page_response(html: String, accepts_gzip: bool) -> Response {
  let body = Bytes::from(html);
  let compressed = (accepts_gzip && body.len() >= gzip::MINIMUM_SIZE)
    .then(|| PAGE_GZIP_MEMO.lock().expect("the page gzip memo mutex is not poisoned").gzip_body(&body));
  let mut builder = Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, "text/html; charset=utf-8");
  let served = match compressed {
    Some(compressed) => {
      builder = builder
        .header(header::CONTENT_LENGTH, compressed.len())
        .header(header::CONTENT_ENCODING, "gzip")
        .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
      compressed
    }
    None => {
      builder = builder.header(header::CONTENT_LENGTH, body.len());
      body
    }
  };
  let mut response = builder.body(Body::from(served)).expect("a page response builds");
  // `no-store` sends every back navigation and tab restore to the server, so the page never runs JavaScript from
  // a cached copy that references an older static asset version.
  response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
  response
}

fn home_service_value(service: &HomeService, up: bool) -> Value {
  json!({
    "name": service.name,
    "description": service.description,
    "url": service.url,
    "status": if up { "up" } else { "down" },
  })
}

fn probe_home_service(url: &str) -> bool {
  let Ok(parsed) = url.parse::<Uri>() else { return false };
  let Some(host) = parsed.host() else { return false };
  let port = parsed.port_u16().unwrap_or(if parsed.scheme_str() == Some("https") { 443 } else { 80 });
  let Ok(addresses) = (host, port).to_socket_addrs() else { return false };
  addresses.into_iter().any(|address| TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok())
}

fn session_query(request: &Parts) -> Option<String> {
  let mut session = None;
  for part in request.uri.query().unwrap_or_default().split('&').filter(|part| !part.is_empty()) {
    let (name, value) = part.split_once('=').unwrap_or((part, ""));
    if decode_query_component(name) == "session" {
      session = Some(decode_query_component(value));
    }
  }
  session
}

fn decode_query_component(value: &str) -> String {
  percent_decode_str(&value.replace('+', " ")).decode_utf8_lossy().into_owned()
}

fn session_not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Session not found"}))
}

fn hostname() -> Result<String, Failure> {
  nix::unistd::gethostname()
    .map_err(|error| failure("OSError", error))?
    .into_string()
    .map_err(|name| failure("UnicodeDecodeError", format!("host name is not UTF-8: {}", name.to_string_lossy())))
}

fn failure(kind: &str, detail: impl ToString) -> Failure {
  Failure { kind: kind.to_string(), detail: detail.to_string() }
}

/// `_compact_facts`: per configured backend type, whether manual compaction runs and the hint.
fn compact_facts(cfg: &CharlieBotConfig, registry: &Registry) -> Result<Map<String, Value>, Failure> {
  let mut facts = Map::new();
  for option in &cfg.backends.options {
    let registration = registry
      .backend_type_of(&option.backend_type)
      .ok_or_else(|| failure("KeyError", format!("backend type {:?} is not registered", option.backend_type)))?;
    let traits = registration.traits;
    facts.insert(option.backend_type.clone(), json!({"manual": traits.manual_compact, "hint": traits.compact_hint}));
  }
  Ok(facts)
}

#[cfg(test)]
mod tests {
  use super::*;
  use flate2::read::GzDecoder;
  use http_body_util::BodyExt;
  use std::io::Read;

  fn gunzip(body: &[u8]) -> String {
    let mut decoded = String::new();
    GzDecoder::new(body).read_to_string(&mut decoded).expect("a gzip page body decodes");
    decoded
  }

  /// A page body that grew pages past the transport's minimum size.
  fn page_html() -> String {
    "a chat page body ".repeat(200)
  }

  #[test]
  fn a_repeat_page_body_keeps_its_own_compressed_bytes() {
    let mut memo = PageGzipMemo::default();
    let first = Bytes::from("a".repeat(PAGE_GZIP_MEMO_MINIMUM_BYTES));
    let second = Bytes::from("b".repeat(PAGE_GZIP_MEMO_MINIMUM_BYTES));
    let compressed = memo.gzip_body(&first);
    assert_eq!(memo.gzip_body(&first), compressed);
    assert_eq!(gunzip(&compressed), String::from_utf8(first.to_vec()).expect("the body is text"));
    let other = memo.gzip_body(&second);
    assert_ne!(other, compressed);
    assert_eq!(gunzip(&other), String::from_utf8(second.to_vec()).expect("the body is text"));
  }

  #[test]
  fn a_small_page_is_compressed_but_not_kept() {
    let mut memo = PageGzipMemo::default();
    let small = Bytes::from("the events viewer of one session");
    assert_eq!(gunzip(&memo.gzip_body(&small)), "the events viewer of one session");
    assert!(memo.entries.is_empty());
  }

  #[tokio::test]
  async fn a_gzip_client_gets_the_compressed_page_with_the_transport_headers() {
    let html = page_html();
    let (parts, body) = page_response(html.clone(), true).into_parts();
    let headers = parts.headers;
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
    assert_eq!(headers[header::VARY], "Accept-Encoding");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let compressed = body.collect().await.expect("a page body collects").to_bytes();
    assert_eq!(
      headers[header::CONTENT_LENGTH].to_str().expect("a length header is text"),
      compressed.len().to_string()
    );
    assert_eq!(gunzip(&compressed), html);
  }

  #[tokio::test]
  async fn an_identity_client_gets_the_page_uncompressed() {
    let html = page_html();
    let (parts, body) = page_response(html.clone(), false).into_parts();
    let headers = parts.headers;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    let served = body.collect().await.expect("a page body collects").to_bytes();
    assert_eq!(headers[header::CONTENT_LENGTH].to_str().expect("a length header is text"), served.len().to_string());
    assert_eq!(served, Bytes::from(html));
  }
}
