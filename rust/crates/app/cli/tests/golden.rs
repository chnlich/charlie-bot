//! The golden-case harness: replay the recorded Python CLI behavior against the built Rust binary.
//!
//! Every case under `tests/golden/<subcommand>/<case>.json` runs once per test binary invocation.
//! Without `CHARLIEBOT_CLI_GOLDEN_RECORD` (the default) the case replays against the built
//! `charliebot` binary and every `expect` field is checked. With the variable set to the Python
//! CLI's entry point, the case records: the command runs instead and the case file's `expect`
//! fields are rewritten from what it produced. `README.md` documents both commands.
//!
//! Each CLI process gets a cleared environment, a temp `CHARLIEBOT_HOME`, and a 20 s timeout; the
//! stub HTTP server binds `127.0.0.1` port 0, so no case can reach the live server or home.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value, json};

/// How long one CLI process may run before the test kills it and fails the case.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

/// The checkout root, for `{repo}` in the record command: the crate sits at `rust/crates/app/cli`.
const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../..");

const CASES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

fn uuid4_regex() -> &'static Regex {
  static UUID4: OnceLock<Regex> = OnceLock::new();
  UUID4.get_or_init(|| Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}").unwrap())
}

#[test]
fn golden_cases() {
  let record_command = std::env::var("CHARLIEBOT_CLI_GOLDEN_RECORD").ok();
  let mut failures = Vec::new();
  let mut cases = 0;
  for path in case_files() {
    cases += 1;
    if let Err(message) = run_case(&path, record_command.as_deref()) {
      failures.push(format!("{}: {message}", path.display()));
    }
  }
  if record_command.is_some() {
    println!("recorded {cases} golden case(s)");
  } else {
    println!("replayed {cases} golden case(s)");
  }
  assert!(failures.is_empty(), "{} golden case(s) failed:\n{}", failures.len(), failures.join("\n"));
}

fn case_files() -> Vec<PathBuf> {
  let mut files = Vec::new();
  let directories = std::fs::read_dir(CASES_DIR).expect("tests/golden exists");
  for directory in directories {
    let directory = directory.expect("readable").path();
    if !directory.is_dir() {
      continue;
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
      .expect("readable")
      .map(|entry| entry.expect("readable").path())
      .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
      .collect();
    paths.sort();
    files.extend(paths);
  }
  files.sort();
  files
}

// ---------------------------------------------------------------------------
// The stub server
// ---------------------------------------------------------------------------

/// One request the stub received, in the shape `expect.requests` records.
#[derive(Debug, Clone)]
struct RecordedRequest {
  method: String,
  path: String,
  query: String,
  authorization: Option<String>,
  caller_session: Option<String>,
  content_type: Option<String>,
  body: Value,
}

impl RecordedRequest {
  fn to_value(&self) -> Value {
    json!({
      "method": self.method,
      "path": self.path,
      "query": self.query,
      "headers": {
        "Authorization": self.authorization,
        "X-CharlieBot-Caller-Session": self.caller_session,
        "Content-Type": self.content_type,
      },
      "body": self.body,
    })
  }
}

enum ScriptedResponse {
  Answer { status: u16, body: Value },
  DropAfterRequest,
}

struct StubServer {
  port: u16,
  requests: Arc<Mutex<Vec<RecordedRequest>>>,
  stop: Arc<AtomicBool>,
}

impl StubServer {
  /// The port with no listener: `refuse_connect` cases reserve one and release it.
  fn refused_port() -> StubServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port binds");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    StubServer { port, requests: Arc::new(Mutex::new(Vec::new())), stop: Arc::new(AtomicBool::new(false)) }
  }

  fn stop(&self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

fn start_stub(responses: &[Value]) -> StubServer {
  let refuse_connect = responses.iter().any(|response| response.get("refuse_connect").is_some());
  if refuse_connect {
    return StubServer::refused_port();
  }
  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port binds");
  let port = listener.local_addr().unwrap().port();
  let mut scripted: VecDeque<ScriptedResponse> = responses
    .iter()
    .map(|response| {
      if response.get("drop_after_request").is_some() {
        ScriptedResponse::DropAfterRequest
      } else {
        ScriptedResponse::Answer {
          status: response["status"].as_u64().unwrap_or(200) as u16,
          body: response["body"].clone(),
        }
      }
    })
    .collect();
  let requests = Arc::new(Mutex::new(Vec::new()));
  let stop = Arc::new(AtomicBool::new(false));
  let thread_requests = requests.clone();
  let thread_stop = stop.clone();
  std::thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    loop {
      match listener.accept() {
        Ok((mut stream, _)) => {
          if let Some(request) = read_request(&mut stream) {
            thread_requests.lock().unwrap().push(request);
            match scripted.pop_front() {
              Some(ScriptedResponse::Answer { status, body }) => write_response(&mut stream, status, &body),
              Some(ScriptedResponse::DropAfterRequest) => {}
              None => write_response(
                &mut stream,
                500,
                &json!({ "detail": "the stub has no scripted response for this request" }),
              ),
            }
          }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          if thread_stop.load(Ordering::Relaxed) {
            break;
          }
          std::thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  });
  StubServer { port, requests, stop }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 4096];
  let header_end = loop {
    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
      break position;
    }
    let read = stream.read(&mut chunk).ok()?;
    if read == 0 {
      return None;
    }
    buffer.extend_from_slice(&chunk[..read]);
  };
  let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
  let mut lines = head.split("\r\n");
  let mut request_line = lines.next()?.split(' ');
  let method = request_line.next()?.to_string();
  let target = request_line.next()?.to_string();
  let (path, query) = match target.split_once('?') {
    Some((path, query)) => (path.to_string(), query.to_string()),
    None => (target, String::new()),
  };
  let mut headers = Vec::new();
  for line in lines {
    if let Some((name, value)) = line.split_once(':') {
      headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
  }
  let header_value = |name: &str| headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
  let content_length = header_value("content-length").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
  let body_start = header_end + 4;
  while buffer.len() < body_start + content_length {
    let read = stream.read(&mut chunk).ok()?;
    if read == 0 {
      break;
    }
    buffer.extend_from_slice(&chunk[..read]);
  }
  let body_end = (body_start + content_length).min(buffer.len());
  let body = if body_end > body_start {
    serde_json::from_slice(&buffer[body_start..body_end]).unwrap_or(Value::Null)
  } else {
    Value::Null
  };
  Some(RecordedRequest {
    method,
    path,
    query,
    authorization: header_value("authorization"),
    caller_session: header_value("x-charliebot-caller-session"),
    content_type: header_value("content-type"),
    body,
  })
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) {
  let body_bytes = serde_json::to_vec(body).expect("the scripted body serializes");
  let response = format!(
    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    status,
    reason_phrase(status),
    body_bytes.len()
  );
  let _ = stream.write_all(response.as_bytes());
  let _ = stream.write_all(&body_bytes);
  let _ = stream.flush();
}

fn reason_phrase(status: u16) -> &'static str {
  match status {
    200 => "OK",
    400 => "Bad Request",
    401 => "Unauthorized",
    403 => "Forbidden",
    404 => "Not Found",
    409 => "Conflict",
    422 => "Unprocessable Entity",
    500 => "Internal Server Error",
    _ => "Status",
  }
}

// ---------------------------------------------------------------------------
// The case runner
// ---------------------------------------------------------------------------

fn run_case(case_path: &Path, record_command: Option<&str>) -> Result<(), String> {
  let case_text = std::fs::read_to_string(case_path).map_err(|error| error.to_string())?;
  let case: Value = serde_json::from_str(&case_text).map_err(|error| format!("invalid case JSON: {error}"))?;
  let temp = tempfile::tempdir().map_err(|error| error.to_string())?;
  let root = temp.path().to_str().ok_or("the temp root is not Unicode")?.to_string();

  // A case with `expect_from: "rust"` records from the Rust binary: its contract is the task
  // text's own spec (the dispatcher vocabulary differs from the Python CLI's by design), so a
  // Python record would bake the Python-only output in.
  let expect_from_rust = case.get("expect_from").and_then(Value::as_str) == Some("rust");
  let recording = record_command.is_some();
  let responses = case.get("responses").and_then(Value::as_array).cloned().unwrap_or_default();
  let stub = start_stub(&responses);
  let substitute = |text: &str| text.replace("{root}", &root).replace("{port}", &stub.port.to_string());

  if let Some(files) = case.get("files").and_then(Value::as_object) {
    for (relative, content) in files {
      let path = temp.path().join(substitute(relative));
      if relative.ends_with('/') {
        std::fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        continue;
      }
      let text = substitute(content.as_str().ok_or("file contents must be strings")?);
      if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
      }
      std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    }
  }

  let cwd = temp.path().join(substitute(case.get("cwd").and_then(Value::as_str).unwrap_or(".")));
  std::fs::create_dir_all(&cwd).map_err(|error| format!("{}: {error}", cwd.display()))?;
  let argv: Vec<String> = case
    .get("argv")
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default()
    .iter()
    .map(|item| item.as_str().ok_or("argv entries must be strings"))
    .collect::<Result<Vec<&str>, _>>()
    .map_err(|message| message.to_string())?
    .iter()
    .map(|item| substitute(item))
    .collect();

  let mut command = match (recording, expect_from_rust) {
    (true, false) => {
      // A relative record command resolves against the checkout root, so the invocation does not
      // bake one host's tree path in; the child's cwd is the case's cwd.
      let record = record_command.expect("recording").replace("{repo}", REPO_ROOT);
      let program = if Path::new(&record).is_absolute() { record } else { format!("{REPO_ROOT}/{record}") };
      Command::new(program)
    }
    _ => Command::new(env!("CARGO_BIN_EXE_charliebot")),
  };
  command
    .args(&argv)
    .current_dir(&cwd)
    .env_clear()
    .env("CHARLIEBOT_HOME", temp.path().join("home"))
    .env("CHARLIEBOT_REPO_ROOT", temp.path().join("repo"))
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  if let Some(env) = case.get("env").and_then(Value::as_object) {
    for (key, value) in env {
      command.env(key, substitute(value.as_str().ok_or("env values must be strings")?));
    }
  }
  let mut child = command.spawn().map_err(|error| format!("spawn failed: {error}"))?;

  let started = Instant::now();
  let status = loop {
    match child.try_wait().map_err(|error| error.to_string())? {
      Some(status) => break Some(status),
      None if started.elapsed() > PROCESS_TIMEOUT => {
        let _ = child.kill();
        let _ = child.wait();
        break None;
      }
      None => std::thread::sleep(Duration::from_millis(10)),
    }
  };
  let Some(status) = status else {
    stub.stop();
    return Err(format!("the CLI process exceeded {} s and was killed", PROCESS_TIMEOUT.as_secs()));
  };
  let mut stdout_bytes = Vec::new();
  let _ = child.stdout.take().expect("stdout piped").read_to_end(&mut stdout_bytes);
  let mut stderr_bytes = Vec::new();
  let _ = child.stderr.take().expect("stderr piped").read_to_end(&mut stderr_bytes);
  stub.stop();

  let requests: Vec<Value> =
    stub.requests.lock().expect("the stub lock").iter().map(RecordedRequest::to_value).collect();
  let scripted =
    if responses.iter().any(|response| response.get("refuse_connect").is_some()) { 0 } else { responses.len() };
  if requests.len() > scripted {
    return Err(format!("the run made {} request(s) but the case scripts {}", requests.len(), scripted));
  }

  let stdout = normalize(&String::from_utf8_lossy(&stdout_bytes), &root);
  let stderr = normalize(&String::from_utf8_lossy(&stderr_bytes), &root);
  let exit = status.code().ok_or("the CLI process exited from a signal")?;

  if recording {
    record_case(case_path, &case, &stdout, &stderr, exit, &requests)
  } else {
    replay_case(temp.path(), &case, &stdout, &stderr, exit, &requests)
  }
}

/// Rewrite the case's `expect` fields from what the recorded command produced.
fn record_case(
  case_path: &Path,
  case: &Value,
  stdout: &str,
  stderr: &str,
  exit: i32,
  requests: &[Value],
) -> Result<(), String> {
  let mut updated = case.clone();
  let mut expect = Map::new();
  if let Some(existing) = case.get("expect").and_then(Value::as_object) {
    for (key, value) in existing {
      if !matches!(key.as_str(), "stdout" | "stderr" | "exit" | "requests") {
        expect.insert(key.clone(), value.clone());
      }
    }
  }
  expect.insert("stdout".to_string(), Value::String(replace_uuid4(stdout)));
  expect.insert("stderr".to_string(), Value::String(replace_uuid4(stderr)));
  expect.insert("exit".to_string(), json!(exit));
  expect.insert("requests".to_string(), Value::Array(requests.iter().map(uuid4_in_value).collect()));
  updated["expect"] = Value::Object(expect);

  let style = charliebot_core::pyfmt::JsonStyle {
    ensure_ascii: false,
    sort_keys: false,
    item_separator: ",",
    key_separator: ": ",
    indent: Some(2),
  };
  let text = format!("{}\n", charliebot_core::pyfmt::json_dumps(&updated, &style));
  std::fs::write(case_path, text).map_err(|error| format!("{}: {error}", case_path.display()))
}

fn replay_case(
  root: &Path,
  case: &Value,
  stdout: &str,
  stderr: &str,
  exit: i32,
  requests: &[Value],
) -> Result<(), String> {
  let expect = case.get("expect").ok_or("the case has no expect section")?;
  let parser_generated = case.get("parser_generated").and_then(Value::as_bool).unwrap_or(false);
  let expected_exit = expect.get("exit").and_then(Value::as_i64).ok_or("the case has no expect.exit")? as i32;
  if exit != expected_exit {
    return Err(format!("exit {exit}, expected {expected_exit}; stderr: {stderr}"));
  }
  if parser_generated {
    return Ok(());
  }
  for (name, actual, expected) in [("stdout", stdout, expect.get("stdout")), ("stderr", stderr, expect.get("stderr"))] {
    let expected = expected.and_then(Value::as_str).unwrap_or("");
    if !text_matches(expected, actual) {
      return Err(format!("{name} differs:\nexpected: {expected:?}\nactual:   {actual:?}"));
    }
  }
  let expected_requests = expect.get("requests").and_then(Value::as_array).cloned().unwrap_or_default();
  if requests.len() != expected_requests.len() {
    return Err(format!("the stub received {} request(s), expected {}", requests.len(), expected_requests.len()));
  }
  for (index, (expected, actual)) in expected_requests.iter().zip(requests).enumerate() {
    if !value_matches(expected, actual) {
      return Err(format!(
        "request {index} differs:\nexpected: {}\nactual:   {}",
        charliebot_core::pyfmt::json_dumps(expected, &indent_two_style()),
        charliebot_core::pyfmt::json_dumps(actual, &indent_two_style())
      ));
    }
  }
  if let Some(files) = expect.get("files").and_then(Value::as_object) {
    for (relative, expected_content) in files {
      let path = root.join(relative);
      let content = std::fs::read_to_string(path).map_err(|error| format!("expect.files {relative}: {error}"))?;
      if content != expected_content.as_str().unwrap_or_default() {
        return Err(format!("expect.files {relative}: content differs"));
      }
    }
  }
  Ok(())
}

fn indent_two_style() -> charliebot_core::pyfmt::JsonStyle {
  charliebot_core::pyfmt::JsonStyle {
    ensure_ascii: false,
    sort_keys: false,
    item_separator: ",",
    key_separator: ": ",
    indent: Some(2),
  }
}

/// The run's temp root reads as `{root}` so a case file does not bake one run's path in.
fn normalize(text: &str, root: &str) -> String {
  text.replace(root, "{root}")
}

fn replace_uuid4(text: &str) -> String {
  uuid4_regex().replace_all(text, "{uuid4}").into_owned()
}

/// Expected text with `{uuid4}` placeholders against actual text.
fn text_matches(expected: &str, actual: &str) -> bool {
  let mut rest = actual;
  let parts: Vec<&str> = expected.split("{uuid4}").collect();
  for (index, part) in parts.iter().enumerate() {
    if index + 1 == parts.len() {
      return rest == *part;
    }
    let Some(after) = rest.strip_prefix(part) else { return false };
    let Some(uuid) = uuid4_regex().find(after) else { return false };
    if uuid.start() != 0 {
      return false;
    }
    rest = &after[uuid.end()..];
  }
  true
}

/// Expected request values against actual ones; `{uuid4}` matches any UUID4 text.
fn value_matches(expected: &Value, actual: &Value) -> bool {
  match (expected, actual) {
    (Value::String(expected), Value::String(actual)) => text_matches(expected, actual),
    (Value::Array(expected), Value::Array(actual)) => {
      expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| value_matches(expected, actual))
    }
    (Value::Object(expected), Value::Object(actual)) => {
      expected.len() == actual.len()
        && expected.iter().all(|(key, value)| actual.get(key).is_some_and(|actual| value_matches(value, actual)))
    }
    (expected, actual) => expected == actual,
  }
}

fn uuid4_in_value(value: &Value) -> Value {
  match value {
    Value::String(text) => Value::String(replace_uuid4(text)),
    Value::Array(items) => Value::Array(items.iter().map(uuid4_in_value).collect()),
    Value::Object(members) => {
      Value::Object(members.iter().map(|(key, value)| (key.clone(), uuid4_in_value(value))).collect())
    }
    other => other.clone(),
  }
}

/// A changed `server.port` in config.yaml takes effect on the next call: the port is read from the
/// config on every call, and there is no port cache to serve a stale one (Python's
/// `test_port_cache_hit_serves_the_cached_port_in_a_fresh_process` has no Rust counterpart).
#[test]
fn changed_server_port_takes_effect_on_the_next_call() {
  let temp = tempfile::tempdir().unwrap();
  let config_path = temp.path().join("home").join("config.yaml");
  let run = |stub: &StubServer| {
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, format!("server:\n  port: {}\n", stub.port)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .args(["session", "tree"])
      .current_dir(temp.path())
      .env_clear()
      .env("CHARLIEBOT_HOME", temp.path().join("home"))
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .output()
      .unwrap();
    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stub.requests.lock().unwrap().len(), 1);
  };

  let first = start_stub(&[json!({ "status": 200, "body": { "name": "first" } })]);
  run(&first);
  first.stop();
  drop(first);

  let second = start_stub(&[json!({ "status": 200, "body": { "name": "second" } })]);
  run(&second);
  second.stop();
}
