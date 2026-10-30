use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use charliebot_core::text_repr;
use charliebot_core::str_methods;
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde_json::{Map, Value, json};
use tempfile::{Builder, TempDir};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client};

const HEADLESS_RENDER_TIMEOUT: Duration = Duration::from_secs(60);
const HEADLESS_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const HEADLESS_TEARDOWN_WAIT: Duration = Duration::from_secs(5);
/// The user-data directory prefix of every renderer browser, here and in the startup orphan sweep.
const RENDERER_TEMP_PREFIX: &str = "headless-render-";

static RENDERER: OnceLock<Mutex<Option<WarmRenderer>>> = OnceLock::new();

/// The page heights that tests fake, keyed by the Chrome path that measures them. Verbs run the assertions on the
/// blocking pool, so a fake set on the test thread would not reach them; a path is unique to one test's fixture.
#[cfg(test)]
static TEST_HEIGHTS: Mutex<Vec<(PathBuf, i32)>> = Mutex::new(Vec::new());

struct WarmRenderer {
  chrome_bin: PathBuf,
  process: Option<Child>,
  socket: Option<WebSocket<TcpStream>>,
  user_data_dir: Option<TempDir>,
  stderr_path: Option<PathBuf>,
  session: Option<String>,
  message_id: u64,
  pending: Vec<Value>,
  deadline: Option<Instant>,
}

enum RenderFailure {
  Launch(String),
  Value(String),
  Timeout,
  Other(String),
}

impl WarmRenderer {
  fn new(chrome_bin: PathBuf) -> Self {
    Self {
      chrome_bin,
      process: None,
      socket: None,
      user_data_dir: None,
      stderr_path: None,
      session: None,
      message_id: 0,
      pending: Vec::new(),
      deadline: None,
    }
  }

  fn render_height(&mut self, probe_uri: &str) -> Result<i32, String> {
    let needs_launch =
      self.process.is_none() || self.process_exited().map_err(|error| error.to_string())? || self.socket.is_none();
    if needs_launch {
      self.close().map_err(|error| error.to_string())?;
      if let Err(failure) = self.launch() {
        return Err(self.failure_message(failure)?);
      }
    }
    self.deadline = Some(Instant::now() + HEADLESS_RENDER_TIMEOUT);
    match self.render_once(probe_uri) {
      Ok(height) => Ok(height),
      Err(failure) => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| error.to_string())?;
        let message = match failure {
          RenderFailure::Value(message) => message,
          RenderFailure::Timeout => format!(
            "headless renderer timed out after {}s while measuring the plan page height{tail}",
            HEADLESS_RENDER_TIMEOUT.as_secs()
          ),
          RenderFailure::Other(message) => {
            format!("headless renderer failed while measuring the plan page height: {message}{tail}")
          }
          RenderFailure::Launch(message) => message,
        };
        Err(message)
      }
    }
  }

  fn render_once(&mut self, probe_uri: &str) -> Result<i32, RenderFailure> {
    self.pending.clear();
    self.command("Page.enable", Map::new())?;
    self.command("Page.navigate", json!({"url": probe_uri}).as_object().cloned().unwrap())?;
    self.wait_load()?;
    let params = json!({
      "expression": "document.getElementById('page-height')?.textContent ?? null",
      "returnByValue": true,
    })
    .as_object()
    .cloned()
    .unwrap();
    loop {
      let reply = self.command("Runtime.evaluate", params.clone())?;
      let value = reply.get("result").and_then(|result| result.get("result")).and_then(|result| result.get("value"));
      if let Some(value) = value.filter(|value| !value.is_null()) {
        return python_int_value(value).map_err(RenderFailure::Value);
      }
      if self.deadline.is_some_and(|deadline| Instant::now() > deadline) {
        return Err(RenderFailure::Value(
          "headless renderer output carried no page-height marker; cannot measure the plan page".to_owned(),
        ));
      }
      thread::sleep(Duration::from_millis(5));
    }
  }

  fn launch(&mut self) -> Result<(), RenderFailure> {
    let user_data_dir =
      Builder::new().prefix(RENDERER_TEMP_PREFIX).tempdir().map_err(|error| RenderFailure::Other(error.to_string()))?;
    let stderr_path = user_data_dir.path().join("stderr.log");
    self.deadline = Some(Instant::now() + HEADLESS_LAUNCH_TIMEOUT);
    let stderr = match OpenOptions::new().write(true).create(true).truncate(true).open(&stderr_path) {
      Ok(stderr) => stderr,
      Err(error) => {
        self.user_data_dir = Some(user_data_dir);
        self.stderr_path = Some(stderr_path);
        let detail = python_os_error(&error, self.stderr_path.as_deref().expect("stderr path was set"));
        return Err(self.launch_failure(detail)?);
      }
    };
    self.user_data_dir = Some(user_data_dir);
    self.stderr_path = Some(stderr_path.clone());
    let user_data_path = self.user_data_dir.as_ref().expect("the renderer profile was stored").path().to_path_buf();
    let child = Command::new(&self.chrome_bin)
      .args([
        "--headless",
        "--disable-gpu",
        "--no-sandbox",
        "--allow-file-access-from-files",
        "--remote-debugging-port=0",
      ])
      .arg(format!("--user-data-dir={}", user_data_path.display()))
      .arg("about:blank")
      .stdout(Stdio::null())
      .stderr(Stdio::from(stderr))
      // The browser leads its own process group, so `close` stops the child processes that Chrome forks.
      .process_group(0)
      .spawn();
    self.process = match child {
      Ok(child) => Some(child),
      Err(error) => return Err(self.launch_failure(python_os_error(&error, &self.chrome_bin))?),
    };

    let port_path = user_data_path.join("DevToolsActivePort");
    let (port, path) = loop {
      if port_path.is_file() {
        let contents = fs::read_to_string(&port_path).map_err(|error| RenderFailure::Other(error.to_string()))?;
        let mut lines = contents.lines();
        let port = lines
          .next()
          .ok_or_else(|| RenderFailure::Value("not enough values to unpack (expected 2, got 0)".to_owned()))?;
        let path = lines
          .next()
          .ok_or_else(|| RenderFailure::Value("not enough values to unpack (expected 2, got 1)".to_owned()))?;
        break (port.to_owned(), path.to_owned());
      }
      if self.process_exited().map_err(|error| RenderFailure::Other(error.to_string()))?
        || self.deadline.is_some_and(|deadline| Instant::now() > deadline)
      {
        return Err(self.launch_failure("no DevToolsActivePort".to_owned())?);
      }
      thread::sleep(Duration::from_millis(50));
    };

    let address = format!("127.0.0.1:{port}");
    let stream = match TcpStream::connect(address.as_str()) {
      Ok(stream) => stream,
      Err(error) => return Err(self.launch_failure(error.to_string())?),
    };
    stream.set_read_timeout(Some(HEADLESS_LAUNCH_TIMEOUT)).map_err(|error| RenderFailure::Other(error.to_string()))?;
    stream.set_write_timeout(Some(HEADLESS_LAUNCH_TIMEOUT)).map_err(|error| RenderFailure::Other(error.to_string()))?;
    let url = format!("ws://{address}{path}");
    let request = url.as_str().into_client_request().map_err(|error| RenderFailure::Other(error.to_string()))?;
    self.socket = match client(request, stream) {
      Ok((socket, _response)) => Some(socket),
      Err(error) => return Err(self.launch_failure(error.to_string())?),
    };
    let target = self.command("Target.createTarget", json!({"url": "about:blank"}).as_object().cloned().unwrap())?;
    let target_id = target
      .get("result")
      .and_then(|result| result.get("targetId"))
      .and_then(Value::as_str)
      .ok_or_else(|| RenderFailure::Other("Target.createTarget returned no targetId".to_owned()))?;
    let attach = self.command(
      "Target.attachToTarget",
      json!({"targetId": target_id, "flatten": true}).as_object().cloned().unwrap(),
    )?;
    self.session = Some(
      attach
        .get("result")
        .and_then(|result| result.get("sessionId"))
        .and_then(Value::as_str)
        .ok_or_else(|| RenderFailure::Other("Target.attachToTarget returned no sessionId".to_owned()))?
        .to_owned(),
    );
    Ok(())
  }

  fn command(&mut self, method: &str, params: Map<String, Value>) -> Result<Value, RenderFailure> {
    self.message_id += 1;
    let id = self.message_id;
    let mut message = Map::new();
    message.insert("id".to_owned(), json!(id));
    message.insert("method".to_owned(), json!(method));
    message.insert("params".to_owned(), Value::Object(params));
    if let Some(session) = &self.session {
      message.insert("sessionId".to_owned(), json!(session));
    }
    let encoded =
      serde_json::to_string(&Value::Object(message)).map_err(|error| RenderFailure::Other(error.to_string()))?;
    self
      .socket
      .as_mut()
      .ok_or_else(|| RenderFailure::Other("headless renderer websocket is closed".to_owned()))?
      .send(Message::Text(encoded.into()))
      .map_err(|error| RenderFailure::Other(error.to_string()))?;
    loop {
      let reply = self.next_message()?;
      if reply.get("id").and_then(Value::as_u64) == Some(id) {
        if let Some(error) = reply.get("error") {
          return Err(RenderFailure::Other(format!("{method}: {}", text_repr::python_str(error))));
        }
        return Ok(reply);
      }
      self.pending.push(reply);
    }
  }

  fn wait_load(&mut self) -> Result<(), RenderFailure> {
    loop {
      if let Some(index) = self.pending.iter().position(is_load_event) {
        self.pending.remove(index);
        return Ok(());
      }
      let message = self.next_message()?;
      if is_load_event(&message) {
        return Ok(());
      }
      self.pending.push(message);
    }
  }

  fn next_message(&mut self) -> Result<Value, RenderFailure> {
    let remaining = self
      .deadline
      .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
      .filter(|duration| !duration.is_zero())
      .ok_or(RenderFailure::Timeout)?;
    self
      .socket
      .as_mut()
      .ok_or_else(|| RenderFailure::Other("headless renderer websocket is closed".to_owned()))?
      .get_mut()
      .set_read_timeout(Some(remaining))
      .map_err(|error| RenderFailure::Other(error.to_string()))?;
    loop {
      let message =
        self.socket.as_mut().expect("the renderer websocket was checked").read().map_err(|error| match &error {
          tungstenite::Error::Io(io_error)
            if matches!(io_error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
          {
            RenderFailure::Timeout
          }
          _ => RenderFailure::Other(error.to_string()),
        })?;
      match message {
        Message::Text(text) => {
          return serde_json::from_str(&text).map_err(|error| RenderFailure::Other(error.to_string()));
        }
        Message::Binary(bytes) => {
          return serde_json::from_slice(&bytes).map_err(|error| RenderFailure::Other(error.to_string()));
        }
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        Message::Close(_) => return Err(RenderFailure::Other("websocket connection closed".to_owned())),
      }
    }
  }

  fn process_exited(&mut self) -> std::io::Result<bool> {
    match self.process.as_mut() {
      Some(process) => Ok(process.try_wait()?.is_some()),
      None => Ok(true),
    }
  }

  fn launch_failure(&mut self, reason: String) -> Result<RenderFailure, RenderFailure> {
    let tail = self.stderr_tail().map_err(|error| RenderFailure::Other(error.to_string()))?;
    self.close().map_err(|error| RenderFailure::Other(error.to_string()))?;
    Ok(RenderFailure::Launch(format!(
      "headless renderer could not be launched: {} ({reason}){tail}",
      self.chrome_bin.display()
    )))
  }

  fn failure_message(&mut self, failure: RenderFailure) -> Result<String, String> {
    match failure {
      RenderFailure::Launch(message) => Ok(message),
      RenderFailure::Value(message) => {
        let _tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| error.to_string())?;
        Ok(message)
      }
      RenderFailure::Timeout => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| error.to_string())?;
        Ok(format!(
          "headless renderer timed out after {}s while measuring the plan page height{tail}",
          HEADLESS_RENDER_TIMEOUT.as_secs()
        ))
      }
      RenderFailure::Other(message) => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| error.to_string())?;
        Ok(format!("headless renderer failed while measuring the plan page height: {message}{tail}"))
      }
    }
  }

  fn stderr_tail(&self) -> std::io::Result<String> {
    let Some(stderr_path) = &self.stderr_path else { return Ok(String::new()) };
    if !stderr_path.is_file() {
      return Ok(String::new());
    }
    let data = fs::read(stderr_path)?;
    let start = data.len().saturating_sub(400);
    let tail = String::from_utf8_lossy(&data[start..]);
    let tail = tail.trim_matches(charliebot_python_whitespace);
    Ok(if tail.is_empty() { String::new() } else { format!(": {tail}") })
  }

  fn close(&mut self) -> anyhow::Result<()> {
    if let Some(socket) = self.socket.take() {
      drop(socket);
    }
    if let Some(mut process) = self.process.take() {
      kill_browser_group(process.id())?;
      let deadline = Instant::now() + HEADLESS_TEARDOWN_WAIT;
      while process.try_wait()?.is_none() {
        if Instant::now() >= deadline {
          return Err(anyhow!(
            "timed out after {}s waiting for headless renderer to exit",
            HEADLESS_TEARDOWN_WAIT.as_secs()
          ));
        }
        thread::sleep(Duration::from_millis(50));
      }
    }
    if let Some(user_data_dir) = self.user_data_dir.take() {
      let path = user_data_dir.path().to_path_buf();
      if let Err(error) = user_data_dir.close() {
        tracing::warn!(path = %path.display(), error = %error, "headless_renderer_tempdir_remove_failed");
      }
    }
    self.stderr_path = None;
    self.session = None;
    self.pending.clear();
    self.deadline = None;
    Ok(())
  }
}

pub(crate) fn render_height(chrome_bin: &Path, probe_uri: &str) -> Result<i32, String> {
  #[cfg(test)]
  if let Some(height) = test_height(chrome_bin) {
    return Ok(height);
  }
  let renderer = RENDERER.get_or_init(|| Mutex::new(None));
  let mut guard = renderer.lock().map_err(|error| error.to_string())?;
  if guard.as_ref().is_none_or(|current| current.chrome_bin != chrome_bin) {
    if let Some(mut current) = guard.take() {
      current.close().map_err(|error| error.to_string())?;
    }
    *guard = Some(WarmRenderer::new(chrome_bin.to_path_buf()));
  }
  guard.as_mut().expect("the renderer was created").render_height(probe_uri)
}

#[cfg(test)]
fn test_height(chrome_bin: &Path) -> Option<i32> {
  let heights = TEST_HEIGHTS.lock().expect("the test height table is not poisoned");
  heights.iter().find(|(path, _)| path.as_path() == chrome_bin).map(|(_, height)| *height)
}

#[cfg(test)]
pub(crate) fn set_test_height(chrome_bin: &Path, height: i32) {
  TEST_HEIGHTS.lock().expect("the test height table is not poisoned").push((chrome_bin.to_path_buf(), height));
}

pub(crate) fn close_renderer() -> anyhow::Result<()> {
  let Some(renderer) = RENDERER.get() else { return Ok(()) };
  let mut guard = renderer.lock().map_err(|error| anyhow!("headless renderer lock was poisoned: {error}"))?;
  if let Some(mut current) = guard.take() {
    current.close()?;
  }
  Ok(())
}

/// One process the orphan sweep reads from `/proc`.
pub(crate) struct ProcessRecord {
  pid: u32,
  parent_pid: u32,
  /// The file name of `/proc/<pid>/exe`.
  exe_name: String,
  argv: Vec<String>,
}

/// Whether `process` is a renderer browser that an ended server or CLI left behind: a `chrome` main process (no
/// `--type=` argument) whose user-data directory is one of the renderer's temp profiles and whose parent is init. Every
/// other Chrome stays untouched: a Chrome with any other user-data directory, such as a worker's measurement browser,
/// and a renderer browser whose parent process is still alive, such as the live server's own renderer or a running
/// CLI's.
fn is_orphaned_renderer(process: &ProcessRecord) -> bool {
  process.exe_name == "chrome"
    && process.parent_pid == 1
    && process.argv.iter().all(|argument| !argument.starts_with("--type="))
    && process.argv.iter().any(|argument| {
      argument
        .strip_prefix("--user-data-dir=")
        .and_then(|directory| Path::new(directory).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(RENDERER_TEMP_PREFIX))
    })
}

/// End every orphaned renderer browser and remove its temp profile, logging `headless_renderer_orphans_ended` with the
/// count. A failed kill or removal is logged with its path and error, and the sweep continues with the next browser.
pub(crate) fn end_orphaned_renderers() {
  let mut ended = 0;
  for orphan in read_process_records().into_iter().filter(is_orphaned_renderer) {
    match end_orphaned_renderer(&orphan) {
      Ok(()) => ended += 1,
      Err(error) => {
        let path = renderer_user_data_dir(&orphan.argv);
        tracing::warn!(pid = orphan.pid, path, error = format!("{error:#}"), "headless_renderer_orphan_end_failed")
      }
    }
  }
  tracing::info!(count = ended, "headless_renderer_orphans_ended");
}

/// Kill `orphan`'s process group, wait for the group to exit, and remove the browser's user-data directory.
fn end_orphaned_renderer(orphan: &ProcessRecord) -> anyhow::Result<()> {
  let group = read_process_group(orphan.pid)?;
  kill_browser_group(group)?;
  let deadline = Instant::now() + HEADLESS_TEARDOWN_WAIT;
  while process_group_has_members(group) {
    if Instant::now() >= deadline {
      return Err(anyhow!("process group {group} still has members after {}s", HEADLESS_TEARDOWN_WAIT.as_secs()));
    }
    thread::sleep(Duration::from_millis(50));
  }
  let user_data_dir =
    renderer_user_data_dir(&orphan.argv).ok_or_else(|| anyhow!("the browser's argv names no user-data directory"))?;
  if let Err(error) = fs::remove_dir_all(user_data_dir) {
    tracing::warn!(path = user_data_dir, error = %error, "headless_renderer_orphan_tempdir_remove_failed");
  }
  Ok(())
}

/// The user-data directory the browser's argv names, if it names one.
fn renderer_user_data_dir(argv: &[String]) -> Option<&str> {
  argv.iter().find_map(|argument| argument.strip_prefix("--user-data-dir="))
}

/// Every process `/proc` shows, as far as this user can read it. A process that exited while the sweep read, a kernel
/// thread, and another user's process stay out.
fn read_process_records() -> Vec<ProcessRecord> {
  let Ok(entries) = fs::read_dir("/proc") else { return Vec::new() };
  entries
    .flatten()
    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok().and_then(read_process_record))
    .collect()
}

fn read_process_record(pid: u32) -> Option<ProcessRecord> {
  let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
  let exe_name = exe.file_name()?.to_str()?.to_owned();
  let argv_raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
  // Chrome rewrites its argv into one space-joined string, so each NUL-separated part splits on spaces too.
  let mut argv = Vec::new();
  for part in argv_raw.split(|byte| *byte == 0) {
    if part.is_empty() {
      continue;
    }
    for argument in String::from_utf8_lossy(part).split(' ').filter(|argument| !argument.is_empty()) {
      argv.push(argument.to_owned());
    }
  }
  let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
  let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
  let parent_pid = fields.nth(1)?.parse().ok()?;
  Some(ProcessRecord { pid, parent_pid, exe_name, argv })
}

/// The process group of `pid`, from `/proc/<pid>/stat`: the renderer browser leads its own group, and an older
/// launcher's browser keeps the ended launcher's group, whose remaining members are its fellow leftovers.
fn read_process_group(pid: u32) -> anyhow::Result<u32> {
  let stat = fs::read_to_string(format!("/proc/{pid}/stat")).context("the browser's proc stat is unreadable")?;
  let mut fields = stat
    .rsplit_once(')')
    .ok_or_else(|| anyhow!("the browser's proc stat has no command delimiter"))?
    .1
    .split_whitespace();
  fields
    .nth(2)
    .ok_or_else(|| anyhow!("the browser's proc stat has no process group field"))?
    .parse()
    .context("the browser's process group is not a number")
}

/// Whether `group` still has a member: signal 0 probes membership, and `ESRCH` alone says it has none.
fn process_group_has_members(group: u32) -> bool {
  let Ok(encoded) = i32::try_from(group) else { return false };
  let result = unsafe { libc::kill(-encoded, 0) };
  result == 0 || (result == -1 && Errno::last() != Errno::ESRCH)
}

fn is_load_event(value: &Value) -> bool {
  value.get("method").and_then(Value::as_str) == Some("Page.loadEventFired")
}

fn python_int_value(value: &Value) -> Result<i32, String> {
  match value {
    Value::String(text) => python_decimal_integer(text),
    Value::Number(number) => number.to_string().parse::<i32>().map_err(|error| error.to_string()),
    Value::Bool(value) => Ok(i32::from(*value)),
    _ => Err(format!("invalid literal for int(): {}", text_repr::python_str(value))),
  }
}

fn python_decimal_integer(text: &str) -> Result<i32, String> {
  let mut digits = String::with_capacity(text.len());
  for character in text.chars() {
    let Some(digit) = str_methods::python_decimal_value(character) else {
      return Err(format!("invalid literal for int() with base 10: {}", text_repr::str_repr(text)));
    };
    digits.push(char::from(b'0' + digit));
  }
  digits.parse::<i32>().map_err(|error| error.to_string())
}

fn python_os_error(error: &std::io::Error, path: &Path) -> String {
  let (number, description) = match error.kind() {
    ErrorKind::NotFound => (2, "No such file or directory"),
    ErrorKind::PermissionDenied => (13, "Permission denied"),
    ErrorKind::AlreadyExists => (17, "File exists"),
    ErrorKind::InvalidInput => (22, "Invalid argument"),
    _ => return error.to_string(),
  };
  format!("[Errno {number}] {description}: {}", text_repr::str_repr(&path.to_string_lossy()))
}

fn charliebot_python_whitespace(character: char) -> bool {
  str_methods::is_python_whitespace(character)
}

/// SIGKILL to the process group `group` ids. The renderer's launch makes the browser its own group leader; the group
/// has members while Chrome's child processes run, and it has none once they are gone, which is not an error.
fn kill_browser_group(group: u32) -> anyhow::Result<()> {
  let encoded = Pid::from_raw(i32::try_from(group)?);
  match signal::killpg(encoded, Signal::SIGKILL) {
    Ok(()) | Err(Errno::ESRCH) => Ok(()),
    Err(error) => Err(error.into()),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::io::{BufRead, BufReader};
  use std::thread::JoinHandle;

  use serde_json::{Value, json};
  use tungstenite::accept;

  use super::*;
  use crate::test_support::chrome_program;

  /// A fake Chrome that records each launch in `launches_path`, writes the DevTools port into the profile it is given,
  /// and runs until it is killed.
  fn chrome_stub(directory: &Path, port: u16, launches_path: &Path) -> PathBuf {
    chrome_program(
      directory,
      json!([
        {"append": {"path": launches_path.display().to_string(), "text": "launch\n"}},
        {"write_in_arg_dir": {
          "arg_prefix": "--user-data-dir=",
          "file": "DevToolsActivePort",
          "text": format!("{port}\n/devtools/browser/stub\n"),
        }},
        {"sleep_ms": 60000},
        {"exit": 0},
      ]),
    )
  }

  fn cdp_server(listener: std::net::TcpListener, count: usize, height: &str) -> JoinHandle<()> {
    let height = height.to_owned();
    thread::spawn(move || {
      for _ in 0..count {
        let (stream, _) = listener.accept().expect("Chrome connects to the fake DevTools server");
        let mut socket = accept(stream).expect("the fake DevTools websocket accepts Chrome");
        while let Ok(message) = socket.read() {
          let Message::Text(text) = message else { continue };
          let request: Value = serde_json::from_str(&text).expect("the DevTools command is JSON");
          let id = request["id"].clone();
          let method = request["method"].as_str().expect("the DevTools command has a method");
          let session = request.get("sessionId").cloned();
          let response = match method {
            "Target.createTarget" => json!({"targetId": "target"}),
            "Target.attachToTarget" => json!({"sessionId": "session"}),
            "Runtime.evaluate" => json!({"result": {"type": "string", "value": height}}),
            _ => json!({}),
          };
          let mut reply = json!({"id": id, "result": response});
          if let Some(session) = session {
            reply["sessionId"] = session;
          }
          socket.send(Message::Text(reply.to_string().into())).expect("the DevTools reply writes");
          if method == "Page.navigate" {
            socket
              .send(Message::Text(json!({"method": "Page.loadEventFired", "sessionId": "session"}).to_string().into()))
              .expect("the page load event writes");
          }
        }
      }
    })
  }

  #[test]
  fn test_render_height_launches_once_and_serves_warm() {
    let directory = tempfile::tempdir().expect("a temporary directory exists");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the DevTools port binds");
    let port = listener.local_addr().expect("the DevTools address is available").port();
    let launches = directory.path().join("launches.txt");
    let chrome = chrome_stub(directory.path(), port, &launches);
    let server = cdp_server(listener, 1, "800");
    let mut renderer = WarmRenderer::new(chrome);
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    renderer.close().unwrap();
    server.join().unwrap();
    assert_eq!(fs::read_to_string(launches).unwrap().lines().count(), 1);
  }

  #[test]
  fn crashed_renderer_relaunches_the_chrome_stub() {
    let directory = tempfile::tempdir().expect("a temporary directory exists");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the DevTools port binds");
    let port = listener.local_addr().expect("the DevTools address is available").port();
    let launches = directory.path().join("launches.txt");
    let chrome = chrome_stub(directory.path(), port, &launches);
    let server = cdp_server(listener, 2, "800");
    let mut renderer = WarmRenderer::new(chrome);
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    renderer.process.as_mut().unwrap().kill().unwrap();
    renderer.process.as_mut().unwrap().wait().unwrap();
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    renderer.close().unwrap();
    server.join().unwrap();
    assert_eq!(fs::read_to_string(launches).unwrap().lines().count(), 2);
  }

  #[test]
  fn chrome_launch_failure_reports_the_python_message_and_stderr_tail() {
    let directory = tempfile::tempdir().expect("a temporary directory exists");
    let chrome = chrome_program(directory.path(), json!([{"stderr": "synthetic renderer failure\n"}, {"exit": 1}]));
    let mut renderer = WarmRenderer::new(chrome.clone());
    let error = renderer.render_height("file:///synthetic/page.html").unwrap_err();
    assert_eq!(
      error,
      format!(
        "headless renderer could not be launched: {} (no DevToolsActivePort): synthetic renderer failure",
        chrome.display()
      )
    );
  }

  /// Whether `pid` still runs. A zombie has exited, and a missing `/proc` entry has been reaped.
  fn process_alive(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let state = stat.rsplit_once(')').and_then(|(_, fields)| fields.trim_start().chars().next());
    !matches!(state, Some('Z' | 'X'))
  }

  /// A process record with the fields the orphan sweep reads, for selection tests without a real orphan.
  fn process_record(pid: u32, parent_pid: u32, exe_name: &str, argv: &[&str]) -> ProcessRecord {
    ProcessRecord {
      pid,
      parent_pid,
      exe_name: exe_name.to_owned(),
      argv: argv.iter().map(|argument| argument.to_string()).collect(),
    }
  }

  #[test]
  fn the_orphan_sweep_selects_only_init_parented_renderer_mains() {
    let orphan = process_record(
      100,
      1,
      "chrome",
      &["/opt/google/chrome/chrome", "--headless", "--user-data-dir=/tmp/headless-render-abc", "about:blank"],
    );
    assert!(is_orphaned_renderer(&orphan));

    let live_parent = process_record(100, 4242, "chrome", &["--user-data-dir=/tmp/headless-render-abc"]);
    assert!(!is_orphaned_renderer(&live_parent), "a renderer browser with a live parent stays");

    let other_profile = process_record(100, 1, "chrome", &["--user-data-dir=/tmp/panel-measure-abc"]);
    assert!(!is_orphaned_renderer(&other_profile), "a Chrome with another user-data directory stays");

    let renderer_child =
      process_record(101, 100, "chrome", &["--type=renderer", "--user-data-dir=/tmp/headless-render-abc"]);
    assert!(!is_orphaned_renderer(&renderer_child), "a renderer child process stays");

    let not_chrome = process_record(102, 1, "sleep", &["--user-data-dir=/tmp/headless-render-abc"]);
    assert!(!is_orphaned_renderer(&not_chrome), "a non-chrome executable stays");
  }

  #[test]
  fn renderer_close_kills_the_whole_process_group() {
    let mut browser = Command::new("sh")
      .args(["-c", "sleep 300 & echo $!; wait"])
      .process_group(0)
      .stdout(Stdio::piped())
      .spawn()
      .expect("the shell starts");
    let mut line = String::new();
    BufReader::new(browser.stdout.take().expect("the shell's output is piped"))
      .read_line(&mut line)
      .expect("the shell reports its background child");
    let child: i32 = line.trim().parse().expect("the background child's pid is a number");
    let mut renderer = WarmRenderer::new(PathBuf::from("unused"));
    renderer.process = Some(browser);
    renderer.close().expect("close stops the browser's group");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(child) && Instant::now() < deadline {
      thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_alive(child), "close left the browser's child {child} running");
  }

  #[test]
  fn renderer_close_drops_the_child_and_profile() {
    let directory = tempfile::tempdir().expect("a temporary directory exists");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the DevTools port binds");
    let port = listener.local_addr().expect("the DevTools address is available").port();
    let chrome = chrome_stub(directory.path(), port, &directory.path().join("launches.txt"));
    let server = cdp_server(listener, 1, "800");
    let mut renderer = WarmRenderer::new(chrome);
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    let browser = Pid::from_raw(renderer.process.as_ref().unwrap().id().try_into().unwrap());
    assert_eq!(nix::unistd::getpgid(Some(browser)).unwrap(), browser, "close kills the browser's own group");
    let profile = renderer.user_data_dir.as_ref().unwrap().path().to_path_buf();
    renderer.close().unwrap();
    server.join().unwrap();
    assert!(!profile.exists());
  }
}
