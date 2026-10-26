//! Runs Python helper scripts under the server's interpreter: a run that reads back one JSON value, or a long-lived
//! helper that answers one JSON line per request.
//!
//! The interpreter is `<repo>/.venv/bin/python`, the one the Python server runs in. The helper starts with the repo
//! root as its working directory and as its `PYTHONPATH`, so it imports the checkout's packages. Its environment is
//! the server's, plus the overrides the caller names. Every helper leads its own process group, so killing the group
//! also kills the children the helper started.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use wait_timeout::ChildExt;

/// The interpreter's path under the repo root.
const INTERPRETER: &str = ".venv/bin/python";

/// Starts helper scripts under the interpreter of one checkout.
pub struct PythonHelper {
  repo_root: PathBuf,
  env: Vec<(OsString, OsString)>,
}

/// What a finished or timed-out child left behind.
pub enum Capture {
  /// The child exited on its own; `output` holds its status and both pipes.
  Exited(Output),
  /// The child ran past its timeout and was killed; `stderr` is what it wrote before the kill.
  TimedOut { after: Duration, stderr: Vec<u8> },
}

/// Why a helper run failed. Every variant that ran the helper carries its stderr, except a long-lived helper's, whose
/// stderr goes to the server log.
#[derive(Debug)]
pub enum HelperError {
  /// `<repo>/.venv/bin/python` does not exist.
  MissingInterpreter(PathBuf),
  /// The interpreter exists, but the process did not start: the repo root is missing, or the file is not executable.
  Start { interpreter: PathBuf, source: io::Error },
  /// The helper exited with a nonzero status, or was killed by a signal (`code` is `None`).
  Exit { code: Option<i32>, stderr: String },
  /// The helper exited zero, but its stdout is not one JSON value.
  Output { code: Option<i32>, stderr: String, source: serde_json::Error },
  /// The helper ran past its timeout and was killed.
  TimedOut { after: Duration, stderr: String },
  /// Writing the helper's stdin or waiting on it failed.
  Io(io::Error),
  /// A long-lived helper stopped answering: it exited, its reply was not one JSON value, or the server stopped it.
  /// Every later request to that helper fails with the same `reason`.
  Unanswered { label: &'static str, reason: String },
}

impl PythonHelper {
  /// A runner for the checkout at `repo_root`, with the server's environment.
  pub fn new(repo_root: &Path) -> PythonHelper {
    PythonHelper { repo_root: repo_root.to_path_buf(), env: Vec::new() }
  }

  /// Set `key` in the helper's environment, replacing the inherited value.
  pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> PythonHelper {
    self.env.push((key.into(), value.into()));
    self
  }

  /// Run `script` (relative to the repo root, or absolute) with `args`, write `stdin` to it when given, and read its
  /// stdout as one JSON value. `timeout` bounds the run; `None` waits however long the helper takes.
  pub fn run(
    &self,
    script: &Path,
    args: &[&OsStr],
    stdin: Option<&[u8]>,
    timeout: Option<Duration>,
  ) -> Result<Value, HelperError> {
    let interpreter = self.interpreter()?;
    let mut command = self.command(&interpreter, script, args);
    command
      .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    let child = command.spawn().map_err(|source| HelperError::Start { interpreter: interpreter.clone(), source })?;
    let output = match capture(child, stdin, timeout).map_err(HelperError::Io)? {
      Capture::Exited(output) => output,
      Capture::TimedOut { after, stderr } => {
        return Err(HelperError::TimedOut { after, stderr: lossy(&stderr) });
      }
    };
    let code = output.status.code();
    if !output.status.success() {
      return Err(HelperError::Exit { code, stderr: lossy(&output.stderr) });
    }
    serde_json::from_slice(&output.stdout).map_err(|source| HelperError::Output {
      code,
      stderr: lossy(&output.stderr),
      source,
    })
  }

  /// Start `script` with `args` as a long-lived helper, under the interpreter, directory, `PYTHONPATH` and environment
  /// that [`PythonHelper::run`] uses. The helper reads one JSON line per request on its stdin and writes one JSON line
  /// per reply on its stdout. Its stderr goes to the server log as `python_helper_stderr` events that carry `label`.
  /// Nothing restarts the helper once it stops; [`LongLived::stop`] kills it.
  pub fn start_long_lived(
    &self,
    script: &Path,
    args: &[&OsStr],
    label: &'static str,
  ) -> Result<LongLived, HelperError> {
    let interpreter = self.interpreter()?;
    let mut child = self
      .command(&interpreter, script, args)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|source| HelperError::Start { interpreter, source })?;
    let stdin = child.stdin.take().expect("the child was spawned with stdin piped");
    let stdout = child.stdout.take().expect("the child was spawned with stdout piped");
    let stderr = child.stderr.take().expect("the child was spawned with stderr piped");
    forward_stderr(stderr, label);
    let pgid = child.id() as libc::pid_t;
    Ok(LongLived {
      label,
      pgid,
      protocol: Mutex::new(Protocol { stdin, stdout: BufReader::new(stdout) }),
      lifecycle: Mutex::new(Lifecycle { child: Some(child), reason: None }),
    })
  }

  fn interpreter(&self) -> Result<PathBuf, HelperError> {
    let interpreter = self.repo_root.join(INTERPRETER);
    if interpreter.exists() { Ok(interpreter) } else { Err(HelperError::MissingInterpreter(interpreter)) }
  }

  /// The command for `script` with `args`: the interpreter, the repo root as directory and `PYTHONPATH`, the caller's
  /// environment overrides, and a process group of its own.
  fn command(&self, interpreter: &Path, script: &Path, args: &[&OsStr]) -> Command {
    let mut command = Command::new(interpreter);
    command
      .arg(self.repo_root.join(script))
      .args(args)
      .current_dir(&self.repo_root)
      .env("PYTHONPATH", &self.repo_root)
      .process_group(0);
    for (key, value) in &self.env {
      command.env(key, value);
    }
    command
  }
}

/// A helper started by [`PythonHelper::start_long_lived`]. One request is in flight at a time: a second request waits
/// until the first reply is read.
pub struct LongLived {
  label: &'static str,
  /// The helper's pid, which is also its process group id: the helper leads the group it was started in.
  pgid: libc::pid_t,
  protocol: Mutex<Protocol>,
  lifecycle: Mutex<Lifecycle>,
}

/// The pipes of a helper. The lock is held from a request's write to its reply, so replies cannot cross requests.
struct Protocol {
  stdin: ChildStdin,
  stdout: BufReader<ChildStdout>,
}

struct Lifecycle {
  /// The child until it is reaped: it is taken once the helper exits or the server stops it.
  child: Option<Child>,
  /// Why the helper stopped answering. Set once; every later request fails with it.
  reason: Option<String>,
}

/// How a helper stopped answering, as [`LongLived::retire`] receives it.
enum Loss {
  /// The helper closed its stdout: it exited, and its exit status is the reason.
  Closed,
  /// The helper is still running, but its reply is unusable: the server kills it and reports this text.
  Broken(String),
}

impl LongLived {
  /// Read one line the helper prints without a request: the ready line a helper prints once it has loaded its model.
  pub fn receive(&self) -> Result<Value, HelperError> {
    let mut protocol = self.protocol.lock().expect("the protocol lock is never poisoned");
    self.ensure_running()?;
    self.read_reply(&mut protocol)
  }

  /// Write `request` as one JSON line, then read the helper's reply line. A helper that exits, or whose reply is not
  /// one JSON value, fails this request and every later one.
  pub fn request(&self, request: &Value) -> Result<Value, HelperError> {
    let mut protocol = self.protocol.lock().expect("the protocol lock is never poisoned");
    self.ensure_running()?;
    let mut line = request.to_string();
    line.push('\n');
    let written = protocol.stdin.write_all(line.as_bytes());
    if let Err(error) = written.and_then(|()| protocol.stdin.flush()) {
      return Err(self.retire(Loss::Broken(format!("writing its request failed: {error}"))));
    }
    self.read_reply(&mut protocol)
  }

  /// Kill the helper's process group and reap the helper. A request in flight fails, and so does every later one.
  /// A helper that already stopped is left as it is.
  pub fn stop(&self) {
    let mut lifecycle = self.lifecycle.lock().expect("the lifecycle lock is never poisoned");
    let Some(mut child) = lifecycle.child.take() else {
      return;
    };
    log_failure(self.label, "kill", kill_group(self.pgid));
    log_failure(self.label, "wait", child.wait().map(drop));
    lifecycle.reason.get_or_insert_with(|| "the server stopped it".to_string());
  }

  fn ensure_running(&self) -> Result<(), HelperError> {
    let lifecycle = self.lifecycle.lock().expect("the lifecycle lock is never poisoned");
    match &lifecycle.reason {
      Some(reason) => Err(HelperError::Unanswered { label: self.label, reason: reason.clone() }),
      None => Ok(()),
    }
  }

  fn read_reply(&self, protocol: &mut Protocol) -> Result<Value, HelperError> {
    let mut line = String::new();
    match protocol.stdout.read_line(&mut line) {
      Ok(0) => Err(self.retire(Loss::Closed)),
      Ok(_) => serde_json::from_str::<Value>(&line)
        .map_err(|error| self.retire(Loss::Broken(format!("its reply is not one JSON value: {error}")))),
      Err(error) => Err(self.retire(Loss::Broken(format!("reading its reply failed: {error}")))),
    }
  }

  /// Reap a helper that stopped answering, kill what its group still holds, and record why. The first reason recorded
  /// stays: a request that fails after a stop reports the stop.
  fn retire(&self, loss: Loss) -> HelperError {
    let mut lifecycle = self.lifecycle.lock().expect("the lifecycle lock is never poisoned");
    if let Some(mut child) = lifecycle.child.take() {
      let reason = match loss {
        Loss::Closed => None,
        Loss::Broken(reason) => {
          // The helper may still be running, so kill it before waiting for it.
          log_failure(self.label, "kill", kill_group(self.pgid));
          Some(reason)
        }
      };
      let status = child.wait();
      // The helper's children may outlive it in the group; they hold the pipes and must go too.
      log_failure(self.label, "kill", kill_group(self.pgid));
      let reason = reason.unwrap_or_else(|| match status {
        Ok(status) => describe_exit(status),
        Err(error) => format!("waiting for its exit failed: {error}"),
      });
      lifecycle.reason.get_or_insert(reason);
    }
    let reason = lifecycle.reason.clone().expect("a helper without a child has recorded its reason");
    HelperError::Unanswered { label: self.label, reason }
  }
}

impl Drop for LongLived {
  fn drop(&mut self) {
    self.stop();
  }
}

/// Wait for `child` and collect its piped stdout and stderr. `stdin`, when given, is written to the child's stdin and
/// then closed, so the child must have been spawned with stdin piped. A child still running after `timeout` is killed.
/// A child that leads its own process group (as [`PythonHelper`] starts every helper) takes the group with it, so the
/// grandchildren that hold its pipes open cannot keep this call waiting.
pub fn capture(mut child: Child, stdin: Option<&[u8]>, timeout: Option<Duration>) -> io::Result<Capture> {
  let writer = stdin.map(|bytes| {
    let mut pipe = child.stdin.take().expect("the child was spawned with stdin piped");
    let bytes = bytes.to_vec();
    thread::spawn(move || write_stdin(&mut pipe, &bytes))
  });
  let stdout = child.stdout.take().expect("the child was spawned with stdout piped");
  let stderr = child.stderr.take().expect("the child was spawned with stderr piped");
  let stdout_reader = thread::spawn(move || read_all(stdout));
  let stderr_reader = thread::spawn(move || read_all(stderr));

  let (status, killed_after) = match timeout {
    None => (child.wait()?, None),
    Some(limit) => match child.wait_timeout(limit)? {
      Some(status) => (status, None),
      None => {
        kill_timed_out(&mut child)?;
        (child.wait()?, Some(limit))
      }
    },
  };
  let stdout = stdout_reader.join().expect("the stdout reader does not panic")?;
  let stderr = stderr_reader.join().expect("the stderr reader does not panic")?;
  if let Some(writer) = writer {
    writer.join().expect("the stdin writer does not panic")?;
  }
  Ok(match killed_after {
    Some(after) => Capture::TimedOut { after, stderr },
    None => Capture::Exited(Output { status, stdout, stderr }),
  })
}

/// Kill a child that ran past its timeout. The whole process group goes when the child leads one; any other child is
/// killed alone.
fn kill_timed_out(child: &mut Child) -> io::Result<()> {
  let pid = child.id() as libc::pid_t;
  if unsafe { libc::getpgid(pid) } == pid {
    kill_group(pid)?;
  }
  child.kill()
}

/// Send SIGKILL to every process in group `pgid`. A group with no members left is not an error: the helper's children
/// may already have exited.
fn kill_group(pgid: libc::pid_t) -> io::Result<()> {
  if unsafe { libc::killpg(pgid, libc::SIGKILL) } == -1 {
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ESRCH) {
      return Err(error);
    }
  }
  Ok(())
}

/// Log a failed kill or wait on a helper's process group or child. The caller carries on, so the failure must be logged.
fn log_failure(label: &'static str, step: &'static str, result: io::Result<()>) {
  if let Err(error) = result {
    tracing::error!(helper = label, step, error = %error, "python_helper_failed");
  }
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
  let mut bytes = Vec::new();
  reader.read_to_end(&mut bytes)?;
  Ok(bytes)
}

/// Write all of `bytes`. A child that exits before reading its whole stdin is not an error here: its exit status is
/// the result the caller reads.
fn write_stdin(pipe: &mut ChildStdin, bytes: &[u8]) -> io::Result<()> {
  match pipe.write_all(bytes) {
    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
    result => result,
  }
}

/// Forward each line of a long-lived helper's stderr to the server log.
fn forward_stderr(stderr: ChildStderr, label: &'static str) {
  thread::spawn(move || {
    for line in BufReader::new(stderr).lines() {
      match line {
        Ok(line) => tracing::info!(helper = label, line = %line, "python_helper_stderr"),
        Err(error) => {
          tracing::error!(helper = label, error = %error, "python_helper_stderr_failed");
          return;
        }
      }
    }
  });
}

fn describe_exit(status: ExitStatus) -> String {
  match status.code() {
    Some(code) => format!("exited with code {code}"),
    None => format!(
      "was killed by signal {}",
      status.signal().expect("a process without an exit code was killed by a signal")
    ),
  }
}

fn lossy(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).into_owned()
}

impl fmt::Display for HelperError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      HelperError::MissingInterpreter(path) => {
        write!(formatter, "Python helper interpreter is missing: {}", path.display())
      }
      HelperError::Start { interpreter, source } => {
        write!(formatter, "Python helper interpreter {} did not start: {source}", interpreter.display())
      }
      HelperError::Exit { code, stderr } => {
        write!(formatter, "Python helper failed (exit code {}){}", exit_code(*code), stderr_suffix(stderr))
      }
      HelperError::Output { code, stderr, source } => write!(
        formatter,
        "Python helper printed no JSON value ({source}; exit code {}){}",
        exit_code(*code),
        stderr_suffix(stderr)
      ),
      HelperError::TimedOut { after, stderr } => {
        write!(formatter, "Python helper timed out after {} s{}", after.as_secs_f64(), stderr_suffix(stderr))
      }
      HelperError::Io(source) => write!(formatter, "Python helper I/O failed: {source}"),
      HelperError::Unanswered { label, reason } => {
        write!(formatter, "Python helper {label} stopped answering: {reason}")
      }
    }
  }
}

impl std::error::Error for HelperError {}

fn exit_code(code: Option<i32>) -> String {
  code.map_or_else(|| "none, killed by a signal".to_string(), |code| code.to_string())
}

fn stderr_suffix(stderr: &str) -> String {
  let trimmed = stderr.trim_end();
  if trimmed.is_empty() { String::new() } else { format!(": {trimmed}") }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::time::Instant;

  use serde_json::json;

  use super::*;

  /// How often a test polls for a process to be gone.
  const POLL_INTERVAL: Duration = Duration::from_millis(5);

  /// The checkout that holds this crate; its `.venv` is the interpreter the helper tests start.
  fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
  }

  fn write_script(directory: &Path, body: &str) -> PathBuf {
    let path = directory.join("helper.py");
    fs::write(&path, body).unwrap();
    path
  }

  /// Whether `pid` has exited: its `/proc` entry is gone, or it is a zombie that nothing has reaped.
  fn process_gone(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
      Err(_) => true,
      Ok(stat) => stat.rsplit(')').next().and_then(|rest| rest.trim().chars().next()) == Some('Z'),
    }
  }

  /// Wait until `pid` has exited. The kill is asynchronous, so a check right after it can still see the process.
  fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_gone(pid) {
      assert!(Instant::now() < deadline, "process {pid} is still running");
      thread::sleep(POLL_INTERVAL);
    }
  }

  #[test]
  fn helper_prints_one_json_value_from_its_checkout() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(
      scripts.path(),
      "import json, os, sys\n\
       print(json.dumps({'args': sys.argv[1:], 'cwd': os.getcwd(), 'python_path': os.environ['PYTHONPATH'],\n\
                         'extra': os.environ['SYNTHETIC_HELPER_VALUE'], 'on_path': sys.path[0:1]}))\n",
    );
    let repo = repo_root();

    let value = PythonHelper::new(&repo)
      .env("SYNTHETIC_HELPER_VALUE", "from-the-caller")
      .run(&script, &[OsStr::new("/synthetic/report.ncu-rep")], None, None)
      .unwrap();

    assert_eq!(
      value,
      json!({
        "args": ["/synthetic/report.ncu-rep"],
        "cwd": repo.display().to_string(),
        "python_path": repo.display().to_string(),
        "extra": "from-the-caller",
        "on_path": [scripts.path().display().to_string()],
      })
    );
  }

  #[test]
  fn helper_reads_stdin_when_given() {
    let scripts = tempfile::tempdir().unwrap();
    let script =
      write_script(scripts.path(), "import json, sys\njson.dump({'echo': json.load(sys.stdin)}, sys.stdout)\n");

    let value = PythonHelper::new(&repo_root()).run(&script, &[], Some(br#"{"n": 7}"#), None).unwrap();

    assert_eq!(value, json!({"echo": {"n": 7}}));
  }

  #[test]
  fn nonzero_exit_carries_the_code_and_stderr() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(scripts.path(), "import sys\nsys.stderr.write('synthetic failure\\n')\nsys.exit(3)\n");

    let error = PythonHelper::new(&repo_root()).run(&script, &[], None, None).unwrap_err();

    let HelperError::Exit { code, stderr } = &error else { panic!("expected an exit error, got {error}") };
    assert_eq!(*code, Some(3));
    assert_eq!(stderr, "synthetic failure\n");
    assert_eq!(error.to_string(), "Python helper failed (exit code 3): synthetic failure");
  }

  #[test]
  fn stdout_that_is_not_one_json_value_is_an_error() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(scripts.path(), "print('plain text, not JSON')\n");

    let error = PythonHelper::new(&repo_root()).run(&script, &[], None, None).unwrap_err();

    let HelperError::Output { code, stderr, .. } = &error else { panic!("expected an output error, got {error}") };
    assert_eq!(*code, Some(0));
    assert_eq!(stderr, "");
  }

  #[test]
  fn missing_interpreter_is_an_error_naming_the_path() {
    let checkout = tempfile::tempdir().unwrap();
    let script = write_script(checkout.path(), "print('{}')\n");

    let error = PythonHelper::new(checkout.path()).run(&script, &[], None, None).unwrap_err();

    let HelperError::MissingInterpreter(path) = &error else { panic!("expected a missing interpreter, got {error}") };
    assert_eq!(path, &checkout.path().join(INTERPRETER));
  }

  #[test]
  fn interpreter_that_cannot_start_is_an_error() {
    let checkout = tempfile::tempdir().unwrap();
    let interpreter = checkout.path().join(INTERPRETER);
    fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
    fs::write(&interpreter, "not executable\n").unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o644)).unwrap();
    let script = write_script(checkout.path(), "print('{}')\n");

    let error = PythonHelper::new(checkout.path()).run(&script, &[], None, None).unwrap_err();

    assert!(matches!(error, HelperError::Start { .. }), "expected a start error, got {error}");
  }

  #[test]
  fn timeout_kills_the_helper_and_the_children_it_started() {
    let scripts = tempfile::tempdir().unwrap();
    let pid_file = scripts.path().join("child.pid");
    let script = write_script(
      scripts.path(),
      "import subprocess, sys, time\n\
       child = subprocess.Popen(['sleep', '60'])\n\
       open(sys.argv[1], 'w').write(str(child.pid))\n\
       time.sleep(60)\n",
    );
    let started = Instant::now();

    let error = PythonHelper::new(&repo_root())
      .run(&script, &[pid_file.as_os_str()], None, Some(Duration::from_secs(1)))
      .unwrap_err();

    assert!(matches!(error, HelperError::TimedOut { .. }), "expected a timeout, got {error}");
    // The grandchild holds the stdout pipe, so a capture that kept only the helper would wait out its 60 s sleep.
    assert!(started.elapsed() < Duration::from_secs(30));
    let child_pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    wait_until_gone(child_pid);
  }

  #[test]
  fn long_lived_helper_answers_each_request_with_its_reply() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(
      scripts.path(),
      "import json, sys\n\
       print(json.dumps({'status': 'ready'}), flush=True)\n\
       for line in sys.stdin:\n\
       \x20 print(json.dumps({'echo': json.loads(line)}), flush=True)\n",
    );
    let helper = PythonHelper::new(&repo_root()).start_long_lived(&script, &[], "echo").unwrap();

    assert_eq!(helper.receive().unwrap(), json!({"status": "ready"}));
    assert_eq!(helper.request(&json!({"n": 1})).unwrap(), json!({"echo": {"n": 1}}));
    assert_eq!(helper.request(&json!({"n": 2})).unwrap(), json!({"echo": {"n": 2}}));
  }

  #[test]
  fn requests_do_not_interleave_on_a_shared_helper() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(
      scripts.path(),
      "import json, sys, time\n\
       print(json.dumps({'status': 'ready'}), flush=True)\n\
       for line in sys.stdin:\n\
       \x20 time.sleep(0.02)\n\
       \x20 print(json.dumps({'echo': json.loads(line)}), flush=True)\n",
    );
    let helper = PythonHelper::new(&repo_root()).start_long_lived(&script, &[], "echo").unwrap();
    helper.receive().unwrap();

    thread::scope(|scope| {
      for thread_id in 0..4 {
        let helper = &helper;
        scope.spawn(move || {
          for i in 0..5 {
            let request = json!({"thread": thread_id, "i": i});
            assert_eq!(helper.request(&request).unwrap(), json!({"echo": request}));
          }
        });
      }
    });
  }

  #[test]
  fn helper_that_exits_fails_the_pending_request_and_every_later_one() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(
      scripts.path(),
      "import sys\n\
       print('{\"status\": \"ready\"}', flush=True)\n\
       sys.stdin.readline()\n\
       sys.exit(3)\n",
    );
    let helper = PythonHelper::new(&repo_root()).start_long_lived(&script, &[], "crash").unwrap();
    helper.receive().unwrap();

    let pending = helper.request(&json!({"n": 1})).unwrap_err();
    let later = helper.request(&json!({"n": 2})).unwrap_err();

    let expected = "Python helper crash stopped answering: exited with code 3";
    assert_eq!(pending.to_string(), expected);
    assert_eq!(later.to_string(), expected);
  }

  #[test]
  fn helper_whose_reply_is_not_json_is_killed_and_fails_every_request() {
    let scripts = tempfile::tempdir().unwrap();
    let pid_file = scripts.path().join("helper.pid");
    let script = write_script(
      scripts.path(),
      "import os, sys, time\n\
       open(sys.argv[1], 'w').write(str(os.getpid()))\n\
       print('{\"status\": \"ready\"}', flush=True)\n\
       sys.stdin.readline()\n\
       print('not json', flush=True)\n\
       time.sleep(60)\n",
    );
    let helper = PythonHelper::new(&repo_root()).start_long_lived(&script, &[pid_file.as_os_str()], "garbled").unwrap();
    helper.receive().unwrap();

    let error = helper.request(&json!({"n": 1})).unwrap_err().to_string();

    assert!(error.starts_with("Python helper garbled stopped answering: its reply is not one JSON value: "), "{error}");
    assert_eq!(helper.request(&json!({"n": 2})).unwrap_err().to_string(), error);
    let helper_pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    wait_until_gone(helper_pid);
  }

  #[test]
  fn stop_kills_the_helper_and_its_children_and_fails_later_requests() {
    let scripts = tempfile::tempdir().unwrap();
    let pid_file = scripts.path().join("child.pid");
    let script = write_script(
      scripts.path(),
      "import subprocess, sys\n\
       child = subprocess.Popen(['sleep', '60'])\n\
       open(sys.argv[1], 'w').write(str(child.pid))\n\
       print('{\"status\": \"ready\"}', flush=True)\n\
       for line in sys.stdin:\n\
       \x20 pass\n",
    );
    let helper = PythonHelper::new(&repo_root()).start_long_lived(&script, &[pid_file.as_os_str()], "sleeper").unwrap();
    helper.receive().unwrap();

    helper.stop();

    let child_pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    wait_until_gone(child_pid);
    assert_eq!(
      helper.request(&json!({"n": 1})).unwrap_err().to_string(),
      "Python helper sleeper stopped answering: the server stopped it"
    );
  }
}
