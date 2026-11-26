use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use charliebot_core::proc_identity::{read_stat_text, stat_fields_after_command};
use charliebot_core::str_methods;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use futures_util::future::BoxFuture;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde_json::{Map, Value, json};
use tempfile::{Builder, TempDir};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client};

const HEADLESS_RENDER_TIMEOUT: Duration = Duration::from_secs(60);
/// The launch window for a headless Chrome: its DevTools port file and its first commands. On the CI runner the port
/// file can take more than 15 s to appear, so the window is 60 s.
const HEADLESS_LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
const HEADLESS_TEARDOWN_WAIT: Duration = Duration::from_secs(5);
/// The user-data directory prefix of every renderer browser, here and in the startup orphan sweep.
const RENDERER_TEMP_PREFIX: &str = "headless-render-";
const CRASHPAD_HANDLER_EXE: &str = "chrome_crashpad_handler";

static RENDERER: OnceLock<Mutex<Option<WarmRenderer>>> = OnceLock::new();

/// The page heights that tests fake, keyed by the Chrome path that measures them. Verbs run the assertions on the
/// blocking pool, so a fake set on the test thread would not reach them. A path is unique to one test's fixture.
#[cfg(test)]
static TEST_HEIGHTS: Mutex<Vec<(PathBuf, i32)>> = Mutex::new(Vec::new());

#[cfg(test)]
static RENDERER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
      self.close().map_err(|error| format!("{error:#}"))?;
      if let Err(failure) = self.launch() {
        return Err(self.failure_message(failure)?);
      }
    }
    self.deadline = Some(Instant::now() + HEADLESS_RENDER_TIMEOUT);
    match self.render_once(probe_uri) {
      Ok(height) => Ok(height),
      Err(failure) => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| format!("{error:#}"))?;
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
        return int_value(value).map_err(RenderFailure::Value);
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
        let detail = os_error(&error, self.stderr_path.as_deref().expect("stderr path was set"));
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
      .spawn();
    self.process = match child {
      Ok(child) => Some(child),
      Err(error) => return Err(self.launch_failure(os_error(&error, &self.chrome_bin))?),
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
          return Err(RenderFailure::Other(format!("{method}: {}", text_repr::value_str(error))));
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
          tungstenite::Error::ConnectionClosed
          | tungstenite::Error::AlreadyClosed
          | tungstenite::Error::Io(_)
          | tungstenite::Error::Tls(_)
          | tungstenite::Error::Capacity(_)
          | tungstenite::Error::Protocol(_)
          | tungstenite::Error::WriteBufferFull(_)
          | tungstenite::Error::Utf8(_)
          | tungstenite::Error::AttackAttempt
          | tungstenite::Error::Url(_)
          | tungstenite::Error::Http(_)
          | tungstenite::Error::HttpFormat(_) => RenderFailure::Other(error.to_string()),
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
    self.close().map_err(|error| RenderFailure::Other(format!("{error:#}")))?;
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
        self.close().map_err(|error| format!("{error:#}"))?;
        Ok(message)
      }
      RenderFailure::Timeout => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| format!("{error:#}"))?;
        Ok(format!(
          "headless renderer timed out after {}s while measuring the plan page height{tail}",
          HEADLESS_RENDER_TIMEOUT.as_secs()
        ))
      }
      RenderFailure::Other(message) => {
        let tail = self.stderr_tail().map_err(|error| error.to_string())?;
        self.close().map_err(|error| format!("{error:#}"))?;
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
    let tail = tail.trim_matches(charliebot_whitespace);
    Ok(if tail.is_empty() { String::new() } else { format!(": {tail}") })
  }

  fn close(&mut self) -> anyhow::Result<()> {
    if let Some(socket) = self.socket.take() {
      drop(socket);
    }
    if let Some(mut process) = self.process.take()
      && process.try_wait()?.is_none()
    {
      process.kill().context("could not stop the headless renderer browser")?;
      process.wait().context("could not wait for the headless renderer browser")?;
    }
    if let Some(user_data_dir) = self.user_data_dir.take() {
      let path = user_data_dir.path().to_path_buf();
      end_renderer_processes(&path)?;
      user_data_dir
        .close()
        .with_context(|| format!("could not remove headless renderer profile {}", path.display()))?;
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

/// The server lifecycle hook that closes the process-wide renderer on every stop path.
pub(crate) struct RendererLifecycle;

impl charliebot_registry::wiring::Service for RendererLifecycle {
  fn name(&self) -> &'static str {
    "headless_renderer"
  }

  fn phase(&self) -> charliebot_registry::wiring::Phase {
    charliebot_registry::wiring::Phase::Early
  }

  fn start<'a>(&'a self, _ctx: charliebot_registry::wiring::ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async {
      tokio::task::spawn_blocking(close_renderer).await??;
      Ok(())
    })
  }
}

/// The startup sweep that ends the renderer browsers an ended server or CLI left behind, on the blocking pool.
pub(crate) fn sweep_headless_renderer_orphans_at_startup(
  _features: Arc<FeatureContext>,
) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    tokio::task::spawn_blocking(end_orphaned_renderers).await??;
    Ok(())
  })
}

/// One process the orphan sweep reads from `/proc`.
pub(crate) struct ProcessRecord {
  pid: u32,
  parent_pid: u32,
  start_time: u64,
  state: char,
  /// The executable file name, read when argv names a renderer profile or a crashpad handler. `None` when that read
  /// was refused, and the argv decides the process's role instead.
  exe_name: Option<String>,
  argv: Vec<String>,
}

/// One pass over `/proc`: the records it read, and the pids whose `/proc` fields refused permission.
struct ProcessScan {
  records: Vec<ProcessRecord>,
  refused: BTreeSet<u32>,
}

/// One read of `/proc/<pid>`.
enum ProcessRead {
  /// The process has no command line, or it exited during the read.
  Gone,
  /// A `/proc` field refused permission, so the process cannot be classified from this read.
  Refused,
  Record(ProcessRecord),
}

/// Whether `process` is a renderer browser whose parent chain has no live CharlieBot server.
fn is_orphaned_renderer(
  process: &ProcessRecord,
  processes: &HashMap<u32, &ProcessRecord>,
  refused: &BTreeSet<u32>,
) -> bool {
  is_renderer_browser(process) && !renderer_has_live_server_ancestor(process, processes, refused)
}

fn is_renderer_browser(process: &ProcessRecord) -> bool {
  let argv0_name =
    process.argv.first().and_then(|argument| Path::new(argument).file_name()).and_then(|name| name.to_str());
  (process.exe_name.as_deref() == Some("chrome") || argv0_name == Some("chrome"))
    && process.state != 'Z'
    && process.state != 'X'
    && process.argv.iter().all(|argument| !argument.starts_with("--type="))
    && renderer_user_data_dir(&process.argv).is_some()
}

/// Whether the parent chain holds a live server, or a process whose `/proc` fields refused permission, which the sweep
/// cannot prove is not one.
fn renderer_has_live_server_ancestor(
  process: &ProcessRecord,
  processes: &HashMap<u32, &ProcessRecord>,
  refused: &BTreeSet<u32>,
) -> bool {
  let mut parent_pid = process.parent_pid;
  let mut visited = HashSet::new();
  while parent_pid > 0 && visited.insert(parent_pid) {
    if refused.contains(&parent_pid) {
      return true;
    }
    let Some(parent) = processes.get(&parent_pid) else { return false };
    let argv0_name =
      parent.argv.first().and_then(|argument| Path::new(argument).file_name()).and_then(|name| name.to_str());
    if (parent.exe_name.as_deref() == Some("charliebot-server") || argv0_name == Some("charliebot-server"))
      && parent.state != 'Z'
      && parent.state != 'X'
    {
      return true;
    }
    parent_pid = parent.parent_pid;
  }
  false
}

/// Ends each renderer whose server is gone and removes its profile. Read and cleanup errors are logged and returned.
/// Permission refusals are named once; a process is left alone when the readable fields cannot prove its ownership.
pub(crate) fn end_orphaned_renderers() -> anyhow::Result<()> {
  let scan = match read_process_scan() {
    Ok(scan) => scan,
    Err(error) => {
      tracing::error!(error = format!("{error:#}"), "headless_renderer_process_scan_failed");
      tracing::info!(count = 0, complete = false, "headless_renderer_orphans_ended");
      return Err(error);
    }
  };
  log_permission_refusals(&scan);
  end_orphaned_renderers_from(&scan).map(|_| ())
}

/// Logs each process whose `/proc` fields refused permission.
fn log_permission_refusals(scan: &ProcessScan) {
  for pid in &scan.refused {
    tracing::info!(pid, "headless_renderer_process_unreadable_left_alone");
  }
  for process in &scan.records {
    let exe_refused = exe_name_needed(&process.argv) && process.exe_name.is_none();
    if exe_refused {
      tracing::info!(pid = process.pid, "headless_renderer_exe_read_refused");
    }
  }
}

fn end_orphaned_renderers_from(scan: &ProcessScan) -> anyhow::Result<usize> {
  let by_pid: HashMap<u32, &ProcessRecord> = scan.records.iter().map(|process| (process.pid, process)).collect();
  let mut profiles = BTreeSet::new();
  for process in scan.records.iter().filter(|process| is_orphaned_renderer(process, &by_pid, &scan.refused)) {
    let profile = renderer_user_data_dir(&process.argv)
      .ok_or_else(|| anyhow!("renderer browser {} has no renderer user-data directory", process.pid))?;
    profiles.insert(PathBuf::from(profile));
  }

  let mut ended = 0;
  let mut failures = Vec::new();
  for profile in profiles {
    match end_orphaned_renderer(&profile) {
      Ok(()) => ended += 1,
      Err(error) => {
        tracing::error!(
          path = %profile.display(),
          error = format!("{error:#}"),
          "headless_renderer_orphan_end_failed"
        );
        failures.push(format!("{}: {error:#}", profile.display()));
      }
    }
  }
  tracing::info!(count = ended, "headless_renderer_orphans_ended");
  if failures.is_empty() {
    Ok(ended)
  } else {
    Err(anyhow!("could not end {} headless renderer orphan(s): {}", failures.len(), failures.join("; ")))
  }
}

/// Ends every process whose command line names `profile`, waits for those processes, and removes the profile.
fn end_orphaned_renderer(profile: &Path) -> anyhow::Result<()> {
  end_renderer_processes(profile)?;
  match fs::remove_dir_all(profile) {
    Ok(()) => {}
    Err(error) if error.kind() == ErrorKind::NotFound => {
      tracing::debug!(path = %profile.display(), "headless_renderer_profile_already_removed");
    }
    Err(error) => {
      return Err(error).with_context(|| format!("could not remove renderer profile {}", profile.display()));
    }
  }
  Ok(())
}

fn end_renderer_processes(profile: &Path) -> anyhow::Result<()> {
  let deadline = Instant::now() + HEADLESS_TEARDOWN_WAIT;
  let mut crashpad_pids: HashMap<u32, u64> = HashMap::new();
  loop {
    let scan = read_process_scan()?;
    let by_pid: HashMap<u32, &ProcessRecord> = scan.records.iter().map(|process| (process.pid, process)).collect();
    let matching: Vec<&ProcessRecord> =
      scan.records.iter().filter(|process| process_uses_renderer_profile(process, profile)).collect();
    for process in &matching {
      for cpid in crashpad_handler_pids(&process.argv) {
        if !crashpad_pids.contains_key(&cpid)
          && let Some(handler) = by_pid.get(&cpid)
          && is_crashpad_handler(handler)
        {
          crashpad_pids.insert(cpid, handler.start_time);
        }
      }
    }
    let crashpad_targets: Vec<&ProcessRecord> = crashpad_pids
      .iter()
      .filter_map(|(&pid, &start_time)| {
        by_pid
          .get(&pid)
          .copied()
          .filter(|process| process.start_time == start_time && process.state != 'Z' && process.state != 'X')
      })
      .collect();
    if matching.is_empty() && crashpad_targets.is_empty() {
      return Ok(());
    }
    for process in matching {
      kill_renderer_process(process, profile)?;
    }
    for handler in crashpad_targets {
      kill_crashpad_process(handler, profile)?;
    }
    if Instant::now() >= deadline {
      return Err(anyhow!(
        "processes using renderer profile {} remain after {}s",
        profile.display(),
        HEADLESS_TEARDOWN_WAIT.as_secs()
      ));
    }
    thread::sleep(Duration::from_millis(50));
  }
}

/// Reads a process again before its kill. A process whose fields refuse permission now cannot be verified, so it is
/// left alone and named.
fn reread_before_kill(process: &ProcessRecord, profile: &Path) -> anyhow::Result<Option<ProcessRecord>> {
  match read_process_record(process.pid)? {
    ProcessRead::Record(current) => Ok(Some(current)),
    ProcessRead::Gone => Ok(None),
    ProcessRead::Refused => {
      tracing::info!(pid = process.pid, path = %profile.display(), "headless_renderer_kill_skipped_unreadable");
      Ok(None)
    }
  }
}

fn kill_renderer_process(process: &ProcessRecord, profile: &Path) -> anyhow::Result<()> {
  let Some(current) = reread_before_kill(process, profile)? else { return Ok(()) };
  if current.start_time != process.start_time {
    return Err(anyhow!("process {} changed while ending renderer profile {}", process.pid, profile.display()));
  }
  if !process_uses_renderer_profile(&current, profile) {
    return Err(anyhow!("process {} no longer names renderer profile {}", process.pid, profile.display()));
  }
  send_sigkill(process.pid)
}

fn kill_crashpad_process(process: &ProcessRecord, profile: &Path) -> anyhow::Result<()> {
  let Some(current) = reread_before_kill(process, profile)? else { return Ok(()) };
  if current.start_time != process.start_time {
    return Err(anyhow!(
      "crashpad process {} changed while ending renderer profile {}",
      process.pid,
      profile.display()
    ));
  }
  if !is_crashpad_handler(&current) {
    return Err(anyhow!(
      "process {} is no longer a crashpad handler while ending renderer profile {}",
      process.pid,
      profile.display()
    ));
  }
  send_sigkill(process.pid)
}

fn send_sigkill(raw_pid: u32) -> anyhow::Result<()> {
  let pid = Pid::from_raw(i32::try_from(raw_pid).context("renderer pid does not fit a signal pid")?);
  match signal::kill(pid, Signal::SIGKILL) {
    Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
    Err(error) => Err(error).with_context(|| format!("could not kill renderer process {raw_pid}")),
  }
}

fn crashpad_handler_pids(argv: &[String]) -> Vec<u32> {
  argv
    .iter()
    .filter_map(|argument| argument.strip_prefix("--crashpad-handler-pid="))
    .filter_map(|pid| pid.parse::<u32>().ok())
    .collect()
}

fn is_crashpad_handler(process: &ProcessRecord) -> bool {
  let argv0_name =
    process.argv.first().and_then(|argument| Path::new(argument).file_name()).and_then(|name| name.to_str());
  process.exe_name.as_deref() == Some(CRASHPAD_HANDLER_EXE) || argv0_name == Some(CRASHPAD_HANDLER_EXE)
}

fn process_uses_renderer_profile(process: &ProcessRecord, profile: &Path) -> bool {
  process.argv.iter().any(|argument| {
    let value = argument.strip_prefix("--user-data-dir=").or_else(|| argument.split_once('=').map(|(_, value)| value));
    value.is_some_and(|value| Path::new(value).starts_with(profile)) || Path::new(argument).starts_with(profile)
  })
}

/// The renderer profile the browser's argv names, if its directory has the renderer prefix.
fn renderer_user_data_dir(argv: &[String]) -> Option<&str> {
  argv.iter().find_map(|argument| {
    argument.strip_prefix("--user-data-dir=").filter(|directory| {
      Path::new(directory)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(RENDERER_TEMP_PREFIX))
    })
  })
}

/// One pass over `/proc`. A process that vanishes mid-scan is skipped, a refused field is recorded, and any other read
/// error stops the scan.
fn read_process_scan() -> anyhow::Result<ProcessScan> {
  let entries = fs::read_dir("/proc").context("could not list /proc")?;
  let mut scan = ProcessScan { records: Vec::new(), refused: BTreeSet::new() };
  for entry in entries {
    let entry = entry.context("could not read a /proc directory entry")?;
    let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
    match read_process_record(pid) {
      Ok(ProcessRead::Record(process)) => scan.records.push(process),
      Ok(ProcessRead::Gone) => tracing::debug!(pid, "headless_renderer_process_disappeared_during_scan"),
      Ok(ProcessRead::Refused) => {
        scan.refused.insert(pid);
      }
      Err(error) => {
        tracing::error!(pid, error = format!("{error:#}"), "headless_renderer_process_read_failed");
        return Err(error).with_context(|| format!("could not inspect process {pid}"));
      }
    }
  }
  Ok(scan)
}

fn is_missing_process_error(error: &std::io::Error) -> bool {
  error.kind() == ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// The outcome of a failed `/proc/<pid>/<field>` read: a vanished process is gone, a refused field is the caller's
/// to skip, and any other failure is an error.
fn field_read_failure(pid: u32, field: &str, error: std::io::Error) -> anyhow::Result<ProcessRead> {
  if is_missing_process_error(&error) {
    Ok(ProcessRead::Gone)
  } else if error.kind() == ErrorKind::PermissionDenied {
    Ok(ProcessRead::Refused)
  } else {
    Err(error).with_context(|| format!("could not read /proc/{pid}/{field}"))
  }
}

/// Whether the sweep reads the executable name for this argv: a renderer profile, or a crashpad handler by argv0.
fn exe_name_needed(argv: &[String]) -> bool {
  let argv0_name = argv.first().and_then(|argument| Path::new(argument).file_name()).and_then(|name| name.to_str());
  argv.iter().any(|argument| argument.contains(RENDERER_TEMP_PREFIX)) || argv0_name == Some(CRASHPAD_HANDLER_EXE)
}

fn read_process_record(pid: u32) -> anyhow::Result<ProcessRead> {
  let argv_raw = match fs::read(format!("/proc/{pid}/cmdline")) {
    Ok(argv) => argv,
    Err(error) => return field_read_failure(pid, "cmdline", error),
  };
  if argv_raw.is_empty() {
    return Ok(ProcessRead::Gone);
  }
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
  let exe_name = if exe_name_needed(&argv) {
    match fs::read_link(format!("/proc/{pid}/exe")) {
      Ok(exe) => Some(
        exe
          .file_name()
          .and_then(|name| name.to_str())
          .ok_or_else(|| anyhow!("/proc/{pid}/exe has no UTF-8 file name"))?
          .to_owned(),
      ),
      // A non-dumpable process, such as a Chrome crashpad handler, refuses this read to a non-root reader. Its argv
      // then decides its role.
      Err(error) if error.kind() == ErrorKind::PermissionDenied => None,
      Err(error) if is_missing_process_error(&error) => return Ok(ProcessRead::Gone),
      Err(error) => return Err(error).with_context(|| format!("could not read /proc/{pid}/exe")),
    }
  } else {
    None
  };
  let stat = match read_stat_text(pid) {
    Ok(stat) => stat,
    Err(error) => return field_read_failure(pid, "stat", error),
  };
  let fields: Vec<&str> = stat_fields_after_command(&stat)
    .ok_or_else(|| anyhow!("/proc/{pid}/stat has no command delimiter"))?
    .split_whitespace()
    .collect();
  let state = fields
    .first()
    .and_then(|field| field.chars().next())
    .ok_or_else(|| anyhow!("/proc/{pid}/stat has no process state"))?;
  let parent_pid = fields
    .get(1)
    .ok_or_else(|| anyhow!("/proc/{pid}/stat has no parent pid"))?
    .parse()
    .with_context(|| format!("/proc/{pid}/stat has an invalid parent pid"))?;
  let start_time = fields
    .get(19)
    .ok_or_else(|| anyhow!("/proc/{pid}/stat has no process start time"))?
    .parse()
    .with_context(|| format!("/proc/{pid}/stat has an invalid process start time"))?;
  Ok(ProcessRead::Record(ProcessRecord { pid, parent_pid, start_time, state, exe_name, argv }))
}

fn is_load_event(value: &Value) -> bool {
  value.get("method").and_then(Value::as_str) == Some("Page.loadEventFired")
}

fn int_value(value: &Value) -> Result<i32, String> {
  match value {
    Value::String(text) => decimal_integer(text),
    Value::Number(number) => number.to_string().parse::<i32>().map_err(|error| error.to_string()),
    Value::Bool(value) => Ok(i32::from(*value)),
    _ => Err(format!("invalid literal for int(): {}", text_repr::value_str(value))),
  }
}

fn decimal_integer(text: &str) -> Result<i32, String> {
  let mut digits = String::with_capacity(text.len());
  for character in text.chars() {
    let Some(digit) = str_methods::decimal_value(character) else {
      return Err(format!("invalid literal for int() with base 10: {}", text_repr::str_repr(text)));
    };
    digits.push(char::from(b'0' + digit));
  }
  digits.parse::<i32>().map_err(|error| error.to_string())
}

fn os_error(error: &std::io::Error, path: &Path) -> String {
  match error.kind() {
    ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::AlreadyExists | ErrorKind::InvalidInput => {
      charliebot_core::os_error::text(error, &text_repr::str_repr(&path.to_string_lossy()))
    }
    _ => error.to_string(),
  }
}

fn charliebot_whitespace(character: char) -> bool {
  str_methods::is_whitespace(character)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::process::CommandExt;
  use std::thread::JoinHandle;

  use charliebot_registry::lifespan::{Lifespan, StartupOutcome};
  use charliebot_registry::startup_timing::StartupClock;
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

  /// A fake crashpad handler that is non-dumpable, as Chrome's handlers are, so a non-root reader's `/proc/<pid>/exe`
  /// read is refused.
  fn crashpad_stub(directory: &Path, name: &str) -> PathBuf {
    let stub = directory.join(name);
    fs::create_dir(&stub).expect("the crashpad stub directory is created");
    let program = stub.join("chrome_crashpad_handler");
    std::os::unix::fs::symlink(charliebot_shell_harness::built_binary("chrome-stub"), &program)
      .expect("the fake links to the helper");
    fs::write(
      stub.join(charliebot_core::paths::PROBE_FILE_NAME),
      json!({"chrome_crashpad_handler": ["non_dumpable", {"sleep_ms": 60000}, {"exit": 0}]}).to_string(),
    )
    .expect("the crashpad steps are written");
    program
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
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
  #[ignore = "integration"]
  fn chrome_launch_failure_reports_the_message_and_stderr_tail() {
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
    let Some(stat) = u32::try_from(pid).ok().and_then(|pid| read_stat_text(pid).ok()) else { return false };
    let state = stat_fields_after_command(&stat).and_then(|fields| fields.trim_start().chars().next());
    !matches!(state, Some('Z' | 'X'))
  }

  /// The record of a process that the test expects to read: `None` once it has exited, and a refused field fails.
  fn read_record(pid: u32) -> Option<ProcessRecord> {
    match read_process_record(pid).expect("the process reads") {
      ProcessRead::Record(process) => Some(process),
      ProcessRead::Gone => None,
      ProcessRead::Refused => panic!("process {pid} refused a /proc field"),
    }
  }

  /// Whether `/proc/<pid>/exe` refuses this reader, as it does for a non-dumpable process.
  fn exe_read_refused(pid: u32) -> bool {
    fs::read_link(format!("/proc/{pid}/exe")).is_err_and(|error| error.kind() == ErrorKind::PermissionDenied)
  }

  /// A process record for an ownership predicate test.
  fn process_record(pid: u32, parent_pid: u32, exe_name: &str, argv: &[&str]) -> ProcessRecord {
    ProcessRecord {
      pid,
      parent_pid,
      start_time: 1,
      state: 'S',
      exe_name: Some(exe_name.to_owned()),
      argv: argv.iter().map(|argument| argument.to_string()).collect(),
    }
  }

  #[test]
  fn the_orphan_sweep_uses_server_ownership_and_the_renderer_profile() {
    let systemd = process_record(815, 1, "systemd", &[]);
    let server = process_record(52, 1, "charliebot-server", &[]);
    let orphan = process_record(
      100,
      systemd.pid,
      "chrome",
      &["/opt/google/chrome/chrome", "--headless", "--user-data-dir=/tmp/headless-render-abc", "about:blank"],
    );
    let live = process_record(
      101,
      server.pid,
      "chrome",
      &["/opt/google/chrome/chrome", "--headless", "--user-data-dir=/tmp/headless-render-def", "about:blank"],
    );
    let other_profile = process_record(
      102,
      systemd.pid,
      "chrome",
      &["/opt/google/chrome/chrome", "--user-data-dir=/tmp/panel-measure-abc"],
    );
    let renderer_child =
      process_record(103, systemd.pid, "chrome", &["--type=renderer", "--user-data-dir=/tmp/headless-render-ghi"]);
    let not_chrome = process_record(104, systemd.pid, "sleep", &["--user-data-dir=/tmp/headless-render-jkl"]);
    let process_refs = [&systemd, &server, &orphan, &live, &other_profile, &renderer_child, &not_chrome];
    let by_pid: HashMap<u32, &ProcessRecord> = process_refs.iter().map(|process| (process.pid, *process)).collect();
    let refused = BTreeSet::new();

    assert!(is_orphaned_renderer(&orphan, &by_pid, &refused));
    assert!(!is_orphaned_renderer(&live, &by_pid, &refused), "a renderer with a live server ancestor stays");
    assert!(!is_orphaned_renderer(&other_profile, &by_pid, &refused), "other Chrome profiles stay");
    assert!(!is_orphaned_renderer(&renderer_child, &by_pid, &refused), "Chrome renderer children are not roots");
    assert!(!is_orphaned_renderer(&not_chrome, &by_pid, &refused), "non-Chrome processes are not roots");

    let child_with_crashpad = process_record(
      105,
      100,
      "chrome",
      &["--type=gpu-process", "--crashpad-handler-pid=200", "--user-data-dir=/tmp/headless-render-abc"],
    );
    assert_eq!(crashpad_handler_pids(&child_with_crashpad.argv), vec![200]);
    let crashpad = process_record(
      200,
      systemd.pid,
      CRASHPAD_HANDLER_EXE,
      &["/opt/google/chrome/chrome_crashpad_handler", "--database=/home/user/.config/google-chrome/Crash Reports"],
    );
    assert!(is_crashpad_handler(&crashpad));
    assert!(!is_crashpad_handler(&not_chrome));
  }

  /// A record whose exe read was refused: the sweep has only its argv.
  fn refused_exe_record(pid: u32, parent_pid: u32, argv: &[&str]) -> ProcessRecord {
    ProcessRecord { exe_name: None, ..process_record(pid, parent_pid, "", argv) }
  }

  #[test]
  fn a_refused_exe_leaves_the_role_to_argv() {
    let systemd = process_record(815, 1, "systemd", &[]);
    let orphan = refused_exe_record(
      100,
      systemd.pid,
      &["/opt/google/chrome/chrome", "--headless", "--user-data-dir=/tmp/headless-render-abc"],
    );
    let renamed =
      refused_exe_record(106, systemd.pid, &["/opt/other/launcher", "--user-data-dir=/tmp/headless-render-x"]);
    let crashpad = refused_exe_record(
      200,
      systemd.pid,
      &["/opt/google/chrome/chrome_crashpad_handler", "--database=/home/user/.config/google-chrome/Crash Reports"],
    );
    let unnamed_crashpad = refused_exe_record(201, systemd.pid, &["/opt/other/handler", "--database=/tmp/db"]);
    let by_pid: HashMap<u32, &ProcessRecord> = [&systemd, &orphan, &renamed, &crashpad, &unnamed_crashpad]
      .into_iter()
      .map(|process| (process.pid, process))
      .collect();
    let refused = BTreeSet::new();

    assert!(is_orphaned_renderer(&orphan, &by_pid, &refused), "argv0 chrome proves a renderer browser");
    assert!(!is_orphaned_renderer(&renamed, &by_pid, &refused), "an argv0 that is not chrome proves nothing");
    assert!(is_crashpad_handler(&crashpad), "argv0 chrome_crashpad_handler proves a crashpad handler");
    assert!(!is_crashpad_handler(&unnamed_crashpad), "an unproven crashpad handler is not ours");
  }

  #[test]
  fn a_refused_ancestor_keeps_its_renderer() {
    let systemd = process_record(815, 1, "systemd", &[]);
    let refused_parent_pid = 60;
    let child = process_record(
      100,
      refused_parent_pid,
      "chrome",
      &["/opt/google/chrome/chrome", "--user-data-dir=/tmp/headless-render-abc"],
    );
    let by_pid: HashMap<u32, &ProcessRecord> =
      [&systemd, &child].into_iter().map(|process| (process.pid, process)).collect();
    let refused: BTreeSet<u32> = [refused_parent_pid].into();

    assert!(
      !is_orphaned_renderer(&child, &by_pid, &refused),
      "a parent whose fields refused permission may be the live server"
    );
    assert!(
      is_orphaned_renderer(&child, &by_pid, &BTreeSet::new()),
      "the same renderer is an orphan once its parent is readable and gone"
    );
  }

  #[test]
  #[ignore = "integration"]
  fn startup_sweep_ends_reparented_renderer_and_crashpad_but_keeps_live_server_tree() {
    // SAFETY: geteuid has no preconditions.
    assert_ne!(unsafe { libc::geteuid() }, 0, "non-dumpable process checks require a non-root test user");
    let mut non_server_parent = Command::new("sleep").arg("60").spawn().expect("the subreaper stand-in starts");
    let directory = tempfile::tempdir().expect("the test directory exists");
    let orphan_profile = Builder::new().prefix(RENDERER_TEMP_PREFIX).tempdir().expect("the orphan profile exists");
    let live_profile = Builder::new().prefix(RENDERER_TEMP_PREFIX).tempdir().expect("the live profile exists");
    let orphan_directory = directory.path().join("orphan");
    let live_directory = directory.path().join("live");
    fs::create_dir(&orphan_directory).unwrap();
    fs::create_dir(&live_directory).unwrap();
    let orphan_browser_bin = chrome_program(&orphan_directory, json!([{"sleep_ms": 60000}, {"exit": 0}]));
    let live_browser_bin = chrome_program(&live_directory, json!([{"sleep_ms": 60000}, {"exit": 0}]));
    let orphan_crashpad_bin = crashpad_stub(directory.path(), "orphan_crashpad");
    let live_crashpad_bin = crashpad_stub(directory.path(), "live_crashpad");
    let unowned_crashpad_bin = crashpad_stub(directory.path(), "unowned_crashpad");
    let orphan_crashpad_db = directory.path().join("CrashReportsOutsideProfile");
    let mut orphan_crashpad = Command::new(orphan_crashpad_bin)
      .arg(format!("--database={}", orphan_crashpad_db.display()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the orphan crashpad handler starts");
    let mut orphan_browser = Command::new(orphan_browser_bin)
      .arg("--headless")
      .arg(format!("--user-data-dir={}", orphan_profile.path().display()))
      .arg(format!("--crashpad-handler-pid={}", orphan_crashpad.id()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the orphan browser starts");
    let mut live_browser = Command::new(live_browser_bin)
      .arg("--headless")
      .arg(format!("--user-data-dir={}", live_profile.path().display()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the live server browser starts");
    let mut live_crashpad = Command::new(live_crashpad_bin)
      .arg(format!("--database={}/Crashpad", live_profile.path().display()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the live server crashpad handler starts");
    let mut unowned_crashpad = Command::new(unowned_crashpad_bin)
      .arg(format!("--database={}", directory.path().join("UnownedCrashReports").display()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the unowned crashpad handler starts");
    let orphan_browser_pid = orphan_browser.id();
    let orphan_crashpad_pid = orphan_crashpad.id();
    let live_browser_pid = live_browser.id();
    let live_crashpad_pid = live_crashpad.id();
    let unowned_crashpad_pid = unowned_crashpad.id();
    let test_pid = std::process::id();
    read_record(test_pid).expect("the test process has a proc record");
    let parent = read_record(non_server_parent.id()).expect("the stand-in parent has a proc record");

    // Each crashpad handler marks itself non-dumpable after its exec, so its exe read is refused only once that runs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut scan = loop {
      let scan = read_process_scan().expect("the process table is readable");
      let ready = [orphan_browser_pid, orphan_crashpad_pid, live_browser_pid, live_crashpad_pid, unowned_crashpad_pid]
        .iter()
        .all(|pid| scan.records.iter().any(|process| process.pid == *pid));
      if ready
        && [orphan_crashpad_pid, live_crashpad_pid, unowned_crashpad_pid].iter().all(|pid| exe_read_refused(*pid))
      {
        break scan;
      }
      assert!(Instant::now() < deadline, "the crashpad handlers' exe reads were not refused (the test needs non-root)");
      thread::sleep(Duration::from_millis(10));
    };
    scan.records.retain(|process| {
      [
        orphan_browser_pid,
        orphan_crashpad_pid,
        live_browser_pid,
        live_crashpad_pid,
        unowned_crashpad_pid,
        test_pid,
        parent.pid,
      ]
      .contains(&process.pid)
    });
    for process in &mut scan.records {
      if process.pid == orphan_browser_pid || process.pid == unowned_crashpad_pid {
        process.parent_pid = parent.pid;
      }
      if process.pid == orphan_crashpad_pid {
        process.parent_pid = orphan_browser_pid;
      }
      if process.pid == live_browser_pid || process.pid == live_crashpad_pid {
        process.parent_pid = if process.pid == live_browser_pid { test_pid } else { live_browser_pid };
      }
      if process.pid == test_pid {
        process.exe_name = Some("charliebot-server".to_owned());
        process.parent_pid = 1;
      }
      if process.pid == parent.pid {
        process.parent_pid = 1;
      }
    }

    let orphan_browser_record = scan.records.iter().find(|process| process.pid == orphan_browser_pid).unwrap();
    let live_browser_record = scan.records.iter().find(|process| process.pid == live_browser_pid).unwrap();
    let by_pid: HashMap<u32, &ProcessRecord> = scan.records.iter().map(|process| (process.pid, process)).collect();
    assert!(is_orphaned_renderer(orphan_browser_record, &by_pid, &scan.refused));
    assert!(!is_orphaned_renderer(live_browser_record, &by_pid, &scan.refused));
    assert_ne!(
      nix::unistd::getpgid(Some(Pid::from_raw(orphan_browser_pid as i32))).unwrap(),
      nix::unistd::getpgid(Some(Pid::from_raw(orphan_crashpad_pid as i32))).unwrap(),
      "the crashpad handler uses a separate process group"
    );

    assert_eq!(end_orphaned_renderers_from(&scan).unwrap(), 1, "the sweep proceeds past the refused exe read");
    let deadline = Instant::now() + Duration::from_secs(5);
    while (process_alive(orphan_browser_pid as i32) || process_alive(orphan_crashpad_pid as i32))
      && Instant::now() < deadline
    {
      thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_alive(orphan_browser_pid as i32));
    assert!(!process_alive(orphan_crashpad_pid as i32));
    assert!(!orphan_profile.path().exists());
    assert!(live_profile.path().exists());
    assert!(live_browser.try_wait().unwrap().is_none(), "the live server browser was ended");
    assert!(live_crashpad.try_wait().unwrap().is_none(), "the live server crashpad handler was ended");
    assert!(unowned_crashpad.try_wait().unwrap().is_none(), "an unowned crashpad handler was ended");

    orphan_browser.wait().unwrap();
    orphan_crashpad.wait().unwrap();
    live_browser.kill().unwrap();
    live_browser.wait().unwrap();
    live_crashpad.kill().unwrap();
    live_crashpad.wait().unwrap();
    unowned_crashpad.kill().unwrap();
    unowned_crashpad.wait().unwrap();
    non_server_parent.kill().unwrap();
    non_server_parent.wait().unwrap();
    live_profile.close().unwrap();
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn registered_lifecycle_closes_warm_renderer_and_its_crashpad_handler() {
    let _serial = RENDERER_TEST_LOCK.lock().await;
    let (_home, features) = crate::test_support::features();
    let mut clock = StartupClock::new(Instant::now());
    let mut lifespan = match Lifespan::start(features, &mut clock).await.unwrap() {
      StartupOutcome::Ready(lifespan) => lifespan,
      StartupOutcome::Stopped(_) => panic!("the test lifecycle stopped during startup"),
    };
    let directory = tempfile::tempdir().expect("the test directory exists");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the DevTools port binds");
    let port = listener.local_addr().expect("the DevTools address is available").port();
    let chrome = chrome_stub(directory.path(), port, &directory.path().join("launches.txt"));
    let server = cdp_server(listener, 1, "800");
    assert_eq!(render_height(&chrome, "file:///synthetic/page.html").unwrap(), 800);
    let profile = RENDERER
      .get()
      .expect("render_height created the renderer")
      .lock()
      .unwrap()
      .as_ref()
      .expect("the renderer is warm")
      .user_data_dir
      .as_ref()
      .expect("the renderer has a profile")
      .path()
      .to_path_buf();
    let crashpad_bin = crashpad_stub(directory.path(), "lifecycle_crashpad");
    let mut crashpad = Command::new(crashpad_bin)
      .arg(format!("--database={}/Crashpad", profile.display()))
      .process_group(0)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("the crashpad handler starts");
    let crashpad_pid = crashpad.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while read_record(crashpad_pid).is_none_or(|process| !process_uses_renderer_profile(&process, &profile))
      && Instant::now() < deadline
    {
      thread::sleep(Duration::from_millis(10));
    }
    assert!(
      read_record(crashpad_pid).is_some_and(|process| process_uses_renderer_profile(&process, &profile)),
      "the crashpad handler command line names the renderer profile"
    );

    let shutdown = lifespan.shutdown().await;
    assert!(shutdown.error.is_none(), "renderer lifecycle stop failed: {:?}", shutdown.error);
    server.join().unwrap();
    assert!(!profile.exists(), "the renderer profile remains after lifecycle shutdown");
    assert!(!process_alive(crashpad_pid as i32), "the renderer crashpad handler remains after shutdown");
    crashpad.wait().unwrap();
    assert!(
      read_process_scan().unwrap().records.iter().all(|process| !process_uses_renderer_profile(process, &profile)),
      "a process with the renderer profile in argv remains after shutdown"
    );
  }

  #[test]
  #[ignore = "integration"]
  fn renderer_close_drops_the_child_and_profile() {
    let directory = tempfile::tempdir().expect("a temporary directory exists");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the DevTools port binds");
    let port = listener.local_addr().expect("the DevTools address is available").port();
    let chrome = chrome_stub(directory.path(), port, &directory.path().join("launches.txt"));
    let server = cdp_server(listener, 1, "800");
    let mut renderer = WarmRenderer::new(chrome);
    assert_eq!(renderer.render_height("file:///synthetic/page.html").unwrap(), 800);
    let profile = renderer.user_data_dir.as_ref().unwrap().path().to_path_buf();
    renderer.close().unwrap();
    server.join().unwrap();
    assert!(!profile.exists());
  }
}
