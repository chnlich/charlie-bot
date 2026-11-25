use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_registry::{FeatureContext, WebSocketRequest};
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;

use crate::{pty, terminal};

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
      terminal::serve_attachment(socket, grid).await;
    })
  })
}
