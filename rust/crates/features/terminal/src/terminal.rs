use axum::extract::ws::WebSocket;
use charliebot_core::home;
use data_encoding::HEXLOWER;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::pty::{self, PtyAttachment};

const TERMINAL_SESSION_ID: &str = "terminal";

static ENSURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn terminal_session_id() -> Result<String, String> {
  let profile_home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  if profile_home == home::default_charliebot_home() {
    return Ok(TERMINAL_SESSION_ID.to_string());
  }
  let digest = Sha256::digest(profile_home.to_string_lossy().as_bytes());
  Ok(format!("{TERMINAL_SESSION_ID}-{}", HEXLOWER.encode(&digest[..4])))
}

pub(super) fn terminal_tmux_name(session_id: &str) -> String {
  pty::tmux_session_name(session_id)
}

pub(super) async fn ensure_terminal_session() -> Result<(), String> {
  let _guard = ENSURE_LOCK.lock().await;
  let session_id = terminal_session_id()?;
  if pty::tmux_session_exists(&session_id).await? {
    return Ok(());
  }

  let name = terminal_tmux_name(&session_id);
  // The pane starts in the operator's home, which is the parent of the default CharlieBot home.
  let default_home = home::default_charliebot_home();
  let cwd = default_home.parent().ok_or("the default CharlieBot home has no parent directory")?;
  // A pane inherits the tmux server's environment, which another profile may have started, so the profile
  // home is passed to the pane explicitly.
  let mut env_args = Vec::new();
  if let Some(profile_home) = home::explicit_charliebot_home().map_err(|error| error.to_string())? {
    env_args.push("-e".to_string());
    env_args.push(format!("{}={}", home::CHARLIEBOT_HOME_ENV, profile_home.display()));
  }
  pty::start_tmux_session(&name, cwd, &env_args, &["bash", "-l"]).await?;
  tracing::info!(name = %name, cwd = %cwd.display(), "terminal_tmux_session_created");
  Ok(())
}

/// Attaches the socket to the profile's terminal. An `Err` is an exception that escaped the handler after accept;
/// the caller logs it and drops the socket.
pub(super) async fn run_terminal_attachment(mut socket: WebSocket) -> Result<(), String> {
  if let Err(error) = ensure_terminal_session().await {
    tracing::error!(error = %error, "terminal_ensure_session_failed");
    if let Err(send_error) = pty::send_json(&mut socket, &json!({"type": pty::PTY_EXIT, "error": error})).await {
      tracing::debug!(error = %send_error, "terminal_ensure_error_send_failed");
    }
    return Ok(());
  }

  let session_id = terminal_session_id()?;
  let attachment = match PtyAttachment::spawn(&session_id) {
    Ok(attachment) => attachment,
    Err(error) => {
      tracing::error!(error = %error, "terminal_pty_spawn_failed");
      if let Err(send_error) = pty::send_json(&mut socket, &json!({"type": pty::PTY_EXIT, "error": error})).await {
        tracing::debug!(error = %send_error, "terminal_spawn_error_send_failed");
      }
      return Ok(());
    }
  };

  pty::run_pty_relay(socket, attachment).await
}
