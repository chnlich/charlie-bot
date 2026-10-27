//! Durable OpenCode connection records and the per-turn event relay.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::home;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::signal::unix::{SignalKind, signal};

use charliebot_agents::transport::identity::{ProcessIdentity, pin_process_identity, read_pid_stat};

pub const RELAY_ARG: &str = "--opencode-relay";
pub const TURN_RECORD_NAME: &str = "opencode.turn.json";
pub const SERVER_DIRECTORY_NAME: &str = "servers";
const END_EVENT_TYPE: &str = "charliebot.opencode_relay.ended";
const PARENT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const INTERRUPT_WAIT: Duration = Duration::from_secs(10);

/// The connection data for one detached OpenCode server process.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerRecord {
  pub pid: u32,
  pub pid_start: String,
  pub base_url: String,
  pub password: String,
  pub proxy_url: Option<String>,
  pub version: String,
  pub started_at: String,
  pub log_path: String,
  #[serde(default)]
  pub exit_reason: Option<String>,
}

/// The session and prompt request assigned to one relay.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnRecord {
  pub server_record: String,
  pub session_id: String,
  pub prompt_request: Value,
  pub model: String,
  #[serde(default)]
  pub model_limit: Option<Map<String, Value>>,
}

/// Invoke the relay subcommand before the server initializes its runtime.
pub fn run_args(args: &[OsString]) -> anyhow::Result<()> {
  let [directory, parent_pid, parent_start] = args else {
    bail!("OpenCode relay needs a run directory, parent pid, and parent pid start");
  };
  let directory = PathBuf::from(directory);
  let parent_pid = parent_pid.to_string_lossy().parse::<u32>().context("parsing OpenCode relay parent pid")?;
  let parent_start = parent_start.to_string_lossy().into_owned();
  let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
  runtime.block_on(run(&directory, parent_pid, &parent_start))
}

/// Install reqwest's rustls provider when this server process has not configured one yet.
pub fn ensure_crypto_provider() -> anyhow::Result<()> {
  if rustls::crypto::CryptoProvider::get_default().is_none() {
    rustls::crypto::ring::default_provider()
      .install_default()
      .map_err(|error| anyhow::anyhow!("installing rustls ring crypto provider: {error:?}"))?;
  }
  Ok(())
}

/// Write a complete connection record with owner-only permissions before publishing its name.
pub fn write_server_record(path: &Path, record: &ServerRecord) -> anyhow::Result<()> {
  write_atomic_json(path, record, 0o600)
}

/// Persist why the OpenCode process or its event stream ended while this server was running.
pub fn write_server_exit_reason(path: &Path, reason: String) -> anyhow::Result<()> {
  let mut record: ServerRecord = serde_json::from_slice(&fs::read(path)?)?;
  record.exit_reason = Some(reason);
  write_atomic_json(path, &record, 0o600)
}

/// Write the per-turn connection and prompt request atomically.
pub fn write_turn_record(path: &Path, record: &TurnRecord) -> anyhow::Result<()> {
  write_atomic_json(path, record, 0o600)
}

fn write_atomic_json(path: &Path, value: &impl Serialize, mode: u32) -> anyhow::Result<()> {
  let parent = path.parent().context("OpenCode record has no parent directory")?;
  fs::create_dir_all(parent)?;
  let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
  let mut file = OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary)?;
  serde_json::to_writer(&mut file, value)?;
  file.sync_all()?;
  fs::rename(&temporary, path)?;
  fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
  fs::File::open(parent)?.sync_all()?;
  Ok(())
}

/// Read a turn record when the run belongs to OpenCode.
pub fn read_turn_record(directory: &Path) -> anyhow::Result<Option<TurnRecord>> {
  let path = directory.join(TURN_RECORD_NAME);
  match fs::read(&path) {
    Ok(bytes) => Ok(Some(
      serde_json::from_slice(&bytes).with_context(|| format!("decoding OpenCode turn record {}", path.display()))?,
    )),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error.into()),
  }
}

/// The OpenCode progress deadline when following a recovered turn: the timeout and the message a silent follow fails
/// with. `None` when the directory holds no turn record.
pub fn recovery_silence(directory: &Path) -> anyhow::Result<Option<(Duration, String)>> {
  let Some(turn) = read_turn_record(directory)? else { return Ok(None) };
  let timeout = super::opencode_limits::OPENCODE_SSE_PROGRESS_TIMEOUT;
  let message = format!("OpenCode session {} produced no event for {} s", turn.session_id, timeout.as_secs_f64());
  Ok(Some((timeout, message)))
}

/// The connection record directory under the CharlieBot profile home.
pub fn server_directory() -> anyhow::Result<PathBuf> {
  if let Some(directory) = std::env::var_os("CHARLIEBOT_OPENCODE_SERVER_DIRECTORY") {
    return Ok(PathBuf::from(directory));
  }
  Ok(home::charliebot_home_dir()?.join(home::OPENCODE_DIR_NAME).join(SERVER_DIRECTORY_NAME))
}

/// Pin a process identity for the relay's parent-death check.
pub fn parent_identity() -> anyhow::Result<ProcessIdentity> {
  let identity = pin_process_identity(std::process::id());
  if identity.pid_start.is_none() {
    bail!("reading the OpenCode relay parent process start time failed");
  }
  Ok(identity)
}

/// Return whether a pid still names the recorded process instance.
pub fn same_process(pid: u32, pid_start: &str) -> bool {
  read_pid_stat(pid).is_some_and(|stat| stat.pid_start == pid_start && stat.state != "Z")
}

/// Return the name stored in each turn record for the server's connection record.
pub fn server_record_name(path: &Path) -> anyhow::Result<String> {
  path
    .file_name()
    .and_then(|name| name.to_str())
    .context("OpenCode server record path has no UTF-8 file name")
    .map(str::to_string)
}

/// Check the per-turn server record reference before joining it to a directory.
pub fn validate_server_record_name(name: &str) -> anyhow::Result<()> {
  let file_name = Path::new(name).file_name().and_then(|name| name.to_str());
  anyhow::ensure!(file_name == Some(name), "OpenCode turn server_record must be a file name");
  Ok(())
}

async fn run(directory: &Path, parent_pid: u32, parent_start: &str) -> anyhow::Result<()> {
  let raw_path = directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  match run_inner(directory, parent_pid, parent_start, &raw_path).await {
    Ok(true) => terminate_by_signal(),
    Ok(false) => Ok(()),
    Err(error) => {
      append_end_record(&raw_path, &error.to_string())?;
      Err(error)
    }
  }
}

async fn run_inner(directory: &Path, parent_pid: u32, parent_start: &str, raw_path: &Path) -> anyhow::Result<bool> {
  ensure_crypto_provider()?;
  let mut terminate = signal(SignalKind::terminate())?;
  let turn = loop {
    tokio::select! {
      _ = terminate.recv() => {
        let reason = if same_process(parent_pid, parent_start) {
          "OpenCode relay stopped before the turn started"
        } else {
          "server restarted before the turn started"
        };
        append_end_record(raw_path, reason)?;
        return Ok(true);
      }
      _ = tokio::time::sleep(PARENT_POLL_INTERVAL) => {
        if let Some(turn) = read_turn_record(directory)? {
          break turn;
        }
        if !same_process(parent_pid, parent_start) {
          append_end_record(raw_path, "server restarted before the turn started")?;
          return Ok(false);
        }
      }
    }
  };
  validate_server_record_name(&turn.server_record)?;
  let record_path = server_directory()?.join(&turn.server_record);
  let record: ServerRecord = serde_json::from_slice(
    &fs::read(&record_path).with_context(|| format!("reading OpenCode server record {}", record_path.display()))?,
  )?;
  let client = Client::builder().no_proxy().build()?;
  let event_url = format!("{}/api/event", record.base_url);
  let response = tokio::select! {
    response = client.get(event_url).basic_auth("opencode", Some(&record.password)).send() => {
      response.and_then(reqwest::Response::error_for_status).context("opening OpenCode event stream")?
    }
    _ = terminate.recv() => {
      interrupt_before_streaming(&client, &record, &turn, raw_path).await?;
      return Ok(true);
    }
  };
  let mut stream = response.bytes_stream();
  let mut pending = Vec::new();
  let mut search_from = 0;
  loop {
    let chunk = tokio::select! {
      chunk = stream.next() => chunk.context("OpenCode event stream ended before server.connected")??,
      _ = terminate.recv() => {
        interrupt_before_streaming(&client, &record, &turn, raw_path).await?;
        return Ok(true);
      }
    };
    pending.extend_from_slice(&chunk);
    if visit_complete_lines(&mut pending, &mut search_from, is_connected_event)? {
      break;
    }
  }
  let prompt_url = format!("{}/api/session/{}/prompt", record.base_url, turn.session_id);
  let prompt_response = tokio::select! {
    response = client.post(prompt_url).basic_auth("opencode", Some(&record.password)).json(&turn.prompt_request).send() => {
      response.context("sending OpenCode prompt")?
    }
    _ = terminate.recv() => {
      interrupt_before_streaming(&client, &record, &turn, raw_path).await?;
      return Ok(true);
    }
  };
  if prompt_response.status() != StatusCode::OK {
    let status = prompt_response.status();
    let text = prompt_response.text().await?;
    bail!("OpenCode prompt returned HTTP {status}: {text}");
  }

  let mut interrupt_deadline = None;
  loop {
    if visit_complete_lines(&mut pending, &mut search_from, |line| record_turn_event(line, &turn, raw_path))? {
      return Ok(interrupt_deadline.is_some());
    }
    tokio::select! {
      _ = terminate.recv(), if interrupt_deadline.is_none() => {
        let interrupt_url = format!("{}/api/session/{}/interrupt", record.base_url, turn.session_id);
        client
          .post(interrupt_url)
          .basic_auth("opencode", Some(&record.password))
          .send()
          .await
          .and_then(reqwest::Response::error_for_status)
          .context("sending OpenCode interrupt for stopped relay")?;
        interrupt_deadline = Some(tokio::time::Instant::now() + INTERRUPT_WAIT);
      }
      _ = tokio::time::sleep_until(
        interrupt_deadline.unwrap_or_else(|| tokio::time::Instant::now() + SERVER_POLL_INTERVAL)
      ) => {
        if interrupt_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
          bail!("OpenCode interrupt did not produce a terminal event");
        }
        if !same_process(record.pid, &record.pid_start) {
          bail!("opencode serve exited");
        }
      }
      chunk = stream.next() => {
        let Some(chunk) = chunk else {
          let reason = if same_process(record.pid, &record.pid_start) {
            "event stream disconnected".to_string()
          } else {
            wait_for_server_exit_reason(&record_path)
              .await?
              .unwrap_or_else(|| "opencode serve exited".to_string())
          };
          bail!("{reason}");
        };
        let chunk = chunk?;
        pending.extend_from_slice(&chunk);
        if visit_complete_lines(&mut pending, &mut search_from, |line| record_turn_event(line, &turn, raw_path))? {
          return Ok(interrupt_deadline.is_some());
        }
      }
    }
  }
}

async fn interrupt_before_streaming(
  client: &Client,
  record: &ServerRecord,
  turn: &TurnRecord,
  raw_path: &Path,
) -> anyhow::Result<()> {
  let url = format!("{}/api/session/{}/interrupt", record.base_url, turn.session_id);
  client.post(url).basic_auth("opencode", Some(&record.password)).send().await?.error_for_status()?;
  append_end_record(raw_path, "OpenCode relay stopped before streaming events")
}

fn is_connected_event(line: &[u8]) -> anyhow::Result<bool> {
  let line = line.strip_suffix(b"\r").unwrap_or(line);
  let Some(data) = line.strip_prefix(b"data:") else { return Ok(false) };
  let event: Value = serde_json::from_slice(data)?;
  Ok(event.get("type").and_then(Value::as_str) == Some("server.connected"))
}

fn record_turn_event(line: &[u8], turn: &TurnRecord, raw_path: &Path) -> anyhow::Result<bool> {
  let line = line.strip_suffix(b"\r").unwrap_or(line);
  let Some(data) = line.strip_prefix(b"data:") else { return Ok(false) };
  let event: Value = serde_json::from_slice(data)?;
  if event.pointer("/data/sessionID").and_then(Value::as_str) != Some(turn.session_id.as_str()) {
    return Ok(false);
  }
  append_raw_event(raw_path, data)?;
  Ok(matches!(
    event.get("type").and_then(Value::as_str),
    Some("session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted")
  ))
}

async fn wait_for_server_exit_reason(path: &Path) -> anyhow::Result<Option<String>> {
  for _ in 0..20 {
    let bytes = match fs::read(path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(error.into()),
    };
    let record: ServerRecord = serde_json::from_slice(&bytes)?;
    if record.exit_reason.is_some() {
      return Ok(record.exit_reason);
    }
    tokio::time::sleep(PARENT_POLL_INTERVAL).await;
  }
  Ok(None)
}

fn terminate_by_signal() -> anyhow::Result<()> {
  let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
  action.sa_sigaction = libc::SIG_DFL;
  unsafe {
    libc::sigemptyset(&mut action.sa_mask);
    if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) == -1 {
      return Err(std::io::Error::last_os_error().into());
    }
    if libc::kill(libc::getpid(), libc::SIGTERM) == -1 {
      return Err(std::io::Error::last_os_error().into());
    }
  }
  Ok(())
}

fn visit_complete_lines(
  pending: &mut Vec<u8>,
  search_from: &mut usize,
  mut visit: impl FnMut(&[u8]) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
  let mut line_start = 0;
  while let Some(relative) = memchr::memchr(b'\n', &pending[*search_from..]) {
    let index = *search_from + relative;
    if visit(&pending[line_start..index])? {
      pending.drain(..index + 1);
      *search_from = 0;
      return Ok(true);
    }
    line_start = index + 1;
    *search_from = line_start;
  }
  if line_start > 0 {
    pending.drain(..line_start);
  }
  *search_from = pending.len();
  Ok(false)
}

fn append_raw_event(path: &Path, raw: &[u8]) -> anyhow::Result<()> {
  let mut file = OpenOptions::new().create(true).append(true).mode(0o644).open(path)?;
  file.write_all(raw)?;
  file.write_all(b"\n")?;
  file.sync_data()?;
  Ok(())
}

fn append_end_record(path: &Path, reason: &str) -> anyhow::Result<()> {
  let event = json!({"type": END_EVENT_TYPE, "reason": reason});
  append_raw_event(path, serde_json::to_string(&event)?.as_bytes())
}
