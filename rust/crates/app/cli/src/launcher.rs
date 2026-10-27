//! The detached-process launcher (`src/runtime/cli/launcher.py`): one wrapper per start, on this host or over ssh.
//!
//! Every start runs the wrapper `(CMD); echo $? > EXIT_FILE` under `bash -lc`, with stdin from /dev/null and stdout
//! and stderr going to the launch's log file. The watched PID is the wrapper itself: the local child's PID, or the
//! remote shell's `$!` over ssh. The wrapper leads its own session on both paths (`setsid`), so its PID is also the
//! process group that a rejected registration kills. Only this CLI starts detached commands.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use charliebot_core::{pyfmt, ssh};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::waitpid;
use nix::unistd::{Pid, setsid};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;

/// The remote side of one launch stages its log, exit file and PID file under this directory, named by launch id.
pub const REMOTE_RUNS_DIR: &str = "/tmp/charliebot_runs";

/// `timeouts.SSH_LAUNCH_TIMEOUT`: the bound on one ssh start and on one remote kill.
const SSH_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The time part of a launch id: `%Y%m%dT%H%M%S` of the UTC start.
const LAUNCH_TIME: &[BorrowedFormatItem<'static>] = format_description!("[year][month][day]T[hour][minute][second]");

/// One started wrapper: the watched PID, its host (None = local), and the two paths its wake reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
  pub pid: i32,
  pub host: Option<String>,
  pub log: String,
  pub exit_file: String,
}

/// A start that did not produce a registrable wrapper.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchError {
  /// The ssh start failed or its output held no PID: the CLI exits 3 and registers nothing.
  Ssh(String),
  /// A local file, spawn or ssh-setup failure: the CLI exits 1, as an uncaught Python exception does.
  Io(String),
}

/// `mint_launch_id`: `<UTC %Y%m%dT%H%M%S>-<6 hex>`, the last part three random bytes in lowercase hex.
pub fn mint_launch_id(started_at: OffsetDateTime) -> String {
  let mut random = [0u8; 3];
  getrandom::fill(&mut random).expect("the OS random source is available");
  let stamp = started_at.format(LAUNCH_TIME).expect("a UTC datetime formats as a launch id");
  let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
  format!("{stamp}-{hex}")
}

/// `shlex.quote`: the text unchanged when every character is safe for the shell, else single-quoted.
pub fn shell_quote(text: &str) -> String {
  if text.is_empty() {
    return "''".to_string();
  }
  if text.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
    return text.to_string();
  }
  format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// The `bash -lc` payload: CMD in a subshell, then the exit code written to the exit file.
///
/// The echo stays outside the subshell, so an `exit` inside CMD ends only the subshell and the exit file is still
/// written. The newline before `)` keeps a trailing `# comment` in CMD from swallowing the closing paren.
fn wrapper_command(cmd: &str, exit_file: &str) -> String {
  format!("({cmd}\n); echo $? > {}", shell_quote(exit_file))
}

/// Start the wrapper on this host in its own session. The log is appended to, and its parent directories are created.
pub fn launch_local(cmd: &str, cwd: &Path, log_path: &Path, exit_file: &Path) -> Result<Launch, LaunchError> {
  let io = |error: io::Error| LaunchError::Io(error.to_string());
  for dir in [log_path.parent(), exit_file.parent()].into_iter().flatten() {
    std::fs::create_dir_all(dir).map_err(io)?;
  }
  let log = OpenOptions::new().create(true).append(true).open(log_path).map_err(io)?;
  let payload = wrapper_command(cmd, &exit_file.display().to_string());
  let mut command = Command::new("bash");
  command
    .args(["-lc", payload.as_str()])
    .current_dir(cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::from(log.try_clone().map_err(io)?))
    .stderr(Stdio::from(log));
  // SAFETY: the closure runs in the forked child before exec and calls only `setsid`, which is async-signal-safe.
  unsafe {
    command.pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from));
  }
  let child = command.spawn().map_err(io)?;
  let pid = i32::try_from(child.id()).expect("a pid fits pid_t");
  Ok(Launch { pid, host: None, log: log_path.display().to_string(), exit_file: exit_file.display().to_string() })
}

/// Start the wrapper on `host` over ssh and `setsid`, and return its remote PID.
///
/// Without `cwd` the command runs in the ssh login directory. An ssh failure or an unparseable PID is `Ssh`.
pub fn launch_remote(host: &str, cmd: &str, cwd: Option<&str>, launch_id: &str) -> Result<Launch, LaunchError> {
  let io = |error: io::Error| LaunchError::Io(error.to_string());
  let remote_dir = format!("{REMOTE_RUNS_DIR}/{launch_id}");
  let remote_log = format!("{remote_dir}/log");
  let remote_exit = format!("{remote_dir}/exit");
  let pid_file = shell_quote(&format!("{remote_dir}/pid"));
  // The brace group carries the stream redirections for both wrapper steps, so the backgrounded setsid process
  // holds no ssh channel fd and ssh returns.
  let inner = format!("{{ {}; }} < /dev/null > {} 2>&1", wrapper_command(cmd, &remote_exit), shell_quote(&remote_log));
  let cd = cwd.map_or(String::new(), |dir| format!("cd {} && ", shell_quote(dir)));
  let wrapper = format!(
    "mkdir -p {} && {cd}{{ setsid bash -lc {} & echo $! > {pid_file} && cat {pid_file}; }}",
    shell_quote(&remote_dir),
    shell_quote(&inner)
  );
  let quoted = shell_quote(&wrapper);
  let argv = ssh::ssh_cmd(host, &["bash", "-c", quoted.as_str()]).map_err(io)?;
  let captured = run_bounded(&argv).map_err(io)?;
  if captured.timed_out {
    let stderr = captured.stderr.trim();
    return Err(LaunchError::Ssh(format!("ssh to {host} timed out after {}s: {stderr}", SSH_LAUNCH_TIMEOUT.as_secs())));
  }
  if !captured.status.success() {
    let stderr = captured.stderr.trim();
    let code = return_code(captured.status);
    return Err(LaunchError::Ssh(format!("ssh to {host} failed (rc={code}): {stderr}")));
  }
  // The PID is the last non-blank line of the remote shell's output: `cat` of the PID file after the `$!` write.
  let pid =
    captured.stdout.lines().rev().find(|line| !line.trim().is_empty()).and_then(|line| line.trim().parse::<i32>().ok());
  let Some(pid) = pid else {
    let stdout = pyfmt::str_repr(&captured.stdout);
    return Err(LaunchError::Ssh(format!("failed to parse remote PID from ssh stdout: {stdout}")));
  };
  Ok(Launch { pid, host: Some(host.to_string()), log: remote_log, exit_file: remote_exit })
}

/// Kill the wrapper's process group after a rejected or unverified registration.
///
/// The wrapper leads its group on both paths, so one kill by group id takes the whole tree. A local wrapper is reaped
/// before this returns, so the caller exits only once the PID stops answering. A remote kill that cannot run is
/// warned about on stderr, and the caller's exit code does not change.
pub fn kill_launch(launch: &Launch) -> Result<(), LaunchError> {
  let io = |error: io::Error| LaunchError::Io(error.to_string());
  match &launch.host {
    None => {
      let pid = Pid::from_raw(launch.pid);
      match killpg(pid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => return Err(LaunchError::Io(error.to_string())),
      }
      // The wrapper is this process's child: reap it. ECHILD means it was already reaped.
      match waitpid(pid, None) {
        Ok(_) | Err(Errno::ECHILD) => Ok(()),
        Err(error) => Err(LaunchError::Io(error.to_string())),
      }
    }
    Some(host) => {
      let kill_command = format!("kill -KILL -- -{}", launch.pid);
      let argv = ssh::ssh_cmd(host, &[kill_command.as_str()]).map_err(io)?;
      let captured = run_bounded(&argv).map_err(io)?;
      if captured.timed_out {
        eprintln!("warning: remote kill of {host}:{} timed out; the process group may still run", launch.pid);
      } else if !captured.status.success() && !captured.stderr.to_lowercase().contains("no such process") {
        let code = return_code(captured.status);
        let stderr = captured.stderr.trim();
        eprintln!("warning: remote kill of {host}:{} exited {code}: {stderr}", launch.pid);
      }
      Ok(())
    }
  }
}

/// What one bounded ssh run produced. `status` is the kill's status when `timed_out` is set.
struct Captured {
  timed_out: bool,
  status: ExitStatus,
  stdout: String,
  stderr: String,
}

/// Run `argv` with stdout and stderr captured, and kill it once it has run for `SSH_LAUNCH_TIMEOUT`.
fn run_bounded(argv: &[String]) -> io::Result<Captured> {
  let mut child = Command::new(&argv[0]).args(&argv[1..]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
  let stdout = drain(child.stdout.take().expect("stdout is piped"));
  let stderr = drain(child.stderr.take().expect("stderr is piped"));
  let started = Instant::now();
  let (status, timed_out) = loop {
    if let Some(status) = child.try_wait()? {
      break (status, false);
    }
    if started.elapsed() >= SSH_LAUNCH_TIMEOUT {
      child.kill()?;
      break (child.wait()?, true);
    }
    thread::sleep(Duration::from_millis(10));
  };
  Ok(Captured { timed_out, status, stdout: join_text(stdout)?, stderr: join_text(stderr)? })
}

/// Read one pipe to its end on a thread, so a full pipe never stalls the wait loop.
fn drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<io::Result<Vec<u8>>> {
  thread::spawn(move || {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
  })
}

fn join_text(reader: JoinHandle<io::Result<Vec<u8>>>) -> io::Result<String> {
  let bytes = reader.join().expect("the pipe reader does not panic")?;
  Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Python's `returncode`: the exit code, or the negated signal number when a signal ended the process.
fn return_code(status: ExitStatus) -> i32 {
  status.code().or_else(|| status.signal().map(|signal| -signal)).expect("an exit status has a code or a signal")
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use nix::sys::signal::kill;

  use super::*;

  /// Poll until `path` exists: the wrapper writes it just before it exits.
  fn wait_for_path(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
      assert!(started.elapsed() < Duration::from_secs(5), "{} never appeared", path.display());
      thread::sleep(Duration::from_millis(10));
    }
  }

  /// The wrapper is this test's child: reap it, so its PID stops answering `kill 0`.
  fn reap(launch: &Launch) {
    let _ = waitpid(Pid::from_raw(launch.pid), None);
  }

  fn assert_gone(pid: i32) {
    assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH), "pid {pid} still answers");
  }

  fn local(dir: &Path, cmd: &str) -> Launch {
    launch_local(cmd, dir, &dir.join("launch").join("log"), &dir.join("launch").join("exit"))
      .expect("the wrapper starts")
  }

  #[test]
  fn test_launch_local_exit_code_lands_in_the_exit_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let launch = local(dir.path(), "exit 3");
    let exit_file = PathBuf::from(&launch.exit_file);
    wait_for_path(&exit_file);
    assert_eq!(std::fs::read_to_string(&exit_file).expect("the exit file reads").trim(), "3");
    // The wrapper exits only after the exit file is written.
    reap(&launch);
    assert_gone(launch.pid);
    kill_launch(&launch).expect("the cleanup kill runs");
  }

  #[test]
  fn test_launch_local_runs_in_cwd_and_logs_output() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let launch = launch_local("pwd; echo hello", dir.path(), &dir.path().join("log"), &dir.path().join("exit"))
      .expect("the wrapper starts");
    wait_for_path(Path::new(&launch.exit_file));
    let out = std::fs::read_to_string(&launch.log).expect("the log reads");
    assert!(out.contains(&dir.path().display().to_string()), "{out}");
    assert!(out.contains("hello"), "{out}");
    assert_eq!(std::fs::read_to_string(&launch.exit_file).expect("the exit file reads").trim(), "0");
    reap(&launch);
    kill_launch(&launch).expect("the cleanup kill runs");
  }

  #[test]
  fn test_launch_local_trailing_comment_in_cmd_still_writes_the_exit_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let launch =
      launch_local("echo hi # trailing comment", dir.path(), &dir.path().join("log"), &dir.path().join("exit"))
        .expect("the wrapper starts");
    wait_for_path(Path::new(&launch.exit_file));
    assert_eq!(std::fs::read_to_string(&launch.exit_file).expect("the exit file reads").trim(), "0");
    assert_eq!(std::fs::read_to_string(&launch.log).expect("the log reads").trim(), "hi");
    reap(&launch);
    kill_launch(&launch).expect("the cleanup kill runs");
  }

  #[test]
  fn test_kill_launch_local_kills_the_process_group() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let launch = local(dir.path(), "sleep 600");
    assert!(kill(Pid::from_raw(launch.pid), None).is_ok(), "the wrapper runs before the kill");
    kill_launch(&launch).expect("the kill runs");
    assert_gone(launch.pid);
    // The group kill reaches the sleep too: no exit file is ever written.
    assert!(!Path::new(&launch.exit_file).exists());
  }

  #[test]
  fn shell_quote_matches_python_shlex() {
    assert_eq!(shell_quote("/tmp/run_1.2-x"), "/tmp/run_1.2-x");
    assert_eq!(shell_quote("a=b:c@d%e+f,g"), "a=b:c@d%e+f,g");
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    assert_eq!(shell_quote("echo hi"), "'echo hi'");
  }
}
