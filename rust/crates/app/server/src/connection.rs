//! Both ends of a connection: the ASGI `scope["client"]` and `scope["server"]`.

use std::net::SocketAddr;

use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use tokio::net::TcpListener;

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
