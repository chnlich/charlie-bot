//! One `http_request` event per HTTP response (`server._RequestLogMiddleware`).
//!
//! The event fires once the response body has been sent or dropped, as the Python middleware logs
//! when the inner app returns after its last body message. Its fields are `method`, the decoded
//! `path` without the query string (the terminal WebSocket carries its credential there), `status`,
//! `duration_ms` and `client`; a failed request adds `error`, the failure's Python exception class.
//! The layer sits outside the auth gate, so the gate's refusals are logged too.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use charliebot_compat::responses::Failure;
use http_body::{Frame, SizeHint};

use crate::app::ConnectionInfo;
use crate::request_path;

/// The fields of one request's event, logged when dropped.
struct PendingEvent {
  method: String,
  path: String,
  status: u16,
  started: Instant,
  client: String,
  error: Option<String>,
}

impl Drop for PendingEvent {
  fn drop(&mut self) {
    // Python's round() rounds half to even.
    let duration_ms = (self.started.elapsed().as_secs_f64() * 1000.0).round_ties_even() as u64;
    match &self.error {
      None => tracing::info!(
        method = %self.method,
        path = %self.path,
        status = self.status,
        duration_ms,
        client = %self.client,
        "http_request"
      ),
      Some(error) => tracing::info!(
        method = %self.method,
        path = %self.path,
        status = self.status,
        duration_ms,
        client = %self.client,
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
  let response = next.run(request).await;
  let event = PendingEvent {
    method,
    path,
    status: response.status().as_u16(),
    started,
    client,
    error: response.extensions().get::<Failure>().map(|failure| failure.kind.clone()),
  };
  let (parts, body) = response.into_parts();
  Response::from_parts(parts, Body::new(LoggedBody { inner: body, _event: event }))
}
