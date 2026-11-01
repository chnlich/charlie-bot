//! The `schedule-trigger` process tests. Each runs the built `charliebot` against a stub internal API on 127.0.0.1
//! and, for `--host`, a stub `ssh` that comes first on the CLI's PATH. Every CLI process gets a cleared environment
//! and only the variables named here, with CHARLIEBOT_HOME and HOME inside the test's temp root. Each test that starts
//! a wrapper kills its process group before it returns. Only the ssh localhost test runs the real ssh.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl StubApi {
  fn start(handler: Handler) -> StubApi {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    listener.set_nonblocking(true).expect("the listener goes nonblocking");
    let port = listener.local_addr().expect("the listener has an address").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
      let bodies = bodies.clone();
      let stop = stop.clone();
      std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              stream.set_nonblocking(false).expect("the connection goes blocking");
              stream.set_read_timeout(Some(PROCESS_TIMEOUT)).expect("the read timeout is set");
              let request = read_request(&mut stream);
              if !request.post {
                // A refusal's version-skew probe: an answer that names no skew.
                write_response(&mut stream, 200, &json!({ "sha": "cafe567", "started_at": FIRE_AT }));
                continue;
              }
              bodies.lock().expect("the body lock").push(request.body.clone());
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
    StubApi { port, bodies, stop, thread: Some(thread) }
  }

  fn bodies(&self) -> Vec<Value> {
    self.bodies.lock().expect("the body lock").clone()
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

/// One request the stub read: whether it is a POST, and its JSON body (a GET carries none, so its body is null).
struct Request {
  post: bool,
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
  Request { post: head.starts_with("POST "), body }
}

fn write_response(stream: &mut TcpStream, status: u16, answer: &Value) {
  let body = serde_json::to_vec(answer).expect("the answer serializes");
  let reason = match status {
    200 => "OK",
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

  /// Run `charliebot schedule-trigger ARGS` with HOME in the temp root.
  fn run(&self, args: &[&str]) -> Run {
    self.run_as(args, self.root.path())
  }

  /// Run `charliebot schedule-trigger ARGS` with the given HOME: the environment holds only these four variables.
  fn run_as(&self, args: &[&str], home: &Path) -> Run {
    let path = format!("{}:/usr/bin:/bin", self.bin.display());
    let mut child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .arg("schedule-trigger")
      .args(args)
      .current_dir(self.root.path())
      .env_clear()
      .env("CHARLIEBOT_HOME", &self.home)
      .env("HOME", home)
      .env("PATH", path)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .expect("the CLI starts");
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

/// A stub `ssh` in the sandbox's bin directory that runs its remote command in this shell: `ssh_cmd` puts five `-o`
/// option pairs and the host before the command, those eleven arguments are dropped, and the rest is joined with
/// spaces and re-parsed, as ssh hands it to the remote login shell. The wrapper's mkdir, setsid, PID capture and exit
/// file then run for real, with no network.
fn stub_ssh_running_locally(sandbox: &Sandbox) {
  charliebot_shell_harness::write_executable(&sandbox.bin.join("ssh"), "#!/bin/sh\nshift 11\neval \"$*\"\n");
}

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
  let started = Instant::now();
  loop {
    if let Ok(text) = fs::read_to_string(path)
      && text.ends_with('\n')
    {
      return text.trim().to_string();
    }
    assert!(started.elapsed() < PROCESS_TIMEOUT, "{} never completed", path.display());
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

#[test]
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
// ssh start failures: exit 3, nothing registered (the usage errors are golden cases)
// ---------------------------------------------------------------------------

#[test]
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
fn test_launch_remote_parses_the_wrapper_pid() {
  let sandbox = Sandbox::new();
  let argv_file = stub_ssh(&sandbox, "24680\n", "", 0);
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
  let remote_dir = format!("/tmp/charliebot_runs/{launch_id}");
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
  assert!(wrapper.contains("} < /dev/null >"), "{wrapper}");
  assert!(wrapper.contains(&format!("{{ (echo hi\n); echo $? > {remote_dir}/exit; }}")), "{wrapper}");
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
fn test_launch_remote_without_cwd_runs_in_the_login_directory() {
  let sandbox = Sandbox::new();
  let argv_file = stub_ssh(&sandbox, "24680\n", "", 0);
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
fn test_launch_remote_unparseable_pid_raises() {
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
    "--cwd",
    "/tmp",
  ]);

  assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
  assert!(run.stderr.contains("failed to parse remote PID from ssh stdout"), "stderr: {}", run.stderr);
}

/// Bounds the skip probe: Python's `subprocess.run(..., timeout=10)` around the same ssh command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether `ssh localhost` works without a password: the probe the Python test skips on.
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
  let remote_dir = PathBuf::from(format!("/tmp/charliebot_runs/{}", out["launch_id"].as_str().expect("text")));
  let _cleanup = RemoteRunDir(remote_dir.clone());
  assert_eq!(wait_for_exit_code(&remote_dir.join("exit")), "0");
  assert!(fs::read_to_string(remote_dir.join("log")).expect("the log reads").contains("hi"));
}

#[test]
fn test_launch_remote_wrapper_runs_the_command_under_a_local_stub_ssh() {
  // The remote path without the network: the wrapper's own shell script runs here, so a syntax or PID-capture slip
  // in it shows up as a missing exit file or a wrong PID.
  let sandbox = Sandbox::new();
  stub_ssh_running_locally(&sandbox);
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
    "sleep 0.2; echo hi",
    "--host",
    "localhost",
    "--cwd",
    "/tmp",
  ]);

  assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
  let out: Value = serde_json::from_str(&run.stdout).expect("stdout is JSON");
  let _group = GroupGuard(pid_of(&out));
  let remote_dir = PathBuf::from(format!("/tmp/charliebot_runs/{}", out["launch_id"].as_str().expect("text")));
  let _cleanup = RemoteRunDir(remote_dir.clone());
  assert_eq!(wait_for_exit_code(&remote_dir.join("exit")), "0");
  assert!(fs::read_to_string(remote_dir.join("log")).expect("the log reads").contains("hi"));
}
