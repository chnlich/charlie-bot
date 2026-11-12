//! One `http_request` event per HTTP response.
//!
//! The event fires once the response body has been sent or dropped. Its fields are `method`, the decoded
//! `path` without the query string (the terminal WebSocket carries its credential there), `status`,
//! `duration_ms`, `client` and `forwarded_for`. A failed request adds `error`, the failure's kind.
//! `client` is the TCP peer address. `forwarded_for` is the first `X-Forwarded-For` address, which the
//! Tailscale Serve proxy sets to the browser's address, and `-` when the request reaches the server directly.
//! The layer sits outside the auth gate, so the gate's refusals are logged too.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use charliebot_compat::forwarded::first_address;
use charliebot_compat::responses::Failure;
use http_body::{Frame, SizeHint};

use crate::connection::ConnectionInfo;
use crate::request_path;

/// The fields of one request's event, logged when dropped.
struct PendingEvent {
  method: String,
  path: String,
  status: u16,
  started: Instant,
  client: String,
  forwarded_for: String,
  error: Option<String>,
}

impl Drop for PendingEvent {
  fn drop(&mut self) {
    // Round half to even.
    let duration_ms = (self.started.elapsed().as_secs_f64() * 1000.0).round_ties_even() as u64;
    match &self.error {
      None => tracing::info!(
        method = %self.method,
        path = %self.path,
        status = self.status,
        duration_ms,
        client = %self.client,
        forwarded_for = %self.forwarded_for,
        "http_request"
      ),
      Some(error) => tracing::info!(
        method = %self.method,
        path = %self.path,
        status = self.status,
        duration_ms,
        client = %self.client,
        forwarded_for = %self.forwarded_for,
        error = %error,
        "http_request"
      ),
    }
  }
}

/// A response body that carries the pending event until the body is finished with.
struct LoggedBody {
  inner: Body,
  _event: PendingEvent,
}

impl http_body::Body for LoggedBody {
  type Data = Bytes;
  type Error = axum::Error;

  fn poll_frame(
    mut self: Pin<&mut Self>,
    context: &mut Context<'_>,
  ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
    Pin::new(&mut self.inner).poll_frame(context)
  }

  fn is_end_stream(&self) -> bool {
    self.inner.is_end_stream()
  }

  fn size_hint(&self) -> SizeHint {
    self.inner.size_hint()
  }
}

/// The request log as an axum layer.
pub async fn layer(request: Request, next: Next) -> Response {
  let started = Instant::now();
  let method = request.method().as_str().to_string();
  let path = request_path::of(&request).to_string();
  let client = request
    .extensions()
    .get::<ConnectInfo<ConnectionInfo>>()
    .map_or_else(|| "-".to_string(), |ConnectInfo(connection)| connection.client.ip().to_string());
  let forwarded_for = first_address(request.headers());
  let response = next.run(request).await;
  let event = PendingEvent {
    method,
    path,
    status: response.status().as_u16(),
    started,
    client,
    forwarded_for,
    error: response.extensions().get::<Failure>().map(|failure| failure.kind.clone()),
  };
  let (parts, body) = response.into_parts();
  Response::from_parts(parts, Body::new(LoggedBody { inner: body, _event: event }))
}

#[cfg(test)]
mod tests {
  use std::fmt;
  use std::sync::{Arc, Mutex};

  use axum::body::Body;
  use axum::extract::{ConnectInfo, Request};
  use axum::http::header::HeaderValue;
  use axum::routing::get;
  use charliebot_core::test_support::{EventSink, install_events};
  use tower::ServiceExt;
  use tracing::Event;
  use tracing::field::{Field, Visit};

  use charliebot_compat::forwarded::X_FORWARDED_FOR;

  use super::layer;
  use crate::connection::ConnectionInfo;
  use crate::request_path;

  /// The `(name, value)` pairs of one event, in the order the event records them.
  type Recorded = Vec<(String, String)>;

  #[derive(Default)]
  struct Visitor(Recorded);

  impl Visit for Visitor {
    fn record_str(&mut self, field: &Field, value: &str) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
      self.0.push((field.name().to_string(), format!("{value:?}")));
    }
  }

  struct Capture(Arc<Mutex<Vec<Recorded>>>);

  impl EventSink for Capture {
    fn event(&self, event: &Event<'_>) {
      let mut visitor = Visitor::default();
      event.record(&mut visitor);
      self.0.lock().unwrap().push(visitor.0);
    }
  }

  fn field<'a>(event: &'a Recorded, name: &str) -> &'a str {
    event
      .iter()
      .find_map(|(field, value)| (field == name).then_some(value.as_str()))
      .unwrap_or_else(|| panic!("the http_request event has no {name} field"))
  }

  /// The layer over one route, under the path layer the real app puts above it, with the connect info a
  /// Tailscale Serve proxy presents. The TCP peer is the proxy on the loopback interface.
  fn app() -> axum::Router {
    axum::Router::new()
      .route("/probe", get(|| async { "ok" }))
      .layer(axum::middleware::from_fn(layer))
      .layer(axum::middleware::from_fn(request_path::layer))
  }

  fn request(forwarded_for: Option<&str>) -> Request<Body> {
    let mut builder = Request::get("/probe").extension(ConnectInfo(ConnectionInfo {
      client: "127.0.0.1:42488".parse().expect("the client address"),
      server: "127.0.0.1:18498".parse().expect("the server address"),
    }));
    if let Some(value) = forwarded_for {
      builder = builder.header(&X_FORWARDED_FOR, HeaderValue::from_str(value).expect("the header value"));
    }
    builder.body(Body::empty()).expect("build the request")
  }

  #[tokio::test]
  async fn forwarded_for_carries_the_proxy_header_and_falls_back_to_the_absent_marker() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let _capture = install_events(Arc::new(Capture(Arc::clone(&events))));
    let app = app();

    for forwarded in [Some("100.84.122.38"), Some("100.84.122.38, 10.1.2.3"), Some(" 100.84.122.38 "), None] {
      let response = app.clone().oneshot(request(forwarded)).await.expect("the route answers");
      assert_eq!(response.status(), axum::http::StatusCode::OK, "{forwarded:?}");
      drop(response);
    }

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 4);
    let forwarded = ["100.84.122.38", "100.84.122.38", "100.84.122.38", "-"];
    for (event, forwarded) in events.iter().zip(forwarded) {
      assert_eq!(field(event, "forwarded_for"), forwarded, "event: {event:?}");
      // The TCP peer keeps its own address; the forwarded header never reaches it.
      assert_eq!(field(event, "client"), "127.0.0.1", "event: {event:?}");
    }
  }
}
