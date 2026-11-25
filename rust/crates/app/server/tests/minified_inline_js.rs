//! The minified inline script check. The server runs twice on one session home, once with USE_MINIFIED_INLINE_JS unset
//! and once with 0, and headless Chrome loads the chat, events, diff and token-usage pages on each. A pair must
//! log the same error classes and page errors, and show the same DOM once the inline script bodies, the spinners'
//! animation delays, and the token page's read time and generation time, are left out. Each page's inline scripts must
//! also differ between
//! the two servers, which shows that the switch changed what the server served.
//!
//! The test is `#[ignore = "integration"]`, so the rust-integration job runs it with CHARLIEBOT_TEST_CHROME_BIN set to
//! the Chrome it finds. It panics when that variable or the server binary is missing, and it never skips.

use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use regex::Regex;
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client};

const ACCESS_KEY: &str = "minified-inline-js-check-key";
const SESSION_ID: &str = "3f1c2d4e-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
const SESSION_NAME: &str = "Minified inline script check";
const USER_TEXT: &str = "Minified check question about the table.";
const ANSWER_TEXT: &str = "Minified check answer with bold text.";
const CHROME_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_TIMEOUT: Duration = Duration::from_secs(60);
const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a loaded page runs before its console, exceptions and DOM are read.
const PAGE_SETTLE: Duration = Duration::from_secs(3);

static UUID: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").expect("the UUID pattern compiles")
});
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("the digit pattern compiles"));
static ANIMATION_DELAY: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r#"animation-delay:[^;"]*;?"#).expect("the animation-delay pattern compiles"));
/// The token page's read time, in the DOM and in the page text. The template writes a thin space before the unit, so
/// the pattern takes any one space; the optional bold tags keep the DOM's form.
static READ_TIME: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"Read from the ledger in (<b>)?[0-9.]+\ss(</b>)?").expect("the read-time pattern compiles")
});
/// The token page's generation time, which moves when two loads straddle a minute.
static GENERATED_AT: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"generated · [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} [A-Z]+")
    .expect("the generation-time pattern compiles")
});

/// A message with its numbers and UUIDs replaced, so a class names the message and not one run's ids or timings.
fn normalize(text: &str) -> String {
  DIGITS.replace_all(&UUID.replace_all(text, "<uuid>"), "<n>").into_owned()
}

/// A page's text or DOM with the token page's clocks replaced, so two loads a few seconds apart compare equal.
fn without_clocks(page: &str) -> String {
  let read = READ_TIME.replace_all(page, "Read from the ledger in ${1}T s${2}");
  GENERATED_AT.replace_all(&read, "generated · T").into_owned()
}

fn config_text(port: u16) -> String {
  format!(
    "server:\n  host: 127.0.0.1\n  port: {port}\n  session_memory_max_mb: 0\n  session_swap_max_mb: 0\n  session_tasks_max: 0\n  min_free_disk_gib: 0\n  enable_local_work_guard: false\nbackends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n    - id: claude-opus\n      label: Synthetic Opus\n      type: cc-claude\n      model: claude-opus-5\n    - id: claude-fable\n      label: Synthetic Fable\n      type: cc-claude\n      model: claude-fable-5-1\n  calls:\n    new_session: claude-synthetic\n"
  )
}

/// One session the server lists and opens: its metadata and two chat events.
fn write_session(home: &Path) {
  let session = home.join("sessions").join(SESSION_ID);
  let data = session.join("data");
  fs::create_dir_all(&data).expect("the session's data directory is created");
  let metadata = json!({
    "id": SESSION_ID,
    "name": SESSION_NAME,
    "status": "active",
    "profile": "manager",
    "backend": "claude-synthetic",
    "created_at": "2026-03-13T14:04:00.000000Z",
    "updated_at": "2026-03-13T14:04:01.000000Z",
    "schema_version": 2,
    "has_unread": false,
    "starred": false,
  });
  fs::write(session.join("metadata.json"), metadata.to_string()).expect("the session metadata is written");
  let events = [
    json!({"type": "user", "content": USER_TEXT, "timestamp": "2026-03-13T14:04:00Z"}),
    json!({"type": "assistant", "message": {"content": ANSWER_TEXT}, "timestamp": "2026-03-13T14:04:01Z"}),
  ];
  let lines: String = events.iter().map(|event| format!("{event}\n")).collect();
  fs::write(data.join("chat_events.jsonl"), lines).expect("the chat events are written");
}

fn free_port() -> u16 {
  TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
    .expect("a loopback port is free")
    .local_addr()
    .expect("the listener has an address")
    .port()
}

/// A server process on its own home. It is killed when the test ends.
struct Server {
  child: Child,
  port: u16,
}

impl Server {
  fn start(root: &Path, mode: &str, server_bin: &Path, repo_root: &Path, minified_switch: Option<&str>) -> Self {
    let home = root.join(mode);
    fs::create_dir_all(&home).expect("the server home is created");
    write_session(&home);
    let port = free_port();
    fs::write(home.join("config.yaml"), config_text(port)).expect("the config is written");
    fs::write(home.join("credentials.yaml"), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
      .expect("the credentials are written");
    let log = File::create(root.join(format!("{mode}-server.log"))).expect("the server log is created");
    let mut command = Command::new(server_bin);
    command
      .current_dir(repo_root)
      .env_clear()
      .env("PATH", "/usr/bin:/bin")
      .env("HOME", root)
      .env("CHARLIEBOT_HOME", &home)
      .env("CHARLIEBOT_REPO_ROOT", repo_root)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::from(log));
    if let Some(value) = minified_switch {
      command.env("USE_MINIFIED_INLINE_JS", value);
    }
    let child = command.spawn().expect("the server process starts");
    let mut server = Self { child, port };
    server.wait_until_listening();
    server
  }

  fn wait_until_listening(&mut self) {
    let deadline = Instant::now() + SERVER_TIMEOUT;
    loop {
      if TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).is_ok() {
        return;
      }
      if let Some(status) = self.child.try_wait().expect("the server status can be read") {
        panic!("the server exited before it listened: {status}");
      }
      assert!(Instant::now() < deadline, "the server did not listen on port {} within {SERVER_TIMEOUT:?}", self.port);
      thread::sleep(Duration::from_millis(50));
    }
  }

  fn base(&self) -> String {
    format!("http://127.0.0.1:{}", self.port)
  }
}

impl Drop for Server {
  fn drop(&mut self) {
    if self.child.try_wait().expect("the server status can be read").is_none() {
      let _ = self.child.kill();
      let _ = self.child.wait();
    }
  }
}

/// The DevTools protocol over one WebSocket. Events that arrive while a call waits are kept in the queue.
struct Cdp {
  socket: WebSocket<TcpStream>,
  next_id: u64,
  queued: VecDeque<Value>,
}

impl Cdp {
  fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> Value {
    let id = self.next_id;
    self.next_id += 1;
    let mut message = json!({"id": id, "method": method, "params": params});
    if let Some(session) = session {
      message["sessionId"] = json!(session);
    }
    self.socket.send(Message::Text(message.to_string().into())).expect("the DevTools call writes");
    let deadline = Instant::now() + PAGE_LOAD_TIMEOUT;
    loop {
      let value = self
        .read_until(deadline)
        .unwrap_or_else(|| panic!("the DevTools call {method} got no answer within {PAGE_LOAD_TIMEOUT:?}"));
      if value.get("id").and_then(Value::as_u64) == Some(id) {
        if let Some(error) = value.get("error") {
          panic!("the DevTools call {method} failed: {error}");
        }
        return value.get("result").cloned().unwrap_or(Value::Null);
      }
      self.queued.push_back(value);
    }
  }

  /// The next message from the socket, or `None` when the deadline passes first.
  fn read_until(&mut self, deadline: Instant) -> Option<Value> {
    loop {
      let remaining = deadline.checked_duration_since(Instant::now())?;
      self
        .socket
        .get_mut()
        .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
        .expect("the socket accepts a read timeout");
      match self.socket.read() {
        Ok(Message::Text(text)) => {
          return Some(serde_json::from_str(text.as_str()).expect("a DevTools message is JSON"));
        }
        Ok(_) => continue,
        Err(tungstenite::Error::Io(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
          return None;
        }
        Err(error) => panic!("the DevTools socket failed: {error}"),
      }
    }
  }

  /// Keeps reading events until the deadline, so the queue holds what the page did in that time.
  fn settle(&mut self, duration: Duration) {
    let deadline = Instant::now() + duration;
    while let Some(value) = self.read_until(deadline) {
      self.queued.push_back(value);
    }
  }
}

fn is_event(value: &Value, method: &str, session: &str) -> bool {
  value.get("method").and_then(Value::as_str) == Some(method)
    && value.get("sessionId").and_then(Value::as_str) == Some(session)
}

/// A headless Chrome with one page target. It is killed with its process group when the test ends.
struct Browser {
  child: Child,
  cdp: Cdp,
  session: String,
}

impl Browser {
  fn launch(chrome: &Path, root: &Path) -> Self {
    let profile = root.join("chrome-profile");
    fs::create_dir_all(&profile).expect("the Chrome profile directory is created");
    let stderr = File::create(root.join("chrome.log")).expect("the Chrome log is created");
    let child = Command::new(chrome)
      .args(["--headless", "--disable-gpu", "--no-sandbox", "--no-first-run", "--remote-debugging-port=0"])
      .arg(format!("--user-data-dir={}", profile.display()))
      .arg("about:blank")
      .stdout(Stdio::null())
      .stderr(Stdio::from(stderr))
      // The browser leads its own process group, so the kill reaches the processes Chrome forks.
      .process_group(0)
      .spawn()
      .unwrap_or_else(|error| panic!("Chrome at {} does not start: {error}", chrome.display()));
    let port_file = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + CHROME_TIMEOUT;
    let (port, path) = loop {
      if let Ok(text) = fs::read_to_string(&port_file) {
        let mut lines = text.lines();
        if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
          break (port.to_owned(), path.to_owned());
        }
      }
      assert!(Instant::now() < deadline, "Chrome wrote no {} within {CHROME_TIMEOUT:?}", port_file.display());
      thread::sleep(Duration::from_millis(50));
    };
    let stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("the DevTools port is a number")))
      .expect("the DevTools port accepts a connection");
    stream.set_read_timeout(Some(CHROME_TIMEOUT)).expect("the socket accepts a timeout");
    stream.set_write_timeout(Some(CHROME_TIMEOUT)).expect("the socket accepts a timeout");
    let url = format!("ws://127.0.0.1:{port}{path}");
    let request = url.as_str().into_client_request().expect("the DevTools URL is a request");
    let (socket, _) = client(request, stream).expect("the DevTools WebSocket handshake succeeds");
    let mut cdp = Cdp { socket, next_id: 1, queued: VecDeque::new() };
    let target = cdp.call(None, "Target.createTarget", json!({"url": "about:blank"}));
    let target_id = target["targetId"].as_str().expect("the target has an id").to_owned();
    let attached = cdp.call(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true}));
    let session = attached["sessionId"].as_str().expect("the attached target has a session").to_owned();
    for method in ["Page.enable", "Runtime.enable", "Log.enable", "Network.enable"] {
      cdp.call(Some(&session), method, json!({}));
    }
    // The chat page loads its math renderer from this CDN on demand. Blocking it makes each load fail the same way,
    // whatever the network's speed, so the two loads of a page see the same scripts and the same errors.
    cdp.call(Some(&session), "Network.setBlockedURLs", json!({"urls": ["https://cdn.jsdelivr.net/*"]}));
    cdp.call(
      Some(&session),
      "Emulation.setDeviceMetricsOverride",
      json!({"width": 1536, "height": 900, "deviceScaleFactor": 1, "mobile": false}),
    );
    Self { child, cdp, session }
  }

  /// Navigates and returns once the page's load event has fired for this navigation.
  fn navigate(&mut self, url: &str) {
    self.cdp.queued.retain(|value| !is_event(value, "Page.loadEventFired", &self.session));
    self.cdp.call(Some(&self.session), "Page.navigate", json!({"url": url}));
    let deadline = Instant::now() + PAGE_LOAD_TIMEOUT;
    loop {
      if let Some(position) =
        self.cdp.queued.iter().position(|value| is_event(value, "Page.loadEventFired", &self.session))
      {
        self.cdp.queued.remove(position);
        return;
      }
      let value = self
        .cdp
        .read_until(deadline)
        .unwrap_or_else(|| panic!("{url} did not finish loading within {PAGE_LOAD_TIMEOUT:?}"));
      self.cdp.queued.push_back(value);
    }
  }

  /// Loads one page of a server. The cookie opens the navigations; the key in localStorage is what the page's own
  /// requests send, as the chat page reads it from its origin.
  fn load(&mut self, base: &str, path: &str) -> PageView {
    self.cdp.call(
      Some(&self.session),
      "Network.setCookie",
      json!({"name": "charliebot_access_key", "value": ACCESS_KEY, "url": base}),
    );
    // A static file runs no page script. The root page would redirect to a session view and open its socket before
    // the key is in localStorage, and that socket's refusal would land in the page's error classes.
    self.navigate(&format!("{base}/static/css/styles.css"));
    self.cdp.call(
      Some(&self.session),
      "Runtime.evaluate",
      json!({"expression": format!("localStorage.setItem('charliebot_access_key', {});", json!(ACCESS_KEY))}),
    );
    // The events of the page are the ones after this point: the origin's own load is not the page under test.
    self.cdp.queued.clear();
    self.navigate(&format!("{base}{path}"));
    self.cdp.settle(PAGE_SETTLE);
    let evaluated = self.cdp.call(
      Some(&self.session),
      "Runtime.evaluate",
      json!({"expression": PAGE_STATE_SCRIPT, "returnByValue": true}),
    );
    assert!(
      evaluated.get("exceptionDetails").is_none(),
      "{path}: reading the page state threw: {}",
      evaluated["exceptionDetails"]
    );
    let state: Value = serde_json::from_str(evaluated["result"]["value"].as_str().expect("the page state is a string"))
      .expect("the page state is JSON");
    PageView::new(&self.cdp.queued, &self.session, state)
  }
}

impl Drop for Browser {
  fn drop(&mut self) {
    let _ = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
    let _ = self.child.wait();
  }
}

/// The page's document with each inline script's text removed, the text of those scripts, the title and the body text.
const PAGE_STATE_SCRIPT: &str = r#"(() => {
  const copy = document.documentElement.cloneNode(true);
  const scripts = [];
  document.querySelectorAll('script').forEach((script) => scripts.push(script.textContent));
  copy.querySelectorAll('script').forEach((script) => {
    script.textContent = '';
  });
  return JSON.stringify({
    dom: copy.outerHTML,
    scripts: scripts.join('\n'),
    title: document.title,
    text: document.body ? document.body.innerText : '',
  });
})()"#;

/// What one page load showed: its error classes and page errors, and its DOM with the parts that differ between two
/// loads of the same page left out.
#[derive(Debug)]
struct PageView {
  error_classes: BTreeSet<String>,
  page_errors: BTreeSet<String>,
  inline_scripts: String,
  title: String,
  text: String,
  dom: String,
}

impl PageView {
  fn new(events: &VecDeque<Value>, session: &str, state: Value) -> Self {
    let mut error_classes = BTreeSet::new();
    let mut page_errors = BTreeSet::new();
    for event in events.iter().filter(|value| value.get("sessionId").and_then(Value::as_str) == Some(session)) {
      let params = &event["params"];
      match event["method"].as_str() {
        Some("Runtime.consoleAPICalled") => {
          let kind = params["type"].as_str().unwrap_or_default();
          if matches!(kind, "error" | "warning" | "assert") {
            let text = params["args"].as_array().into_iter().flatten().map(argument_text).collect::<Vec<_>>().join(" ");
            error_classes.insert(normalize(&format!("console.{kind}: {text}")));
          }
        }
        Some("Runtime.exceptionThrown") => {
          let details = &params["exceptionDetails"];
          let text = details["exception"]["description"].as_str().or_else(|| details["text"].as_str());
          let first_line = text.unwrap_or_default().lines().next().unwrap_or_default();
          page_errors.insert(normalize(first_line));
        }
        Some("Log.entryAdded") => {
          let entry = &params["entry"];
          let level = entry["level"].as_str().unwrap_or_default();
          if matches!(level, "error" | "warning") {
            let text = entry["text"].as_str().unwrap_or_default();
            let url = entry["url"].as_str().unwrap_or_default();
            error_classes.insert(normalize(&format!("log.{level}: {text} {url}")));
          }
        }
        _ => {}
      }
    }
    let dom = ANIMATION_DELAY.replace_all(state["dom"].as_str().expect("the DOM is a string"), "");
    let dom = without_clocks(&dom);
    let text = without_clocks(state["text"].as_str().unwrap_or_default());
    Self {
      error_classes,
      page_errors,
      inline_scripts: state["scripts"].as_str().expect("the inline scripts are a string").to_owned(),
      title: state["title"].as_str().unwrap_or_default().to_owned(),
      text,
      dom,
    }
  }
}

/// A console argument as text: its value when it has one, else its description.
fn argument_text(argument: &Value) -> String {
  match argument.get("value") {
    Some(Value::String(text)) => text.clone(),
    Some(value) => value.to_string(),
    None => argument["description"].as_str().unwrap_or_default().to_owned(),
  }
}

/// The first byte where two strings part, with the text around it from each.
fn first_difference(on: &str, off: &str) -> String {
  let at = on.bytes().zip(off.bytes()).position(|(a, b)| a != b).unwrap_or(on.len().min(off.len()));
  let excerpt = |text: &str| {
    let start = at.saturating_sub(120);
    let end = (at + 120).min(text.len());
    String::from_utf8_lossy(&text.as_bytes()[start..end]).into_owned()
  };
  format!("at byte {at}\n  on:  {}\n  off: {}", excerpt(on), excerpt(off))
}

/// The scripts with a server's home path replaced, so two servers' scripts compare by their text alone.
fn without_home(scripts: &str, home: &Path) -> String {
  let home = home.to_str().expect("the test home path is UTF-8");
  scripts.replace(home, "<home>")
}

fn chrome_binary() -> PathBuf {
  let chrome = env::var_os("CHARLIEBOT_TEST_CHROME_BIN")
    .map(PathBuf::from)
    .expect("CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome this check drives");
  assert!(chrome.is_file(), "CHARLIEBOT_TEST_CHROME_BIN names no file: {}", chrome.display());
  chrome
}

#[test]
#[ignore = "integration"]
fn the_minified_inline_scripts_render_each_page_as_the_source_scripts_do() {
  let chrome = chrome_binary();
  let server_bin = PathBuf::from(env!("CARGO_BIN_EXE_charliebot-server"));
  assert!(server_bin.is_file(), "the server binary is missing: {}", server_bin.display());
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let root = tempfile::Builder::new()
    .prefix("minified-inline-js-")
    .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
    .expect("the test directory is created");

  let on = Server::start(root.path(), "on", &server_bin, &repo_root, None);
  let off = Server::start(root.path(), "off", &server_bin, &repo_root, Some("0"));
  let mut browser = Browser::launch(&chrome, root.path());

  let pages = [
    ("chat", format!("/?session={SESSION_ID}")),
    ("events", format!("/sessions/{SESSION_ID}/events")),
    ("diff", "/diff".to_owned()),
    ("token-usage", "/token-usage".to_owned()),
  ];
  let (on_home, off_home) = (root.path().join("on"), root.path().join("off"));
  for (name, path) in pages {
    let on_view = browser.load(&on.base(), &path);
    let off_view = browser.load(&off.base(), &path);
    // Each server's home path is in its page's scripts, and the two paths differ, so the paths are replaced before the
    // scripts are compared. Only the switch can then make them differ.
    let on_scripts = without_home(&on_view.inline_scripts, &on_home);
    let off_scripts = without_home(&off_view.inline_scripts, &off_home);
    assert!(
      on_scripts != off_scripts,
      "{name}: the switch changed no inline script ({} bytes on each side)",
      on_scripts.len()
    );
    assert_eq!(on_view.error_classes, off_view.error_classes, "{name}: console error classes differ");
    assert_eq!(on_view.page_errors, off_view.page_errors, "{name}: page errors differ");
    assert!(on_view.dom == off_view.dom, "{name}: the DOM differs {}", first_difference(&on_view.dom, &off_view.dom));
    assert!(
      on_view.text == off_view.text,
      "{name}: the page text differs {}",
      first_difference(&on_view.text, &off_view.text)
    );
    assert_page_shows_its_content(name, &on_view);
  }
}

/// Each page shows its own content, so a pair that matches because both pages failed to load does not pass.
fn assert_page_shows_its_content(name: &str, view: &PageView) {
  let (title, marker) = match name {
    "chat" => (SESSION_NAME, USER_TEXT),
    "events" => ("Events", USER_TEXT),
    "diff" => ("Diff Viewer", "Open in code-server"),
    "token-usage" => ("Token usage", "Read from the ledger"),
    other => panic!("no content check is set for {other}"),
  };
  assert!(view.title.contains(title), "{name}: the title {:?} does not name {title:?}", view.title);
  assert!(view.dom.contains(marker), "{name}: the DOM does not carry {marker:?}");
}
