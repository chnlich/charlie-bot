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
use charliebot_registry::templating::{self, Templating};
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

/// Stands in for the page's session bootstrap JSON, as [`INITIAL_SESSIONS_PLACEHOLDER`] does for the sidebar text.
///
/// The bootstrap is the page's other large value, and the page writes it as JSON text; the finished page substitutes
/// the text once. JSON escapes control characters, so the bootstrap text never carries this placeholder.
const SESSION_BOOTSTRAP_PLACEHOLDER: &str = "\u{2}charliebot-session-bootstrap\u{2}";

/// The text a page without a session bootstrap carries: JSON's `null`.
const NO_SESSION_BOOTSTRAP: &str = "null";

/// The chat page's rendered row texts, reused across requests while a row's inputs are unchanged.
static PAGE_ROWS_MEMO: LazyLock<listing::PageRowsMemo> = LazyLock::new(listing::PageRowsMemo::default);

/// The session bootstrap one page request built last, reused while every payload input is unchanged.
static PAGE_BOOTSTRAP_MEMO: LazyLock<detail::PageBootstrapMemo> = LazyLock::new(detail::PageBootstrapMemo::default);

/// The finished chat page body one request built last, kept while the template output and both substituted texts
/// still hold their value.
///
/// The page carries the sidebar text and, when a session is open, that session's bootstrap: a build copies about
/// nine megabytes and, for the largest session, about another hundred and seventy. A repeat request would copy and
/// compare those bytes again, so the memo serves the body it already built. The rendered template is part of the
/// key: a template, page part or template directory edited on disk renders different bytes and never matches. The
/// two texts compare by identity, and the entry keeps their `Arc`s alive, so an address the entry holds is never
/// handed to a different text.
#[derive(Default)]
struct PageBodyMemo {
  entry: Option<PageBodyEntry>,
}

struct PageBodyEntry {
  template_html: String,
  sidebar_text: Arc<String>,
  bootstrap_text: Option<Arc<String>>,
  body: Bytes,
  gzip: Option<Bytes>,
}

impl PageBodyMemo {
  /// The finished page body, from the memo when every input still holds its value.
  fn body(
    &mut self,
    name: &str,
    template_html: &str,
    substitutions: &[(&str, &str)],
    sidebar_text: &Arc<String>,
    bootstrap_text: Option<&Arc<String>>,
    accepts_gzip: bool,
  ) -> Result<(Bytes, Option<Bytes>), Failure> {
    let matches = self.entry.as_ref().is_some_and(|entry| {
      entry.template_html == template_html
        && Arc::ptr_eq(&entry.sidebar_text, sidebar_text)
        && match (entry.bootstrap_text.as_ref(), bootstrap_text) {
          (Some(entry), Some(current)) => Arc::ptr_eq(entry, current),
          (None, None) => true,
          _ => false,
        }
    });
    if !matches {
      let html = templating::substitute_placeholders(name, template_html.to_string(), substitutions)
        .map_err(|error| failure(error.kind(), error))?;
      self.entry = Some(PageBodyEntry {
        template_html: template_html.to_string(),
        sidebar_text: Arc::clone(sidebar_text),
        bootstrap_text: bootstrap_text.cloned(),
        body: Bytes::from(html),
        gzip: None,
      });
    }
    let entry = self.entry.as_mut().expect("the page body entry matched or was built");
    let body = entry.body.clone();
    let gzip = if accepts_gzip && body.len() >= gzip::MINIMUM_SIZE {
      Some(match &entry.gzip {
        Some(compressed) => compressed.clone(),
        None => {
          let compressed = Bytes::from(gzip_level_one(&body));
          entry.gzip = Some(compressed.clone());
          compressed
        }
      })
    } else {
      None
    };
    Ok((body, gzip))
  }
}

static PAGE_BODY_MEMO: LazyLock<Mutex<PageBodyMemo>> = LazyLock::new(|| Mutex::new(PageBodyMemo::default()));

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
  let mut bootstrap_text = None;
  let mut clear_unread = None;
  if let Some(session_id) = session.as_deref().filter(|session_id| !session_id.is_empty()) {
    match state.features.store.get_session(session_id).await {
      Ok(Some(meta)) => {
        active_session = state.features.store.session_metadata_value(&meta);
        let store = Arc::clone(&state.features.store);
        let registry = Arc::clone(&state.features.registry);
        let bootstrap_cfg = Arc::clone(&cfg);
        let bootstrap = tokio::task::spawn_blocking(move || {
          detail::page_bootstrap(&PAGE_BOOTSTRAP_MEMO, &store, &registry, &bootstrap_cfg, &meta)
        })
        .await
        .map_err(|error| charliebot_store::StoreError::new(error.to_string()))
        .and_then(|result| result);
        match bootstrap {
          Ok(bootstrap) => {
            let bootstrap_session = bootstrap.session.as_object().expect("a bootstrap session is an object");
            let active = active_session.as_object_mut().expect("session metadata serializes to an object");
            for name in ["run_backend", "thinking_since"] {
              if let Some(value) = bootstrap_session.get(name) {
                active.insert(name.to_string(), value.clone());
              }
            }
            pending_draft = bootstrap.pending_draft;
            event_count = bootstrap.event_count;
            bootstrap_text = Some(bootstrap.text);
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
    "session_bootstrap_json": SESSION_BOOTSTRAP_PLACEHOLDER,
    "active_session": active_session,
    "pending_draft": pending_draft,
    "event_count": event_count,
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
    PageRuntime::Index {
      cfg,
      auth_enabled: credentials.access_key().is_some(),
      sidebar_text: initial_sessions_json,
      bootstrap_text,
    },
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
  Index {
    cfg: Arc<CharlieBotConfig>,
    auth_enabled: bool,
    sidebar_text: Arc<String>,
    /// The session bootstrap JSON text, or `None` for a page without one.
    bootstrap_text: Option<Arc<String>>,
  },
  Home,
  Events,
}

impl PageRuntime {
  /// The values the page carries as placeholders, as `(placeholder, text)` pairs for the finished page.
  fn substitutions(&self) -> Vec<(&'static str, &str)> {
    match self {
      Self::Index { sidebar_text, bootstrap_text, .. } => vec![
        (INITIAL_SESSIONS_PLACEHOLDER, sidebar_text.as_str()),
        (SESSION_BOOTSTRAP_PLACEHOLDER, bootstrap_text.as_deref().map_or(NO_SESSION_BOOTSTRAP, String::as_str)),
      ],
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
      PageRuntime::Home | PageRuntime::Events => {
        let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
        object.insert("static_asset_version".to_string(), Value::String(asset_version));
      }
    }
    let template_html = templating
      .render_page_template(&state.features, name, &context, &request)
      .map_err(|error| failure(error.kind(), error))?;
    page_body(name, &template_html, &runtime, accepts_gzip)
  })
  .await
  .expect("rendering a page does not panic");
  match rendered {
    Ok((body, gzip)) => page_response(body, gzip),
    Err(error) => responses::internal_error(&error.kind, error.detail),
  }
}

/// The finished body of a page, with its compressed form when a gzip client asked for it.
///
/// The chat page keeps its finished body between requests. The home page and the events viewer are a few kilobytes,
/// so compressing them again costs less than holding their bodies would.
fn page_body(
  name: &str,
  template_html: &str,
  runtime: &PageRuntime,
  accepts_gzip: bool,
) -> Result<(Bytes, Option<Bytes>), Failure> {
  match runtime {
    PageRuntime::Index { sidebar_text, bootstrap_text, .. } => PAGE_BODY_MEMO
      .lock()
      .expect("the page body memo mutex is not poisoned")
      .body(name, template_html, &runtime.substitutions(), sidebar_text, bootstrap_text.as_ref(), accepts_gzip),
    PageRuntime::Home | PageRuntime::Events => {
      let html = templating::substitute_placeholders(name, template_html.to_string(), &runtime.substitutions())
        .map_err(|error| failure(error.kind(), error))?;
      let body = Bytes::from(html);
      let gzip = (accepts_gzip && body.len() >= gzip::MINIMUM_SIZE).then(|| Bytes::from(gzip_level_one(&body)));
      Ok((body, gzip))
    }
  }
}

/// Wrap a finished page body in the response, with its compressed form when one is given.
///
/// For a body at or above the transport's minimum size, the compressed form travels with its own headers here
/// instead of under the transport gzip; the transport passes a response that already sets `Content-Encoding`
/// through untouched.
fn page_response(body: Bytes, gzip: Option<Bytes>) -> Response {
  let mut builder = Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, "text/html; charset=utf-8");
  let served = match gzip {
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

  /// A chat page template carrying both placeholders, so the memo's substitution path runs.
  fn index_template() -> String {
    format!(
      "const INITIAL_SESSIONS = {INITIAL_SESSIONS_PLACEHOLDER}; const SESSION_BOOTSTRAP = {SESSION_BOOTSTRAP_PLACEHOLDER};"
    )
  }

  fn substitutions<'a>(sidebar: &'a str, bootstrap: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![(INITIAL_SESSIONS_PLACEHOLDER, sidebar), (SESSION_BOOTSTRAP_PLACEHOLDER, bootstrap)]
  }

  #[test]
  fn a_repeat_page_body_is_served_from_the_memo_until_an_input_changes() {
    let mut memo = PageBodyMemo::default();
    let sidebar = Arc::new("x".repeat(4096));
    let bootstrap = Arc::new("{\"messages\":[]}".to_string());
    let template = index_template();
    let (first, gzip) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), bootstrap.as_str()),
        &sidebar,
        Some(&bootstrap),
        true,
      )
      .unwrap();
    let gzip = gzip.expect("a gzip client gets a compressed body");
    assert_eq!(gunzip(&gzip), String::from_utf8(first.to_vec()).unwrap());
    let (again, again_gzip) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), bootstrap.as_str()),
        &sidebar,
        Some(&bootstrap),
        true,
      )
      .unwrap();
    assert_eq!(again.as_ptr(), first.as_ptr(), "a repeat request serves the body the memo kept");
    assert_eq!(again_gzip.expect("the compressed body was kept"), gzip);

    let new_bootstrap = Arc::new("{\"messages\":[1]}".to_string());
    let (rebuilt, _) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), new_bootstrap.as_str()),
        &sidebar,
        Some(&new_bootstrap),
        false,
      )
      .unwrap();
    assert!(rebuilt.ends_with(b"{\"messages\":[1]};"), "a new bootstrap text rebuilds the body");
  }

  #[test]
  fn a_changed_template_rebuilds_the_page_body() {
    let mut memo = PageBodyMemo::default();
    let sidebar = Arc::new("[]".to_string());
    let bootstrap = Arc::new("null".to_string());
    let substitutions = substitutions(sidebar.as_str(), bootstrap.as_str());
    let (first, _) =
      memo.body("index.html", &index_template(), &substitutions, &sidebar, Some(&bootstrap), false).unwrap();
    let edited = format!("{} // edited on disk", index_template());
    let (second, _) = memo.body("index.html", &edited, &substitutions, &sidebar, Some(&bootstrap), false).unwrap();
    assert_ne!(first, second, "a template edited on disk never matches the memo");
    assert!(second.ends_with(b"// edited on disk"));
  }

  #[tokio::test]
  async fn a_gzip_client_gets_the_compressed_page_with_the_transport_headers() {
    let html = page_html();
    let compressed = Bytes::from(gzip_level_one(html.as_bytes()));
    let (parts, body) = page_response(Bytes::from(html.clone()), Some(compressed)).into_parts();
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
    let (parts, body) = page_response(Bytes::from(html.clone()), None).into_parts();
    let headers = parts.headers;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    let served = body.collect().await.expect("a page body collects").to_bytes();
    assert_eq!(headers[header::CONTENT_LENGTH].to_str().expect("a length header is text"), served.len().to_string());
    assert_eq!(served, Bytes::from(html));
  }
}
