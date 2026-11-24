//! The `session trigger` process tests. Each runs the built `charliebot` against a stub internal API on 127.0.0.1
//! and, for `--host`, a stub `ssh` that comes first on the CLI's PATH. Every CLI process gets a cleared environment
//! and only the variables named here, with CHARLIEBOT_HOME and HOME inside the test's temp root. Each test that starts
//! a wrapper kills its process group before it returns. Only the ssh localhost test runs the real ssh.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;

/// How long one CLI process, or one wait for a file, may run before the test fails.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

const FIRE_AT: &str = "2026-10-08T00:11:00+00:00";
const REJECTION: &str = "session s1 has 5 pending triggers (limit 5); trigger rejected";
const SESSION_TRIGGERS: &str = "sessions/s1/triggers";

/// A request handler: the answer for one POST body, or `None` to drop the connection after reading the request.
type Handler = Box<dyn Fn(&Value) -> Option<(u16, Value)> + Send + Sync>;

/// The stub internal API: it records each POST body and answers through its handler.
struct StubApi {
  port: u16,
  bodies: Arc<Mutex<Vec<Value>>>,
  paths: Arc<Mutex<Vec<String>>>,
  requests: Arc<AtomicUsize>,
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl StubApi {
  fn start(handler: Handler) -> StubApi {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    listener.set_nonblocking(true).expect("the listener goes nonblocking");
    let port = listener.local_addr().expect("the listener has an address").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
      let bodies = bodies.clone();
      let paths = paths.clone();
      let requests = requests.clone();
      let stop = stop.clone();
      std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              requests.fetch_add(1, Ordering::Relaxed);
              stream.set_nonblocking(false).expect("the connection goes blocking");
              stream.set_read_timeout(Some(PROCESS_TIMEOUT)).expect("the read timeout is set");
              let request = read_request(&mut stream);
              if !request.post {
                // A refusal's version-skew probe: an answer that names no skew.
                write_response(&mut stream, 200, &json!({ "sha": "cafe567", "started_at": FIRE_AT }));
                continue;
              }
              bodies.lock().expect("the body lock").push(request.body.clone());
              paths.lock().expect("the path lock").push(request.path.clone());
              if let Some((status, answer)) = handler(&request.body) {
                write_response(&mut stream, status, &answer);
              }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("the stub's accept failed: {error}"),
          }
        }
      })
    };
    StubApi { port, bodies, paths, requests, stop, thread: Some(thread) }
  }

  fn bodies(&self) -> Vec<Value> {
    self.bodies.lock().expect("the body lock").clone()
  }

  /// The request path of each POST the stub read, in arrival order.
  fn paths(&self) -> Vec<String> {
    self.paths.lock().expect("the path lock").clone()
  }

  fn request_count(&self) -> usize {
    self.requests.load(Ordering::Relaxed)
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

/// One request the stub read: whether it is a POST, its path, and its JSON body. A GET carries no body,
/// so its body is null.
struct Request {
  post: bool,
  path: String,
  body: Value,
}

fn read_request(stream: &mut TcpStream) -> Request {
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
  let path = head.split_whitespace().nth(1).expect("the request line names a path").to_string();
  Request { post: head.starts_with("POST "), path, body }
}

fn write_response(stream: &mut TcpStream, status: u16, answer: &Value) {
  let body = serde_json::to_vec(answer).expect("the answer serializes");
  let reason = match status {
    200 => "OK",
    404 => "Not Found",
    409 => "Conflict",
    422 => "Unprocessable Entity",
    other => panic!("the stub has no reason phrase for {other}"),
  };
  let head = format!(
    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
  stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body)).expect("the response is written");
}

/// The registering double: it persists the trigger the way the server does, then answers.
fn registering(home: PathBuf) -> Handler {
  Box::new(move |body| {
    let triggers = home.join(SESSION_TRIGGERS);
    fs::create_dir_all(&triggers).expect("the triggers directory is created");
    let stored = json!({
      "id": "trig-1",
      "session_id": "s1",
      "status": "pending",
      "message": body["message"],
      "watch_targets": body["watch_targets"],
      "created_at": "2026-10-08T00:00:00+00:00",
      "fire_at": FIRE_AT,
    });
    fs::write(triggers.join("trig-1.json"), stored.to_string()).expect("the trigger file is written");
    Some((200, json!({ "trigger_id": "trig-1", "fire_at": FIRE_AT })))
  })
}

/// A temp root for one CLI run: the CHARLIEBOT_HOME (its config names the stub's port), a `bin` directory first on
/// PATH for a stub `ssh`, and the working directory.
struct Sandbox {
  root: TempDir,
  home: PathBuf,
  bin: PathBuf,
}

/// What one CLI process printed and how it ended. `code` is None when a signal ended it.
struct Run {
  code: Option<i32>,
  stdout: String,
  stderr: String,
}

impl Sandbox {
  fn new() -> Sandbox {
    let root = tempfile::tempdir().expect("a temp root");
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).expect("the home is created");
    fs::create_dir_all(&bin).expect("the bin directory is created");
    Sandbox { root, home, bin }
  }

  fn write_config(&self, port: u16) {
    fs::write(self.home.join("config.yaml"), format!("server:\n  port: {port}\n")).expect("the config is written");
  }

  fn sessions_dir(&self) -> PathBuf {
    self.home.join("sessions")
  }

  /// Run `charliebot session trigger ARGS` with HOME in the temp root.
  fn run(&self, args: &[&str]) -> Run {
    self.run_as(args, self.root.path())
  }

  /// Run `charliebot session trigger ARGS` with the given HOME: the environment holds only these three variables.
  fn run_as(&self, args: &[&str], home: &Path) -> Run {
    self.run_with(args, &[("CHARLIEBOT_HOME", self.home.as_os_str()), ("HOME", home.as_os_str())])
  }

  /// Run `charliebot session trigger ARGS` inside a Run of session `s1`: the server writes that session's id into
  /// CHARLIEBOT_SESSION_ID, and the CLI resolves its session from there.
  fn run_in_session(&self, args: &[&str]) -> Run {
    self.run_with(
      args,
      &[
        ("CHARLIEBOT_HOME", self.home.as_os_str()),
        ("HOME", self.root.path().as_os_str()),
        ("CHARLIEBOT_SESSION_ID", OsStr::new("s1")),
      ],
    )
  }

  /// Run `charliebot session trigger ARGS` with exactly `vars` set beyond PATH, whose stub `ssh` directory comes first.
  fn run_with(&self, args: &[&str], vars: &[(&str, &OsStr)]) -> Run {
    self.run_cli_with(&["session", "trigger"], args, vars)
  }

  /// Run the named `charliebot` command with exactly `vars` set beyond PATH.
  fn run_cli_with(&self, command_args: &[&str], args: &[&str], vars: &[(&str, &OsStr)]) -> Run {
    let path = format!("{}:/usr/bin:/bin", self.bin.display());
    let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot"));
    command.args(command_args).args(args).current_dir(self.root.path()).env_clear().env("PATH", path);
    for (name, value) in vars {
      command.env(name, value);
    }
    let mut child =
      command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the CLI starts");
    let code = wait_bounded(&mut child);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    Run { code, stdout: read_text(stdout), stderr: read_text(stderr) }
  }
}

fn read_text(mut pipe: impl Read) -> String {
  let mut text = String::new();
  pipe.read_to_string(&mut text).expect("the CLI output reads as UTF-8");
  text
}

/// Wait for a CLI process: past `PROCESS_TIMEOUT` it is killed and the test fails.
fn wait_bounded(child: &mut Child) -> Option<i32> {
  let started = Instant::now();
  loop {
    if let Some(status) = child.try_wait().expect("the CLI status reads") {
      return status.code();
    }
    if started.elapsed() > PROCESS_TIMEOUT {
      child.kill().expect("the overdue CLI is killed");
      child.wait().expect("the killed CLI is reaped");
      panic!("the CLI ran past {} s", PROCESS_TIMEOUT.as_secs());
    }
    std::thread::sleep(Duration::from_millis(10));
  }
}

/// A stub `ssh` in the sandbox's bin directory that runs its remote command in this shell. `ssh_cmd` puts five `-o`
/// option pairs and the host before the command, and those eleven arguments are dropped. The rest is joined with
/// spaces and re-parsed, as ssh hands it to the remote login shell. The wrapper's mkdir, setsid, PID capture and exit
/// file then run for real, with no network.
///
/// As with ssh, the stub stays up until the command's output closes. The substitution reads both streams to their
/// end, so a started process that keeps either stream open holds the stub, and so the launch, until that process
/// ends. The stub's stderr is merged into its stdout, and its exit status is the command's.
fn stub_ssh_running_locally(sandbox: &Sandbox) {
  charliebot_shell_harness::write_executable(
    &sandbox.bin.join("ssh"),
    "#!/bin/sh\nshift 11\nout=$(eval \"$*\" 2>&1)\nstatus=$?\nprintf '%s\\n' \"$out\"\nexit $status\n",
  );
}

/// What the remote shell prints for a launch that starts: its home, then the wrapper's PID.
const REMOTE_REPLY: &str = "/home/remote\n24680\n";

/// A stub `ssh` in the sandbox's bin directory: it records its argv NUL-separated, prints `stdout`, and exits `code`.
fn stub_ssh(sandbox: &Sandbox, stdout: &str, stderr: &str, code: i32) -> PathBuf {
  let argv_file = sandbox.root.path().join("ssh-argv");
  let stdout_file = sandbox.root.path().join("ssh-stdout");
  let stderr_file = sandbox.root.path().join("ssh-stderr");
  fs::write(&stdout_file, stdout).expect("the stub's stdout is staged");
  fs::write(&stderr_file, stderr).expect("the stub's stderr is staged");
  let script = format!(
    "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\ncat '{}'\ncat '{}' >&2\nexit {code}\n",
    argv_file.display(),
    stdout_file.display(),
    stderr_file.display()
  );
  charliebot_shell_harness::write_executable(&sandbox.bin.join("ssh"), script);
  argv_file
}

/// The argv the stub ssh recorded for its last run.
fn recorded_argv(argv_file: &Path) -> Vec<String> {
  let text = fs::read_to_string(argv_file).expect("the stub ssh recorded its argv");
  text.split('\0').filter(|part| !part.is_empty()).map(str::to_string).collect()
}

/// The one launch record of the session, as the CLI wrote it.
fn only_launch_record(sandbox: &Sandbox) -> Value {
  let records = launch_records(sandbox);
  assert_eq!(records.len(), 1, "one launch record");
  records.into_iter().next().expect("one record")
}

fn launch_records(sandbox: &Sandbox) -> Vec<Value> {
  let launches = sandbox.sessions_dir().join("s1").join("launches");
  if !launches.is_dir() {
    return Vec::new();
  }
  let mut records = Vec::new();
  for entry in fs::read_dir(&launches).expect("the launches directory reads") {
    let metadata = entry.expect("a launch directory entry reads").path().join("metadata.json");
    if metadata.exists() {
      let text = fs::read_to_string(&metadata).expect("the launch record reads");
      records.push(serde_json::from_str(&text).expect("the launch record is JSON"));
    }
  }
  records
}

fn pid_of(record: &Value) -> i32 {
  i32::try_from(record["pid"].as_i64().expect("the pid is an integer")).expect("a pid fits pid_t")
}

/// Poll until the exit file holds its code and returns it. The wrapper writes the code with one `echo`, so a complete
/// code ends in a newline.
fn wait_for_exit_code(path: &Path) -> String {
  wait_for_exit_code_within(path, PROCESS_TIMEOUT)
}

/// `wait_for_exit_code` with its own bound, for a command that runs longer than `PROCESS_TIMEOUT`.
fn wait_for_exit_code_within(path: &Path, bound: Duration) -> String {
  let started = Instant::now();
  loop {
    if let Ok(text) = fs::read_to_string(path)
      && text.ends_with('\n')
    {
      return text.trim().to_string();
    }
    assert!(started.elapsed() < bound, "{} never completed", path.display());
    std::thread::sleep(Duration::from_millis(10));
  }
}

/// Kills a started wrapper's process group when the test ends, so no failed assertion leaves a wrapper running.
struct GroupGuard(i32);

impl Drop for GroupGuard {
  fn drop(&mut self) {
    match killpg(Pid::from_raw(self.0), Signal::SIGKILL) {
      Ok(()) | Err(Errno::ESRCH) => {}
      Err(error) => eprintln!("could not kill the process group {}: {error}", self.0),
    }
  }
}

/// Removes the remote staging directory of a localhost launch when the test ends.
struct RemoteRunDir(PathBuf);

impl Drop for RemoteRunDir {
  fn drop(&mut self) {
    match fs::remove_dir_all(&self.0) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => eprintln!("could not remove {}: {error}", self.0.display()),
    }
  }
}

fn accept_any(_: &Value) -> Option<(u16, Value)> {
  Some((200, json!({ "trigger_id": "trig-1", "fire_at": FIRE_AT })))
}

// ---------------------------------------------------------------------------
// Success: the launch record, the registered target, the exit file
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_cmd_exit_3_writes_the_launch_record_and_the_exit_file() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "60", "--message", "m", "--cmd", "exit 3"]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let meta = only_launch_record(&sandbox);
  let _group = GroupGuard(pid_of(&meta));
  let keys: BTreeSet<&str> = meta.as_object().expect("the record is an object").keys().map(String::as_str).collect();
  let expected_keys = BTreeSet::from([
    "launch_id",
    "session_id",
    "host",
    "pid",
    "cwd",
    "cmd",
    "started_at",
    "log",
    "exit_file",
    "trigger_id",
  ]);
  assert_eq!(keys, expected_keys);
  assert_eq!(meta["session_id"], "s1");
  assert_eq!(meta["host"], Value::Null);
  assert!(meta["pid"].is_i64());
  assert_eq!(meta["cwd"], sandbox.root.path().display().to_string());
  assert_eq!(meta["cmd"], "exit 3");
  assert!(meta["started_at"].as_str().expect("started_at is text").ends_with('Z'));
  assert_eq!(meta["trigger_id"], "trig-1");
  let launch_dir = sandbox.sessions_dir().join("s1").join("launches").join(meta["launch_id"].as_str().expect("text"));
  assert_eq!(meta["log"], launch_dir.join("log").display().to_string());
  assert_eq!(meta["exit_file"], launch_dir.join("exit").display().to_string());
  // Stdout prints trigger_id, fire_at and the launch record's fields as one object.
  let mut expected = meta.clone();
  expected["fire_at"] = json!(FIRE_AT);
  assert_eq!(out, expected);

  assert_eq!(wait_for_exit_code(&PathBuf::from(meta["exit_file"].as_str().expect("text"))), "3");
}

#[tokio::test]
#[ignore = "integration"]
async fn test_missing_cmd_wakes_as_watch_failed() {
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry};
  use charliebot_store::metadata::{TaskApprover, TaskProfile, TaskSpec};
  use charliebot_store::{CreateTask, Store, TaskCaller};
  use charliebot_tasks::completion::TaskCompletion;
  use charliebot_tasks::dispatch::Dispatcher;
  use charliebot_triggers::triggers::TriggerManager;

  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);
  fs::write(sandbox.home.join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
  let registry = Arc::new(Registry::new());
  let config_value = registry.load_config(&sandbox.home).expect("test config");
  let store = Arc::new(
    Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = sandbox.home.clone();
  let config = Arc::new(HotReload::new(
    config_value.config_file(),
    "trigger_test_config_reload_failed",
    Box::new(move || config_registry.load_config(&config_home)),
  ));
  let credential_home = sandbox.home.clone();
  let credentials = Arc::new(HotReload::new(
    sandbox.home.join(CREDENTIALS_FILENAME),
    "trigger_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let features =
    Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
  features
    .store
    .create_task(
      CreateTask {
        request_id: "create-s1".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: Some(TaskSpec {
          goal: "Synthetic trigger test".to_string(),
          repo_path: None,
          base_branch: None,
          task_type: None,
          keep_worktree: false,
          cold_read: None,
          approver: TaskApprover::User,
          improve_host: None,
        }),
        name: Some("Synthetic trigger test".to_string()),
        backend: Some("synthetic".to_string()),
        group: None,
        session_id: Some("s1".to_string()),
      },
      TaskCaller::Operator,
      serde_json::Map::new(),
    )
    .await
    .expect("create task");
  let completion = Arc::new(TaskCompletion::new(Arc::clone(&features)));
  features
    .startup
    .insert(Dispatcher::new(Arc::clone(&features.store), Arc::clone(&features), completion.clone(), completion))
    .expect("install dispatcher");

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "charliebot-test-command-that-does-not-exist",
  ]);
  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let meta = only_launch_record(&sandbox);
  let _group = GroupGuard(pid_of(&meta));
  assert_eq!(wait_for_exit_code(Path::new(meta["exit_file"].as_str().expect("exit file"))), "127");

  let trigger_path = sandbox.home.join(SESSION_TRIGGERS).join("trig-1.json");
  let mut stored: Value = serde_json::from_slice(&fs::read(&trigger_path).expect("read registered trigger"))
    .expect("registered trigger is JSON");
  stored["fire_at"] = json!(
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(60))
      .format(&time::format_description::well_known::Rfc3339)
      .expect("format deadline")
  );
  fs::write(trigger_path, stored.to_string()).expect("set the watch deadline");
  let manager = Arc::new(TriggerManager::new(Arc::clone(&features)));
  manager.recover_pending().await.expect("recover the registered watch");
  let fired = tokio::time::timeout(PROCESS_TIMEOUT, async {
    loop {
      let triggers = manager.list_triggers("s1").await.expect("read trigger");
      if triggers[0].fire_reason.is_some() {
        break triggers[0].clone();
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the watch fires");
  assert_eq!(fired.fire_reason.as_deref(), Some("watch failed"));
  let events = features.store.load_all_facts("s1").expect("read task events");
  let content = events
    .iter()
    .find(|event| {
      event.get("type").and_then(|value| value.as_str()) == Some(charliebot_core::event_types::SCHEDULED_TRIGGER)
    })
    .and_then(|event| event.get("content"))
    .and_then(|value| value.as_str())
    .expect("the wake is admitted to the session");
  assert!(content.contains("[Scheduled trigger fired | watch failed]"), "{content}");
  assert!(!content.contains("completed"), "{content}");
}

#[test]
#[ignore = "integration"]
fn test_registered_target_carries_exit_file_and_log() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "60", "--message", "m", "--cmd", "exit 0"]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let meta = only_launch_record(&sandbox);
  let _group = GroupGuard(pid_of(&meta));
  let bodies = stub.bodies();
  assert_eq!(
    bodies[0]["watch_targets"],
    json!([{
      "kind": "local_pid",
      "pid": meta["pid"],
      "exit_file": meta["exit_file"],
      "log": meta["log"],
    }])
  );
}

// ---------------------------------------------------------------------------
// Rejection: the started process group dies, exit 2
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_rejected_registration_kills_the_process_group() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(Box::new(|_| Some((422, json!({ "detail": REJECTION })))));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "60", "--message", "m", "--cmd", "sleep 600"]);

  assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
  assert!(run.stderr.contains(REJECTION), "stderr: {}", run.stderr);
  let pid = i32::try_from(stub.bodies()[0]["watch_targets"][0]["pid"].as_i64().expect("text")).expect("a pid");
  let _group = GroupGuard(pid);
  assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH), "the started wrapper is killed");
  // No launch record: the registration never landed.
  assert!(launch_records(&sandbox).is_empty());
}

#[test]
#[ignore = "integration"]
fn test_failed_readback_kills_the_process_group() {
  let sandbox = Sandbox::new();
  // The stub answers but persists nothing, so the readback finds no pending trigger.
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "60", "--message", "m", "--cmd", "sleep 600"]);

  assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
  assert!(run.stderr.contains("readback"), "stderr: {}", run.stderr);
  let pid = i32::try_from(stub.bodies()[0]["watch_targets"][0]["pid"].as_i64().expect("text")).expect("a pid");
  let _group = GroupGuard(pid);
  assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH), "the started wrapper is killed");
}

// ---------------------------------------------------------------------------
// The wait cap: refused before anything is posted or started, and named in --help
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_max_wait_past_the_cap_exits_2_and_posts_nothing() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "43200", "--message", "m", "--cmd", "exit 0"]);

  assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
  assert!(run.stderr.contains("14400"), "stderr names the cap: {}", run.stderr);
  assert!(stub.bodies().is_empty(), "a refused wait posts nothing");
}

#[test]
#[ignore = "integration"]
fn test_max_wait_at_the_cap_registers() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&["--session", "s1", "--max-wait", "14400", "--message", "m"]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  assert_eq!(stub.bodies()[0]["delay_seconds"], 14400);
}

#[test]
#[ignore = "integration"]
fn test_cancel_posts_the_run_session_route_and_prints_the_trigger() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(Box::new(|_| Some((200, json!({ "ok": true })))));
  sandbox.write_config(stub.port);

  let run = sandbox.run_in_session(&["cancel", "trig-1"]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  assert_eq!(stub.paths(), ["/api/internal/triggers/s1/trig-1/cancel"]);
  let printed: Value = serde_json::from_str(&run.stdout).expect("the cancel prints one JSON object");
  assert_eq!(printed, json!({ "trigger_id": "trig-1", "status": "cancelled" }));
}

#[test]
#[ignore = "integration"]
fn test_cancel_refusal_exits_1_with_the_server_reason() {
  let refusals =
    [(404, "Trigger not found"), (409, "trigger trig-1 is already fired: only a pending trigger can be cancelled")];
  for (status, reason) in refusals {
    let sandbox = Sandbox::new();
    let stub = StubApi::start(Box::new(move |_| Some((status, json!({ "detail": reason })))));
    sandbox.write_config(stub.port);

    let run = sandbox.run_in_session(&["cancel", "trig-1"]);

    assert_eq!(run.code, Some(1), "a {status} refusal exits 1: stderr: {}", run.stderr);
    assert!(run.stderr.contains(reason), "stderr carries the server's reason: {}", run.stderr);
    assert!(run.stdout.is_empty(), "a refused cancel prints no success object: {}", run.stdout);
  }
}

#[test]
#[ignore = "integration"]
fn test_help_names_the_wait_cap() {
  let sandbox = Sandbox::new();

  let run = sandbox.run(&["--help"]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  assert!(run.stdout.contains("14400"), "--help names the cap: {}", run.stdout);
}

/// The direction of the round tests, whose kit holds the prompt file `dev/improve/decoupling.md`.
const ROUND_DIRECTION: &str = "decoupling";

/// The round's environment as the runner exports it: HOME, CHARLIEBOT_HOME at the round home beneath HOME, the launch
/// directory, the direction and the kit directory. The round home's config names a live stub API, so a refusal can
/// only come from the round test, not from a dead port.
struct RoundEnv {
  home: PathBuf,
  round_home: PathBuf,
  launch_dir: PathBuf,
  kit_dir: PathBuf,
}

impl RoundEnv {
  /// The variables that the runner exports to the round.
  fn vars(&self) -> Vec<(&'static str, &OsStr)> {
    vec![
      ("CHARLIEBOT_HOME", self.round_home.as_os_str()),
      ("HOME", self.home.as_os_str()),
      ("IMPROVE_LAUNCH_DIR", self.launch_dir.as_os_str()),
      ("IMPROVE_DIRECTION", OsStr::new(ROUND_DIRECTION)),
      ("IMPROVE_KIT_DIR", self.kit_dir.as_os_str()),
    ]
  }
}

fn round_sandbox(sandbox: &Sandbox, stub: &StubApi) -> RoundEnv {
  let home = sandbox.root.path().to_path_buf();
  let round_home = home.join("improve").join("home");
  fs::create_dir_all(&round_home).expect("the round home is created");
  fs::write(round_home.join("config.yaml"), format!("server:\n  port: {}\n", stub.port))
    .expect("the round config is written");
  let launch_dir = home.join("improve").join("launch").join("20261119T000000Z-1-1");
  let kit_dir = home.join("kit");
  RoundEnv { home, round_home, launch_dir, kit_dir }
}

/// The refusal names each state source of the round by its path: the launch directory, the direction prompt in the
/// kit, and the evidence directory beneath HOME.
fn assert_names_state_sources(run: &Run, round: &RoundEnv) {
  let error: Value = serde_json::from_str(&run.stderr).expect("stderr is a JSON error");
  let message = error["error"].as_str().expect("the error is a message");
  let prompt = round.kit_dir.join("dev").join("improve").join(format!("{ROUND_DIRECTION}.md"));
  let evidence = round.home.join("improve").join(ROUND_DIRECTION).join("evidence");
  for path in [&round.launch_dir, &prompt, &evidence] {
    assert!(message.contains(&path.display().to_string()), "the refusal does not name {}: {message}", path.display());
  }
}

#[test]
#[ignore = "integration"]
fn test_round_refuses_at_once_and_calls_no_server() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  let round = round_sandbox(&sandbox, &stub);
  let start = Instant::now();

  let run = sandbox.run_with(&["--session", "s1", "--max-wait", "60", "--message", "m"], &round.vars());

  assert!(start.elapsed() < Duration::from_secs(5), "the round waited {:?} before it refused", start.elapsed());
  assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
  assert_names_state_sources(&run, &round);
  assert!(stub.bodies().is_empty(), "the round reached the server: {:?}", stub.bodies());
}

#[test]
#[ignore = "integration"]
fn test_round_refuses_another_server_verb_at_once() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  let round = round_sandbox(&sandbox, &stub);
  let start = Instant::now();

  let run = sandbox.run_cli_with(&["session", "tree"], &[], &round.vars());

  assert!(start.elapsed() < Duration::from_secs(5), "the round waited {:?} before it refused", start.elapsed());
  assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
  assert_names_state_sources(&run, &round);
  assert_eq!(stub.request_count(), 0, "the round reached the server");
}

#[test]
#[ignore = "integration"]
fn test_round_refuses_cmd_before_it_starts_the_command() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  let round = round_sandbox(&sandbox, &stub);

  let run = sandbox
    .run_with(&["--session", "s1", "--max-wait", "60", "--message", "m", "--cmd", "touch started"], &round.vars());

  assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
  // A launch writes its record under the session directory before it starts the command: none was written.
  assert!(!round.round_home.join("sessions").exists(), "the round started its command");
  assert!(stub.bodies().is_empty(), "the round reached the server: {:?}", stub.bodies());
}

#[test]
#[ignore = "integration"]
fn test_round_names_the_state_source_variable_it_lacks() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  let round = round_sandbox(&sandbox, &stub);

  for name in ["IMPROVE_LAUNCH_DIR", "IMPROVE_DIRECTION", "IMPROVE_KIT_DIR"] {
    let mut vars = round.vars();
    vars.retain(|(var, _)| *var != name);

    let run = sandbox.run_with(&["--session", "s1", "--max-wait", "60", "--message", "m"], &vars);

    assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
    let error: Value = serde_json::from_str(&run.stderr).expect("stderr is a JSON error");
    assert!(error["error"].as_str().expect("the error is a message").contains(name), "stderr: {}", run.stderr);
  }
  assert!(stub.bodies().is_empty(), "the round reached the server: {:?}", stub.bodies());
}

#[test]
#[ignore = "integration"]
fn test_round_runs_an_offline_command_beside_its_refusal() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  let round = round_sandbox(&sandbox, &stub);

  let run = sandbox.run_cli_with(&["config", "get", "server"], &[], &round.vars());

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let server: Value = serde_json::from_str(&run.stdout).expect("stdout is the server section as JSON");
  assert_eq!(server["port"], json!(stub.port));
}

#[test]
#[ignore = "integration"]
fn test_trial_home_keeps_its_server_calls_inside_a_round_environment() {
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);
  let launch_dir = sandbox.root.path().join("improve").join("launch").join("20261119T000000Z-1-2");

  let run = sandbox.run_with(
    &["--session", "s1", "--max-wait", "60", "--message", "m"],
    &[
      ("CHARLIEBOT_HOME", sandbox.home.as_os_str()),
      ("HOME", sandbox.root.path().as_os_str()),
      ("IMPROVE_LAUNCH_DIR", launch_dir.as_os_str()),
    ],
  );

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  assert_eq!(stub.bodies().len(), 1, "the trial's trigger did not reach its server");
}

// ---------------------------------------------------------------------------
// ssh start failures: exit 3, nothing registered (the usage errors are golden cases)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_ssh_failure_exits_3_and_posts_nothing() {
  let sandbox = Sandbox::new();
  stub_ssh(&sandbox, "", "ssh: could not resolve hostname nonexistent.invalid\n", 255);
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "nonexistent.invalid",
  ]);

  assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
  assert!(run.stderr.to_lowercase().contains("ssh"), "stderr: {}", run.stderr);
  assert!(stub.bodies().is_empty(), "no registration reaches the server");
}

#[test]
#[ignore = "integration"]
fn test_unparseable_remote_pid_exits_3_and_posts_nothing() {
  let sandbox = Sandbox::new();
  stub_ssh(&sandbox, "not-a-pid\n", "", 0);
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "remote.example.com",
  ]);

  assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
  assert!(stub.bodies().is_empty(), "no registration reaches the server");
}

// ---------------------------------------------------------------------------
// The remote launcher through the stub ssh
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn test_launch_remote_parses_the_wrapper_pid() {
  let sandbox = Sandbox::new();
  let argv_file = stub_ssh(&sandbox, REMOTE_REPLY, "", 0);
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "remote.example.com",
    "--cwd",
    "/data",
  ]);

  // The pid is the stub's: no guard kills it, since it names no local process.
  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let launch_id = out["launch_id"].as_str().expect("text");
  let remote_dir = format!("/home/remote/charliebot_runs/{launch_id}");
  assert_eq!(out["host"], "remote.example.com");
  assert_eq!(out["pid"], 24680);
  assert_eq!(out["cwd"], "/data");
  assert_eq!(out["log"], format!("{remote_dir}/log"));
  assert_eq!(out["exit_file"], format!("{remote_dir}/exit"));
  let argv = recorded_argv(&argv_file);
  let wrapper = argv.last().expect("the ssh argv ends with the wrapper");
  assert_eq!(argv[argv.len() - 3..argv.len() - 1], ["bash", "-c"]);
  assert!(wrapper.contains("setsid bash -lc"), "{wrapper}");
  assert!(wrapper.contains("echo $!"), "{wrapper}");
  assert!(wrapper.contains("cd /data && "), "{wrapper}");
  assert!(wrapper.contains(&format!("< /dev/null > \"$HOME\"/charliebot_runs/{launch_id}/log 2>&1 & ")), "{wrapper}");
  assert!(wrapper.contains("echo \"$HOME\" && "), "{wrapper}");
  assert!(wrapper.contains(&format!("mkdir -p \"$HOME\"/charliebot_runs/{launch_id} && ")), "{wrapper}");
  let exit_write = format!("(echo hi\n); echo $? > \"$HOME\"/charliebot_runs/{launch_id}/exit");
  assert!(wrapper.contains(&exit_write), "{wrapper}");
  let bodies = stub.bodies();
  assert_eq!(
    bodies[0]["watch_targets"],
    json!([{
      "kind": "remote_pid",
      "host": "remote.example.com",
      "pid": 24680,
      "exit_file": format!("{remote_dir}/exit"),
      "log": format!("{remote_dir}/log"),
    }])
  );
}

#[test]
#[ignore = "integration"]
fn test_launch_remote_without_cwd_runs_in_the_login_directory() {
  let sandbox = Sandbox::new();
  let argv_file = stub_ssh(&sandbox, REMOTE_REPLY, "", 0);
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "remote.example.com",
  ]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  assert_eq!(out["cwd"], "~");
  let wrapper = recorded_argv(&argv_file).pop().expect("the ssh argv ends with the wrapper");
  // The launch id is random and may end in "cd", so the check names the directory change itself.
  assert!(!wrapper.contains("&& cd "), "{wrapper}");
}

#[test]
#[ignore = "integration"]
fn test_launch_remote_ssh_failure_raises() {
  let sandbox = Sandbox::new();
  stub_ssh(&sandbox, "", "ssh: connect to host nonexistent.invalid port 22: Name or service not known\n", 255);
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "nonexistent.invalid",
    "--cwd",
    "/tmp",
  ]);

  assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
  assert!(
    run.stderr.contains("ssh to nonexistent.invalid failed (rc=255): ssh: connect to host"),
    "stderr: {}",
    run.stderr
  );
}

#[test]
#[ignore = "integration"]
fn test_launch_remote_unparseable_pid_raises() {
  let sandbox = Sandbox::new();
  stub_ssh(&sandbox, "/home/remote\nnot-a-pid\n", "", 0);
  let stub = StubApi::start(Box::new(accept_any));
  sandbox.write_config(stub.port);

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "echo hi",
    "--host",
    "remote.example.com",
    "--cwd",
    "/tmp",
  ]);

  assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
  assert!(run.stderr.contains("failed to parse the remote home and PID from ssh stdout"), "stderr: {}", run.stderr);
}

/// Bounds the skip probe: 10 seconds around the ssh command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether `ssh localhost` works without a password: a test skips on this probe.
fn ssh_localhost_works() -> bool {
  let Ok(mut child) = Command::new("ssh")
    .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "localhost", "true"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
  else {
    return false;
  };
  let started = Instant::now();
  loop {
    match child.try_wait() {
      Ok(Some(status)) => return status.success(),
      Ok(None) if started.elapsed() < PROBE_TIMEOUT => std::thread::sleep(Duration::from_millis(10)),
      _ => {
        let _ = child.kill();
        let _ = child.wait();
        return false;
      }
    }
  }
}

#[test]
#[ignore = "integration"]
fn test_launch_remote_end_to_end_localhost() {
  if !ssh_localhost_works() {
    eprintln!("skipped: ssh localhost does not work without a password");
    return;
  }
  // The one test that runs the real ssh: it reads the user's keys, so HOME stays the user's home.
  let user_home = PathBuf::from(std::env::var_os("HOME").expect("HOME names the user's keys"));
  let sandbox = Sandbox::new();
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  let run = sandbox.run_as(
    &[
      "--session",
      "s1",
      "--max-wait",
      "60",
      "--message",
      "m",
      "--cmd",
      "sleep 0.2; echo hi",
      "--host",
      "localhost",
      "--cwd",
      "/tmp",
    ],
    &user_home,
  );

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let _group = GroupGuard(pid_of(&out));
  let remote_dir = user_home.join("charliebot_runs").join(out["launch_id"].as_str().expect("text"));
  assert_eq!(out["log"], remote_dir.join("log").display().to_string());
  let _cleanup = RemoteRunDir(remote_dir.clone());
  assert_eq!(wait_for_exit_code(&remote_dir.join("exit")), "0");
  assert!(fs::read_to_string(remote_dir.join("log")).expect("the log reads").contains("hi"));
}

#[test]
#[ignore = "integration"]
fn test_launch_remote_wrapper_runs_the_command_under_a_local_stub_ssh() {
  // The remote path without the network: the wrapper's own shell script runs here. A syntax or PID-capture slip in
  // it shows up as a missing exit file or a wrong PID. The `~/` working directory is the stub shell's HOME, which
  // is the sandbox root.
  let sandbox = Sandbox::new();
  stub_ssh_running_locally(&sandbox);
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);
  let work = sandbox.root.path().join("work");
  fs::create_dir_all(&work).expect("the working directory is created");

  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "60",
    "--message",
    "m",
    "--cmd",
    "pwd; sleep 0.2; echo hi",
    "--host",
    "localhost",
    "--cwd",
    "~/work",
  ]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let _group = GroupGuard(pid_of(&out));
  // The stub shell runs with the test's HOME, so the run directory lands under the sandbox root.
  let remote_dir = sandbox.root.path().join("charliebot_runs").join(out["launch_id"].as_str().expect("text"));
  assert_eq!(out["log"], remote_dir.join("log").display().to_string());
  let _cleanup = RemoteRunDir(remote_dir.clone());
  assert_eq!(wait_for_exit_code(&remote_dir.join("exit")), "0");
  let log = fs::read_to_string(remote_dir.join("log")).expect("the log reads");
  assert_eq!(log.lines().next(), Some(work.display().to_string().as_str()), "{log}");
  assert!(log.contains("hi"), "{log}");
}

#[test]
#[ignore = "integration"]
fn test_remote_cmd_longer_than_the_launch_timeout_registers_and_watches_its_end() {
  let sandbox = Sandbox::new();
  stub_ssh_running_locally(&sandbox);
  let stub = StubApi::start(registering(sandbox.home.clone()));
  sandbox.write_config(stub.port);

  // The command runs past the 30 s ssh launch bound. The run must return inside PROCESS_TIMEOUT (20 s), which is
  // inside that bound: a launch that waits for the command fails here.
  let run = sandbox.run(&[
    "--session",
    "s1",
    "--max-wait",
    "120",
    "--message",
    "m",
    "--cmd",
    "sleep 32; echo done",
    "--host",
    "remote.example.com",
  ]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let _group = GroupGuard(pid_of(&out));
  // The watched PID is the started command's wrapper, which is still running when the registration returns.
  assert!(kill(Pid::from_raw(pid_of(&out)), None).is_ok(), "the started wrapper is not running");
  let remote_dir = sandbox.root.path().join("charliebot_runs").join(out["launch_id"].as_str().expect("text"));
  let _cleanup = RemoteRunDir(remote_dir.clone());
  let bodies = stub.bodies();
  assert_eq!(bodies[0]["watch_targets"][0]["exit_file"], out["exit_file"]);
  // The watch's exit file reports the command's end: its code, and its output in the log.
  assert_eq!(wait_for_exit_code_within(&remote_dir.join("exit"), Duration::from_secs(60)), "0");
  assert!(fs::read_to_string(remote_dir.join("log")).expect("the log reads").contains("done"));
}
