//! The `charliebot hook pending-wake` process tests and the turn-end runner test.
//!
//! Each process test runs the built `charliebot` with a cleared environment and a synthetic CHARLIEBOT_HOME whose
//! config names a stub internal API on 127.0.0.1. The turn input goes on the process's stdin. The stub answers the
//! reads the check makes (the session tree, the pending triggers, and each session's directory), and every test asserts
//! that the check sent only GET requests.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use charliebot_tasks::turn_hooks::{Entry, Outcome, TurnInput, run_entries};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

/// How long one check process, or one stub connection, may take before the test fails.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
/// The session tree's default page size, which the stub serves.
const PAGE: usize = 100;
const ROOT: &str = "root-session";
const EXPECTED_BLOCK_REASON: &str = concat!(
  "pending-wake check: a turn that ends with the task open leaves the line in one of these states:\n",
  "- When the work can go on now, continue it in this turn or in a delegated task.\n",
  "- When a job, a restart or another session is still waited on, set a trigger that fires within four hours ",
  "and checks the awaited state.\n",
  "- When a wait on the user exists, list each one in Waiting on you.\n",
  "- When the work is delivered, send it with a close request.\n",
  "- When the line cannot go on, list one Waiting on you item that says the line has stopped, why, ",
  "and the reply that restarts it.",
);
const EXPECTED_NOTICE: &str =
  "pending-wake check: this turn left no wake, so the line has stopped. A reply restarts it.";

/// One open child row of the synthetic session tree.
#[derive(Clone)]
struct Row {
  id: String,
  work_state: &'static str,
  open_descendant_count: u64,
}

/// The synthetic sessions: the open children under each parent, each session's metadata and pending trigger count, and
/// the read failures to inject.
#[derive(Clone, Default)]
struct World {
  children: BTreeMap<String, Vec<Row>>,
  metadata: BTreeMap<String, Map<String, Value>>,
  triggers: BTreeMap<String, usize>,
  fail_tree: bool,
  fail_triggers: bool,
}

impl World {
  /// Adds an open child under `parent` in the given work state, with `open_descendants` open descendants.
  fn child(mut self, parent: &str, id: &str, work_state: &'static str, open_descendants: u64) -> Self {
    let row = Row { id: id.to_string(), work_state, open_descendant_count: open_descendants };
    self.children.entry(parent.to_string()).or_default().push(row);
    self
  }

  /// Sets the fields of a session's metadata.json, over its default metadata.
  fn metadata(mut self, id: &str, extra: Value) -> Self {
    let mut object = default_metadata(id);
    object.extend(extra.as_object().expect("the extra metadata is an object").clone());
    self.metadata.insert(id.to_string(), object);
    self
  }

  fn triggers(mut self, id: &str, count: usize) -> Self {
    self.triggers.insert(id.to_string(), count);
    self
  }

  /// Every session id the world names, the root included.
  fn session_ids(&self) -> Vec<String> {
    let mut ids = vec![ROOT.to_string()];
    for rows in self.children.values() {
      ids.extend(rows.iter().map(|row| row.id.clone()));
    }
    ids.extend(self.metadata.keys().cloned());
    ids.sort();
    ids.dedup();
    ids
  }

  /// The stub's answer to one GET, given its target (path and query).
  fn answer(&self, target: &str) -> (u16, Value) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params: BTreeMap<&str, &str> = query.split('&').filter_map(|pair| pair.split_once('=')).collect();
    if path == "/api/sessions/tree" {
      if self.fail_tree {
        return (500, json!({"detail": "synthetic tree failure"}));
      }
      let parent = params.get("parent_id").copied().unwrap_or_default();
      let start: usize = params.get("cursor").map_or(0, |cursor| cursor.parse().expect("the cursor is an offset"));
      let rows = self.children.get(parent).map_or(&[][..], Vec::as_slice);
      let end = (start + PAGE).min(rows.len());
      let items: Vec<Value> = rows[start..end].iter().map(row_json).collect();
      let next = if end < rows.len() { json!(end.to_string()) } else { Value::Null };
      return (200, json!({"items": items, "next_cursor": next}));
    }
    if let Some(id) = path.strip_prefix("/api/sessions/").and_then(|rest| rest.strip_suffix("/pending-triggers")) {
      if self.fail_triggers {
        return (500, json!({"detail": "synthetic trigger failure"}));
      }
      let count = self.triggers.get(id).copied().unwrap_or(0);
      return (200, Value::Array((0..count).map(|index| json!({"trigger_id": format!("t{index}")})).collect()));
    }
    (404, json!({"detail": "no such route"}))
  }
}

fn default_metadata(id: &str) -> Map<String, Value> {
  json!({"id": id, "name": id, "profile": "manager", "schema_version": 2}).as_object().expect("an object").clone()
}

fn row_json(row: &Row) -> Value {
  json!({
    "id": row.id,
    "name": row.id,
    "task_state": "open",
    "work_state": row.work_state,
    "archived": false,
    "child_count": row.open_descendant_count,
    "open_descendant_count": row.open_descendant_count,
    "running_descendant_count": 0,
    "has_unread": false,
  })
}

/// A synthetic CHARLIEBOT_HOME with the world's session directories, and the stub that serves its reads.
struct Sandbox {
  home: TempDir,
  stub: Stub,
}

impl Sandbox {
  fn new(world: World) -> Sandbox {
    let home = TempDir::new().expect("a temp home");
    for id in world.session_ids() {
      let directory = home.path().join("sessions").join(&id);
      fs::create_dir_all(&directory).expect("a session directory");
      let metadata = world.metadata.get(&id).cloned().unwrap_or_else(|| default_metadata(&id));
      fs::write(directory.join("metadata.json"), Value::Object(metadata).to_string()).expect("the session metadata");
    }
    let stub = Stub::start(Arc::new(world));
    fs::write(home.path().join("config.yaml"), format!("server:\n  port: {}\n", stub.port)).expect("the config");
    Sandbox { home, stub }
  }

  fn path(&self) -> &Path {
    self.home.path()
  }
}

/// The stub internal API: it answers GETs from the world and records each request line.
struct Stub {
  port: u16,
  requests: Arc<Mutex<Vec<String>>>,
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl Stub {
  fn start(world: Arc<World>) -> Stub {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    listener.set_nonblocking(true).expect("the listener goes nonblocking");
    let port = listener.local_addr().expect("the listener has an address").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
      let requests = requests.clone();
      let stop = stop.clone();
      std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream.set_nonblocking(false).expect("the connection goes blocking");
              stream.set_read_timeout(Some(PROCESS_TIMEOUT)).expect("the read timeout is set");
              let request_line = read_request_line(&mut stream);
              let target = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
              requests.lock().expect("the request lock").push(request_line);
              let (status, answer) = world.answer(&target);
              write_response(&mut stream, status, &answer);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the stub's accept failed: {error}"),
          }
        }
      })
    };
    Stub { port, requests, stop, thread: Some(thread) }
  }

  fn requests(&self) -> Vec<String> {
    self.requests.lock().expect("the request lock").clone()
  }
}

impl Drop for Stub {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(thread) = self.thread.take() {
      thread.join().expect("the stub thread ends");
    }
  }
}

/// The request line of one request, read through the blank line that ends its head.
fn read_request_line(stream: &mut TcpStream) -> String {
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 1024];
  loop {
    if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
      let head = String::from_utf8_lossy(&buffer[..position]).into_owned();
      return head.lines().next().unwrap_or_default().to_string();
    }
    let read = stream.read(&mut chunk).expect("the request head arrives");
    assert!(read > 0, "the client closed before the request head ended");
    buffer.extend_from_slice(&chunk[..read]);
  }
}

fn write_response(stream: &mut TcpStream, status: u16, answer: &Value) {
  let body = serde_json::to_vec(answer).expect("the answer serializes");
  let reason = match status {
    200 => "OK",
    404 => "Not Found",
    500 => "Internal Server Error",
    other => panic!("the stub has no reason phrase for {other}"),
  };
  let head = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
  stream.write_all(head.as_bytes()).expect("the head is written");
  stream.write_all(&body).expect("the body is written");
}

/// One check process's exit code, its stdout and stderr, and the requests the stub served during it.
#[derive(Debug)]
struct Run {
  status: Option<i32>,
  stdout: String,
  stderr: String,
  requests: Vec<String>,
}

/// Runs `charliebot hook pending-wake` in the sandbox with `input` on stdin, and asserts that it sent only GETs.
fn run_check(sandbox: &Sandbox, input: &Value) -> Run {
  let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args(["hook", "pending-wake"])
    .env_clear()
    .env("CHARLIEBOT_HOME", sandbox.path())
    .env("HOME", sandbox.path())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("the charliebot binary starts");
  child
    .stdin
    .take()
    .expect("the stdin is piped")
    .write_all(input.to_string().as_bytes())
    .expect("the input is written");
  let deadline = Instant::now() + PROCESS_TIMEOUT;
  while child.try_wait().expect("the process status reads").is_none() {
    assert!(Instant::now() < deadline, "the check process ran past {PROCESS_TIMEOUT:?}");
    std::thread::sleep(Duration::from_millis(5));
  }
  let output = child.wait_with_output().expect("the output reads");
  let requests = sandbox.stub.requests();
  assert!(requests.iter().all(|line| line.starts_with("GET ")), "the check only reads: {requests:?}");
  Run {
    status: output.status.code(),
    stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
    stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    requests,
  }
}

/// A turn input for the root session with the given kind, exit code, stored status and turn events.
fn turn_input(kind: &str, exit_code: i64, status: Value, turn: Vec<Value>) -> Value {
  json!({"session_id": ROOT, "kind": kind, "exit_code": exit_code, "status": status, "turn": turn})
}

/// A stored status with a goal and the given Waiting on you list.
fn status(waiting: Value) -> Value {
  json!({"goal": "a goal", "waiting": waiting})
}

/// A web turn that ends with no stored Waiting on you item: the input every wake case starts from.
fn web_turn() -> Value {
  turn_input("web", 0, status(json!([])), user_turn())
}

/// A turn whose only event is the user message it answers.
fn user_turn() -> Vec<Value> {
  vec![json!({"type": "user", "id": "u1", "content": "go on", "received_at": "2026-11-19T10:00:00Z"})]
}

/// The first miss of the check, as the turn-end runner records it.
fn block_record() -> Value {
  json!({
    "type": "turn_hooks",
    "id": "record-1",
    "results": [{"name": "pending-wake", "result": "block", "reason": EXPECTED_BLOCK_REASON}],
  })
}

fn stdout_json(run: &Run) -> Value {
  serde_json::from_str(run.stdout.trim()).unwrap_or_else(|error| panic!("stdout is one JSON object ({error}): {run:?}"))
}

#[test]
fn a_turn_that_leaves_no_wake_is_blocked_once_with_the_check_text() {
  let sandbox = Sandbox::new(World::default());
  let run = run_check(&sandbox, &web_turn());

  assert_eq!(run.status, Some(0), "{run:?}");
  let answer = stdout_json(&run);
  assert_eq!(answer["decision"], "block", "{answer}");
  let reason = answer["reason"].as_str().expect("a block names its reason");
  assert_eq!(reason, EXPECTED_BLOCK_REASON);
}

#[test]
fn each_single_wake_item_passes_the_check_silently() {
  let scheduled = json!({"schedule_enabled": true, "schedule_next_run": "2026-11-20T09:00:00Z"});
  let worlds = [
    ("a pending trigger of the session", World::default().triggers(ROOT, 1)),
    ("a bound schedule with a next run", World::default().metadata(ROOT, scheduled.clone())),
    ("a running worker", World::default().child(ROOT, "worker", "running", 0)),
    ("a worker holding a trigger", World::default().child(ROOT, "worker", "idle", 0).triggers("worker", 1)),
    (
      "a worker listing Waiting on you",
      World::default()
        .child(ROOT, "worker", "idle", 0)
        .metadata("worker", json!({"web_status": {"waiting": [{"text": "an answer"}]}})),
    ),
    (
      "an open child manager holding a running worker",
      World::default().child(ROOT, "manager", "idle", 1).child("manager", "grandchild", "running", 0),
    ),
    ("a bound schedule on a child", World::default().child(ROOT, "worker", "idle", 0).metadata("worker", scheduled)),
  ];
  for (label, world) in worlds {
    let run = run_check(&Sandbox::new(world), &web_turn());
    assert_eq!(run.status, Some(0), "{label}: {run:?}");
    assert!(run.stdout.is_empty(), "{label} passes silently: {run:?}");
  }

  let waiting = turn_input("web", 0, status(json!([{"text": "an answer"}])), user_turn());
  let run = run_check(&Sandbox::new(World::default()), &waiting);
  assert_eq!(run.status, Some(0), "a Waiting on you item: {run:?}");
  assert!(run.stdout.is_empty(), "a Waiting on you item passes silently: {run:?}");
}

#[test]
fn a_close_request_in_this_turn_a_thread_and_a_failed_turn_pass_silently() {
  let mut close = user_turn();
  close.push(json!({"type": "task_close_requested", "id": "c1"}));
  let cases = [
    ("a close request in this turn", turn_input("web", 0, status(json!([])), close)),
    ("a Slack thread", turn_input("slack", 0, status(json!([])), user_turn())),
    ("a turn that exited 1", turn_input("web", 1, status(json!([])), user_turn())),
  ];
  for (label, input) in cases {
    let run = run_check(&Sandbox::new(World::default()), &input);
    assert_eq!(run.status, Some(0), "{label}: {run:?}");
    assert!(run.stdout.is_empty(), "{label} passes silently: {run:?}");
  }
}

#[test]
fn a_failed_read_exits_one_and_shows_its_error() {
  let tree = World { fail_tree: true, ..World::default() };
  let run = run_check(&Sandbox::new(tree), &web_turn());
  assert_eq!(run.status, Some(1), "{run:?}");
  assert!(run.stdout.is_empty(), "{run:?}");
  assert!(!run.stderr.is_empty(), "the failure is shown: {run:?}");

  let triggers = World { fail_triggers: true, ..World::default() };
  let run = run_check(&Sandbox::new(triggers), &web_turn());
  assert_eq!(run.status, Some(1), "{run:?}");
  assert!(!run.stderr.is_empty(), "the failure is shown: {run:?}");
}

#[test]
fn a_hundred_and_one_children_are_read_to_the_last_page() {
  let build = |last_state: &'static str| {
    let mut world = World::default();
    for index in 0..100 {
      world = world.child(ROOT, &format!("child-{index:03}"), "idle", 0);
    }
    world.child(ROOT, "child-100", last_state, 0)
  };

  let running_last = run_check(&Sandbox::new(build("running")), &web_turn());
  assert_eq!(running_last.status, Some(0), "{running_last:?}");
  assert!(running_last.stdout.is_empty(), "the 101st child runs: {running_last:?}");
  let second_page = running_last.requests.iter().filter(|line| line.contains("cursor=100")).count();
  assert_eq!(second_page, 1, "the second page is read: {:?}", running_last.requests);

  let idle_last = run_check(&Sandbox::new(build("idle")), &web_turn());
  assert_eq!(stdout_json(&idle_last)["decision"], "block", "{idle_last:?}");
  let second_page = idle_last.requests.iter().filter(|line| line.contains("cursor=100")).count();
  assert_eq!(second_page, 1, "the second page is read: {:?}", idle_last.requests);
}

#[test]
fn a_miss_after_a_block_in_the_same_turn_passes_with_the_notice_only() {
  let mut turn = user_turn();
  turn.push(block_record());
  let run = run_check(&Sandbox::new(World::default()), &turn_input("web", 0, status(json!([])), turn));

  assert_eq!(run.status, Some(0), "{run:?}");
  let answer = stdout_json(&run);
  let keys: Vec<&str> = answer.as_object().expect("the notice is an object").keys().map(String::as_str).collect();
  assert_eq!(keys, ["systemMessage"], "{answer}");
  assert_eq!(answer["systemMessage"], EXPECTED_NOTICE);
}

/// The turn-end runner's entry run: the second miss of a turn sees the first miss's `turn_hooks` record in its input,
/// and passes with the notice. The entry is the hook list's own `pending-wake` entry, run through the runner's
/// `run_entries`. Its command names the built binary by path, since this test process's PATH does not hold it, and it
/// sets the sandbox home. The record the test appends to the turn has the shape the runner writes for a block.
#[tokio::test]
async fn the_turn_end_runner_passes_the_second_miss_of_a_turn_with_the_notice() {
  let hook_list: Value =
    serde_json::from_str(include_str!("../../../../../hooks/turn_end.json")).expect("the hook list parses");
  let listed = hook_list
    .as_array()
    .expect("the hook list is an array")
    .iter()
    .find(|entry| entry["name"] == "pending-wake")
    .expect("the hook list names pending-wake");
  assert_eq!(listed["command"], "charliebot hook pending-wake");

  let sandbox = Sandbox::new(World::default());
  let entry = Entry {
    name: "pending-wake".to_string(),
    command: format!(
      "CHARLIEBOT_HOME={home} HOME={home} {bin} hook pending-wake",
      home = sandbox.path().display(),
      bin = env!("CARGO_BIN_EXE_charliebot"),
    ),
  };
  let input: TurnInput = serde_json::from_value(web_turn()).expect("the turn input parses");

  let first =
    run_entries(std::slice::from_ref(&entry), &input, sandbox.path(), PROCESS_TIMEOUT).await.expect("the first run");
  let Outcome::Block { reason } = &first[0].outcome else {
    panic!("the first miss blocks: {first:?}");
  };

  let mut turn = input.turn.clone();
  turn.push(json!({
    "type": "turn_hooks",
    "id": "record-1",
    "results": [{"name": "pending-wake", "result": "block", "reason": reason}],
  }));
  let second_input = TurnInput { turn, ..input };
  let second = run_entries(std::slice::from_ref(&entry), &second_input, sandbox.path(), PROCESS_TIMEOUT)
    .await
    .expect("the second run");
  assert!(
    matches!(&second[0].outcome, Outcome::Pass { notice: Some(notice) } if notice == EXPECTED_NOTICE),
    "{second:?}"
  );
}
