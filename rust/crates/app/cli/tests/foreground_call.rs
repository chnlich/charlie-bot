//! The foreground call entry on a real host: a call cgroup under a real session cgroup, a real guard unit started by
//! `systemd-run --user`, and the 895 s deadline injected as 15 s. Each test needs `systemd --user` and cgroup v2, so
//! they are ignored by default; run them on such a host with `cargo test -p charliebot-cli --test foreground_call --
//! --ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use charliebot_agents::foreground::{SESSION_CGROUP_ENV, deadline_error_text, encode_hex};
use charliebot_agents::transport::cgroup::{SessionCgroupConfig, ensure_session_cgroup};
use charliebot_core::cgroup::system_cgroup_base_dir;

const DEADLINE_SECS: &str = "15";
const STOP_SECS: &str = "5";

/// A session cgroup for one test, created under the host's agents slice and removed on drop.
struct TestSession {
  dir: PathBuf,
  /// Scratch files the test writes. A cgroup directory holds no regular files.
  scratch: tempfile::TempDir,
}

impl TestSession {
  fn new(name: &str) -> Self {
    // A session cgroup is named by the first eight characters of its session id, so the tag that opens the id must differ
    // between the tests of this process.
    let tag = name.bytes().fold(std::process::id(), |acc, byte| acc.wrapping_mul(31).wrapping_add(u32::from(byte)));
    let session_id = format!("{tag:08x}-fgtest-{name}");
    let config = SessionCgroupConfig {
      base_dir: system_cgroup_base_dir(),
      memory_max_mb: 80,
      swap_max_mb: 0,
      tasks_max: 64,
      enable_cpu_weight: false,
      enable_io_weight: false,
    };
    let dir = ensure_session_cgroup(&config, &session_id).expect("the agents slice holds a test session cgroup");
    Self { dir, scratch: tempfile::tempdir().unwrap() }
  }

  /// The call directories the entry created and has not removed yet.
  fn call_dirs(&self) -> Vec<PathBuf> {
    fs::read_dir(&self.dir)
      .unwrap()
      .map(|entry| entry.unwrap().path())
      .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("call-")))
      .collect()
  }
}

impl Drop for TestSession {
  fn drop(&mut self) {
    // Kill what a failed test left behind, wait for it to leave, then remove the call and session directories.
    for call in self.call_dirs() {
      let _ = fs::write(call.join("cgroup.kill"), "1");
    }
    let _ = fs::write(self.dir.join("cgroup.kill"), "1");
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until && !self.dir_is_empty() {
      thread::sleep(Duration::from_millis(100));
    }
    for call in self.call_dirs() {
      let _ = fs::remove_dir(&call);
    }
    let _ = fs::remove_dir(&self.dir);
  }
}

impl TestSession {
  fn dir_is_empty(&self) -> bool {
    let procs = fs::read_to_string(self.dir.join("cgroup.procs")).unwrap_or_default();
    self.call_dirs().iter().all(|call| fs::read_to_string(call.join("cgroup.procs")).unwrap_or_default().is_empty())
      && procs.is_empty()
  }
}

/// Run `command` through the call entry with the injected 15 s deadline, under `session`.
fn run_call(session: &TestSession, command: &str) -> Output {
  Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args([
      "foreground-call",
      "--cmd-hex",
      &encode_hex(command),
      "--deadline-secs",
      DEADLINE_SECS,
      "--stop-secs",
      STOP_SECS,
    ])
    .env(SESSION_CGROUP_ENV, &session.dir)
    .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
    .output()
    .expect("the charliebot binary runs")
}

fn stdout(output: &Output) -> String {
  String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
  String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Poll `condition` every 100 ms until it holds or `limit` passes.
fn wait_until(condition: impl Fn() -> bool, limit: Duration) {
  let until = Instant::now() + limit;
  while Instant::now() < until && !condition() {
    thread::sleep(Duration::from_millis(100));
  }
}

/// The deadline error text, with the injected limit.
fn deadline_text() -> String {
  deadline_error_text(DEADLINE_SECS.parse().unwrap())
}

#[test]
#[ignore = "integration"]
fn a_command_past_the_deadline_returns_its_partial_output_and_the_error_text() {
  let session = TestSession::new("partial");
  let started = Instant::now();
  let output = run_call(&session, "echo BEFORE; sleep 25; echo AFTER");
  let elapsed = started.elapsed();
  assert_eq!(output.status.code(), Some(143), "stderr: {}", stderr(&output));
  assert_eq!(stdout(&output), "BEFORE\n");
  assert!(!stdout(&output).contains("AFTER"));
  assert_eq!(stderr(&output).trim_end(), deadline_text());
  assert!(elapsed >= Duration::from_secs(14) && elapsed < Duration::from_secs(20), "elapsed {elapsed:?}");
}

#[test]
#[ignore = "integration"]
fn a_child_that_ignores_sigterm_ends_by_cgroup_kill() {
  let session = TestSession::new("sigterm");
  let started = Instant::now();
  let output = run_call(&session, "trap '' TERM; sleep 60");
  let elapsed = started.elapsed();
  assert_eq!(output.status.code(), Some(143), "stderr: {}", stderr(&output));
  assert_eq!(stderr(&output).trim_end(), deadline_text());
  assert!(elapsed >= Duration::from_secs(14) && elapsed < Duration::from_secs(25), "elapsed {elapsed:?}");
  // The entry returns when its child dies, which `cgroup.kill` causes; the guard then removes the call cgroup.
  wait_until(|| session.call_dirs().is_empty(), Duration::from_secs(10));
  assert!(session.call_dirs().is_empty(), "the guard removes the call cgroup after cgroup.kill");
}

#[test]
#[ignore = "integration"]
fn the_deadline_holds_after_the_starting_process_is_killed() {
  let session = TestSession::new("starter-dies");
  let marker = session.scratch.path().join("never-created");
  let command = format!("sleep 60; touch {}", marker.display());
  let mut starter: Child = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args([
      "foreground-call",
      "--cmd-hex",
      &encode_hex(&command),
      "--deadline-secs",
      DEADLINE_SECS,
      "--stop-secs",
      STOP_SECS,
    ])
    .env(SESSION_CGROUP_ENV, &session.dir)
    .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
    .spawn()
    .expect("the charliebot binary starts");
  let started = Instant::now();
  while session.call_dirs().is_empty() && started.elapsed() < Duration::from_secs(10) {
    thread::sleep(Duration::from_millis(50));
  }
  let calls = session.call_dirs();
  assert_eq!(calls.len(), 1, "the entry created one call cgroup");
  starter.kill().unwrap();
  starter.wait().unwrap();
  let until = Instant::now() + Duration::from_secs(40);
  while Instant::now() < until && !session.call_dirs().is_empty() {
    thread::sleep(Duration::from_millis(100));
  }
  let elapsed = started.elapsed();
  assert!(session.call_dirs().is_empty(), "the guard ends and removes the call after its starter died");
  assert!(elapsed >= Duration::from_secs(14) && elapsed < Duration::from_secs(30), "elapsed {elapsed:?}");
  assert!(!marker.exists(), "the command was ended before it reached its last step");
}

#[test]
#[ignore = "integration"]
fn a_call_over_the_session_memory_cap_is_killed_with_rc_137() {
  let session = TestSession::new("memcap");
  // 200 MiB touched page by page, against the 80 MiB session cap.
  let command = "python3 -I -c 'x=bytearray(200<<20)\nx[::4096]=b\"\\x01\"*len(x[::4096])\nprint(\"COMMITTED\")'";
  let output = run_call(&session, command);
  assert_eq!(output.status.code(), Some(137), "stdout: {} stderr: {}", stdout(&output), stderr(&output));
  assert!(!stdout(&output).contains("COMMITTED"));
}

#[test]
#[ignore = "integration"]
fn a_call_child_processes_stay_under_the_session_pids_cap() {
  let session = TestSession::new("pidcap");
  let count_file = session.scratch.path().join("fork-count");
  let command = format!(
    "python3 -c 'import os,time; started=0; refused=0\nwhile started+refused<100:\n try:\n  pid=os.fork()\n except OSError:\n  refused+=1\n  continue\n if pid==0:\n  time.sleep(60); os._exit(0)\n started+=1\nopen(\"{}\",\"w\").write(str(started)+\" \"+str(refused)+\"\\n\")\ntime.sleep(60)'",
    count_file.display()
  );
  let output = run_call(&session, &command);
  assert_eq!(output.status.code(), Some(143), "stderr: {}", stderr(&output));
  let counts = fs::read_to_string(count_file).expect("the command records its fork attempts before waiting");
  let mut counts = counts.split_whitespace().map(|count| count.parse::<usize>().unwrap());
  let started = counts.next().unwrap();
  let refused = counts.next().unwrap();
  assert!(started < 100, "the session pids.max allows fewer than 100 children, got {started}");
  assert!(refused > 0, "the session pids.max refuses forks after reaching its limit");
}

#[test]
#[ignore = "integration"]
fn a_job_moved_into_the_session_cgroup_survives_the_deadline() {
  let session = TestSession::new("detach");
  let pid_file = session.scratch.path().join("job.pid");
  // The job's output goes to a file: a job that kept the call's stdout would hold the pipe this test reads to the end.
  let command = format!(
    "sleep 300 >/dev/null 2>&1 & echo $! > {pid}; echo $! > \"${env}/cgroup.procs\"; sleep 25",
    pid = pid_file.display(),
    env = SESSION_CGROUP_ENV
  );
  let output = run_call(&session, &command);
  assert_eq!(output.status.code(), Some(143), "stderr: {}", stderr(&output));
  let pid: u32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
  let proc_cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("the detached job still runs");
  assert!(
    proc_cgroup.trim_end().ends_with(session.dir.file_name().unwrap().to_str().unwrap()),
    "the detached job stays in the session cgroup, got {proc_cgroup:?}"
  );
  nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
}

#[test]
#[ignore = "integration"]
fn a_completed_call_removes_its_cgroup_and_passes_its_status_through() {
  let session = TestSession::new("completed");
  let output = run_call(&session, "echo hi; exit 3");
  assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
  assert_eq!(stdout(&output), "hi\n");
  assert!(stderr(&output).is_empty());
  assert!(session.call_dirs().is_empty(), "a finished call leaves no call cgroup");
}

#[test]
#[ignore = "integration"]
fn a_call_without_the_session_cgroup_variable_never_runs() {
  let session = TestSession::new("no-variable");
  let marker = session.scratch.path().join("ran");
  let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args([
      "foreground-call",
      "--cmd-hex",
      &encode_hex(&format!("touch {}", marker.display())),
      "--deadline-secs",
      DEADLINE_SECS,
      "--stop-secs",
      STOP_SECS,
    ])
    .env_remove(SESSION_CGROUP_ENV)
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(1));
  assert!(stderr(&output).contains("CHARLIEBOT_SESSION_CGROUP is unset"));
  assert!(!marker.exists());
}

#[test]
#[ignore = "integration"]
fn a_call_whose_guard_cannot_start_never_runs() {
  let session = TestSession::new("no-guard");
  let marker = session.scratch.path().join("ran");
  let output = Command::new(env!("CARGO_BIN_EXE_charliebot"))
    .args([
      "foreground-call",
      "--cmd-hex",
      &encode_hex(&format!("touch {}", marker.display())),
      "--deadline-secs",
      DEADLINE_SECS,
      "--stop-secs",
      STOP_SECS,
    ])
    .env(SESSION_CGROUP_ENV, &session.dir)
    .env("XDG_RUNTIME_DIR", Path::new("/nonexistent/charliebot-test-runtime"))
    .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/charliebot-test-bus")
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
  assert!(stderr(&output).contains("did not start, so the command did not run"), "stderr: {}", stderr(&output));
  assert!(!marker.exists());
  assert!(session.call_dirs().is_empty(), "the call cgroup is removed when its guard does not start");
}
