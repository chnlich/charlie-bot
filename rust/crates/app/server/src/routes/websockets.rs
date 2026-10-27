//! WebSocket upgrades: `/ws/sessions/{session_id}`, `/ws/terminal` and `/ws/voice/{session_id}`.
//!
//! uvicorn hands a request whose `Connection` tokens include `upgrade` and whose `Upgrade` is
//! `websocket` to its WebSocket protocol, outside the HTTP middleware: no auth gate, no request log, no
//! gzip. An unknown path, or a route whose `token` query parameter fails `check_ws_auth`, closes before
//! accepting, which the protocol answers as a bare 403. The terminal and voice routes still have no
//! registered Rust owner, so an authorized upgrade on either path answers the same 403 and logs
//! `websocket_route_not_ported`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode, header};

use crate::auth;
use crate::request_path;
use crate::state::AppState;

/// The WebSocket route patterns, `*` standing for one path segment.
const ROUTES: &[&str] = &["/ws/sessions/*", "/ws/terminal", "/ws/voice/*"];

/// uvicorn's `_get_upgrade`: the last `Upgrade` value, lowercased, when the last `Connection` header's
/// comma-separated tokens include `upgrade`.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
  let last_text = |name: header::HeaderName| {
    headers.get_all(name).iter().next_back().map(|value| value.as_bytes().to_ascii_lowercase())
  };
  let connection_upgrade = last_text(header::CONNECTION)
    .is_some_and(|value| value.split(|byte| *byte == b',').any(|token| token.trim_ascii() == b"upgrade"));
  connection_upgrade && last_text(header::UPGRADE).is_some_and(|value| value == b"websocket")
}

/// Whether a decoded path matches a WebSocket route.
fn matches_route(path: &str) -> bool {
  let segments: Vec<&str> = path.split('/').collect();
  ROUTES.iter().any(|route| {
    let pattern: Vec<&str> = route.split('/').collect();
    pattern.len() == segments.len()
      && pattern.iter().zip(&segments).all(|(want, got)| if *want == "*" { !got.is_empty() } else { want == got })
  })
}

/// The gate in front of the HTTP stack: upgrades end here.
pub async fn gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
  if !is_websocket_upgrade(request.headers()) {
    return next.run(request).await;
  }
  let path = request_path::of(&request).to_string();
  let registered = state.features.registry.websocket_match(&path);
  if registered.is_some() || matches_route(&path) {
    let key = match state.access_key() {
      Ok(key) => key,
      Err(error) => {
        tracing::error!(error = %error, "Exception in ASGI application");
        return forbidden();
      }
    };
    match auth::websocket_authorized(request.uri().query(), key.as_deref()) {
      Ok(true) => {
        if let Some(registered) = registered {
          let (mut parts, _) = request.into_parts();
          let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return rejection.into_response(),
          };
          let handler = registered.handler;
          let query = parts.uri.query().unwrap_or_default().to_string();
          return handler(
            Arc::clone(&state.features),
            charliebot_registry::WebSocketRequest { path, segments: registered.segments, query, upgrade },
          )
          .await;
        }
        tracing::warn!(path = %path, "websocket_route_not_ported");
      }
      Ok(false) => {}
      Err(auth::NonAsciiCredential) => {
        tracing::error!(error = "TypeError", "Exception in ASGI application");
      }
    }
  }
  forbidden()
}

/// The `websockets` protocol's answer to a close before accept.
fn forbidden() -> Response {
  Response::builder()
    .status(StatusCode::FORBIDDEN)
    .header(header::CONTENT_LENGTH, "0")
    .header(header::CONTENT_TYPE, "text/plain")
    .header(header::CONNECTION, "close")
    .body(Body::empty())
    .expect("a fixed 403 response builds")
}

#[cfg(test)]
mod tests {
  use std::net::SocketAddr;
  use std::path::{Path, PathBuf};
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::sync::{Arc, Mutex};

  use axum::extract::ws::Message;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_core::test_support::EventSink;
  use charliebot_registry::{FeatureContext, Registry, WebSocketRequest};
  use charliebot_store::Store;
  use futures_util::StreamExt;
  use futures_util::future::BoxFuture;
  use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
  use tokio::net::{TcpListener, TcpStream};
  use tracing::Event;
  use tracing::field::{Field, Visit};

  use super::*;
  use crate::buildinfo::BuildInfo;
  use crate::connection::ConnectionInfo;
  use crate::state::AppState;
  use http::HeaderValue;

  struct ContextMarker;

  struct SyntheticHome(PathBuf);

  impl SyntheticHome {
    fn new() -> Self {
      static NEXT_HOME: AtomicU64 = AtomicU64::new(0);
      let sequence = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
      let path = std::env::temp_dir().join(format!("charliebot-websocket-test-{}-{sequence}", std::process::id()));
      std::fs::create_dir(&path).expect("create a synthetic home");
      Self(path)
    }
  }

  impl Drop for SyntheticHome {
    fn drop(&mut self) {
      std::fs::remove_dir_all(&self.0).expect("remove the synthetic home");
    }
  }

  impl AsRef<Path> for SyntheticHome {
    fn as_ref(&self) -> &Path {
      &self.0
    }
  }

  fn exchange(features: Arc<FeatureContext>, request: WebSocketRequest) -> BoxFuture<'static, Response> {
    let WebSocketRequest { path, segments, query, upgrade } = request;
    Box::pin(async move {
      upgrade.on_upgrade(move |mut socket| async move {
        assert!(features.startup.get::<ContextMarker>().is_some());
        let Some(Ok(Message::Text(text))) = socket.next().await else {
          return;
        };
        let reply = format!("{}|{}|{}|{}|{}", path, segments.join(","), query, text, "context");
        socket.send(Message::Text(reply.into())).await.expect("send the echo frame");
      })
    })
  }
  fn register_exchange(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
    registry.websocket("/ws/test/*", exchange)
  }

  fn app_state(registry: Registry) -> (Arc<AppState>, SyntheticHome) {
    let home = SyntheticHome::new();
    std::fs::write(home.as_ref().join("credentials.yaml"), "charliebot:\n  access_key: synthetic-key\n")
      .expect("write synthetic credentials");
    let registry = Arc::new(registry);
    let config = registry.load_config(home.as_ref()).expect("load the empty test config");
    let config_registry = Arc::clone(&registry);
    let config_home = home.as_ref().to_path_buf();
    let config_cache = Arc::new(HotReload::new(
      home.as_ref().join("config.yaml"),
      "test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = home.as_ref().to_path_buf();
    let credentials_cache = Arc::new(HotReload::new(
      home.as_ref().join("credentials.yaml"),
      "test_credentials_reload_failed",
      Box::new(move || charliebot_core::credentials::load_credentials(&credentials_home)),
    ));
    let store = Arc::new(
      Store::new(
        config,
        registry.metadata_slots(),
        charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)),
      )
      .expect("construct the test store"),
    );
    let features = Arc::new(FeatureContext {
      config: config_cache,
      credentials: credentials_cache,
      store,
      registry: Arc::clone(&registry),
      startup: Arc::new(Default::default()),
    });
    features.startup.insert(ContextMarker).expect("insert the context marker");
    let state = Arc::new(AppState {
      features,
      build_info: BuildInfo { sha: "synthetic".to_string(), started_at: "synthetic".to_string() },
      static_mounts: Vec::new(),
      mime_types: charliebot_compat::mimetypes::MimeTypes::builtin(),
    });
    (state, home)
  }

  async fn start_server(state: Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback listener");
    let address = listener.local_addr().expect("read the loopback address");
    let service =
      axum::ServiceExt::<Request>::into_make_service_with_connect_info::<ConnectionInfo>(crate::app::build(state));
    let server = tokio::spawn(async move {
      axum::serve(listener, service).await.expect("serve the websocket test app");
    });
    (address, server)
  }

  async fn handshake(address: SocketAddr, path: &str) -> (BufReader<TcpStream>, u16) {
    let stream = TcpStream::connect(address).await.expect("connect to loopback server");
    let mut stream = BufReader::new(stream);
    let request = format!(
      "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
       Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.get_mut().write_all(request.as_bytes()).await.expect("write the websocket request");
    let mut status = String::new();
    stream.read_line(&mut status).await.expect("read the websocket status");
    let status = status
      .split_ascii_whitespace()
      .nth(1)
      .expect("the response has a status")
      .parse::<u16>()
      .expect("the response status is numeric");
    loop {
      let mut line = String::new();
      stream.read_line(&mut line).await.expect("read a response header");
      if line == "\r\n" || line.is_empty() {
        break;
      }
    }
    (stream, status)
  }

  async fn send_text(stream: &mut BufReader<TcpStream>, text: &str) {
    let payload = text.as_bytes();
    assert!(payload.len() < 126);
    let mask = [3, 7, 11, 19];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % mask.len()]));
    stream.get_mut().write_all(&frame).await.expect("send a masked text frame");
  }

  async fn receive_text(stream: &mut BufReader<TcpStream>) -> String {
    let mut header = [0; 2];
    stream.read_exact(&mut header).await.expect("read a websocket frame header");
    assert_eq!(header[0] & 0x0f, 1);
    assert_eq!(header[1] & 0x80, 0);
    let length = usize::from(header[1] & 0x7f);
    assert!(length < 126);
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.expect("read a websocket frame body");
    String::from_utf8(payload).expect("the reply is UTF-8")
  }

  struct LogMessages(Arc<Mutex<Vec<String>>>);

  struct MessageVisitor(Option<String>);

  impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
      if field.name() == "message" {
        self.0 = Some(format!("{value:?}"));
      }
    }
  }

  impl EventSink for LogMessages {
    fn event(&self, event: &Event<'_>) {
      let mut visitor = MessageVisitor(None);
      event.record(&mut visitor);
      if let Some(message) = visitor.0 {
        self.0.lock().expect("lock captured log messages").push(message);
      }
    }
  }

  /// Captures this test's log messages through the shared test capture, so each test reads its own messages.
  fn captured_logs() -> (Arc<Mutex<Vec<String>>>, charliebot_core::test_support::CaptureGuard) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let guard = charliebot_core::test_support::install_events(Arc::new(LogMessages(Arc::clone(&logs))));
    (logs, guard)
  }

  #[test]
  fn upgrade_detection_follows_uvicorn() {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
    assert!(is_websocket_upgrade(&headers));
    headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
    assert!(!is_websocket_upgrade(&headers));
    headers.remove(header::CONNECTION);
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    assert!(!is_websocket_upgrade(&headers));
  }

  #[test]
  fn routes_match_one_segment_per_parameter() {
    assert!(matches_route("/ws/sessions/s-1"));
    assert!(matches_route("/ws/terminal"));
    assert!(!matches_route("/ws/sessions/"));
    assert!(!matches_route("/ws/sessions/a/b"));
    assert!(!matches_route("/ws/terminal/"));
  }

  #[tokio::test]
  async fn registered_routes_authorize_dispatch_and_keep_unowned_routes_closed() {
    let (logs, _capture) = captured_logs();
    let mut registry = Registry::new();
    registry.register_package("src.features.test", register_exchange).expect("register the test route");
    let (state, _home) = app_state(registry);
    let (address, server) = start_server(state).await;

    let (mut socket, status) = handshake(address, "/ws/test/first?token=synthetic-key&part=raw%20query").await;
    assert_eq!(status, 101);
    send_text(&mut socket, "hello").await;
    assert_eq!(
      receive_text(&mut socket).await,
      "/ws/test/first|first|token=synthetic-key&part=raw%20query|hello|context"
    );

    let (_socket, status) = handshake(address, "/ws/test/unauthorized").await;
    assert_eq!(status, 403);

    for path in ["/ws/terminal?token=synthetic-key", "/ws/voice/synthetic?token=synthetic-key"] {
      logs.lock().expect("lock captured log messages").clear();
      let (_socket, status) = handshake(address, path).await;
      assert_eq!(status, 403);
      assert!(
        logs
          .lock()
          .expect("lock captured log messages")
          .iter()
          .any(|message| message.contains("websocket_route_not_ported")),
        "an unowned known route logs websocket_route_not_ported"
      );
    }

    server.abort();
    let _ = server.await;
  }
}
