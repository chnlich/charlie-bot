use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use charliebot_core::pyfmt::python_truthy;
use charliebot_core::run_token::SESSION_ID_ENV;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::{ForkptyResult, Winsize, forkpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::waitpid;
use nix::unistd::{Pid, close};
use serde_json::{Value, json};
use tokio::io::unix::AsyncFd;
use tokio::process::Command;
use tokio::sync::mpsc::{self, UnboundedSender};

nix::ioctl_write_ptr_bad!(pty_set_winsize, libc::TIOCSWINSZ, Winsize);

pub(super) const PTY_EXIT: &str = "pty_exit";

const PTY_INPUT: &str = "pty_input";
const PTY_OUTPUT: &str = "pty_output";
const PTY_RESIZE: &str = "pty_resize";
const TMUX_SOCKET: &str = "charliebot";
const INITIAL_COLS: i128 = 80;
const INITIAL_ROWS: i128 = 24;
const HISTORY_LIMIT: &str = "50000";
const PTY_READ_CHUNK: usize = 4096;
const PTY_CLIENT_TERM: &str = "xterm-256color";
const PTY_WS_RECV_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn tmux_session_name(session_id: &str) -> String {
  format!("charliebot-{session_id}")
}

pub(super) fn filtered_env(
  source: impl IntoIterator<Item = (OsString, OsString)>,
  remove_tmux: bool,
  replace_term: bool,
) -> Vec<(OsString, OsString)> {
  let mut env = source
    .into_iter()
    .filter(|(key, _)| {
      key != OsStr::new(SESSION_ID_ENV)
        && (!remove_tmux || key != OsStr::new("TMUX"))
        && (!replace_term || key != OsStr::new("TERM"))
    })
    .collect::<Vec<_>>();
  if replace_term {
    env.push((OsString::from("TERM"), OsString::from(PTY_CLIENT_TERM)));
  }
  env
}

fn tmux_command_env() -> Vec<(OsString, OsString)> {
  filtered_env(std::env::vars_os(), true, false)
}

fn tmux_pty_env() -> Vec<(OsString, OsString)> {
  filtered_env(std::env::vars_os(), false, true)
}

pub(super) fn tmux_binary() -> Result<PathBuf, String> {
  let Some(path) = std::env::var_os("PATH") else {
    return Err("tmux binary not found on PATH — install tmux".to_string());
  };
  for directory in std::env::split_paths(&path) {
    let candidate = directory.join("tmux");
    if let Ok(metadata) = std::fs::metadata(&candidate)
      && metadata.is_file()
      && metadata.permissions().mode() & 0o111 != 0
    {
      return Ok(candidate);
    }
  }
  Err("tmux binary not found on PATH — install tmux".to_string())
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
  use std::os::unix::process::ExitStatusExt;

  status.code().or_else(|| status.signal().map(|signal| -signal)).unwrap_or_default()
}

fn python_os_error(error: &io::Error, path: Option<&Path>) -> String {
  let Some(errno) = error.raw_os_error() else {
    return error.to_string();
  };
  let message = error.to_string();
  let suffix = format!(" (os error {errno})");
  let description = message.strip_suffix(&suffix).unwrap_or(&message);
  match path {
    Some(path) => format!("[Errno {errno}] {description}: '{}'", path.display()),
    None => format!("[Errno {errno}] {description}"),
  }
}

pub(super) async fn run_tmux(args: &[String], capture: bool) -> Result<(i32, String), String> {
  let binary = tmux_binary()?;
  let mut stderr_file = tempfile::tempfile().map_err(|error| python_os_error(&error, None))?;
  let child_stderr = stderr_file.try_clone().map_err(|error| python_os_error(&error, None))?;
  let mut command = Command::new(&binary);
  command
    .arg("-L")
    .arg(TMUX_SOCKET)
    .args(args)
    .env_clear()
    .envs(tmux_command_env())
    .stdin(Stdio::null())
    .stdout(if capture { Stdio::piped() } else { Stdio::null() })
    .stderr(Stdio::from(child_stderr));
  let child = command.spawn().map_err(|error| python_os_error(&error, Some(&binary)))?;
  let output = child.wait_with_output().await.map_err(|error| python_os_error(&error, Some(&binary)))?;
  stderr_file.seek(SeekFrom::Start(0)).map_err(|error| python_os_error(&error, None))?;
  let mut stderr_bytes = Vec::new();
  stderr_file.read_to_end(&mut stderr_bytes).map_err(|error| python_os_error(&error, None))?;

  let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let code = exit_code(output.status);
  let text = if capture && code == 0 {
    stdout
  } else if !stderr.is_empty() {
    stderr
  } else {
    stdout
  };
  Ok((code, text))
}

pub(super) async fn tmux_session_exists(session_id: &str) -> Result<bool, String> {
  let (code, _) = run_tmux(&["has-session".into(), "-t".into(), tmux_session_name(session_id)], false).await?;
  Ok(code == 0)
}

pub(super) async fn start_tmux_session(
  name: &str,
  cwd: &Path,
  env_args: &[String],
  command_argv: &[&str],
) -> Result<(), String> {
  let mut args = vec![
    "new-session".to_string(),
    "-d".to_string(),
    "-s".to_string(),
    name.to_string(),
    "-x".to_string(),
    INITIAL_COLS.to_string(),
    "-y".to_string(),
    INITIAL_ROWS.to_string(),
    "-c".to_string(),
    cwd.display().to_string(),
  ];
  args.extend(env_args.iter().cloned());
  args.extend(command_argv.iter().map(|arg| (*arg).to_string()));
  let (code, stderr) = run_tmux(&args, false).await?;
  if code != 0 {
    return Err(format!("tmux new-session failed for {name}: {}", stderr.trim()));
  }
  let _ = run_tmux(
    &["set-option".into(), "-t".into(), name.to_string(), "history-limit".into(), HISTORY_LIMIT.into()],
    false,
  )
  .await?;
  Ok(())
}

#[derive(Debug)]
enum ResizeFailure {
  Io(io::Error),
  PackedValue,
}

pub(super) struct PtyAttachment {
  session_id: String,
  pid: Option<Pid>,
  fd: RawFd,
  closed: bool,
}

impl PtyAttachment {
  pub(super) fn spawn(session_id: &str) -> Result<PtyAttachment, String> {
    let binary = tmux_binary()?;
    let name = tmux_session_name(session_id);
    let path = CString::new(binary.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    let argv = [
      binary.as_os_str().as_bytes(),
      b"-L".as_slice(),
      TMUX_SOCKET.as_bytes(),
      b"attach".as_slice(),
      b"-t".as_slice(),
      name.as_bytes(),
    ]
    .into_iter()
    .map(CString::new)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| error.to_string())?;
    let env = tmux_pty_env()
      .into_iter()
      .map(|(key, value)| CString::new([key.as_os_str().as_bytes(), b"=", value.as_os_str().as_bytes()].concat()))
      .collect::<Result<Vec<_>, _>>()
      .map_err(|error| error.to_string())?;
    let argv_ptrs = argv.iter().map(|arg| arg.as_ptr()).chain(std::iter::once(std::ptr::null())).collect::<Vec<_>>();
    let env_ptrs = env.iter().map(|item| item.as_ptr()).chain(std::iter::once(std::ptr::null())).collect::<Vec<_>>();

    let mut attachment = PtyAttachment { session_id: session_id.to_string(), pid: None, fd: -1, closed: false };
    match unsafe { forkpty(None, None) }.map_err(|error| python_os_error(&io::Error::from(error), None))? {
      ForkptyResult::Child => unsafe {
        libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        let error_number = *libc::__errno_location();
        let description = std::ffi::CStr::from_ptr(libc::strerror(error_number)).to_string_lossy();
        let error = format!("tmux exec failed: [Errno {error_number}] {description}: '{}'\n", binary.display());
        libc::write(libc::STDERR_FILENO, error.as_ptr().cast(), error.len());
        libc::_exit(1);
      },
      ForkptyResult::Parent { child, master } => {
        attachment.pid = Some(child);
        attachment.fd = master.into_raw_fd();
      }
    }
    attachment.set_winsize(INITIAL_ROWS, INITIAL_COLS).map_err(resize_failure_text)?;
    let fd = unsafe { BorrowedFd::borrow_raw(attachment.fd) };
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(|error| python_os_error(&io::Error::from(error), None))?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(|error| python_os_error(&io::Error::from(error), None))?;
    let pid = attachment.pid.expect("the parent PTY holds its child pid");
    tracing::info!(session_id = %attachment.session_id, pid = pid.as_raw(), fd = attachment.fd, "tui_pty_spawned");
    Ok(attachment)
  }

  fn write(&self, data: &[u8]) {
    if self.closed || self.fd < 0 || data.is_empty() {
      return;
    }
    if unsafe { libc::write(self.fd, data.as_ptr().cast(), data.len()) } < 0 {
      tracing::warn!(
        session_id = %self.session_id,
        error = %io::Error::last_os_error(),
        "tui_pty_write_failed"
      );
    }
  }

  fn resize(&self, cols: i128, rows: i128) -> Result<(), ResizeFailure> {
    if self.closed || self.fd < 0 {
      return Ok(());
    }
    self.set_winsize(rows.max(1), cols.max(1))
  }

  fn set_winsize(&self, rows: i128, cols: i128) -> Result<(), ResizeFailure> {
    let rows = u16::try_from(rows).map_err(|_| ResizeFailure::PackedValue)?;
    let cols = u16::try_from(cols).map_err(|_| ResizeFailure::PackedValue)?;
    let size = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { pty_set_winsize(self.fd, &size) }.map_err(|error| ResizeFailure::Io(io::Error::from(error)))?;
    Ok(())
  }

  fn close(&mut self) -> io::Result<()> {
    if self.closed {
      return Ok(());
    }
    self.closed = true;
    let mut first_error = None;
    if self.fd >= 0 {
      if let Err(error) = close(self.fd) {
        first_error = Some(io::Error::from(error));
      }
      self.fd = -1;
    }
    if let Some(pid) = self.pid.take() {
      if let Err(error) = kill(pid, Signal::SIGTERM)
        && error != Errno::ESRCH
      {
        first_error.get_or_insert_with(|| io::Error::from(error));
      }
      loop {
        match waitpid(pid, None) {
          Ok(_) | Err(Errno::ECHILD) => break,
          Err(Errno::EINTR) => continue,
          Err(error) => {
            first_error.get_or_insert_with(|| io::Error::from(error));
            break;
          }
        }
      }
    }
    match first_error {
      Some(error) => Err(error),
      None => Ok(()),
    }
  }
}

impl Drop for PtyAttachment {
  fn drop(&mut self) {
    if let Err(error) = self.close() {
      tracing::error!(session_id = %self.session_id, error = %error, "tui_pty_close_failed");
    }
  }
}

enum PumpMessage {
  Data(Vec<u8>),
  End,
}

struct BorrowedRawFd(RawFd);

impl AsRawFd for BorrowedRawFd {
  fn as_raw_fd(&self) -> RawFd {
    self.0
  }
}

async fn pump_pty_to_channel(fd: RawFd, session_id: String, sender: UnboundedSender<PumpMessage>) -> io::Result<()> {
  // SAFETY: the relay cancels and joins this pump before it closes the attachment's descriptor.
  let pty = unsafe { AsyncFd::register(BorrowedRawFd(fd)) }.map_err(|error| io::Error::other(error.to_string()))?;
  loop {
    let mut readable = pty.readable().await?;
    let result = readable.try_io(|registered| {
      let mut chunk = vec![0; PTY_READ_CHUNK];
      let count = unsafe { libc::read(registered.get_ref().as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
      if count < 0 {
        return Err(io::Error::last_os_error());
      }
      chunk.truncate(count as usize);
      Ok(chunk)
    });
    match result {
      Ok(Ok(chunk)) if chunk.is_empty() => {
        let _ = sender.send(PumpMessage::End);
        return Ok(());
      }
      Ok(Ok(chunk)) => {
        if sender.send(PumpMessage::Data(chunk)).is_err() {
          return Ok(());
        }
      }
      Ok(Err(error)) => {
        tracing::debug!(session_id = %session_id, error = %error, "tui_pty_read_oserror");
        let _ = sender.send(PumpMessage::End);
        return Ok(());
      }
      Err(_) => continue,
    }
  }
}

async fn send_exit_frame(socket: &mut WebSocket, session_id: &str) {
  if let Err(error) = send_json(socket, &json!({"type": PTY_EXIT})).await {
    tracing::debug!(session_id = %session_id, error = %error, "tui_pty_exit_send_failed");
  }
}

pub(super) async fn send_json(socket: &mut WebSocket, value: &Value) -> Result<(), axum::Error> {
  let text =
    String::from_utf8(charliebot_compat::json::starlette_bytes(value)).expect("Starlette JSON bytes are valid UTF-8");
  socket.send(Message::Text(text.into())).await
}

/// Relays the PTY and the socket until either one ends.
///
/// An `Err` returns without a close frame. uvicorn 0.42.0's websockets implementation
/// (`WebSocketProtocol.run_asgi` in `uvicorn/protocols/websockets/websockets_impl.py`) closes the transport
/// after an exception raised past `accept`, and sends no close frame, so the client sees the connection end.
pub(super) async fn run_pty_relay(mut socket: WebSocket, mut attachment: PtyAttachment) -> Result<(), String> {
  let (sender, mut receiver) = mpsc::unbounded_channel();
  let pump_fd = attachment.fd;
  let pump_session_id = attachment.session_id.clone();
  let pump = tokio::spawn(pump_pty_to_channel(pump_fd, pump_session_id.clone(), sender));
  let mut pump_active = true;
  let mut timeout = Box::pin(tokio::time::sleep(PTY_WS_RECV_TIMEOUT));
  let mut relay_error = None;

  loop {
    tokio::select! {
      _ = &mut timeout => {
        if let Err(error) = send_json(&mut socket, &json!({"type": "ping"})).await {
          tracing::debug!(session_id = %pump_session_id, error = %error, "tui_pty_ping_send_failed");
          break;
        }
        timeout.as_mut().reset(tokio::time::Instant::now() + PTY_WS_RECV_TIMEOUT);
      }
      output = receiver.recv(), if pump_active => {
        match output {
          Some(PumpMessage::Data(bytes)) => {
            let encoded = STANDARD.encode(bytes);
            if let Err(error) = send_json(&mut socket, &json!({"type": PTY_OUTPUT, "data": encoded})).await {
              tracing::debug!(session_id = %pump_session_id, error = %error, "tui_pty_ws_send_failed");
              pump.abort();
              pump_active = false;
              send_exit_frame(&mut socket, &pump_session_id).await;
            }
          }
          Some(PumpMessage::End) => {
            pump_active = false;
            send_exit_frame(&mut socket, &pump_session_id).await;
          }
          None => pump_active = false,
        }
      }
      incoming = socket.recv() => {
        match incoming {
          Some(Ok(Message::Text(raw))) => {
            process_text_frame(&raw, &attachment, &mut relay_error);
            timeout.as_mut().reset(tokio::time::Instant::now() + PTY_WS_RECV_TIMEOUT);
            if relay_error.is_some() {
              break;
            }
          }
          Some(Ok(Message::Close(_))) | None => break,
          Some(Ok(Message::Binary(_))) => {
            relay_error = Some("Expected a text WebSocket message".to_string());
            break;
          }
          Some(Err(error)) => {
            relay_error = Some(error.to_string());
            break;
          }
          Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
        }
      }
    }
  }

  pump.abort();
  match pump.await {
    Ok(Ok(())) => {}
    Ok(Err(error)) => tracing::debug!(session_id = %pump_session_id, error = %error, "tui_pump_task_exit"),
    Err(error) if error.is_cancelled() => {}
    Err(error) => tracing::debug!(session_id = %pump_session_id, error = %error, "tui_pump_task_exit"),
  }
  attachment.close().map_err(|error| error.to_string())?;
  match relay_error {
    Some(error) => Err(error),
    None => Ok(()),
  }
}

fn process_text_frame(raw: &str, attachment: &PtyAttachment, relay_error: &mut Option<String>) {
  let message = match serde_json::from_str::<Value>(raw) {
    Ok(message) => message,
    Err(error) => {
      tracing::debug!(session_id = %attachment.session_id, error = %error, "tui_pty_ws_json_decode_failed");
      return;
    }
  };
  let Some(message) = message.as_object() else {
    *relay_error = Some("JSON WebSocket frame is not an object".to_string());
    return;
  };
  match message.get("type").and_then(Value::as_str) {
    Some(PTY_INPUT) => match decode_input(message.get("data")) {
      Ok(chunk) => attachment.write(&chunk),
      Err(error) => tracing::debug!(session_id = %attachment.session_id, error = %error, "tui_pty_input_decode_failed"),
    },
    Some(PTY_RESIZE) => {
      let cols = match parse_resize(message.get("cols"), INITIAL_COLS) {
        Ok(cols) => cols,
        Err(error) => {
          tracing::debug!(session_id = %attachment.session_id, error = %error, "tui_pty_resize_parse_failed");
          return;
        }
      };
      let rows = match parse_resize(message.get("rows"), INITIAL_ROWS) {
        Ok(rows) => rows,
        Err(error) => {
          tracing::debug!(session_id = %attachment.session_id, error = %error, "tui_pty_resize_parse_failed");
          return;
        }
      };
      match attachment.resize(cols, rows) {
        Ok(()) => {}
        Err(ResizeFailure::Io(error)) => {
          tracing::warn!(session_id = %attachment.session_id, error = %error, "tui_pty_resize_failed");
        }
        Err(ResizeFailure::PackedValue) => {
          *relay_error = Some("ushort format requires 0 <= number <= 65535".to_string());
        }
      }
    }
    _ => {}
  }
}

fn decode_input(value: Option<&Value>) -> Result<Vec<u8>, String> {
  let Some(value) = value.filter(|value| python_truthy(value)) else {
    return Ok(Vec::new());
  };
  let Some(encoded) = value.as_str() else {
    return Err("base64 input must be a string".to_string());
  };
  if !encoded.is_ascii() {
    return Err("base64 input must contain only ASCII characters".to_string());
  }
  let filtered = encoded
    .bytes()
    .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    .collect::<Vec<_>>();
  STANDARD.decode(filtered).map_err(|error| error.to_string())
}

fn parse_resize(value: Option<&Value>, default: i128) -> Result<i128, String> {
  let Some(value) = value.filter(|value| python_truthy(value)) else {
    return Ok(default);
  };
  match value {
    Value::Bool(value) => Ok(i128::from(*value)),
    Value::Number(value) => value
      .as_i64()
      .map(i128::from)
      .or_else(|| value.as_u64().map(i128::from))
      .or_else(|| value.as_f64().map(|number| number.trunc() as i128))
      .ok_or_else(|| "value cannot be converted to an integer".to_string()),
    Value::String(value) => parse_python_int(value),
    _ => Err("value cannot be converted to an integer".to_string()),
  }
}

fn parse_python_int(value: &str) -> Result<i128, String> {
  let trimmed = value.trim();
  let bytes = trimmed.as_bytes();
  let mut normalized = String::with_capacity(trimmed.len());
  for (index, byte) in bytes.iter().copied().enumerate() {
    if byte == b'_' {
      let digit_before = index > 0 && bytes[index - 1].is_ascii_digit();
      let digit_after = bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
      if !digit_before || !digit_after {
        return Err("value cannot be converted to an integer".to_string());
      }
    } else {
      normalized.push(byte as char);
    }
  }
  normalized.parse::<i128>().map_err(|_| "value cannot be converted to an integer".to_string())
}

fn resize_failure_text(error: ResizeFailure) -> String {
  match error {
    ResizeFailure::Io(error) => python_os_error(&error, None),
    ResizeFailure::PackedValue => "ushort format requires 0 <= number <= 65535".to_string(),
  }
}
