use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use charliebot_core::pyfmt;
use charliebot_core::pystr;
use serde_json::{Map, Value, json};
use tempfile::{Builder, TempDir};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client};

const HEADLESS_RENDER_TIMEOUT: Duration = Duration::from_secs(60);
const HEADLESS_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const HEADLESS_TEARDOWN_WAIT: Duration = Duration::from_secs(5);

static RENDERER: OnceLock<Mutex<Option<WarmRenderer>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
  static TEST_HEIGHT: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

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
      Builder::new().prefix("headless-render-").tempdir().map_err(|error| RenderFailure::Other(error.to_string()))?;
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
          return Err(RenderFailure::Other(format!("{method}: {}", pyfmt::python_str(error))));
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
      if process.try_wait()?.is_none() {
        process.kill()?;
      }
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
  if let Some(height) = TEST_HEIGHT.with(std::cell::Cell::get) {
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
pub(crate) fn set_test_height(height: Option<i32>) {
  TEST_HEIGHT.with(|value| value.set(height));
}

pub(crate) fn close_renderer() -> anyhow::Result<()> {
  let Some(renderer) = RENDERER.get() else { return Ok(()) };
  let mut guard = renderer.lock().map_err(|error| anyhow!("headless renderer lock was poisoned: {error}"))?;
  if let Some(mut current) = guard.take() {
    current.close()?;
  }
  Ok(())
}

fn is_load_event(value: &Value) -> bool {
  value.get("method").and_then(Value::as_str) == Some("Page.loadEventFired")
}

fn python_int_value(value: &Value) -> Result<i32, String> {
  match value {
    Value::String(text) => python_decimal_integer(text),
    Value::Number(number) => number.to_string().parse::<i32>().map_err(|error| error.to_string()),
    Value::Bool(value) => Ok(i32::from(*value)),
    _ => Err(format!("invalid literal for int(): {}", pyfmt::python_str(value))),
  }
}

fn python_decimal_integer(text: &str) -> Result<i32, String> {
  let mut digits = String::with_capacity(text.len());
  for character in text.chars() {
    let Some(digit) = pystr::python_decimal_value(character) else {
      return Err(format!("invalid literal for int() with base 10: {}", pyfmt::str_repr(text)));
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
  format!("[Errno {number}] {description}: {}", pyfmt::str_repr(&path.to_string_lossy()))
}

fn charliebot_python_whitespace(character: char) -> bool {
  pystr::is_python_whitespace(character)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::thread::JoinHandle;

  use serde_json::Value;
  use tungstenite::accept;

  use super::*;

  fn chrome_stub(directory: &Path, port: u16, launches_path: &Path) -> PathBuf {
    let script = directory.join("chrome-stub.sh");
    let source = format!(
      "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --user-data-dir=*) udd=${{arg#--user-data-dir=}} ;;\n  esac\ndone\nprintf '%s\\n' launch >> '{}'\nprintf '%s\\n%s\\n' '{}' '/devtools/browser/stub' > \"$udd/DevToolsActivePort\"\nexec sleep 60\n",
      launches_path.display(),
      port
    );
    fs::write(&script, source).expect("the Chrome stub writes");
    let mut permissions = fs::metadata(&script).expect("the Chrome stub metadata reads").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("the Chrome stub becomes executable");
    script
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
    let chrome = directory.path().join("chrome-fails.sh");
    fs::write(&chrome, "#!/bin/sh\nprintf '%s\\n' 'synthetic renderer failure' >&2\nexit 1\n")
      .expect("the failing Chrome stub writes");
    let mut permissions = fs::metadata(&chrome).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&chrome, permissions).unwrap();
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

  #[test]
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
