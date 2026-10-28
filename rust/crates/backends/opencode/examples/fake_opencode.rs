//! A stand-in for the opencode 2.0 binary, for the backend tests.
//!
//! `serve --port 0` speaks the shared server's HTTP surface on loopback: the routes the backend calls, one global SSE
//! stream, and a control route the test uses to inject events, drop the stream, or end the process. `run` prints the
//! NDJSON lines of a one-shot run.
//!
//! Behaviour comes from the JSON file named by FAKE_OC_SCRIPT: `prompts` maps a prompt's text to a step with `events`
//! (session events, sessionID filled in), `shells` (running shell commands the session starts) and `hold` (keep the
//! execution running until an interrupt). `release_status` makes DELETE /api/debug/location answer that status instead
//! of 204. `event_subscribe_delay` and `interrupt_event_delay` set the timing of the stream and of interrupt events,
//! and `run_lines` and `run_exit` script a one-shot run. FAKE_OC_VERSION sets the version that /api/info reports.
//! Every request is appended to the FAKE_OC_LOG file as one JSON line, tagged with the serving process's pid.
//!
//! The loaded locations are the directories a session create, a shell list or a model list named; GET
//! /api/debug/location lists them and DELETE /api/debug/location unloads one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// A query string's parameters, as `urllib.parse.parse_qs` returns them: each name maps to its values in order.
type Params = BTreeMap<String, Vec<String>>;

/// An SSE subscriber's queue. A `None` ends that subscriber's stream.
type Queue = UnboundedSender<Option<Value>>;

/// What the process reads from its environment once, at start.
struct Settings {
  script: Value,
  log: Option<PathBuf>,
  version: String,
}

impl Settings {
  fn from_env() -> Self {
    let script = match std::env::var_os("FAKE_OC_SCRIPT").filter(|path| !path.is_empty()) {
      Some(path) => {
        serde_json::from_slice(&fs::read(path).expect("the script file reads")).expect("the script is JSON")
      }
      None => json!({}),
    };
    Self {
      script,
      log: std::env::var_os("FAKE_OC_LOG").filter(|path| !path.is_empty()).map(PathBuf::from),
      version: std::env::var("FAKE_OC_VERSION").unwrap_or_else(|_| "2.0.32".to_string()),
    }
  }

  /// Appends `entry` to the request log as one JSON line, tagged with this process's pid.
  fn record(&self, mut entry: Value) {
    let Some(log) = &self.log else { return };
    entry["pid"] = json!(std::process::id());
    let mut file = OpenOptions::new().create(true).append(true).open(log).expect("the request log opens");
    writeln!(file, "{entry}").expect("the request log takes a line");
  }
}

/// The state that the serving process keeps between requests.
#[derive(Default)]
struct State {
  /// Each session's directory, by session id.
  sessions: HashMap<String, String>,
  shells: Vec<Value>,
  active: BTreeSet<String>,
  loaded: BTreeSet<String>,
  subscribers: Vec<Queue>,
  next_id: u64,
}

impl State {
  fn new_id(&mut self, prefix: &str) -> String {
    self.next_id += 1;
    format!("{prefix}_{}", self.next_id)
  }
}

struct Fake {
  settings: Settings,
  state: Mutex<State>,
}

impl Fake {
  fn lock(&self) -> MutexGuard<'_, State> {
    self.state.lock().expect("the fake's state is not poisoned")
  }

  fn emit(&self, event: Value) {
    for queue in &self.lock().subscribers {
      queue.send(Some(event.clone())).expect("a subscriber's queue is open while it is subscribed");
    }
  }

  /// A script key that holds a number of seconds, read as zero when the script does not set it.
  fn seconds(&self, key: &str) -> Duration {
    Duration::from_secs_f64(self.settings.script[key].as_f64().unwrap_or(0.0))
  }
}

async fn run_prompt(fake: Arc<Fake>, session_id: String, body: Value) {
  let step = body["text"]
    .as_str()
    .and_then(|text| fake.settings.script["prompts"].get(text))
    .cloned()
    .unwrap_or_else(|| json!({}));
  fake.settings.record(json!({
    "prompt_text": body["text"],
    "prompt_files": body["files"].as_array().map_or(0, Vec::len),
    "session": session_id,
  }));
  fake.lock().active.insert(session_id.clone());
  fake.emit(json!({"type": "session.execution.started", "data": {"sessionID": session_id}}));
  for _ in step["shells"].as_array().into_iter().flatten() {
    let mut state = fake.lock();
    let shell_id = state.new_id("sh");
    let directory = state.sessions[&session_id].clone();
    state.shells.push(json!({
      "id": shell_id,
      "status": "running",
      "metadata": {"sessionID": session_id},
      "directory": directory,
    }));
  }
  for mut event in step["events"].as_array().into_iter().flatten().cloned() {
    event["data"]["sessionID"] = json!(session_id);
    fake.emit(event);
  }
  if step["hold"].as_bool().unwrap_or(false) {
    return;
  }
  let finished = fake.lock().active.remove(&session_id);
  if finished {
    fake.emit(json!({"type": "session.execution.succeeded", "data": {"sessionID": session_id}}));
  }
}

async fn handle(fake: Arc<Fake>, socket: TcpStream) -> io::Result<()> {
  let (read, mut write) = socket.into_split();
  let mut reader = BufReader::new(read);
  let mut request_line = String::new();
  reader.read_line(&mut request_line).await?;
  let mut headers = BTreeMap::new();
  loop {
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let text = line.trim_end();
    if text.is_empty() {
      break;
    }
    if let Some((name, value)) = text.split_once(':') {
      headers.insert(name.trim().to_lowercase(), value.trim().to_string());
    }
  }
  let request_line = request_line.trim_end();
  let mut words = request_line.splitn(3, ' ');
  let method = words.next().expect("the request line names a method").to_string();
  let target = words.next().expect("the request line names a target").to_string();
  let length =
    headers.get("content-length").map_or(0, |length| length.parse::<usize>().expect("content-length is a number"));
  let body = if length == 0 {
    Value::Null
  } else {
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).expect("the request body is JSON")
  };
  let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
  let params = parse_query(query);
  fake.settings.record(json!({
    "method": method,
    "path": path,
    "query": params_json(&params),
    "body": body,
    "auth": headers.get("authorization").map_or("", String::as_str),
  }));

  if method == "GET" && path == "/api/event" {
    return event_stream(&fake, &mut write).await;
  }
  if path == "/__control" {
    return control(&fake, &mut write, &body).await;
  }
  let (status, payload) = route(&fake, &method, path, &params, &body)
    .unwrap_or_else(|| (404, Some(json!({"error": format!("no route {method} {path}")}))));
  let data = payload.map(|payload| payload.to_string()).unwrap_or_default();
  let response = format!(
    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
    data.len()
  );
  write.write_all(response.as_bytes()).await
}

/// The HTTP answer to a request: its status and its JSON body, which is absent for a status with no body. `None`
/// means that no route matches the request.
fn route(fake: &Arc<Fake>, method: &str, path: &str, params: &Params, body: &Value) -> Option<(u16, Option<Value>)> {
  let directory = param(params, "location[directory]");
  let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
  match (method, path) {
    ("GET", "/api/info") => Some((200, Some(json!({"version": fake.settings.version})))),
    ("GET", "/api/debug/location") => {
      let locations = fake.lock().loaded.iter().map(|directory| json!({"directory": directory})).collect();
      Some((200, Some(Value::Array(locations))))
    }
    ("DELETE", "/api/debug/location") => {
      let status = fake.settings.script["release_status"].as_u64().unwrap_or(204) as u16;
      if (200..300).contains(&status) {
        fake.lock().loaded.remove(&directory);
      }
      Some((status, None))
    }
    ("GET", "/api/model") => {
      fake.lock().loaded.insert(directory);
      let model = json!({
        "id": "fake-model",
        "providerID": "fakeprov",
        "limit": {"context": 200000, "output": 8000},
      });
      Some((200, Some(json!({"data": [model]}))))
    }
    ("POST", "/api/session") => {
      let directory = body["location"]["directory"].as_str().expect("a session create names its directory");
      let mut state = fake.lock();
      let session_id = state.new_id("ses");
      state.sessions.insert(session_id.clone(), directory.to_string());
      state.loaded.insert(directory.to_string());
      Some((200, Some(json!({"data": {"id": session_id}}))))
    }
    ("POST", "/api/location/reload") => Some((204, None)),
    ("GET", "/api/shell") => {
      let mut state = fake.lock();
      state.loaded.insert(directory.clone());
      let shells: Vec<Value> = state.shells.iter().filter(|shell| shell["directory"] == directory).cloned().collect();
      Some((200, Some(json!({"location": {"directory": directory}, "data": shells}))))
    }
    _ => match parts.as_slice() {
      ["api", "session", session_id, action @ ..] => {
        session_route(fake, method, session_id, action.first().copied(), body)
      }
      ["api", "shell", shell_id] if method == "DELETE" => {
        fake.lock().shells.retain(|shell| shell["id"] != *shell_id);
        Some((204, None))
      }
      _ => None,
    },
  }
}

/// The routes under `/api/session/{id}`. A session that was never created answers 404, and so does an action that
/// the session does not serve.
fn session_route(
  fake: &Arc<Fake>,
  method: &str,
  session_id: &str,
  action: Option<&str>,
  body: &Value,
) -> Option<(u16, Option<Value>)> {
  if !fake.lock().sessions.contains_key(session_id) {
    return Some((404, Some(json!({"_tag": "SessionNotFoundError", "sessionID": session_id}))));
  }
  match (method, action) {
    ("GET", None) => Some((200, Some(json!({"data": {"id": session_id}})))),
    ("PUT", Some("environment")) => Some((204, None)),
    ("POST", Some("prompt")) => {
      tokio::spawn(run_prompt(Arc::clone(fake), session_id.to_string(), body.clone()));
      let message_id = fake.lock().new_id("msg");
      Some((200, Some(json!({"data": {"id": message_id}}))))
    }
    ("POST", Some("interrupt")) => {
      let interrupted = fake.lock().active.remove(session_id);
      if interrupted {
        let event = json!({
          "type": "session.execution.interrupted",
          "data": {"sessionID": session_id, "reason": "user"},
        });
        let delay = fake.seconds("interrupt_event_delay");
        let fake = Arc::clone(fake);
        tokio::spawn(async move {
          tokio::time::sleep(delay).await;
          fake.emit(event);
        });
      }
      Some((200, Some(json!({"interrupted": interrupted}))))
    }
    _ => None,
  }
}

/// The global SSE stream: it answers its headers at once, then sends each event that the fake emits while it is
/// subscribed. Events emitted before the subscription delay ends are not delivered.
async fn event_stream(fake: &Arc<Fake>, write: &mut OwnedWriteHalf) -> io::Result<()> {
  write.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await?;
  tokio::time::sleep(fake.seconds("event_subscribe_delay")).await;
  let (queue, mut events) = mpsc::unbounded_channel();
  fake.lock().subscribers.push(queue.clone());
  queue.send(Some(json!({"type": "server.connected", "data": {}}))).expect("the new queue is open");
  let result = forward(write, &mut events).await;
  fake.lock().subscribers.retain(|subscriber| !subscriber.same_channel(&queue));
  result
}

async fn forward(write: &mut OwnedWriteHalf, events: &mut UnboundedReceiver<Option<Value>>) -> io::Result<()> {
  while let Some(Some(event)) = events.recv().await {
    write.write_all(format!("data: {event}\n\n").as_bytes()).await?;
  }
  Ok(())
}

/// The control route. It answers 204 first, then applies the keys the test sent: `activate` marks a session
/// running, `emit` sends an event, `drop_stream` ends every SSE stream, and `exit` ends the process with that status.
async fn control(fake: &Arc<Fake>, write: &mut OwnedWriteHalf, body: &Value) -> io::Result<()> {
  assert!(body.is_object(), "a control request carries a JSON object");
  write.write_all(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
  if let Some(session_id) = body.get("activate") {
    fake.lock().active.insert(session_id.as_str().expect("activate names a session").to_string());
  }
  if let Some(event) = body.get("emit") {
    fake.emit(event.clone());
  }
  if body["drop_stream"].as_bool().unwrap_or(false) {
    for subscriber in &fake.lock().subscribers {
      subscriber.send(None).expect("a subscriber's queue is open while it is subscribed");
    }
  }
  if let Some(status) = body.get("exit") {
    std::process::exit(status.as_i64().expect("exit names a status") as i32);
  }
  Ok(())
}

/// `serve`: prints the listening address, then answers each connection until the process is ended.
async fn serve(fake: Arc<Fake>) {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("the loopback port binds");
  let port = listener.local_addr().expect("the listening address reads").port();
  println!("server listening on http://127.0.0.1:{port}");
  io::stdout().flush().expect("the listening address is flushed");
  loop {
    let (socket, _) = listener.accept().await.expect("the listener accepts connections");
    let fake = Arc::clone(&fake);
    tokio::spawn(async move {
      if let Err(error) = handle(fake, socket).await {
        eprintln!("fake_opencode: {error}");
      }
    });
  }
}

/// `run`: records the one-shot's environment, prints the scripted lines, and exits with the scripted status.
fn run_once(settings: &Settings) {
  let data_home = PathBuf::from(std::env::var_os("XDG_DATA_HOME").expect("a one-shot sets XDG_DATA_HOME"));
  fs::create_dir_all(data_home.join("opencode/shell")).expect("the shell directory is created");
  let args: Vec<String> = std::env::args().skip(2).collect();
  settings.record(json!({
    "run_args": args,
    "xdg_data_home": data_home,
    "xdg_state_home": std::env::var("XDG_STATE_HOME").ok(),
    "xdg_cache_home": std::env::var("XDG_CACHE_HOME").ok(),
    "config": std::env::var("OPENCODE_CONFIG_CONTENT").ok(),
  }));
  let mut stdout = io::stdout().lock();
  for line in settings.script["run_lines"].as_array().into_iter().flatten() {
    writeln!(stdout, "{line}").expect("the run line prints");
  }
  stdout.flush().expect("the run output is flushed");
  std::process::exit(settings.script["run_exit"].as_i64().unwrap_or(0) as i32);
}

/// The value of `name`'s first occurrence, or "" when the query does not name it.
fn param(params: &Params, name: &str) -> String {
  params.get(name).and_then(|values| values.first()).cloned().unwrap_or_default()
}

/// `urllib.parse.parse_qs` with its default of dropping blank values.
fn parse_query(query: &str) -> Params {
  let mut params = Params::new();
  for pair in query.split('&') {
    let Some((name, value)) = pair.split_once('=') else { continue };
    if !value.is_empty() {
      params.entry(unquote(name)).or_default().push(unquote(value));
    }
  }
  params
}

fn params_json(params: &Params) -> Value {
  Value::Object(params.iter().map(|(name, values)| (name.clone(), json!(values))).collect())
}

/// `urllib.parse.unquote_plus`: a `+` is a space, and each `%XX` is the byte that its two hex digits name.
fn unquote(text: &str) -> String {
  let bytes = text.replace('+', " ").into_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut rest = bytes.as_slice();
  while let [first, after @ ..] = rest {
    match (*first, rest.get(1..3).and_then(hex_byte)) {
      (b'%', Some(byte)) => {
        decoded.push(byte);
        rest = &rest[3..];
      }
      _ => {
        decoded.push(*first);
        rest = after;
      }
    }
  }
  String::from_utf8_lossy(&decoded).into_owned()
}

/// The byte that two hex digits name, or `None` when either digit is not one.
fn hex_byte(digits: &[u8]) -> Option<u8> {
  let [high, low] = digits else { return None };
  let high = (*high as char).to_digit(16)?;
  let low = (*low as char).to_digit(16)?;
  Some((high * 16 + low) as u8)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let settings = Settings::from_env();
  let command = std::env::args().nth(1).expect("the command is serve or run");
  match command.as_str() {
    "serve" => serve(Arc::new(Fake { settings, state: Mutex::new(State::default()) })).await,
    "run" => run_once(&settings),
    _ => {
      eprintln!("fake_opencode: unknown command {command:?}");
      std::process::exit(1);
    }
  }
}
