//! The `/ws/terminal` route and its per-profile tmux and PTY relay.

use std::sync::Arc;

use axum::response::Response;
use charliebot_registry::{FeatureContext, WebSocketRequest};
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;

mod pty;
mod terminal;

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.websocket("/ws/terminal", terminal_websocket)
}

fn terminal_websocket(_features: Arc<FeatureContext>, request: WebSocketRequest) -> BoxFuture<'static, Response> {
  Box::pin(async move {
    request.upgrade.on_upgrade(|socket| async move {
      tracing::info!("terminal_ws_connected");
      let _disconnected = DisconnectLog;
      terminal::run_terminal_attachment(socket).await;
    })
  })
}

struct DisconnectLog;

impl Drop for DisconnectLog {
  fn drop(&mut self) {
    tracing::info!("terminal_ws_disconnected");
  }
}

#[cfg(test)]
mod tests;
