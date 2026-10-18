use std::path::PathBuf;

use axum::extract::ws::WebSocket;
use charliebot_core::{home, paths};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::pty::{self, PtyAttachment};

const TERMINAL_SESSION_ID: &str = "terminal";

static ENSURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn terminal_session_id() -> Result<String, String> {
  let profile_home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let default_home = PathBuf::from(paths::expanduser("~")).join(".charliebot");
  if profile_home == default_home {
    return Ok(TERMINAL_SESSION_ID.to_string());
  }
  let digest = Sha256::digest(profile_home.to_string_lossy().as_bytes());
  let suffix = digest[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
  Ok(format!("{TERMINAL_SESSION_ID}-{suffix}"))
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
  let cwd = PathBuf::from(paths::expanduser("~"));
  let mut env_args = Vec::new();
  if std::env::var_os(home::CHARLIEBOT_HOME_ENV).is_some_and(|value| !value.to_string_lossy().trim().is_empty()) {
    let profile_home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
    env_args.push("-e".to_string());
    env_args.push(format!("{}={}", home::CHARLIEBOT_HOME_ENV, profile_home.display()));
  }
  pty::start_tmux_session(&name, &cwd, &env_args, &["bash", "-l"]).await?;
  tracing::info!(name = %name, cwd = %cwd.display(), "terminal_tmux_session_created");
  Ok(())
}

pub(super) async fn run_terminal_attachment(mut socket: WebSocket) {
  if let Err(error) = ensure_terminal_session().await {
    tracing::error!(error = %error, "terminal_ensure_session_failed");
    if let Err(send_error) = pty::send_json(&mut socket, &json!({"type": pty::PTY_EXIT, "error": error})).await {
      tracing::debug!(error = %send_error, "terminal_ensure_error_send_failed");
    }
    return;
  }

  let session_id = terminal_session_id().expect("the profile home resolved while ensuring the terminal session");
  let attachment = match PtyAttachment::spawn(&session_id) {
    Ok(attachment) => attachment,
    Err(error) => {
      tracing::error!(error = %error, "terminal_pty_spawn_failed");
      if let Err(send_error) = pty::send_json(&mut socket, &json!({"type": pty::PTY_EXIT, "error": error})).await {
        tracing::debug!(error = %send_error, "terminal_spawn_error_send_failed");
      }
      return;
    }
  };

  if let Err(error) = pty::run_pty_relay(socket, attachment).await {
    tracing::error!(error = %error, "Exception in ASGI application");
  }
}
