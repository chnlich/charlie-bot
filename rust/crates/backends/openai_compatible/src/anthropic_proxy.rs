//! `POST /api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages`: an Anthropic Messages request answered
//! by an OpenAI-compatible upstream. The upstream call goes through the shared HTTP client.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{self, Sse};
use axum::response::{IntoResponse, Response};
use bstr::ByteSlice;
use charliebot_compat::http_client;
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use futures_util::stream::{self, Stream};
use serde_json::{Value, json};

use super::options::CcOpenAiOption;
use super::sse::{LineFramer, sse_event};
use super::translate::{self, Event, OpenAiChatStream};
use super::value_ops::{self, Fault};

const ROUTE: &str = "/api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages";
const ROUTE_PREFIX: &str = "/api/anthropic-proxy/openai-compatible/";
const ROUTE_SUFFIX: &str = "/v1/messages";

/// The route's router, mounted by the registry.
pub fn router(features: &Arc<FeatureContext>) -> Router {
  Router::new().route(ROUTE, post_only(messages)).with_state(Arc::clone(features))
}

/// Whether `path` is one of the route's paths: a single backend id between the prefix and the `v1/messages` suffix.
pub fn matches(path: &str) -> bool {
  path
    .strip_prefix(ROUTE_PREFIX)
    .and_then(|rest| rest.strip_suffix(ROUTE_SUFFIX))
    .is_some_and(|backend_id| !backend_id.is_empty() && !backend_id.contains('/'))
}

/// How a failed request is answered: a FastAPI `HTTPException` with its detail, or an unhandled exception.
enum Refusal {
  Detail(StatusCode, String),
  Unhandled { kind: String, message: String },
}

impl From<Fault> for Refusal {
  fn from(fault: Fault) -> Self {
    match fault {
      Fault::Value(text) => Refusal::Detail(StatusCode::BAD_REQUEST, text),
      Fault::Raised { kind, message } => Refusal::Unhandled { kind: kind.to_string(), message },
    }
  }
}

impl Refusal {
  fn into_response(self) -> Response {
    match self {
      Refusal::Detail(status, detail) => responses::json(status, &json!({ "detail": detail })),
      Refusal::Unhandled { kind, message } => responses::internal_error(&kind, message),
    }
  }
}

fn unhandled(kind: &str, message: impl Into<String>) -> Refusal {
  Refusal::Unhandled { kind: kind.to_string(), message: message.into() }
}

fn detail(status: StatusCode, text: impl Into<String>) -> Refusal {
  Refusal::Detail(status, text.into())
}

/// A failed upstream request: httpx's class names for the two failures a request can meet.
fn upstream_failure(error: reqwest::Error) -> Refusal {
  let kind = if error.is_connect() { "ConnectError" } else { "RequestError" };
  unhandled(kind, error.to_string())
}

async fn messages(State(features): State<Arc<FeatureContext>>, Path(backend_id): Path<String>, body: Body) -> Response {
  match forward(&features, &backend_id, body).await {
    Ok(response) => response,
    Err(refusal) => refusal.into_response(),
  }
}

async fn forward(features: &FeatureContext, backend_id: &str, body: Body) -> Result<Response, Refusal> {
  let cfg = features.config.get().map_err(|error| unhandled("ConfigError", error.to_string()))?;
  let option = cfg
    .get_backend_option(backend_id)
    .ok_or_else(|| detail(StatusCode::NOT_FOUND, format!("unknown backend id: {backend_id}")))?;
  if option.backend_type != "cc-openai-compatible" {
    return Err(detail(
      StatusCode::BAD_REQUEST,
      format!("backend '{backend_id}' is not type 'cc-openai-compatible' (got '{}')", option.backend_type),
    ));
  }
  let fields = CcOpenAiOption::from_fields(&option.type_fields).map_err(|error| unhandled("ConfigError", error))?;
  if fields.api_base.is_empty() {
    return Err(detail(StatusCode::BAD_REQUEST, format!("backend '{backend_id}' missing api_base")));
  }
  if option.model.is_empty() {
    return Err(detail(StatusCode::BAD_REQUEST, format!("backend '{backend_id}' missing model")));
  }

  let body = to_bytes(body, usize::MAX).await.map_err(|error| unhandled("ClientDisconnect", error.to_string()))?;
  let anthropic_payload = value_ops::loads_bytes(&body)?;
  let gemini_stream_rules =
    translate::gemini_stream_rules(&fields.api_base).map_err(|error| unhandled("ValueError", error))?;
  let openai_payload = translate::anthropic_to_openai_chat_request(&anthropic_payload, &option.model)?;
  let upstream_url = format!("{}/chat/completions", fields.api_base.trim_end_matches('/'));
  let authorization = match fields.credential.as_deref().filter(|section| !section.is_empty()) {
    None => None,
    Some(section) => {
      let credentials =
        features.credentials.get().map_err(|error| detail(StatusCode::BAD_REQUEST, error.to_string()))?;
      let key =
        credentials.require(section, "api_key").map_err(|error| detail(StatusCode::BAD_REQUEST, error.to_string()))?;
      Some(format!("Bearer {key}"))
    }
  };

  // The shared client is built on the first proxy request, so a bad TLS setting fails that request rather than the
  // server's start.
  let client = http_client::shared_client().map_err(|error| unhandled("ClientError", format!("{error:#}")))?;
  let mut request = client
    .post(&upstream_url)
    .header(header::CONTENT_TYPE, "application/json")
    .body(text_repr::json_dumps(&openai_payload, &text_repr::STARLETTE_JSON));
  if let Some(authorization) = authorization {
    request = request.header(header::AUTHORIZATION, authorization);
  }
  let upstream = request.send().await.map_err(upstream_failure)?;
  if upstream.status().as_u16() >= 400 {
    return Err(upstream_error(upstream).await);
  }

  if openai_payload.get("stream").and_then(Value::as_bool) == Some(true) {
    return Ok(stream_response(upstream, &option.model, gemini_stream_rules));
  }
  let answer = upstream.bytes().await.map_err(upstream_failure)?;
  let upstream_json = value_ops::loads_bytes(&answer).map_err(answer_refusal)?;
  let message = translate::openai_chat_response_to_anthropic(&upstream_json, &option.model).map_err(answer_refusal)?;
  Ok(responses::json(StatusCode::OK, &message))
}

/// A translation failure of the upstream's answer: a `ValueError` is a 502 with its text, any other exception a 500.
fn answer_refusal(fault: Fault) -> Refusal {
  match fault {
    Fault::Value(text) => detail(StatusCode::BAD_GATEWAY, text),
    other => other.into(),
  }
}

/// The upstream's error status with its body as the detail. The body decodes with replacement, as httpx's text does.
async fn upstream_error(upstream: reqwest::Response) -> Refusal {
  let status = upstream.status();
  match upstream.bytes().await {
    Ok(body) => detail(status, String::from_utf8_lossy(&body).into_owned()),
    Err(error) => upstream_failure(error),
  }
}

/// The streamed answer: the start event first, then the translated events as the upstream's lines arrive.
fn stream_response(upstream: reqwest::Response, model: &str, gemini_stream_rules: bool) -> Response {
  let translator = OpenAiChatStream::new(model, gemini_stream_rules);
  let mut relay = Relay {
    upstream,
    framer: LineFramer::default(),
    translator,
    frames: VecDeque::new(),
    failure: None,
    reading: true,
    done: false,
  };
  relay.queue(relay.translator.start_events());
  sse_response(relay_frames(relay))
}

/// The SSE body of `events`, with the answer's headers: `text/event-stream` carries a charset, and `no-cache` is
/// dropped.
fn sse_response(events: impl Stream<Item = Result<sse::Event, io::Error>> + Send + 'static) -> Response {
  let mut response = Sse::new(events).into_response();
  let headers = response.headers_mut();
  headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
  headers.remove(header::CACHE_CONTROL);
  response
}

/// The state of one streamed answer: the upstream being read, the line framer, the translator, and the frames ready
/// to send.
struct Relay {
  upstream: reqwest::Response,
  framer: LineFramer,
  translator: OpenAiChatStream,
  frames: VecDeque<sse::Event>,
  /// The failure that ends the body, sent after the frames queued before it.
  failure: Option<io::Error>,
  /// Whether the upstream still has lines to read. `[DONE]` and the upstream's end both clear it.
  reading: bool,
  done: bool,
}

/// The body's frames: every queued frame, then the failure that ends the body early, then the end. A failure leaves the
/// body without `message_stop`.
fn relay_frames(state: Relay) -> impl Stream<Item = Result<sse::Event, io::Error>> {
  stream::unfold(state, |mut relay| async move {
    loop {
      if let Some(frame) = relay.frames.pop_front() {
        return Some((Ok(frame), relay));
      }
      if let Some(failure) = relay.failure.take() {
        // hyper writes the frames it has been given only when the body waits, so yield once. The frames already
        // produced then reach the client before the failure closes the connection.
        tokio::task::yield_now().await;
        return Some((Err(failure), relay));
      }
      if relay.done {
        return None;
      }
      relay.advance().await;
    }
  })
}

impl Relay {
  fn queue(&mut self, events: Vec<Event>) {
    for (name, data) in events {
      self.frames.push_back(sse_event(name, &data));
    }
  }

  async fn advance(&mut self) {
    if let Err((kind, message)) = self.step().await {
      tracing::error!(error = %kind, detail = %message, "Exception in ASGI application");
      self.failure = Some(io::Error::other(message));
      self.done = true;
    }
  }

  /// Reads the upstream's next chunk and handles its complete lines, or finishes the message once the upstream is done.
  async fn step(&mut self) -> Result<(), (String, String)> {
    if !self.reading {
      let events = self.translator.finish_events().map_err(fault_failure)?;
      self.queue(events);
      self.done = true;
      return Ok(());
    }
    match self.upstream.chunk().await.map_err(|error| ("RequestError".to_string(), error.to_string()))? {
      Some(chunk) => {
        for line in self.framer.push(&chunk) {
          self.handle_line(&line)?;
          if !self.reading {
            break;
          }
        }
      }
      None => {
        if let Some(tail) = std::mem::take(&mut self.framer).finish() {
          self.handle_line(&tail)?;
        }
        self.reading = false;
      }
    }
    Ok(())
  }

  /// `_iter_anthropic_sse`'s handling of one line: a `data:` line carries one chunk, and `[DONE]` ends the reading.
  fn handle_line(&mut self, line: &[u8]) -> Result<(), (String, String)> {
    let Some(raw) = line.strip_prefix(b"data:") else {
      return Ok(());
    };
    let raw = trim_space(raw);
    if raw == b"[DONE]" {
      self.reading = false;
      return Ok(());
    }
    if raw.is_empty() {
      return Ok(());
    }
    let chunk: Value =
      serde_json::from_slice(raw).map_err(|error| ("JSONDecodeError".to_string(), error.to_string()))?;
    let events = self.translator.events_for_chunk(&chunk).map_err(fault_failure)?;
    self.queue(events);
    Ok(())
  }
}

fn fault_failure(fault: Fault) -> (String, String) {
  match fault {
    Fault::Value(text) => ("ValueError".to_string(), text),
    Fault::Raised { kind, message } => (kind.to_string(), message),
  }
}

/// `bytes.strip()`: the ASCII whitespace of a Python bytes value, which includes the vertical tab.
fn trim_space(raw: &[u8]) -> &[u8] {
  raw.trim_with(|character| matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}'))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use axum::response::Response;
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry};
  use charliebot_store::Store;
  use serde_json::{Value, json};
  use tower::ServiceExt;

  use super::*;

  fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(Registry::new());
    let config_value = registry.load_config(home.path()).unwrap();
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .unwrap(),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "proxy_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "proxy_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    (home, features)
  }

  async fn send(router: &Router, method: Method, path: &str) -> Response {
    let request = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
    router.clone().oneshot(request).await.unwrap()
  }

  #[test]
  fn the_route_matches_one_backend_id_under_the_prefix() {
    assert!(matches("/api/anthropic-proxy/openai-compatible/cc-glm52/v1/messages"));
    assert!(!matches("/api/anthropic-proxy/openai-compatible//v1/messages"));
    assert!(!matches("/api/anthropic-proxy/openai-compatible/a/b/v1/messages"));
    assert!(!matches("/api/anthropic-proxy/openai-compatible/cc-glm52/v1/messages/x"));
  }

  #[test]
  fn strip_removes_the_vertical_tab_too() {
    assert_eq!(trim_space(b" \x0b{}\r\x0c"), b"{}");
    assert_eq!(trim_space(b" \t "), b"");
  }

  #[tokio::test]
  async fn a_streamed_answer_carries_the_recorded_headers_and_frames() {
    let events = stream::iter([Ok::<_, io::Error>(sse_event("message_stop", &json!({})))]);
    let response = sse_response(events);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().len(), 1);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream; charset=utf-8");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
  }

  #[tokio::test]
  async fn proxy_route_refuses_head_and_put_and_serves_post() {
    let (_home, features) = feature_context();
    let router = router(&features);
    let path = "/api/anthropic-proxy/openai-compatible/synthetic-missing/v1/messages";

    let head = send(&router, Method::HEAD, path).await;
    assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(head.headers()[header::ALLOW], "POST");

    let put = send(&router, Method::PUT, path).await;
    assert_eq!(put.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(put.headers()[header::ALLOW], "POST");
    let body = to_bytes(put.into_body(), usize::MAX).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({ "detail": "Method Not Allowed" }));

    let post = send(&router, Method::POST, path).await;
    assert_eq!(post.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(post.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
      serde_json::from_slice::<Value>(&body).unwrap(),
      json!({ "detail": "unknown backend id: synthetic-missing" })
    );
  }
}
