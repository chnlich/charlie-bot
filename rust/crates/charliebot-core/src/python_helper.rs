//! Runs one Python helper script under the server's interpreter and reads back one JSON value.
//!
//! The interpreter is `<repo>/.venv/bin/python`, the one the Python server runs in. The helper starts with the repo
//! root as its working directory and as its `PYTHONPATH`, so it imports the checkout's packages. Its environment is
//! the server's, plus the overrides the caller names. Its stdout must hold one JSON value; its stderr carries logs and
//! tracebacks.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The interpreter's path under the repo root.
const INTERPRETER: &str = ".venv/bin/python";
/// How often a timed wait checks whether the child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

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

/// Why a helper run failed. Every variant that ran the helper carries its stderr.
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
    let interpreter = self.repo_root.join(INTERPRETER);
    if !interpreter.exists() {
      return Err(HelperError::MissingInterpreter(interpreter));
    }
    let mut command = Command::new(&interpreter);
    command
      .arg(self.repo_root.join(script))
      .args(args)
      .current_dir(&self.repo_root)
      .env("PYTHONPATH", &self.repo_root)
      .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    for (key, value) in &self.env {
      command.env(key, value);
    }
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
}

/// Wait for `child` and collect its piped stdout and stderr. `stdin`, when given, is written to the child's stdin and
/// then closed, so the child must have been spawned with stdin piped. A child still running after `timeout` is killed.
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
    Some(limit) => match wait_within(&mut child, limit)? {
      Some(status) => (status, None),
      None => {
        child.kill()?;
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

/// Poll `child` until it exits or `limit` passes. `None` means it is still running at the limit.
fn wait_within(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
  let deadline = Instant::now() + limit;
  loop {
    if let Some(status) = child.try_wait()? {
      return Ok(Some(status));
    }
    if Instant::now() >= deadline {
      return Ok(None);
    }
    thread::sleep(POLL_INTERVAL);
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

  use serde_json::json;

  use super::*;

  /// The checkout that holds this crate; its `.venv` is the interpreter the helper tests start.
  fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
  }

  fn write_script(directory: &Path, body: &str) -> PathBuf {
    let path = directory.join("helper.py");
    fs::write(&path, body).unwrap();
    path
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
  fn helper_past_its_timeout_is_killed() {
    let scripts = tempfile::tempdir().unwrap();
    let script = write_script(scripts.path(), "import time\ntime.sleep(60)\n");
    let started = Instant::now();

    let error = PythonHelper::new(&repo_root()).run(&script, &[], None, Some(Duration::from_millis(200))).unwrap_err();

    assert!(matches!(error, HelperError::TimedOut { .. }), "expected a timeout, got {error}");
    assert!(started.elapsed() < Duration::from_secs(30));
  }
}
