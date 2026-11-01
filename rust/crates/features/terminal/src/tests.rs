use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use charliebot_core::home;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::{Error, Message};

use crate::{pty, register, serve_attachment, terminal};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScopedEnv {
  _lock: MutexGuard<'static, ()>,
  original: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
  fn new(keys: &[&'static str]) -> ScopedEnv {
    let lock = ENV_LOCK.lock().expect("the test environment lock is not poisoned");
    let original = keys.iter().map(|key| (*key, std::env::var_os(key))).collect();
    ScopedEnv { _lock: lock, original }
  }

  fn set(&self, key: &'static str, value: Option<&OsStr>) {
    unsafe {
      match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
      }
    }
  }
}

impl Drop for ScopedEnv {
  fn drop(&mut self) {
    for (key, value) in &self.original {
      unsafe {
        match value {
          Some(value) => std::env::set_var(key, value),
          None => std::env::remove_var(key),
        }
      }
    }
  }
}

struct TmuxCleanup {
  binary: std::path::PathBuf,
  socket_dir: std::path::PathBuf,
  armed: bool,
}

impl TmuxCleanup {
  fn new(binary: std::path::PathBuf, socket_dir: std::path::PathBuf) -> TmuxCleanup {
    TmuxCleanup { binary, socket_dir, armed: true }
  }

  fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for TmuxCleanup {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    match Command::new(&self.binary)
      .arg("-L")
      .arg("charliebot")
      .arg("kill-server")
      .env("TMUX_TMPDIR", &self.socket_dir)
      .env_remove("TMUX")
      .env_remove("CHARLIEBOT_SESSION_ID")
      .stdout(Stdio::null())
      .output()
    {
      Ok(output) if output.status.success() => {}
      Ok(output) => eprintln!("tmux test cleanup failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
      Err(error) => eprintln!("tmux test cleanup failed: {error}"),
    }
  }
}

#[test]
fn register_adds_the_terminal_websocket_route() {
  let mut registry = charliebot_registry::Registry::new();
  registry.register_package("src.features.terminal", register).unwrap();
  assert!(registry.websocket_match("/ws/terminal").is_some());
  assert!(registry.websocket_match("/ws/terminal/extra").is_none());
}

#[test]
fn terminal_ids_follow_the_profile_home() {
  let root = tempfile::tempdir().unwrap();
  let home_dir = root.path().join("operator-home");
  std::fs::create_dir(&home_dir).unwrap();
  let _env = ScopedEnv::new(&["HOME", home::CHARLIEBOT_HOME_ENV]);
  _env.set("HOME", Some(home_dir.as_os_str()));
  _env.set(home::CHARLIEBOT_HOME_ENV, None);
  assert_eq!(terminal::terminal_session_id().unwrap(), "terminal");
  assert_eq!(terminal::terminal_tmux_name("terminal"), "charliebot-terminal");

  _env.set(home::CHARLIEBOT_HOME_ENV, Some(OsStr::new("   ")));
  assert_eq!(terminal::terminal_session_id().unwrap(), "terminal");

  let profile = root.path().join("profile");
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(profile.as_os_str()));
  let profile_id = terminal::terminal_session_id().unwrap();
  assert_eq!(profile_id, expected_terminal_id(&profile));
  assert_eq!(terminal::terminal_tmux_name(&profile_id), format!("charliebot-{profile_id}"));
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(OsStr::new(&format!("{}/", profile.display()))));
  assert_eq!(terminal::terminal_session_id().unwrap(), profile_id);

  let second_profile = root.path().join("second-profile");
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(second_profile.as_os_str()));
  let second_profile_id = terminal::terminal_session_id().unwrap();
  assert_ne!(profile_id, second_profile_id);
  assert_ne!(terminal::terminal_tmux_name(&profile_id), terminal::terminal_tmux_name(&second_profile_id));
  assert!(terminal::terminal_tmux_name(&second_profile_id).starts_with("charliebot-terminal-"));

  _env.set(home::CHARLIEBOT_HOME_ENV, Some(OsStr::new("~/debug-profile")));
  assert_eq!(terminal::terminal_session_id().unwrap(), expected_terminal_id(&home_dir.join("debug-profile")));
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(OsStr::new("~/.charliebot")));
  assert_eq!(terminal::terminal_session_id().unwrap(), "terminal");
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(OsStr::new("relative-profile")));
  assert_eq!(
    terminal::terminal_session_id().unwrap_err(),
    "CHARLIEBOT_HOME must be an absolute path or start with '~'; got 'relative-profile'"
  );
}

fn expected_terminal_id(home: &Path) -> String {
  let digest = Sha256::digest(home.to_string_lossy().as_bytes());
  let suffix = digest[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
  format!("terminal-{suffix}")
}

#[test]
fn tmux_environment_filters_session_identity_and_tmux_client_state() {
  let source = vec![
    (OsString::from("PATH"), OsString::from("/usr/bin")),
    (OsString::from("TMUX"), OsString::from("stale-tmux")),
    (OsString::from("CHARLIEBOT_SESSION_ID"), OsString::from("stale-session")),
    (OsString::from("TERM"), OsString::from("screen-256color")),
  ];
  let command_env = pty::filtered_env(source.clone(), true, false);
  assert!(command_env.iter().all(|(key, _)| key != OsStr::new("TMUX")));
  assert!(command_env.iter().all(|(key, _)| key != OsStr::new("CHARLIEBOT_SESSION_ID")));

  let attach_env = pty::filtered_env(source, false, true);
  assert!(attach_env.iter().all(|(key, _)| key != OsStr::new("CHARLIEBOT_SESSION_ID")));
  assert!(attach_env.iter().any(|(key, value)| key == OsStr::new("TMUX") && value == "stale-tmux"));
  assert!(attach_env.iter().any(|(key, value)| key == OsStr::new("TERM") && value == "xterm-256color"));
}

#[tokio::test]
async fn failed_tmux_session_reports_stderr_from_the_temporary_file() {
  let root = tempfile::tempdir().unwrap();
  let fake_bin = root.path().join("fake-bin");
  std::fs::create_dir(&fake_bin).unwrap();
  let fake_tmux = fake_bin.join("tmux");
  charliebot_shell_harness::write_executable(
    &fake_tmux,
    "#!/bin/sh
echo 'synthetic tmux stderr line' >&2
exit 1
",
  );

  let _env = ScopedEnv::new(&["PATH"]);
  _env.set("PATH", Some(fake_bin.as_os_str()));
  let error = pty::start_tmux_session("synthetic-session", root.path(), &[], &["/bin/true"]).await.unwrap_err();

  assert_eq!(error, "tmux new-session failed for synthetic-session: synthetic tmux stderr line");
}

#[tokio::test]
async fn tmux_session_and_pty_attachment_match_terminal_behavior() {
  let root = tempfile::tempdir().unwrap();
  let home_dir = root.path().join("operator-home");
  let profile = root.path().join("profile");
  let socket_dir = root.path().join("tmux-sockets");
  std::fs::create_dir(&home_dir).unwrap();
  std::fs::create_dir(&profile).unwrap();
  std::fs::create_dir(&socket_dir).unwrap();
  let _env = ScopedEnv::new(&["HOME", home::CHARLIEBOT_HOME_ENV, "TMUX_TMPDIR"]);
  _env.set("HOME", Some(home_dir.as_os_str()));
  _env.set(home::CHARLIEBOT_HOME_ENV, Some(profile.as_os_str()));
  _env.set("TMUX_TMPDIR", Some(socket_dir.as_os_str()));

  let binary = pty::tmux_binary().expect("tmux is installed for the integration test");
  let mut cleanup = TmuxCleanup::new(binary, socket_dir.clone());
  let session_id = terminal::terminal_session_id().unwrap();
  let session_name = terminal::terminal_tmux_name(&session_id);
  terminal::ensure_terminal_session().await.unwrap();
  terminal::ensure_terminal_session().await.unwrap();

  let (code, sessions) =
    pty::run_tmux(&["list-sessions".into(), "-F".into(), "#{session_name}".into()], true).await.unwrap();
  assert_eq!(code, 0);
  assert_eq!(sessions.lines().collect::<Vec<_>>(), [session_name.as_str()]);
  let (code, history) =
    pty::run_tmux(&["show-option".into(), "-t".into(), session_name.clone(), "history-limit".into()], true)
      .await
      .unwrap();
  assert_eq!(code, 0);
  assert!(history.contains("50000"));
  let (code, pane_env) =
    pty::run_tmux(&["show-environment".into(), "-t".into(), session_name.clone(), "CHARLIEBOT_HOME".into()], true)
      .await
      .unwrap();
  assert_eq!(code, 0);
  assert!(pane_env.contains(&format!("CHARLIEBOT_HOME={}", profile.display())));
  // The pane's process can change directory after tmux acknowledges new-session.
  let (code, cwd) = tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      let (code, cwd) = pty::run_tmux(
        &["display-message".into(), "-p".into(), "-t".into(), session_name.clone(), "#{pane_current_path}".into()],
        true,
      )
      .await
      .unwrap();
      if code != 0 || cwd.trim() == home_dir.to_string_lossy() {
        break (code, cwd);
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  assert_eq!(code, 0);
  assert_eq!(cwd.trim(), home_dir.to_string_lossy());

  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let app = Router::new().route("/ws", get(test_socket));
  let server = tokio::spawn(async move {
    axum::serve(listener, app).await.unwrap();
  });
  let _server = AbortOnDrop(server);
  let stream = TcpStream::connect(address).await.unwrap();
  let (mut socket, _) = tokio_tungstenite::client_async(format!("ws://{address}/ws"), stream).await.unwrap();
  send_command(&mut socket, "printf 'terminal-echo-%s\\n' \"$((6*7))\"\n").await;
  let output = receive_until(&mut socket, "terminal-echo-42").await;
  assert!(output.contains("terminal-echo-42"));

  send_command(&mut socket, "tmux set-option status off; printf 'terminal-status-off-ready\\n'\n").await;
  let output = receive_until(&mut socket, "terminal-status-off-ready").await;
  assert!(output.contains("terminal-status-off-ready"));

  send_frame(&mut socket, json!({"type": "pty_resize", "cols": 100, "rows": 30})).await;
  tokio::time::sleep(Duration::from_millis(300)).await;
  send_command(&mut socket, "stty size\n").await;
  let output = receive_until(&mut socket, "30 100").await;
  assert!(output.contains("30 100"));

  let (code, _) = pty::run_tmux(&["kill-server".into()], false).await.unwrap();
  assert_eq!(code, 0);
  assert!(receive_frame_until_exit(&mut socket).await);
  send_frame(&mut socket, json!({"type": "pty_input", "data": ""})).await;
  assert!(timeout(Duration::from_millis(100), socket.next()).await.is_err());
  send_frame(&mut socket, json!([])).await;
  let message = timeout(Duration::from_secs(5), socket.next()).await.expect("non-object JSON ends the handler");
  // uvicorn drops the transport without a close frame, so the client sees the reset rather than a Close message.
  assert!(
    matches!(message, Some(Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)))),
    "uvicorn sends no close frame, observed {message:?}"
  );

  cleanup.disarm();
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
  fn drop(&mut self) {
    self.0.abort();
  }
}

async fn test_socket(upgrade: WebSocketUpgrade) -> axum::response::Response {
  upgrade.on_upgrade(serve_attachment)
}

async fn send_frame(socket: &mut WebSocketStream<TcpStream>, value: Value) {
  socket.send(Message::Text(value.to_string().into())).await.unwrap();
}

async fn send_command(socket: &mut WebSocketStream<TcpStream>, command: &str) {
  send_frame(socket, json!({"type": "pty_input", "data": STANDARD.encode(command.as_bytes())})).await;
}

async fn receive_until(socket: &mut WebSocketStream<TcpStream>, needle: &str) -> String {
  let mut output = String::new();
  loop {
    let message = timeout(Duration::from_secs(5), socket.next())
      .await
      .unwrap_or_else(|_| panic!("PTY output arrives within five seconds; observed output: {output:?}"));
    let Some(Ok(Message::Text(text))) = message else {
      panic!("terminal WebSocket closed before output {needle:?}");
    };
    let frame: Value = serde_json::from_str(&text).unwrap();
    if frame.get("type") == Some(&json!("pty_output")) {
      let data = frame["data"].as_str().unwrap();
      output.push_str(&String::from_utf8_lossy(&STANDARD.decode(data).unwrap()));
    }
    if output.contains(needle) {
      return output;
    }
  }
}

async fn receive_frame_until_exit(socket: &mut WebSocketStream<TcpStream>) -> bool {
  loop {
    let message = timeout(Duration::from_secs(5), socket.next()).await.expect("PTY exit arrives within five seconds");
    let Some(Ok(Message::Text(text))) = message else {
      panic!("terminal WebSocket closed before its PTY exit frame");
    };
    let frame: Value = serde_json::from_str(&text).unwrap();
    if frame.get("type") == Some(&json!("pty_exit")) {
      return frame.get("error").is_none();
    }
  }
}
