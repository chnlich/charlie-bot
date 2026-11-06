//! The `/ws/terminal` route and its per-profile tmux and PTY relay.

use std::sync::Arc;

use axum::extract::ws::WebSocket;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_registry::{FeatureContext, WebSocketRequest};
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;

mod pty;
mod terminal;

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.websocket("/ws/terminal", terminal_websocket)
}

fn terminal_websocket(_features: Arc<FeatureContext>, request: WebSocketRequest) -> BoxFuture<'static, Response> {
  let grid = match pty::Grid::from_query(&request.query) {
    Ok(grid) => grid,
    Err(error) => {
      // The query carries the access token, so the log names only the refusal.
      tracing::warn!(error = %error, "terminal_ws_bad_grid");
      return Box::pin(async move { (StatusCode::BAD_REQUEST, error).into_response() });
    }
  };
  Box::pin(async move {
    request.upgrade.on_upgrade(move |socket| async move {
      tracing::info!("terminal_ws_connected");
      serve_attachment(socket, grid).await;
    })
  })
}

/// Runs one attachment and logs its end: `terminal_ws_disconnected` from the disconnect guard,
/// then the exception that escaped the handler.
async fn serve_attachment(socket: WebSocket, grid: pty::Grid) {
  let outcome = {
    let _disconnected = DisconnectLog;
    terminal::run_terminal_attachment(socket, grid).await
  };
  if let Err(error) = outcome {
    tracing::error!(error = %error, "Exception in ASGI application");
  }
}

struct DisconnectLog;

impl Drop for DisconnectLog {
  fn drop(&mut self) {
    tracing::info!("terminal_ws_disconnected");
  }
}

#[cfg(test)]
mod tests;
