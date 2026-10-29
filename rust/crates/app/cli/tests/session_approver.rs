//! The `session set-approver` and `session create --approver` process tests. Each runs the built `charliebot` against a
//! stub internal API on 127.0.0.1 and reads the request the CLI sent.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

/// One request the stub read.
#[derive(Clone)]
struct Recorded {
  method: String,
  path: String,
  body: Value,
}

/// The stub internal API: it records each request and answers 200 with the task's id and the approver it was sent.
struct StubApi {
  port: u16,
  requests: Arc<Mutex<Vec<Recorded>>>,
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl StubApi {
  fn start() -> StubApi {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    listener.set_nonblocking(true).expect("the listener goes nonblocking");
    let port = listener.local_addr().expect("the listener has an address").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
      let requests = Arc::clone(&requests);
      let stop = Arc::clone(&stop);
      std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream.set_nonblocking(false).expect("the connection goes blocking");
              stream.set_read_timeout(Some(Duration::from_secs(10))).expect("the read timeout is set");
              let request = read_request(&mut stream);
              let answer = json!({"id": request.path.rsplit('/').next(), "approver": request.body.get("approver")});
              requests.lock().expect("the request lock").push(request);
              write_response(&mut stream, &answer);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the stub's accept failed: {error}"),
          }
        }
      })
    };
    StubApi { port, requests, stop, thread: Some(thread) }
  }

  fn requests(&self) -> Vec<Recorded> {
    self.requests.lock().expect("the request lock").clone()
  }
}

impl Drop for StubApi {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(thread) = self.thread.take() {
      thread.join().expect("the stub thread ends");
    }
  }
}

fn read_request(stream: &mut TcpStream) -> Recorded {
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 1024];
  let head_end = loop {
    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
      break position;
    }
    let read = stream.read(&mut chunk).expect("the request head arrives");
    assert!(read > 0, "the client closed before the request head ended");
    buffer.extend_from_slice(&chunk[..read]);
  };
  let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
  let mut request_line = head.lines().next().expect("the request has a line").split(' ');
  let method = request_line.next().expect("the request names a method").to_string();
  let path = request_line.next().expect("the request names a path").to_string();
  let length: usize = head
    .to_ascii_lowercase()
    .lines()
    .find_map(|line| line.strip_prefix("content-length:").map(|value| value.trim().to_string()))
    .map_or(0, |value| value.parse().expect("Content-Length is a number"));
  let body_start = head_end + 4;
  while buffer.len() < body_start + length {
    let read = stream.read(&mut chunk).expect("the request body arrives");
    assert!(read > 0, "the client closed before the request body ended");
    buffer.extend_from_slice(&chunk[..read]);
  }
  let body = if length == 0 {
    Value::Null
  } else {
    serde_json::from_slice(&buffer[body_start..body_start + length]).expect("the request body is JSON")
  };
  Recorded { method, path, body }
}

fn write_response(stream: &mut TcpStream, answer: &Value) {
  let body = serde_json::to_vec(answer).expect("the answer serializes");
  let head = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
  stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body)).expect("the response is written");
}

/// A temp CHARLIEBOT_HOME whose config names the stub's port.
struct Sandbox {
  root: TempDir,
  home: PathBuf,
}

impl Sandbox {
  fn new(port: u16) -> Sandbox {
    let root = tempfile::tempdir().expect("a temp root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    fs::write(home.join("config.yaml"), format!("server:\n  port: {port}\n")).expect("the config is written");
    Sandbox { root, home }
  }

  /// Run `charliebot session ARGS` and return its exit code and stderr.
  fn run(&self, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .arg("session")
      .args(args)
      .current_dir(self.root.path())
      .env_clear()
      .env("CHARLIEBOT_HOME", &self.home)
      .env("HOME", self.root.path())
      .env("PATH", "/usr/bin:/bin")
      .stdin(Stdio::null())
      .output()
      .expect("the CLI starts");
    (output.status.code(), String::from_utf8_lossy(&output.stderr).into_owned())
  }
}

const TASK_ID: &str = "4f1c2a0e-7b1d-5c3e-9a2f-1d2e3f4a5b6c";
const PARENT_ID: &str = "9a8b7c6d-5e4f-5a3b-8c2d-1e0f9a8b7c6d";

#[test]
fn set_approver_patches_the_task_with_the_approver() {
  let stub = StubApi::start();
  let sandbox = Sandbox::new(stub.port);

  let (code, stderr) = sandbox.run(&["set-approver", TASK_ID, "parent"]);

  assert_eq!(code, Some(0), "{stderr}");
  let requests = stub.requests();
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].method, "PATCH");
  assert_eq!(requests[0].path, format!("/api/sessions/{TASK_ID}"));
  assert_eq!(requests[0].body, json!({"approver": "parent"}));
}

#[test]
fn set_approver_refuses_an_unknown_approver_before_any_request() {
  let stub = StubApi::start();
  let sandbox = Sandbox::new(stub.port);

  let (code, _stderr) = sandbox.run(&["set-approver", TASK_ID, "owner"]);

  assert_eq!(code, Some(2));
  assert!(stub.requests().is_empty());
}

#[test]
fn create_sends_the_approver_with_the_task_object() {
  let stub = StubApi::start();
  let sandbox = Sandbox::new(stub.port);
  let task_file = sandbox.root.path().join("task.json");
  fs::write(&task_file, json!({"goal": "deliver the page"}).to_string()).expect("the task file is written");

  let (code, stderr) = sandbox.run(&[
    "create",
    "--parent",
    PARENT_ID,
    "--profile",
    "manager",
    "--task-file",
    task_file.to_str().expect("the task path is UTF-8"),
    "--approver",
    "parent",
  ]);

  assert_eq!(code, Some(0), "{stderr}");
  let requests = stub.requests();
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].method, "POST");
  assert_eq!(requests[0].body["approver"], "parent");
  assert_eq!(requests[0].body["task_parent_id"], PARENT_ID);
  assert_eq!(requests[0].body["task"], json!({"goal": "deliver the page"}));
}
