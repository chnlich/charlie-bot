//! The chat page, home page and per-session events viewer.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header, request::Parts};
use axum::response::Response;
use bytes::Bytes;
use charliebot_agents::model_pool::POOL_MEMBER_FIELD;
use charliebot_cli_common::route;
use charliebot_compat::compression::{PageTail, gzip_memoized_body, zstd_memoized_body};
use charliebot_compat::mimetypes;
use charliebot_compat::mimetypes_table::TEXT_HTML;
use charliebot_compat::responses::{self, Failure};
use charliebot_compat::routing::get_only;
use charliebot_core::config::{CharlieBotConfig, HomeService};
use charliebot_core::headers::ACCEPT_ENCODING;
use charliebot_core::paths;
use charliebot_registry::Registry;
use charliebot_registry::templating::{self, STATIC_ASSET_VERSION_KEY, TemplateGlobal, Templating};
use charliebot_session_api::api as session_routes;
use charliebot_session_api::{detail, listing};
use charliebot_store::metadata::{NATIVE_MODEL, RUN_BACKEND, SessionMetadata, THINKING_SINCE};
use charliebot_store::{StoreError, StoreResult};
use futures_util::future::join_all;
use percent_encoding::percent_decode_str;
use serde_json::{Map, Value, json};

use super::family::Family;

use crate::gzip;
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

/// The session bootstraps the page built for its last few sessions, each reused while every payload input is
/// unchanged.
static PAGE_BOOTSTRAP_MEMO: LazyLock<detail::PageBootstrapMemo> = LazyLock::new(detail::PageBootstrapMemo::default);

/// The number of finished bodies one page keeps: the body it served most recently, and the ones before it that the
/// reader comes back to. A reader who moves between two sessions reopens a body the page built a moment ago, and a
/// single slot would rebuild it on every switch.
const PAGE_BODY_MEMO_ENTRIES_PER_PAGE: usize = 4;

/// The finished bodies of the pages, up to `PAGE_BODY_MEMO_ENTRIES_PER_PAGE` of each, every body kept while its
/// rendered template and its own texts still hold their value.
///
/// A page's body is its rendered template with the sidebar text and, on the chat page, the session bootstrap
/// substituted in. The chat page's sidebar text runs to about nine megabytes for a full home, and the events viewer's
/// document carries its stylesheet, so a body is far larger than the template it came from. Rendering and deflating
/// it again on every request costs the copy, the substitution and the deflate each time, so the memo serves the body
/// it already built.
///
/// The rendered template is part of the key: a template, page part or template directory edited on disk renders
/// different bytes and never matches. The chat page's two large texts compare by identity, and an entry keeps their
/// `Arc`s alive, so an address an entry holds is never handed to a different text.
#[derive(Default)]
struct PageBodyMemo {
  entries: Vec<PageBodyEntry>,
  /// The compressed forms of each page's static tail, one entry per page, kept while the tail's bytes hold.
  tails: Vec<PageTailEntry>,
  #[cfg(test)]
  builds: usize,
}

struct PageBodyEntry {
  name: &'static str,
  template_html: Arc<String>,
  sidebar_text: Option<Arc<String>>,
  bootstrap_text: Option<Arc<String>>,
  body: Bytes,
  /// The bytes this body ends with in common with the page's last body: the tail a page keeps compressed once.
  tail: Bytes,
  gzip: Option<Bytes>,
  zstd: Option<Bytes>,
}

struct PageTailEntry {
  name: &'static str,
  tail: PageTail,
}

/// The encoding a page body travels in: the smallest form of the body the request names as acceptable.
///
/// A client that accepts zstd takes it, because zstd's form of a page is smaller than gzip's; the page's gzip form is
/// unchanged for every other client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageEncoding {
  Identity,
  Gzip,
  Zstd,
}

impl PageEncoding {
  /// The encoding a request's `Accept-Encoding` prefers among the forms a page body keeps.
  ///
  /// A form with a higher weight wins, and equal weights take the smaller form. A form the request does not name, or
  /// names with `q=0`, is not acceptable. The transport reads the same weights through [`gzip::accepted_weight`], so a
  /// client that forbids gzip takes the identity body from both.
  fn for_request(headers: &HeaderMap) -> PageEncoding {
    match (gzip::accepted_weight(headers, "zstd"), gzip::accepted_weight(headers, "gzip")) {
      (Some(zstd), Some(gzip)) if zstd >= gzip => PageEncoding::Zstd,
      (_, Some(_)) => PageEncoding::Gzip,
      (Some(_), None) => PageEncoding::Zstd,
      (None, None) => PageEncoding::Identity,
    }
  }

  /// The `Content-Encoding` this encoding names, for the forms a body travels compressed in.
  fn content_encoding(self) -> &'static str {
    match self {
      PageEncoding::Gzip => "gzip",
      PageEncoding::Zstd => "zstd",
      PageEncoding::Identity => unreachable!("an identity page body travels whole"),
    }
  }
}

/// A page body's compressed form and the `Content-Encoding` it names.
#[derive(Clone)]
struct PageCompressed {
  encoding: &'static str,
  bytes: Bytes,
}

/// The shortest tail worth keeping compressed: a smaller one saves less deflate than the memo costs to compare.
const PAGE_TAIL_MINIMUM: usize = 16 * 1024;

/// The tail `body` shares with the page's last body: the bytes it ends with that the last body ended with too.
///
/// A page places every value that changes with the request before its scripts and its trailing markup, so the bytes
/// after the last of them are the same in every body the page serves. The shared suffix is the longest such run this
/// body and the last one have, and any run shorter than [`PAGE_TAIL_MINIMUM`] is no tail at all. The memo's kept tail
/// answers first, because a request that repeats a page's round takes it without a scan.
fn page_tail(name: &'static str, body: &Bytes, previous: Option<&Bytes>, tails: &[PageTailEntry]) -> Bytes {
  if let Some(entry) = tails.iter().find(|entry| entry.name == name)
    && body.ends_with(entry.tail.bytes())
  {
    return body.slice(body.len() - entry.tail.len()..);
  }
  let Some(previous) = previous else { return Bytes::new() };
  let len = common_suffix_len(previous, body);
  if len < PAGE_TAIL_MINIMUM { Bytes::new() } else { body.slice(body.len() - len..) }
}

/// The length of the longest suffix `new` shares with `last`, at most the shorter body's length.
fn common_suffix_len(last: &[u8], new: &[u8]) -> usize {
  let max = last.len().min(new.len());
  let (mut low, mut high) = (0, max);
  while low < high {
    let middle = low + (high - low).div_ceil(2);
    if new[new.len() - middle..] == last[last.len() - middle..] {
      low = middle;
    } else {
      high = middle - 1;
    }
  }
  low
}

/// The compressed form of `body`, whose kept tail is `tail`.
///
/// A body without a tail — every page but the chat page, and the first body of each version — deflates whole. A body
/// with one deflates the head a request built and appends the tail's form from the memo, which the first body with
/// those tail bytes built.
fn compressed_page(
  name: &'static str,
  body: &Bytes,
  tail: &Bytes,
  encoding: PageEncoding,
  tails: &mut Vec<PageTailEntry>,
) -> Bytes {
  if tail.is_empty() {
    return Bytes::from(match encoding {
      PageEncoding::Gzip => gzip_memoized_body(body),
      PageEncoding::Zstd => zstd_memoized_body(body),
      PageEncoding::Identity => unreachable!("an identity page body travels whole"),
    });
  }
  let head = &body[..body.len() - tail.len()];
  let held = tails.iter().position(|entry| entry.name == name && entry.tail.holds(tail));
  let index = match held {
    Some(index) => index,
    None => {
      let entry = PageTailEntry { name, tail: PageTail::new(tail) };
      match tails.iter().position(|entry| entry.name == name) {
        Some(index) => {
          tails[index] = entry;
          index
        }
        None => {
          tails.push(entry);
          tails.len() - 1
        }
      }
    }
  };
  Bytes::from(match encoding {
    PageEncoding::Gzip => tails[index].tail.gzip(head),
    PageEncoding::Zstd => tails[index].tail.zstd(head),
    PageEncoding::Identity => unreachable!("an identity page body travels whole"),
  })
}

/// Whether an optional substituted text still holds its value: both absent, or the same `Arc`.
fn same_text(held: Option<&Arc<String>>, current: Option<&Arc<String>>) -> bool {
  match (held, current) {
    (Some(held), Some(current)) => Arc::ptr_eq(held, current),
    (None, None) => true,
    _ => false,
  }
}

impl PageBodyMemo {
  /// The finished page body, from the memo when every input still holds its value.
  fn body(
    &mut self,
    name: &'static str,
    template_html: &Arc<String>,
    substitutions: &[(&str, &str)],
    sidebar_text: Option<&Arc<String>>,
    bootstrap_text: Option<&Arc<String>>,
    encoding: PageEncoding,
  ) -> Result<(Bytes, Option<PageCompressed>), Failure> {
    let held = self.entries.iter().position(|entry| {
      entry.name == name
        && (Arc::ptr_eq(&entry.template_html, template_html) || entry.template_html.as_str() == template_html.as_str())
        && same_text(entry.sidebar_text.as_ref(), sidebar_text)
        && same_text(entry.bootstrap_text.as_ref(), bootstrap_text)
    });
    let index = match held {
      Some(index) => {
        // A body served again is this page's most recent entry, so an eviction takes the body it served longest ago.
        let entry = self.entries.remove(index);
        self.entries.push(entry);
        self.entries.len() - 1
      }
      None => {
        let html = templating::substitute_placeholders(name, template_html.as_str().to_string(), substitutions)
          .map_err(|error| failure(error.kind(), error))?;
        let body = Bytes::from(html);
        let previous = self.entries.iter().find(|entry| entry.name == name).map(|entry| &entry.body);
        let tail = page_tail(name, &body, previous, &self.tails);
        let entry = PageBodyEntry {
          name,
          template_html: Arc::clone(template_html),
          sidebar_text: sidebar_text.cloned(),
          bootstrap_text: bootstrap_text.cloned(),
          body,
          tail,
          gzip: None,
          zstd: None,
        };
        self.entries.push(entry);
        #[cfg(test)]
        {
          self.builds += 1;
        }
        let mut index = self.entries.len() - 1;
        // The page's least recently served body goes once the page holds one past its limit.
        let mut held_bodies = self.entries.iter().filter(|entry| entry.name == name).count();
        while held_bodies > PAGE_BODY_MEMO_ENTRIES_PER_PAGE {
          let oldest = self.entries.iter().position(|entry| entry.name == name).expect("the page holds a body");
          self.entries.remove(oldest);
          if oldest < index {
            index -= 1;
          }
          held_bodies -= 1;
        }
        index
      }
    };
    let PageBodyMemo { entries, tails, .. } = self;
    let entry = entries.get_mut(index).expect("the page body entry matched or was built");
    let body = entry.body.clone();
    let tail = entry.tail.clone();
    let compressed = if encoding != PageEncoding::Identity && body.len() >= gzip::MINIMUM_SIZE {
      let slot = match encoding {
        PageEncoding::Gzip => &mut entry.gzip,
        PageEncoding::Zstd => &mut entry.zstd,
        PageEncoding::Identity => unreachable!("an identity page body travels whole"),
      };
      let bytes = slot.get_or_insert_with(|| compressed_page(name, &body, &tail, encoding, tails)).clone();
      Some(PageCompressed { encoding: encoding.content_encoding(), bytes })
    } else {
      None
    };
    Ok((body, compressed))
  }
}

static PAGE_BODY_MEMO: LazyLock<Mutex<PageBodyMemo>> = LazyLock::new(|| Mutex::new(PageBodyMemo::default()));

/// The chat page's finished answers, up to [`PAGE_BODY_MEMO_ENTRIES_PER_PAGE`] of them, each kept while every
/// input that shaped it still holds.
///
/// A repeat page request re-checks the session row, its bootstrap and the sidebar text through their own memos,
/// and then pays the page's context again: the session's response value, the backend facts, the context object
/// and the template render. Those read the same values the memo can name, so the route keeps the answer beside
/// the inputs it compared and serves it again. On the 7,160-session mock home the context and the render cost
/// about 65 microseconds of the page's 150-microsecond handler.
#[derive(Default)]
struct IndexAnswerMemo {
  entries: Vec<IndexAnswerEntry>,
}

struct IndexAnswerEntry {
  key: IndexAnswerKey,
  body: Bytes,
  compressed: Option<PageCompressed>,
}

/// Every input that shapes one chat page answer.
///
/// The bootstrap and the sidebar text compare by the shared allocation their own memo keeps, so a repeat request
/// proves them with a pointer. The session row, the template globals and the load errors compare by value: the
/// store hands a caller its own copy of the row, and the globals carry the config and credentials a reload
/// replaced. The config compares by pointer because a reload builds a new one, and the template revision names
/// the template and static tree. The rest are the small values the request and the page state carry.
#[derive(Clone)]
struct IndexAnswerKey {
  revision: u64,
  encoding: PageEncoding,
  method: String,
  path: String,
  query: String,
  hostname: String,
  auth_enabled: bool,
  session: String,
  meta: SessionMetadata,
  bootstrap_text: Arc<String>,
  bootstrap_session: Arc<Value>,
  pending_draft: Value,
  event_count: Value,
  sidebar_text: Arc<String>,
  config: Arc<CharlieBotConfig>,
  globals: Vec<TemplateGlobal>,
  load_errors: Vec<String>,
}

impl IndexAnswerKey {
  /// Whether every input still names the value this key holds.
  fn matches(&self, other: &Self) -> bool {
    self.revision == other.revision
      && self.encoding == other.encoding
      && self.method == other.method
      && self.path == other.path
      && self.query == other.query
      && self.hostname == other.hostname
      && self.auth_enabled == other.auth_enabled
      && self.session == other.session
      && self.meta == other.meta
      && Arc::ptr_eq(&self.bootstrap_text, &other.bootstrap_text)
      && Arc::ptr_eq(&self.bootstrap_session, &other.bootstrap_session)
      && self.pending_draft == other.pending_draft
      && self.event_count == other.event_count
      && Arc::ptr_eq(&self.sidebar_text, &other.sidebar_text)
      && Arc::ptr_eq(&self.config, &other.config)
      && self.globals == other.globals
      && self.load_errors == other.load_errors
  }
}

impl IndexAnswerMemo {
  /// The answer for these inputs, or the one `build` renders and this memo keeps.
  fn answer(
    &mut self,
    key: &IndexAnswerKey,
    build: impl FnOnce() -> Result<(Bytes, Option<PageCompressed>), Failure>,
  ) -> Result<(Bytes, Option<PageCompressed>), Failure> {
    if let Some(at) = self.entries.iter().position(|entry| entry.key.matches(key)) {
      // The answer served again is the newest, so an eviction takes the one served longest ago.
      let entry = self.entries.remove(at);
      let answer = (entry.body.clone(), entry.compressed.clone());
      self.entries.push(entry);
      return Ok(answer);
    }
    let (body, compressed) = build()?;
    self.entries.push(IndexAnswerEntry { key: key.clone(), body: body.clone(), compressed: compressed.clone() });
    if self.entries.len() > PAGE_BODY_MEMO_ENTRIES_PER_PAGE {
      self.entries.remove(0);
    }
    Ok((body, compressed))
  }
}

static INDEX_ANSWER_MEMO: LazyLock<Mutex<IndexAnswerMemo>> = LazyLock::new(|| Mutex::new(IndexAnswerMemo::default()));

async fn index(State(state): State<Arc<AppState>>, request: Parts) -> Response {
  let cfg = match state.config() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  let credentials = match state.credentials() {
    Ok(credentials) => credentials,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  let auth_enabled = credentials.access_key().is_some();
  let session = session_query(&request);
  // The page reads the session row, its bootstrap, the sidebar rows and the rendered body, and a repeat request
  // re-checks the memo of each. One blocking task runs every check and the render, so the request pays one
  // blocking-pool round trip instead of one per piece.
  let rendered = tokio::task::spawn_blocking(move || chat_page(&state, &request, cfg, auth_enabled, session))
    .await
    .expect("the chat page task does not panic");
  match rendered {
    Ok(response) => response,
    Err(error) => responses::internal_error(&error.kind, error.detail),
  }
}

/// Build the chat page: the session row, its bootstrap, the sidebar rows and the rendered body.
///
/// Runs on the blocking pool, because every read it makes is a blocking store or template read.
fn chat_page(
  state: &Arc<AppState>,
  request: &Parts,
  cfg: Arc<CharlieBotConfig>,
  auth_enabled: bool,
  session: Option<String>,
) -> Result<Response, Failure> {
  let mut load_errors = Vec::new();
  let mut meta = None;
  let mut bootstrap = None;
  let mut clear_unread = None;
  if let Some(session_id) = session.as_deref().filter(|session_id| !session_id.is_empty()) {
    match state.features.store.get_session_cached_or_read_sync(session_id) {
      Ok(Some(row)) => {
        let loaded =
          detail::page_bootstrap(&PAGE_BOOTSTRAP_MEMO, &state.features.store, &state.features.registry, &cfg, &row);
        // The row identifies the open session whether or not its bootstrap loads: the page keeps showing it
        // after a failed load, and only the bootstrap fields stay out.
        meta = Some(row);
        match loaded {
          Ok(loaded) => {
            // The active row is marked read only when its bootstrap loaded; a failed load keeps
            // the unread mark.
            clear_unread = Some(session_id.to_string());
            bootstrap = Some(loaded);
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

  // A page opened without a session redirects to the first row before any row renders.
  let redirect_to_first = session.is_none();
  let listing_result = (|| -> StoreResult<PageListing> {
    if redirect_to_first && let Some(first) = listing::first_page_row_id(&state.features.store)? {
      return Ok(PageListing::Redirect(first));
    }
    let page = listing::active_page_rows(&state.features.store, &state.features.registry)?;
    listing::active_page_json_text(
      &state.features.store,
      &page,
      clear_unread.as_deref(),
      session.as_deref(),
      &PAGE_ROWS_MEMO,
    )
    .map(PageListing::Text)
  })();
  let initial_sessions_json = match listing_result {
    Ok(PageListing::Redirect(session_id)) => return Ok(responses::redirect(&format!("/?session={session_id}"))),
    Ok(PageListing::Text(text)) => text,
    Err(error) => {
      tracing::error!(error = %error, "list_sessions_failed");
      load_errors.push("Failed to load sessions. Check server logs for details.".to_string());
      Arc::new("[]".to_string())
    }
  };

  let encoding = PageEncoding::for_request(&request.headers);
  let hostname = hostname()?;
  let build = |meta: Option<&SessionMetadata>, bootstrap: Option<&detail::PageBootstrap>, load_errors: Vec<String>| {
    index_answer(
      state,
      request,
      &cfg,
      auth_enabled,
      &hostname,
      encoding,
      meta,
      bootstrap,
      Arc::clone(&initial_sessions_json),
      load_errors,
    )
  };
  let (body, compressed) = match (meta.as_ref(), bootstrap.as_ref()) {
    (Some(meta), Some(bootstrap)) => {
      let templating =
        state.features.startup.require::<Templating>().expect("run() inserts the Templating before the server binds");
      let globals = templating.render_globals(&state.features).map_err(|error| failure(error.kind(), error))?;
      let key = IndexAnswerKey {
        revision: templating.revision(),
        encoding,
        method: request.method.as_str().to_string(),
        path: request.uri.path().to_string(),
        query: request.uri.query().unwrap_or("").to_string(),
        hostname: hostname.clone(),
        auth_enabled,
        session: session.clone().unwrap_or_default(),
        meta: meta.clone(),
        bootstrap_text: Arc::clone(&bootstrap.text),
        bootstrap_session: Arc::clone(&bootstrap.session),
        pending_draft: bootstrap.pending_draft.clone(),
        event_count: bootstrap.event_count.clone(),
        sidebar_text: Arc::clone(&initial_sessions_json),
        config: Arc::clone(&cfg),
        globals,
        load_errors: load_errors.clone(),
      };
      let mut memo = INDEX_ANSWER_MEMO.lock().expect("the index answer memo mutex is not poisoned");
      memo.answer(&key, || build(Some(meta), Some(bootstrap), load_errors))?
    }
    _ => build(meta.as_ref(), bootstrap.as_ref(), load_errors)?,
  };
  Ok(page_response(body, compressed, StatusCode::OK))
}

/// The chat page's answer for the values the request read: the context the template renders from, and the body.
///
/// The session's response value, its bootstrap fields and the backend facts are built here rather than in
/// [`chat_page`], so a request the answer memo answers builds none of them.
#[allow(clippy::too_many_arguments)]
fn index_answer(
  state: &Arc<AppState>,
  request: &Parts,
  cfg: &Arc<CharlieBotConfig>,
  auth_enabled: bool,
  hostname: &str,
  encoding: PageEncoding,
  meta: Option<&SessionMetadata>,
  bootstrap: Option<&detail::PageBootstrap>,
  sidebar_text: Arc<String>,
  load_errors: Vec<String>,
) -> Result<(Bytes, Option<PageCompressed>), Failure> {
  let mut active_session = Value::Null;
  let mut pending_draft = Value::Null;
  let mut event_count = Value::from(0);
  let mut bootstrap_text = None;
  if let Some(meta) = meta {
    active_session = state.features.store.session_metadata_value(meta);
    // A bootstrap that failed to load leaves only its own fields out; the session row itself still renders.
    if let Some(bootstrap) = bootstrap {
      let bootstrap_session = bootstrap.session.as_object().expect("a bootstrap session is an object");
      let active = active_session.as_object_mut().expect("session metadata serializes to an object");
      for name in [RUN_BACKEND, THINKING_SINCE] {
        if let Some(value) = bootstrap_session.get(name) {
          active.insert(name.to_string(), value.clone());
        }
      }
      pending_draft = bootstrap.pending_draft.clone();
      event_count = bootstrap.event_count.clone();
      bootstrap_text = Some(Arc::clone(&bootstrap.text));
    }
  }
  let default_backend_id = cfg.backends.new_session_backend().unwrap_or_else(|| "claude".to_string());
  let active_backend = active_backend_id(cfg, &active_session, &default_backend_id);
  let active_option = cfg.get_backend_option(active_backend);
  let backend_pools =
    cfg.backends.pools.iter().map(|pool| json!({"id": pool.id, "label": pool.label})).collect::<Vec<_>>();
  let picker_entries =
    cfg.backends.picker_entries().into_iter().map(|(id, label)| json!({"id": id, "label": label})).collect::<Vec<_>>();
  let active_pool_member = active_session.get(POOL_MEMBER_FIELD).cloned().unwrap_or(Value::Null);
  let active_native_model = active_session.get(NATIVE_MODEL).cloned().unwrap_or(Value::Null);
  let active_backend_label = active_backend_label(cfg, active_backend, &active_pool_member, &active_native_model);
  let backend_compact = compact_facts(cfg, &state.features.registry)?;
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
    "backend_pools": backend_pools,
    "picker_entries": picker_entries,
    "active_pool_member": active_pool_member,
    "active_native_model": active_native_model,
    "backend_compact": Value::Object(backend_compact),
    "default_backend_id": default_backend_id,
    (detail::ACTIVE_BACKEND_FIELD): active_backend,
    "active_backend_label": active_backend_label,
    (detail::ACTIVE_BACKEND_TYPE_FIELD): active_option.map_or("", |option| option.backend_type.as_str()),
    "load_errors": load_errors,
  });
  let runtime = PageRuntime::Index { cfg: Arc::clone(cfg), auth_enabled, sidebar_text, bootstrap_text };
  page_answer(state, request, "index.html", context, &runtime, encoding, hostname)
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
    StatusCode::OK,
  )
  .await
}

/// The events viewer's finished answers, up to [`EVENTS_ANSWER_MEMO_ENTRIES`], each kept while every input that
/// shaped it still holds.
///
/// The viewer serializes its session row and renders its template on every request. A reader who comes back to the
/// same viewer asks for the body the page served a moment ago, and the row, the template globals and the request
/// line prove it. The body and its compressed form are kept whole, so a repeat request reads neither the row's
/// response value nor the engine.
#[derive(Default)]
struct EventsAnswerMemo {
  entries: Vec<EventsAnswerEntry>,
}

struct EventsAnswerEntry {
  key: EventsAnswerKey,
  body: Bytes,
  compressed: Option<PageCompressed>,
}

/// Every input that shapes one events viewer answer.
///
/// The session row is the one `get_session` answered, and it is that row's value the page rendered before this memo,
/// so a request whose row differs renders again. The row follows the store's own metadata cache window: the route
/// serves the row the store hands out, as it did before the memo. The template globals compare by value, as the render
/// memo compares them, so a reloaded config or credentials file still reaches the page.
#[derive(Clone, PartialEq)]
struct EventsAnswerKey {
  revision: u64,
  asset_version: String,
  hostname: String,
  encoding: PageEncoding,
  method: String,
  path: String,
  query: String,
  session_id: String,
  events_url: String,
  session: SessionMetadata,
  globals: Vec<TemplateGlobal>,
}

impl EventsAnswerMemo {
  /// The answer for these inputs, or the one `build` renders and this memo keeps.
  fn answer(
    &mut self,
    key: &EventsAnswerKey,
    build: impl FnOnce() -> Result<(Bytes, Option<PageCompressed>), Failure>,
  ) -> Result<(Bytes, Option<PageCompressed>), Failure> {
    if let Some(at) = self.entries.iter().position(|entry| entry.key == *key) {
      // The answer served again is the newest, so an eviction takes the one served longest ago.
      let entry = self.entries.remove(at);
      let answer = (entry.body.clone(), entry.compressed.clone());
      self.entries.push(entry);
      return Ok(answer);
    }
    let (body, compressed) = build()?;
    self.entries.push(EventsAnswerEntry { key: key.clone(), body: body.clone(), compressed: compressed.clone() });
    if self.entries.len() > EVENTS_ANSWER_MEMO_ENTRIES {
      self.entries.remove(0);
    }
    Ok((body, compressed))
  }
}

static EVENTS_ANSWER_MEMO: LazyLock<Mutex<EventsAnswerMemo>> =
  LazyLock::new(|| Mutex::new(EventsAnswerMemo::default()));

/// The finished viewer answers one process keeps: a reader who moves between two sessions comes back to the viewer
/// it opened a moment ago, and a single slot would render it again on every switch.
const EVENTS_ANSWER_MEMO_ENTRIES: usize = 4;

async fn events_viewer(
  State(state): State<Arc<AppState>>,
  AxumPath(session_id): AxumPath<String>,
  request: Parts,
) -> Response {
  let session = match state.features.store.get_session(&session_id).await {
    Ok(Some(session)) => session,
    Ok(None) | Err(StoreError::NotFound(_)) => {
      return render_events_unavailable(state, request, session_id, StatusCode::NOT_FOUND, "missing").await;
    }
    Err(error) => {
      tracing::error!(session_id, error = %error, "get_session_failed");
      return render_events_unavailable(state, request, session_id, StatusCode::INTERNAL_SERVER_ERROR, "failed").await;
    }
  };
  let rendered = tokio::task::spawn_blocking(move || events_page(&state, &request, &session_id, &session))
    .await
    .expect("the events viewer task does not panic");
  match rendered {
    Ok(response) => response,
    Err(error) => responses::internal_error(&error.kind, error.detail),
  }
}

/// The events viewer's answer for a session it can show.
///
/// A repeat request re-checks the session row and the template inputs, and then pays the page's context again: the
/// row's response value, the context object and the template render. The answer memo keeps the finished body beside
/// the inputs it read, so a request whose row and template inputs still hold serves that body without building the
/// row or rendering the page.
fn events_page(
  state: &Arc<AppState>,
  request: &Parts,
  session_id: &str,
  session: &SessionMetadata,
) -> Result<Response, Failure> {
  let templating =
    state.features.startup.require::<Templating>().expect("run() inserts the Templating before the server binds");
  let encoding = PageEncoding::for_request(&request.headers);
  let hostname = hostname()?;
  let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
  let globals = templating.render_globals(&state.features).map_err(|error| failure(error.kind(), error))?;
  let key = EventsAnswerKey {
    revision: templating.revision(),
    asset_version,
    hostname,
    encoding,
    method: request.method.as_str().to_string(),
    path: request.uri.path().to_string(),
    query: request.uri.query().unwrap_or("").to_string(),
    session_id: session_id.to_string(),
    events_url: route::fill(session_routes::SESSION_EVENTS_JSONL_PATH, &[session_id]),
    session: session.clone(),
    globals,
  };
  let answer = EVENTS_ANSWER_MEMO.lock().expect("the events answer memo mutex is not poisoned").answer(&key, || {
    let context = json!({
      "session": state.features.store.session_metadata_value(session),
      "session_id": key.session_id,
      "events_url": key.events_url,
    });
    page_answer(state, request, "events_viewer.html", context, &PageRuntime::Events, encoding, &key.hostname)
  })?;
  Ok(page_response(answer.0, answer.1, StatusCode::OK))
}

/// The events page's answer for a session it cannot show: the page keeps its top bar and states the outcome in its own
/// words, so a page request never answers with the API's JSON. `reason` is the template's word for the outcome, so the
/// page names the reader's next step instead of the store's failure.
async fn render_events_unavailable(
  state: Arc<AppState>,
  request: Parts,
  session_id: String,
  status: StatusCode,
  reason: &'static str,
) -> Response {
  let context = json!({"unavailable": reason, "session_id": session_id});
  render_page(state, request, "events_viewer.html", context, PageRuntime::Events, status).await
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
  context: Value,
  runtime: PageRuntime,
  status: StatusCode,
) -> Response {
  let rendered =
    tokio::task::spawn_blocking(move || render_page_blocking(&state, &request, name, context, runtime, status))
      .await
      .expect("rendering a page does not panic");
  match rendered {
    Ok(response) => response,
    Err(error) => responses::internal_error(&error.kind, error.detail),
  }
}

/// Render one page's body in the blocking pool: the blocking half of [`render_page`], so a handler that owns more
/// blocking work renders the page in the same round trip.
fn render_page_blocking(
  state: &Arc<AppState>,
  request: &Parts,
  name: &'static str,
  context: Value,
  runtime: PageRuntime,
  status: StatusCode,
) -> Result<Response, Failure> {
  let encoding = PageEncoding::for_request(&request.headers);
  let hostname = hostname()?;
  let (body, compressed) = page_answer(state, request, name, context, &runtime, encoding, &hostname)?;
  Ok(page_response(body, compressed, status))
}

/// One page's body, from the template and the body memo, with the values the page reads from the state it runs in.
fn page_answer(
  state: &Arc<AppState>,
  request: &Parts,
  name: &'static str,
  mut context: Value,
  runtime: &PageRuntime,
  encoding: PageEncoding,
  hostname: &str,
) -> Result<(Bytes, Option<PageCompressed>), Failure> {
  let templating =
    state.features.startup.require::<Templating>().expect("run() inserts the Templating before the server binds");
  let object = context.as_object_mut().expect("a page context is a JSON object");
  object.insert("hostname".to_string(), Value::String(hostname.to_string()));
  match runtime {
    PageRuntime::Index { cfg, auth_enabled, .. } => {
      let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
      object.insert("sessions_root".to_string(), Value::String(paths::display(&cfg.sessions_dir())));
      object.insert("version".to_string(), Value::String(templating.git_version().to_string()));
      object.insert(STATIC_ASSET_VERSION_KEY.to_string(), Value::String(asset_version));
      object.insert("auth_enabled".to_string(), Value::Bool(*auth_enabled));
    }
    PageRuntime::Home | PageRuntime::Events => {
      let asset_version = templating.static_asset_version().map_err(|error| failure("OSError", error))?;
      object.insert(STATIC_ASSET_VERSION_KEY.to_string(), Value::String(asset_version));
    }
  }
  let template_html = templating
    .render_page_template(&state.features, name, &context, request)
    .map_err(|error| failure(error.kind(), error))?;
  page_body(name, &template_html, runtime, encoding)
}

/// The finished body of a page, with its compressed form when the client asked for one.
fn page_body(
  name: &'static str,
  template_html: &Arc<String>,
  runtime: &PageRuntime,
  encoding: PageEncoding,
) -> Result<(Bytes, Option<PageCompressed>), Failure> {
  let (sidebar_text, bootstrap_text) = match runtime {
    PageRuntime::Index { sidebar_text, bootstrap_text, .. } => (Some(sidebar_text), bootstrap_text.as_ref()),
    PageRuntime::Home | PageRuntime::Events => (None, None),
  };
  PAGE_BODY_MEMO.lock().expect("the page body memo mutex is not poisoned").body(
    name,
    template_html,
    &runtime.substitutions(),
    sidebar_text,
    bootstrap_text,
    encoding,
  )
}

/// Wrap a finished page body in the response at `status`, with its compressed form when one is given.
///
/// For a body at or above the transport's minimum size, the compressed form travels with its own headers here
/// instead of under the transport gzip; the transport passes a response that already sets `Content-Encoding`
/// through untouched.
fn page_response(body: Bytes, compressed: Option<PageCompressed>, status: StatusCode) -> Response {
  let mut builder =
    Response::builder().status(status).header(header::CONTENT_TYPE, mimetypes::with_utf8_charset(TEXT_HTML));
  let served = match compressed {
    Some(compressed) => {
      builder = builder
        .header(header::CONTENT_LENGTH, compressed.bytes.len())
        .header(header::CONTENT_ENCODING, compressed.encoding)
        .header(header::VARY, HeaderValue::from_static(ACCEPT_ENCODING));
      compressed.bytes
    }
    None => {
      builder = builder.header(header::CONTENT_LENGTH, body.len());
      body
    }
  };
  let mut response = builder.body(Body::from(served)).expect("a page response builds");
  // `no-store` names this body as a page: a page must never come out of a cache, because it names the static asset
  // version it was built against and an older body runs an older script. The transport's entity-tag layer, outside
  // this one, replaces the marker with `no-cache` and the body's tag, so every navigation still reaches the server and
  // an unchanged body costs a `304` instead of a transfer.
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

fn hostname() -> Result<String, Failure> {
  nix::unistd::gethostname()
    .map_err(|error| failure("OSError", error))?
    .into_string()
    .map_err(|name| failure("UnicodeDecodeError", format!("host name is not UTF-8: {}", name.to_string_lossy())))
}

fn failure(kind: &str, detail: impl ToString) -> Failure {
  Failure { kind: kind.to_string(), detail: detail.to_string() }
}

/// The backend the page shows as active. A session on a `backends.pools` id stays on that id even while a run records
/// the member it resolved to in `run_backend`; every other session shows the live run's backend when one exists.
fn active_backend_id<'a>(cfg: &CharlieBotConfig, session: &'a Value, default_backend: &'a str) -> &'a str {
  let session_backend = session.get("backend").and_then(Value::as_str).filter(|backend| !backend.is_empty());
  if let Some(backend) = session_backend.filter(|backend| cfg.backends.pool(backend).is_some()) {
    return backend;
  }
  session
    .get(RUN_BACKEND)
    .and_then(Value::as_str)
    .filter(|backend| !backend.is_empty())
    .or(session_backend)
    .unwrap_or(default_backend)
}

/// The header label of a session's backend: a pool session names the member its current turn runs on,
/// with the model its records hold.
fn active_backend_label(
  cfg: &CharlieBotConfig,
  active_backend: &str,
  active_pool_member: &Value,
  active_native_model: &Value,
) -> String {
  if let Some(pool) = cfg.backends.pool(active_backend) {
    return match active_pool_member.as_str() {
      Some(member) => {
        let member_label = cfg.get_backend_option(member).map_or(member, |option| option.label.as_str());
        match active_native_model.as_str().filter(|model| !model.is_empty()) {
          Some(model) => format!("{} → {member_label} · {model}", pool.label),
          None => format!("{} → {member_label}", pool.label),
        }
      }
      None => pool.label.clone(),
    };
  }
  cfg.get_backend_option(active_backend).map_or_else(|| active_backend.to_string(), |option| option.label.clone())
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
  use charliebot_compat::compression::gzip_level_one;
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

  /// A chat page template whose static scripts carry more than [`PAGE_TAIL_MINIMUM`], as the served page's do.
  fn tailed_index_template() -> String {
    format!("{}<script>{}</script>", index_template(), "const tail = 1;".repeat(1600))
  }

  fn substitutions<'a>(sidebar: &'a str, bootstrap: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![(INITIAL_SESSIONS_PLACEHOLDER, sidebar), (SESSION_BOOTSTRAP_PLACEHOLDER, bootstrap)]
  }

  fn label_config() -> CharlieBotConfig {
    use charliebot_core::config::{
      BackendOption, BackendsConfig, CallsConfig, PathsConfig, PoolConfig, PoolMember, PoolRule, Sections,
      ServerConfig, TelegramConfig, UiConfig,
    };
    CharlieBotConfig {
      charliebot_home: std::path::PathBuf::from("/synthetic/home"),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        public_url: None,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig {
        routing: None,
        options: vec![BackendOption {
          id: "claude-opus-5".to_string(),
          label: "CC · Opus 5".to_string(),
          model: "claude-opus-5".to_string(),
          prompt_overlay: None,
          billing: None,
          backend_type: "synthetic".to_string(),
          type_fields: Default::default(),
          visible: true,
        }],
        pools: vec![PoolConfig {
          id: "auto".to_string(),
          label: "Auto".to_string(),
          rule: PoolRule::Priority,
          members: vec![PoolMember { backend: "claude-opus-5".to_string() }],
          visible: true,
        }],
        calls: CallsConfig::default(),
      },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      remote_run: charliebot_core::config::RemoteRunConfig::default(),
      github: Default::default(),
      sections: Sections::default(),
    }
  }

  #[test]
  fn the_header_label_names_the_member_a_pool_session_runs_on() {
    let cfg = label_config();
    assert_eq!(
      active_backend_label(&cfg, "auto", &json!("claude-opus-5"), &json!("claude-opus-5-5")),
      "Auto → CC · Opus 5 · claude-opus-5-5"
    );
    assert_eq!(active_backend_label(&cfg, "auto", &json!("claude-opus-5"), &Value::Null), "Auto → CC · Opus 5");
    assert_eq!(active_backend_label(&cfg, "auto", &Value::Null, &json!("claude-opus-5-5")), "Auto");
    assert_eq!(active_backend_label(&cfg, "claude-opus-5", &Value::Null, &json!("claude-opus-5-5")), "CC · Opus 5");
  }

  #[test]
  fn a_pool_session_stays_active_while_a_run_records_its_member() {
    let cfg = label_config();
    let pooled = json!({"backend": "auto", "run_backend": "claude-opus-5", "pool_member": "claude-opus-5"});
    assert_eq!(active_backend_id(&cfg, &pooled, "claude-opus-5"), "auto");

    let pinned = json!({"backend": "claude-opus-5", "run_backend": "claude-fable-5"});
    assert_eq!(active_backend_id(&cfg, &pinned, "claude-opus-5"), "claude-fable-5", "a live run's backend still wins");

    assert_eq!(active_backend_id(&cfg, &json!({}), "synthetic-default"), "synthetic-default");
  }

  #[test]
  fn a_retired_pool_member_renders_its_raw_id() {
    let cfg = label_config();
    assert_eq!(
      active_backend_label(&cfg, "auto", &json!("retired"), &json!("gpt-6-luna")),
      "Auto → retired · gpt-6-luna"
    );
    assert_eq!(active_backend_label(&cfg, "auto", &json!("retired"), &Value::Null), "Auto → retired");
  }

  /// A repeat page request serves the answer the memo kept, and a moved input renders again.
  #[test]
  fn a_repeat_page_answer_is_served_until_an_input_moves() {
    use charliebot_store::metadata::TaskProfile;

    let config = Arc::new(label_config());
    let meta = SessionMetadata::new("synthetic", "Memo page", TaskProfile::Manager);
    let bootstrap_text = Arc::new("{\"messages\":[]}".to_string());
    let bootstrap_session = Arc::new(json!({"run_backend": null}));
    let sidebar = Arc::new("{}".to_string());
    let key = |sidebar: &Arc<String>, encoding, globals: Vec<TemplateGlobal>| IndexAnswerKey {
      revision: 1,
      encoding,
      method: "GET".to_string(),
      path: "/".to_string(),
      query: "session=synthetic".to_string(),
      hostname: "host".to_string(),
      auth_enabled: false,
      session: "synthetic".to_string(),
      meta: meta.clone(),
      bootstrap_text: Arc::clone(&bootstrap_text),
      bootstrap_session: Arc::clone(&bootstrap_session),
      pending_draft: Value::Null,
      event_count: Value::from(0),
      sidebar_text: Arc::clone(sidebar),
      config: Arc::clone(&config),
      globals,
      load_errors: Vec::new(),
    };
    fn answer(memo: &mut IndexAnswerMemo, key: &IndexAnswerKey, builds: &mut usize) -> Bytes {
      memo
        .answer(key, || {
          *builds += 1;
          Ok((Bytes::from_static(b"page body"), None))
        })
        .unwrap()
        .0
    }

    let mut memo = IndexAnswerMemo::default();
    let mut builds = 0;
    assert_eq!(
      answer(&mut memo, &key(&sidebar, PageEncoding::Gzip, Vec::new()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 1);
    assert_eq!(
      answer(&mut memo, &key(&sidebar, PageEncoding::Gzip, Vec::new()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 1, "a repeat request serves the answer the memo kept");

    assert_eq!(
      answer(&mut memo, &key(&sidebar, PageEncoding::Identity, Vec::new()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 2, "another encoding renders its own body");

    let moved_sidebar = Arc::new("{\"name\":[]}".to_string());
    assert_eq!(
      answer(&mut memo, &key(&moved_sidebar, PageEncoding::Gzip, Vec::new()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 3, "a moved sidebar renders again");

    let globals = || {
      vec![("voice_backends", charliebot_registry::templating::TemplateValue::from_serialize(json!(["code-server"])))]
    };
    assert_eq!(
      answer(&mut memo, &key(&sidebar, PageEncoding::Gzip, globals()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 4, "a changed template global renders again");
    assert_eq!(
      answer(&mut memo, &key(&sidebar, PageEncoding::Gzip, globals()), &mut builds),
      Bytes::from_static(b"page body")
    );
    assert_eq!(builds, 4, "the same globals serve the answer the memo kept");
  }

  /// A repeat events viewer request serves the answer the memo kept, and a moved row renders again.
  #[test]
  fn a_repeat_events_answer_is_served_until_the_row_moves() {
    use charliebot_store::metadata::TaskProfile;

    let session = SessionMetadata::new("synthetic", "Events page", TaskProfile::Manager);
    let key = |session: &SessionMetadata, encoding| EventsAnswerKey {
      revision: 1,
      asset_version: "v1".to_string(),
      hostname: "host".to_string(),
      encoding,
      method: "GET".to_string(),
      path: "/sessions/synthetic/events".to_string(),
      query: String::new(),
      session_id: "synthetic".to_string(),
      events_url: "/api/sessions/synthetic/events.jsonl".to_string(),
      session: session.clone(),
      globals: Vec::new(),
    };
    fn answer(memo: &mut EventsAnswerMemo, key: &EventsAnswerKey, builds: &mut usize) -> Bytes {
      memo
        .answer(key, || {
          *builds += 1;
          Ok((Bytes::from_static(b"events body"), None))
        })
        .unwrap()
        .0
    }

    let mut memo = EventsAnswerMemo::default();
    let mut builds = 0;
    assert_eq!(answer(&mut memo, &key(&session, PageEncoding::Gzip), &mut builds), Bytes::from_static(b"events body"));
    assert_eq!(builds, 1);
    assert_eq!(answer(&mut memo, &key(&session, PageEncoding::Gzip), &mut builds), Bytes::from_static(b"events body"));
    assert_eq!(builds, 1, "a repeat request serves the answer the memo kept");

    assert_eq!(
      answer(&mut memo, &key(&session, PageEncoding::Identity), &mut builds),
      Bytes::from_static(b"events body")
    );
    assert_eq!(builds, 2, "another encoding renders its own body");

    let renamed = SessionMetadata::new("synthetic", "Renamed page", TaskProfile::Manager);
    assert_eq!(answer(&mut memo, &key(&renamed, PageEncoding::Gzip), &mut builds), Bytes::from_static(b"events body"));
    assert_eq!(builds, 3, "a moved session row renders again");
  }

  #[test]
  fn a_repeat_page_body_is_served_from_the_memo_until_an_input_changes() {
    let mut memo = PageBodyMemo::default();
    let sidebar = Arc::new("x".repeat(4096));
    let bootstrap = Arc::new("{\"messages\":[]}".to_string());
    let template = Arc::new(index_template());
    let (first, gzip) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), bootstrap.as_str()),
        Some(&sidebar),
        Some(&bootstrap),
        PageEncoding::Gzip,
      )
      .unwrap();
    let gzip = gzip.expect("a gzip client gets a compressed body");
    assert_eq!(gunzip(&gzip.bytes), String::from_utf8(first.to_vec()).unwrap());
    let (again, again_gzip) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), bootstrap.as_str()),
        Some(&sidebar),
        Some(&bootstrap),
        PageEncoding::Gzip,
      )
      .unwrap();
    assert_eq!(again.as_ptr(), first.as_ptr(), "a repeat request serves the body the memo kept");
    assert_eq!(again_gzip.expect("the compressed body was kept").bytes, gzip.bytes);

    let new_bootstrap = Arc::new("{\"messages\":[1]}".to_string());
    let (rebuilt, _) = memo
      .body(
        "index.html",
        &template,
        &substitutions(sidebar.as_str(), new_bootstrap.as_str()),
        Some(&sidebar),
        Some(&new_bootstrap),
        PageEncoding::Identity,
      )
      .unwrap();
    assert!(rebuilt.ends_with(b"{\"messages\":[1]};"), "a new bootstrap text rebuilds the body");
  }

  #[test]
  fn every_session_of_a_page_serves_its_one_static_tail_form() {
    let mut memo = PageBodyMemo::default();
    let template = Arc::new(tailed_index_template());
    let sidebar = Arc::new("x".repeat(4096));
    for bootstrap_text in ["{\"messages\":[]}", "{\"messages\":[1,2,3]}"] {
      let bootstrap = Arc::new(bootstrap_text.to_string());
      let (body, compressed) = memo
        .body(
          "index.html",
          &template,
          &substitutions(sidebar.as_str(), bootstrap.as_str()),
          Some(&sidebar),
          Some(&bootstrap),
          PageEncoding::Gzip,
        )
        .unwrap();
      let body = String::from_utf8(body.to_vec()).unwrap();
      assert!(body.ends_with("</script>"), "the body carries the template's tail");
      let gzip = compressed.expect("a gzip client gets a compressed body");
      assert_eq!(gunzip(&gzip.bytes), body);
    }
    assert_eq!(memo.tails.len(), 1, "both bodies take the same tail form, built once");
    let tail = memo.tails[0].tail.bytes();
    assert!(tail.len() >= PAGE_TAIL_MINIMUM && tail.ends_with(b"</script>"), "the kept tail is the template's scripts");

    let bootstrap = Arc::new("{\"messages\":[1,2,3]}".to_string());
    let mut page = |encoding| {
      memo
        .body(
          "index.html",
          &template,
          &substitutions(sidebar.as_str(), bootstrap.as_str()),
          Some(&sidebar),
          Some(&bootstrap),
          encoding,
        )
        .unwrap()
    };
    let (body, _) = page(PageEncoding::Identity);
    let (_, compressed) = page(PageEncoding::Zstd);
    let stream = compressed.expect("a zstd client gets a compressed body").bytes;
    let decoded = zstd::decode_all(stream.as_ref()).expect("a zstd page body decodes");
    assert_eq!(decoded, body.to_vec(), "the two zstd frames decode to the head and the tail");
  }

  #[test]
  fn a_reader_switching_between_two_sessions_serves_each_kept_body() {
    let mut memo = PageBodyMemo::default();
    let template = Arc::new(index_template());
    let sidebar = Arc::new("x".repeat(4096));
    let a = Arc::new("{\"messages\":[\"a\"]}".to_string());
    let b = Arc::new("{\"messages\":[\"b\"]}".to_string());
    let open = |memo: &mut PageBodyMemo, bootstrap: &Arc<String>| {
      memo
        .body(
          "index.html",
          &template,
          &substitutions(sidebar.as_str(), bootstrap.as_str()),
          Some(&sidebar),
          Some(bootstrap),
          PageEncoding::Identity,
        )
        .unwrap()
        .0
    };
    let first_a = open(&mut memo, &a);
    let first_b = open(&mut memo, &b);
    let again_a = open(&mut memo, &a);
    let again_b = open(&mut memo, &b);
    assert_eq!(again_a.as_ptr(), first_a.as_ptr(), "the session opened before is served from the memo");
    assert_eq!(again_b.as_ptr(), first_b.as_ptr(), "and so is the one opened after it");
    assert_eq!(memo.builds, 2, "each session's body is built once");
  }

  #[test]
  fn a_page_keeps_its_newest_bodies_and_rebuilds_the_one_it_dropped() {
    let mut memo = PageBodyMemo::default();
    let template = Arc::new(index_template());
    let sidebar = Arc::new("x".repeat(4096));
    let bootstraps: Vec<Arc<String>> =
      (0..=PAGE_BODY_MEMO_ENTRIES_PER_PAGE).map(|at| Arc::new(format!("{{\"messages\":[{at}]}}"))).collect();
    let open = |memo: &mut PageBodyMemo, bootstrap: &Arc<String>| {
      memo
        .body(
          "index.html",
          &template,
          &substitutions(sidebar.as_str(), bootstrap.as_str()),
          Some(&sidebar),
          Some(bootstrap),
          PageEncoding::Identity,
        )
        .unwrap()
        .0
    };
    for bootstrap in &bootstraps {
      open(&mut memo, bootstrap);
    }
    assert_eq!(memo.builds, PAGE_BODY_MEMO_ENTRIES_PER_PAGE + 1, "each new session's body is built once");
    open(&mut memo, &bootstraps[PAGE_BODY_MEMO_ENTRIES_PER_PAGE - 1]);
    assert_eq!(memo.builds, PAGE_BODY_MEMO_ENTRIES_PER_PAGE + 1, "the last bodies the page kept are still there");
    open(&mut memo, &bootstraps[0]);
    assert_eq!(memo.builds, PAGE_BODY_MEMO_ENTRIES_PER_PAGE + 2, "the body the page dropped is built again");
  }

  #[test]
  fn a_changed_template_rebuilds_the_page_body() {
    let mut memo = PageBodyMemo::default();
    let sidebar = Arc::new("[]".to_string());
    let bootstrap = Arc::new("null".to_string());
    let substitutions = substitutions(sidebar.as_str(), bootstrap.as_str());
    let (first, _) = memo
      .body(
        "index.html",
        &Arc::new(index_template()),
        &substitutions,
        Some(&sidebar),
        Some(&bootstrap),
        PageEncoding::Identity,
      )
      .unwrap();
    let edited = Arc::new(format!("{} // edited on disk", index_template()));
    let (second, _) = memo
      .body("index.html", &edited, &substitutions, Some(&sidebar), Some(&bootstrap), PageEncoding::Identity)
      .unwrap();
    assert_ne!(first, second, "a template edited on disk never matches the memo");
    assert!(second.ends_with(b"// edited on disk"));
  }

  #[test]
  fn each_page_keeps_its_own_body() {
    // A page's body is keyed by its name as well as its rendered template: rendering the home page between two
    // events viewer requests leaves the viewer's body in the memo, and a returning request serves it again.
    let mut memo = PageBodyMemo::default();
    let home_template: Arc<String> = Arc::new("<p>home</p>".to_string());
    let events_template: Arc<String> = Arc::new("<p>events</p>".to_string());
    let (home, _) = memo.body("home.html", &home_template, &[], None, None, PageEncoding::Identity).unwrap();
    let (events, _) =
      memo.body("events_viewer.html", &events_template, &[], None, None, PageEncoding::Identity).unwrap();
    let (home_again, _) = memo.body("home.html", &home_template, &[], None, None, PageEncoding::Identity).unwrap();
    assert_eq!(home_again.as_ptr(), home.as_ptr(), "the home page keeps its body beside another page's");
    assert_eq!(events, "<p>events</p>");
    let changed: Arc<String> = Arc::new("<p>other session</p>".to_string());
    let (events_changed, _) =
      memo.body("events_viewer.html", &changed, &[], None, None, PageEncoding::Identity).unwrap();
    assert_eq!(events_changed, "<p>other session</p>", "a changed context rebuilds that page's body");
  }

  #[tokio::test]
  async fn a_gzip_client_gets_the_compressed_page_with_the_transport_headers() {
    let html = page_html();
    let compressed = PageCompressed { encoding: "gzip", bytes: Bytes::from(gzip_level_one(html.as_bytes())) };
    let (parts, body) = page_response(Bytes::from(html.clone()), Some(compressed), StatusCode::OK).into_parts();
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
  async fn a_zstd_client_gets_the_memo_smaller_form_under_its_own_header() {
    let html = page_html();
    let compressed = PageCompressed {
      encoding: "zstd",
      bytes: Bytes::from(charliebot_compat::compression::zstd_memoized_body(html.as_bytes())),
    };
    let (parts, body) = page_response(Bytes::from(html.clone()), Some(compressed), StatusCode::OK).into_parts();
    let headers = parts.headers;
    assert_eq!(headers[header::CONTENT_ENCODING], "zstd");
    assert_eq!(headers[header::VARY], "Accept-Encoding");
    let served = body.collect().await.expect("a page body collects").to_bytes();
    assert_eq!(headers[header::CONTENT_LENGTH].to_str().expect("a length header is text"), served.len().to_string());
    let decoded = zstd::decode_all(served.as_ref()).expect("a zstd page body decodes");
    assert_eq!(String::from_utf8(decoded).expect("the page body is UTF-8"), html);
  }

  #[test]
  fn the_encoding_follows_the_accept_encoding_weights() {
    let headers = |value: &'static str| {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
      headers
    };
    assert_eq!(PageEncoding::for_request(&headers("")), PageEncoding::Identity);
    assert_eq!(PageEncoding::for_request(&headers("gzip")), PageEncoding::Gzip);
    assert_eq!(PageEncoding::for_request(&headers("gzip, deflate, br, zstd")), PageEncoding::Zstd);
    assert_eq!(PageEncoding::for_request(&headers("zstd;q=0.5, gzip;q=1")), PageEncoding::Gzip);
    assert_eq!(PageEncoding::for_request(&headers("gzip;q=0, zstd")), PageEncoding::Zstd);
    assert_eq!(PageEncoding::for_request(&headers("gzip;q=0")), PageEncoding::Identity);
  }

  #[test]
  fn a_client_that_forbids_gzip_takes_the_identity_page_from_both_layers() {
    let headers = |value: &'static str| {
      let mut headers = HeaderMap::new();
      headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
      headers
    };
    assert_eq!(PageEncoding::for_request(&headers("gzip;q=0")), PageEncoding::Identity);
    assert_eq!(gzip::Responder::for_request(&headers("gzip;q=0")), gzip::Responder::Identity);
    assert_eq!(PageEncoding::for_request(&headers("gzip")), PageEncoding::Gzip);
    assert_eq!(gzip::Responder::for_request(&headers("gzip")), gzip::Responder::Gzip);
  }

  #[tokio::test]
  async fn the_memo_keeps_each_clients_form_of_one_body() {
    let mut memo = PageBodyMemo::default();
    let template = Arc::new(page_html());
    let (plain, none) = memo.body("home.html", &template, &[], None, None, PageEncoding::Identity).unwrap();
    assert!(none.is_none(), "an identity client gets no compressed form");
    let (_, gzip) = memo.body("home.html", &template, &[], None, None, PageEncoding::Gzip).unwrap();
    let (_, zstd) = memo.body("home.html", &template, &[], None, None, PageEncoding::Zstd).unwrap();
    let gzip = gzip.expect("a gzip client gets the gzip form");
    let zstd = zstd.expect("a zstd client gets the zstd form");
    assert_eq!(gzip.encoding, "gzip");
    assert_eq!(zstd.encoding, "zstd");
    assert_eq!(gunzip(&gzip.bytes), String::from_utf8(plain.to_vec()).unwrap());
    assert!(zstd.bytes.len() < gzip.bytes.len(), "the zstd form is smaller than the gzip form");
  }

  #[tokio::test]
  async fn an_identity_client_gets_the_page_uncompressed() {
    let html = page_html();
    let (parts, body) = page_response(Bytes::from(html.clone()), None, StatusCode::OK).into_parts();
    let headers = parts.headers;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    let served = body.collect().await.expect("a page body collects").to_bytes();
    assert_eq!(headers[header::CONTENT_LENGTH].to_str().expect("a length header is text"), served.len().to_string());
    assert_eq!(served, Bytes::from(html));
  }

  #[test]
  fn a_page_response_carries_the_status_its_caller_names() {
    let response = page_response(Bytes::from(page_html()), None, StatusCode::NOT_FOUND);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
  }
}
