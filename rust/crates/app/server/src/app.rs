//! The application: the route families under the Python app's middleware stack.
//!
//! Outermost first: the decoded-path rewrite (so routing sees `scope["path"]`), the WebSocket gate
//! (uvicorn's protocol switch, outside every HTTP middleware), the request log, the panic catcher
//! (`ServerErrorMiddleware`), the auth gate, transport gzip, then the router.

use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::Request;
use axum::extract::connect_info::Connected;
use axum::middleware;
use axum::response::Response;
use axum::serve::IncomingStream;
use charliebot_compat::responses;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower::util::BoxCloneSyncService;
use tower_http::catch_panic::CatchPanicLayer;

use crate::routes::{self, websockets};
use crate::state::AppState;
use crate::{auth, gzip, request_log, request_path};

/// Both ends of a connection: `scope["client"]` and `scope["server"]`.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionInfo {
  pub client: SocketAddr,
  pub server: SocketAddr,
}

impl Connected<IncomingStream<'_, TcpListener>> for ConnectionInfo {
  fn connect_info(stream: IncomingStream<'_, TcpListener>) -> ConnectionInfo {
    ConnectionInfo {
      client: *stream.remote_addr(),
      server: stream.io().local_addr().expect("an accepted TCP socket has a local address"),
    }
  }
}

/// The application service: the route families, then each registered router in registration order, under the
/// middleware stack.
pub fn build(state: Arc<AppState>) -> BoxCloneSyncService<Request, Response, std::convert::Infallible> {
  let features = Arc::clone(&state.features);
  let router = features
    .registry
    .routers()
    .iter()
    .fold(routes::router().with_state(state.clone()), |router, route| router.merge((route.build)(&features)))
    .layer(middleware::from_fn(gzip::layer))
    .layer(middleware::from_fn_with_state(state.clone(), auth::layer))
    .layer(CatchPanicLayer::custom(panic_response))
    .layer(middleware::from_fn(request_log::layer));
  let service = ServiceBuilder::new()
    .layer(middleware::from_fn(request_path::layer))
    .layer(middleware::from_fn_with_state(state, websockets::gate))
    .service(router);
  BoxCloneSyncService::new(service)
}

fn panic_response(panic: Box<dyn Any + Send + 'static>) -> Response {
  let detail = panic
    .downcast_ref::<String>()
    .cloned()
    .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
    .unwrap_or_else(|| "a handler panicked".to_string());
  responses::internal_error("RustPanic", detail)
}
