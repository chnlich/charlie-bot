//! The stand-in for a release's `charliebot-server` in the switch tests. It serves the routes and log lines that the
//! switch reads, with its behavior from `behavior.json` beside it and its home from CHARLIEBOT_HOME.
//!
//! Each request past the version probe is appended to `calls.log` beside the program, as "METHOD PATH". At startup it
//! logs a recovery line for each live run record when it adopts runs, and a line for each live run that names the
//! run; then it logs `server_ready`. Every log line is one JSON object, as the server writes them.

use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use charliebot_cli_common::route;
use charliebot_cli_common::skew;
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types;
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use charliebot_session_api::api as session_routes;
use charliebot_store::layout;
use charliebot_store::run_records::RUN_EVENTS_NAME;
use charliebot_tasks::routes as task_routes;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const PROBE_SESSION: &str = "probe-session";
/// The delegation check's parent session, the child session its task runs in, and the task's run.
const PARENT_SESSION: &str = "delegation-session";
const CHILD_SESSION: &str = "delegation-child";
const TASK_RUN: &str = "task-run";
const TERMINAL: [&str; 4] = ["success", "failed", "interrupted", "stopped"];
const ACCEPTED: u16 = 202;

static MESSAGE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/api/chat/[^/]+/message$").expect("regex"));
static RUNS_LISTING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/api/sessions/[^/]+/runs\?.*$").expect("regex"));
static PROBE_CANCEL: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"^/api/sessions/probe-session/runs/([^/]+)/cancel$").expect("regex"));

/// What one test sets for this server. Every field is required: the test writes the full set.
#[derive(Deserialize)]
struct Behavior {
  check_config: String,
  adopt: bool,
  run_state: String,
  output: String,
  output_after_polls: u64,
  cancel: String,
  followup: String,
  message_status: u16,
  archive_runs_delay: f64,
  archive_runs_delay_after: u64,
  ignore_sigterm: bool,
  delegate: String,
  task_state: String,
  worktree: String,
  task_error: String,
}

/// One run of the probe session, in the order the server created it.
struct RunRecord {
  id: String,
  state: String,
}

/// What the server has done so far: the probe session's runs, the read counts, the delegated task and the messages
/// the parent session has received. The delegation check needs a message before the server delegates.
#[derive(Default)]
struct State {
  runs: Vec<RunRecord>,
  bootstrap_reads: u64,
  run_listings: u64,
  task: Option<Value>,
  opened: Vec<String>,
}

struct Server {
  dir: PathBuf,
  home: PathBuf,
  key: String,
  behavior: Behavior,
  state: Mutex<State>,
}

fn main() {
  if let Err(error) = serve() {
    eprintln!("fake server: {error}");
    process::exit(1);
  }
}

fn serve() -> io::Result<()> {
  let program = env::current_exe()?;
  let dir = program.parent().ok_or_else(|| io::Error::other("the program has no directory"))?.to_path_buf();
  let behavior: Behavior =
    serde_json::from_str(&fs::read_to_string(dir.join("behavior.json"))?).map_err(io::Error::other)?;
  if env::args().any(|arg| arg == "--check-config") {
    if behavior.check_config == "fail" {
      eprintln!("config error: unknown key server.bogus");
      process::exit(1);
    }
    process::exit(0);
  }
  if behavior.ignore_sigterm {
    // SAFETY: SIG_IGN installs no handler, so no code runs in the signal path.
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
  }
  let home = PathBuf::from(
    env::var_os(CHARLIEBOT_HOME_ENV).ok_or_else(|| io::Error::other(format!("{CHARLIEBOT_HOME_ENV} is not set")))?,
  );
  let config = fs::read_to_string(home.join("config.yaml"))?;
  let port: u16 = capture(r"(?m)^\s*port:\s*(\d+)", &config)?.parse().map_err(io::Error::other)?;
  let key = capture(r"access_key:\s*(\S+)", &fs::read_to_string(home.join("credentials.yaml"))?)?;
  recovery_logs(&home, behavior.adopt)?;
  let listener = TcpListener::bind(("127.0.0.1", port))?;
  log("server_ready", json!({"ready_in_ms": 1}));
  let server = Arc::new(Server { dir, home, key, behavior, state: Mutex::new(State::default()) });
  for stream in listener.incoming() {
    let stream = stream?;
    let server = Arc::clone(&server);
    thread::spawn(move || {
      if let Err(error) = server.handle(stream) {
        eprintln!("fake server: request failed: {error}");
      }
    });
  }
  Ok(())
}

/// The first capture group of `pattern` in `text`.
fn capture(pattern: &str, text: &str) -> io::Result<String> {
  let regex = Regex::new(pattern).map_err(io::Error::other)?;
  let found = regex.captures(text).ok_or_else(|| io::Error::other(format!("no match for {pattern}")))?;
  Ok(found[1].to_string())
}

/// The startup recovery lines. A run whose record has no end gets a resume line when this server adopts runs. Every
/// such run also gets a master line, which the switch must not count as adoption.
fn recovery_logs(home: &Path, adopt: bool) -> io::Result<()> {
  let mut metas = Vec::new();
  for session in sorted_entries(&sessions_dir_in(home))? {
    for run in sorted_entries(&layout::runs_dir(&session))? {
      let meta = layout::metadata_path(&run);
      if meta.is_file() {
        metas.push(meta);
      }
    }
  }
  for path in metas {
    let meta: Value = serde_json::from_str(&fs::read_to_string(&path)?).map_err(io::Error::other)?;
    let ended_at =
      meta.get("ended_at").ok_or_else(|| io::Error::other(format!("{} has no ended_at", path.display())))?;
    if ended_at.is_null() {
      if adopt {
        log("task_recovery_resume", json!({"session_id": meta["session_id"], "run_id": meta["id"]}));
      }
      log("master_cc_resuming", json!({"session_id": meta["session_id"], "run_id": meta["id"]}));
    }
  }
  Ok(())
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
  if !dir.is_dir() {
    return Ok(Vec::new());
  }
  let mut paths = fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
  paths.sort();
  Ok(paths)
}

/// One JSON line on stdout: the event, its fields, the level and the time.
fn log(event: &str, fields: Value) {
  let mut record = Map::new();
  record.insert("event".to_string(), json!(event));
  if let Value::Object(fields) = fields {
    record.extend(fields);
  }
  record.insert("level".to_string(), json!("info"));
  record.insert("timestamp".to_string(), json!(timestamp()));
  println!("{}", Value::Object(record));
}

fn timestamp() -> String {
  let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
  format!("{}.{:06}", since.as_secs(), since.subsec_micros())
}

impl Server {
  fn lock(&self) -> MutexGuard<'_, State> {
    self.state.lock().expect("the server state lock")
  }

  fn handle(&self, mut stream: TcpStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
      return Ok(());
    }
    let mut words = request_line.split_whitespace();
    let method = words.next().ok_or_else(|| io::Error::other("no request method"))?.to_string();
    let path = words.next().ok_or_else(|| io::Error::other("no request path"))?.to_string();
    let mut headers = HashMap::new();
    loop {
      let mut line = String::new();
      reader.read_line(&mut line)?;
      let line = line.trim_end();
      if line.is_empty() {
        break;
      }
      if let Some((name, value)) = line.split_once(':') {
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
      }
    }
    let length: usize = match headers.get("content-length") {
      Some(value) => value.parse().map_err(io::Error::other)?,
      None => 0,
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: Value =
      if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).map_err(io::Error::other)? };
    let authorized = headers.get("authorization").is_some_and(|value| *value == format!("Bearer {}", self.key));
    let (status, reply) =
      if authorized { self.route(&method, &path, &request)? } else { (401, json!({"detail": "unauthorized"})) };
    write_response(&mut stream, status, &reply)
  }

  fn route(&self, method: &str, path: &str, request: &Value) -> io::Result<(u16, Value)> {
    if method == "GET" && path == skew::VERSION_PATH {
      return Ok((200, json!({"ok": true})));
    }
    self.log_call(method, path)?;
    let delegation_prefixes = [
      task_routes::DELEGATE_PATH.to_string(),
      format!("/api/chat/{PARENT_SESSION}"),
      route::fill(session_routes::SESSION_PATH, &[PARENT_SESSION]),
      route::fill(session_routes::SESSION_PATH, &[CHILD_SESSION]),
    ];
    if delegation_prefixes.iter().any(|prefix| path.starts_with(prefix.as_str())) {
      return self.delegation(method, path, request);
    }
    if method == "POST" && path == session_routes::SESSIONS_PATH {
      let session = if request["name"] == "switch delegation" { PARENT_SESSION } else { PROBE_SESSION };
      return Ok((200, json!({"id": session})));
    }
    if method == "POST" && MESSAGE_PATH.is_match(path) {
      if request_id_missing(request) {
        return Ok((422, json!({"detail": "request_id is required"})));
      }
      if self.behavior.message_status != ACCEPTED {
        return Ok((self.behavior.message_status, json!({"detail": "message refused"})));
      }
      self.lock().runs.push(RunRecord { id: "probe-run".to_string(), state: self.behavior.run_state.clone() });
      return Ok((ACCEPTED, json!({(event_types::INPUT_EVENT_ID): "probe-input"})));
    }
    if method == "GET" && RUNS_LISTING.is_match(path) {
      return Ok(self.run_listing(path));
    }
    if method == "GET" && path == route::fill(session_routes::SESSION_BOOTSTRAP_PATH, &[PROBE_SESSION]) {
      return Ok((200, self.bootstrap()));
    }
    if method == "POST"
      && let Some(found) = PROBE_CANCEL.captures(path)
    {
      return Ok(self.cancel_probe_run(&found[1]));
    }
    if method == "POST" && path == route::fill(task_routes::SESSION_ARCHIVE_PATH, &[PROBE_SESSION]) {
      return Ok(self.archive_probe_session(request));
    }
    Ok((404, json!({"detail": "not found"})))
  }

  fn log_call(&self, method: &str, path: &str) -> io::Result<()> {
    let mut calls = OpenOptions::new().create(true).append(true).open(self.dir.join("calls.log"))?;
    writeln!(calls, "{method} {path}")
  }

  /// The probe session's runs. The archive step's listing stalls once `archive_runs_delay_after` listings have been
  /// answered fast; the stall happens outside the lock, so the other requests still answer.
  fn run_listing(&self, path: &str) -> (u16, Value) {
    let newest_first = path.contains("order=desc");
    if !newest_first {
      let listings = {
        let mut state = self.lock();
        state.run_listings += 1;
        state.run_listings
      };
      if listings > self.behavior.archive_runs_delay_after {
        thread::sleep(Duration::from_secs_f64(self.behavior.archive_runs_delay));
      }
    }
    let mut items: Vec<Value> = self.lock().runs.iter().map(|run| json!({"id": run.id, "state": run.state})).collect();
    if newest_first {
      items.reverse();
    }
    (200, json!({"items": items, "next_cursor": null}))
  }

  fn bootstrap(&self) -> Value {
    let reads = {
      let mut state = self.lock();
      state.bootstrap_reads += 1;
      state.bootstrap_reads
    };
    let mut messages =
      vec![json!({"role": "user", "content": "Reply with the single word HELLO.", "id": "probe-user"})];
    let mut draft = Value::Null;
    if reads > self.behavior.output_after_polls {
      if self.behavior.output == "message" {
        messages.push(json!({"role": "assistant", "content": "HELLO", "id": "probe-reply"}));
      } else if self.behavior.output == "tool" {
        draft = json!({"role": "assistant", "content": "", "tools": [{"name": "Bash"}]});
      }
    }
    json!({
      "messages": messages,
      "pending_draft": draft,
      "event_count": messages.len(),
      "oldest_message_ordinal": 0,
      "has_more": false,
    })
  }

  fn cancel_probe_run(&self, run_id: &str) -> (u16, Value) {
    let mut state = self.lock();
    let Some(run) = state.runs.iter_mut().find(|run| run.id == run_id) else {
      return (404, json!({"detail": "run not found"}));
    };
    if self.behavior.cancel == "settles" && !TERMINAL.contains(&run.state.as_str()) {
      run.state = "interrupted".to_string();
    }
    (200, json!({"run_id": run_id, "stop_requested": true, "outcome": null}))
  }

  /// The archive is refused while a run is unfinished. The follow-up run starts at the first archive, as the web reply
  /// nudge does after a turn.
  fn archive_probe_session(&self, request: &Value) -> (u16, Value) {
    if request_id_missing(request) {
      return (422, json!({"detail": "request_id is required"}));
    }
    let mut state = self.lock();
    if self.behavior.followup == "nudge" && !state.runs.iter().any(|run| run.id == "nudge-run") {
      state.runs.push(RunRecord { id: "nudge-run".to_string(), state: "running".to_string() });
    }
    if state.runs.iter().any(|run| !TERMINAL.contains(&run.state.as_str())) {
      return (409, json!({"detail": "a run of the session is unfinished"}));
    }
    (200, json!({"archived": [PROBE_SESSION]}))
  }

  fn delegation(&self, method: &str, path_and_query: &str, request: &Value) -> io::Result<(u16, Value)> {
    let path = path_and_query.split('?').next().unwrap_or_default();
    if method == "POST" && path == route::fill(task_routes::CHAT_MESSAGE_PATH, &[PARENT_SESSION]) {
      let content = string_field(request, "content")?.to_string();
      self.lock().opened.push(content);
      return Ok((ACCEPTED, json!({(event_types::INPUT_EVENT_ID): "open-input"})));
    }
    if method == "POST" && path == task_routes::DELEGATE_PATH {
      if self.behavior.delegate == "refused" {
        return Ok((400, json!({"detail": "backend router failed: no backend is available"})));
      }
      // The server's rule: a delegation needs a user message on its task chain.
      let opened = !self.lock().opened.is_empty();
      if !opened {
        let detail =
          format!("Delegation blocked: no user request on the task chain from task {PARENT_SESSION} to its root.");
        return Ok((403, json!({"detail": detail})));
      }
      let task = self.start_task(string_field(request, "repo_path")?)?;
      self.lock().task = Some(task);
      return Ok((
        200,
        json!({
          "session_id": CHILD_SESSION,
          "parent_session_id": request["session_id"],
          "run_id": TASK_RUN,
          "thread_id": TASK_RUN,
          "description": request["description"],
        }),
      ));
    }
    if method == "GET" && path == route::fill(session_routes::SESSION_RUNS_PATH, &[CHILD_SESSION]) {
      let items: Vec<Value> = self.lock().task.iter().cloned().collect();
      return Ok((200, json!({"items": items, "next_cursor": null})));
    }
    if method == "POST" && path == route::fill(task_routes::RUN_CANCEL_PATH, &[CHILD_SESSION, TASK_RUN]) {
      let mut state = self.lock();
      let Some(task) = state.task.as_mut() else {
        return Ok((404, json!({"detail": "run not found"})));
      };
      if self.behavior.cancel == "settles" && !TERMINAL.contains(&task["state"].as_str().unwrap_or_default()) {
        task["state"] = json!("interrupted");
      }
      return Ok((200, json!({"run_id": TASK_RUN, "stop_requested": true, "outcome": null})));
    }
    if method == "GET" && path == route::fill(session_routes::SESSION_RUNS_PATH, &[PARENT_SESSION]) {
      return Ok((200, json!({"items": [], "next_cursor": null})));
    }
    if method == "POST" && path == route::fill(task_routes::SESSION_ARCHIVE_PATH, &[PARENT_SESSION]) {
      return Ok((200, json!({"archived": [PARENT_SESSION]})));
    }
    Ok((404, json!({"detail": "not found"})))
  }

  /// The task's run record. A created worktree is a real worktree of the repository, as the server makes one. A task
  /// error goes to the run's event log, where the server writes its launch failures.
  fn start_task(&self, repo: &str) -> io::Result<Value> {
    let mut task = json!({
      "id": TASK_RUN,
      "state": self.behavior.task_state,
      "repo_path": repo,
      "branch_name": null,
      "worktree_path": null,
    });
    if self.behavior.worktree == "created" {
      let worktree = self.home.join("worktrees/task-switch-check");
      fs::create_dir_all(self.home.join("worktrees"))?;
      let status = Command::new("git")
        .args(["-C", repo, "worktree", "add", "-q", "-b", "charliebot/task-switch-check"])
        .arg(&worktree)
        .arg("main")
        .status()?;
      if !status.success() {
        return Err(io::Error::other(format!("git worktree add failed: {status}")));
      }
      task["branch_name"] = json!("charliebot/task-switch-check");
      task["worktree_path"] = json!(fs::canonicalize(&worktree)?.display().to_string());
    }
    if !self.behavior.task_error.is_empty() {
      let run_dir = layout::runs_dir(&sessions_dir_in(&self.home).join(CHILD_SESSION)).join(TASK_RUN);
      fs::create_dir_all(&run_dir)?;
      let event = json!({"type": "error", "message": self.behavior.task_error});
      fs::write(run_dir.join(RUN_EVENTS_NAME), format!("{event}\n"))?;
    }
    Ok(task)
  }
}

/// The server answers a message, an archive or a stop only with a request id.
fn request_id_missing(request: &Value) -> bool {
  request.get("request_id").and_then(Value::as_str).is_none_or(str::is_empty)
}

fn string_field<'a>(request: &'a Value, name: &str) -> io::Result<&'a str> {
  request[name].as_str().ok_or_else(|| io::Error::other(format!("the request has no string {name}")))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) -> io::Result<()> {
  let data = serde_json::to_vec(body).map_err(io::Error::other)?;
  let reason = http::StatusCode::from_u16(status).map_err(io::Error::other)?.canonical_reason().unwrap_or("Unknown");
  write!(
    stream,
    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    data.len()
  )?;
  stream.write_all(&data)
}
