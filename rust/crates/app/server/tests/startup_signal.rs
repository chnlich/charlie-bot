use std::fmt::Write;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

const RECOVERY_GATE_MS: &str = "30000";
const BOOT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

struct ServerChild(Child);

impl Drop for ServerChild {
  fn drop(&mut self) {
    if self.0.try_wait().expect("the server child status can be read").is_none() {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }
}

fn config_text() -> &'static str {
  "server:\n  host: 127.0.0.1\n  port: 0\n  session_memory_max_mb: 0\n  session_swap_max_mb: 0\n  session_tasks_max: 0\n  min_free_disk_gib: 0\nbackends:\n  options:\n    - id: claude-synthetic\n      label: Synthetic\n      type: cc-claude\n      model: claude-fable-5-1\n"
}

fn prepare_home(root: &Path) -> PathBuf {
  let home = root.join("home");
  std::fs::create_dir_all(&home).expect("the synthetic home is created");
  std::fs::write(home.join("config.yaml"), config_text()).expect("the synthetic config is written");
  std::fs::write(home.join("credentials.yaml"), "").expect("the synthetic credentials file is written");
  home
}

fn test_server_binary(root: &Path) -> PathBuf {
  let directory = root.join("server-bin");
  std::fs::create_dir_all(&directory).expect("the synthetic server directory is created");
  let server = directory.join("charliebot-server");
  if !server.exists() {
    std::fs::copy(env!("CARGO_BIN_EXE_charliebot-server"), &server).expect("the server binary is copied");
    let mut permissions = std::fs::metadata(&server).expect("the copied server metadata is read").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&server, permissions).expect("the copied server is executable");
  }
  let cli = directory.join("charliebot");
  if !cli.exists() {
    std::fs::write(&cli, "test fixture; this file is not run\n").expect("the fake CLI file is written");
    let mut permissions = std::fs::metadata(&cli).expect("the fake CLI metadata is read").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cli, permissions).expect("the fake CLI is executable");
  }
  server
}

fn start_server(root: &Path, home: &Path, gate_recovery: bool) -> (ServerChild, Receiver<io::Result<String>>) {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let mut command = Command::new(test_server_binary(root));
  command
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", root)
    .env("CHARLIEBOT_HOME", home)
    .env("CHARLIEBOT_REPO_ROOT", &repo_root)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
  if gate_recovery {
    command.env("CHARLIEBOT_TEST_TASK_RECOVERY_DELAY_MS", RECOVERY_GATE_MS);
  }
  let mut child = ServerChild(command.spawn().expect("the server process starts"));
  let stdout = child.0.stdout.take().expect("the server stdout is piped");
  let (sender, receiver) = mpsc::channel();
  thread::spawn(move || {
    for line in BufReader::new(stdout).lines() {
      if sender.send(line).is_err() {
        break;
      }
    }
  });
  (child, receiver)
}

fn wait_for_event(
  child: &mut ServerChild,
  receiver: &Receiver<io::Result<String>>,
  event: &str,
  timeout: Duration,
) -> Vec<String> {
  let deadline = Instant::now() + timeout;
  let mut lines = Vec::new();
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "the server did not log {event}; output: {lines:?}");
    match receiver.recv_timeout(remaining) {
      Ok(Ok(line)) => {
        let matched = line.contains(&format!("\"event\":\"{event}\""));
        lines.push(line);
        if matched {
          return lines;
        }
      }
      Ok(Err(error)) => panic!("reading the server log failed: {error}"),
      Err(mpsc::RecvTimeoutError::Timeout) => {
        let status = child.0.try_wait().expect("the server child status can be read");
        panic!("the server did not log {event} within {timeout:?}; status: {status:?}; output: {lines:?}");
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        let status = child.0.try_wait().expect("the server child status can be read");
        panic!("the server log closed before {event}; status: {status:?}; output: {lines:?}");
      }
    }
  }
}

fn send_signal(child: &ServerChild, signal_to_send: Signal) {
  signal::kill(Pid::from_raw(child.0.id() as i32), signal_to_send).expect("the signal reaches the server process");
}

fn collect_available(receiver: &Receiver<io::Result<String>>, lines: &mut Vec<String>) {
  loop {
    match receiver.try_recv() {
      Ok(Ok(line)) => lines.push(line),
      Ok(Err(error)) => panic!("reading the server log failed: {error}"),
      Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return,
    }
  }
}

fn wait_for_exit(
  child: &mut ServerChild,
  receiver: &Receiver<io::Result<String>>,
  lines: &mut Vec<String>,
  timeout: Duration,
) -> (ExitStatus, Duration) {
  let started = Instant::now();
  let deadline = started + timeout;
  loop {
    collect_available(receiver, lines);
    if let Some(status) = child.0.try_wait().expect("the server child status can be read") {
      return (status, started.elapsed());
    }
    assert!(Instant::now() < deadline, "the server did not exit within {timeout:?}; output: {lines:?}");
    thread::sleep(Duration::from_millis(10));
  }
}

fn drain_logs(receiver: &Receiver<io::Result<String>>, lines: &mut Vec<String>) {
  loop {
    match receiver.recv_timeout(Duration::from_secs(2)) {
      Ok(Ok(line)) => lines.push(line),
      Ok(Err(error)) => panic!("reading the server log failed: {error}"),
      Err(mpsc::RecvTimeoutError::Disconnected) => return,
      Err(mpsc::RecvTimeoutError::Timeout) => panic!("the server log reader did not close after process exit"),
    }
  }
}

#[cfg(debug_assertions)]
fn assert_boot_signal(signal_to_send: Signal) {
  let temporary = tempfile::tempdir().expect("the synthetic home tempdir is created");
  let home = prepare_home(temporary.path());
  let (mut server, receiver) = start_server(temporary.path(), &home, true);
  let mut lines = wait_for_event(&mut server, &receiver, "task_tree_recovery_test_gate", Duration::from_secs(20));
  assert!(
    lines.iter().any(|line| line.contains(&format!("\"delay_ms\":{RECOVERY_GATE_MS}"))),
    "the test recovery delay is 30 seconds: {lines:?}"
  );
  assert!(!lines.iter().any(|line| line.contains("\"event\":\"server_ready\"")));

  send_signal(&server, signal_to_send);
  let (status, elapsed) = wait_for_exit(&mut server, &receiver, &mut lines, BOOT_STOP_TIMEOUT);
  drain_logs(&receiver, &mut lines);
  println!("{signal_to_send:?} during task recovery exited in {:.3}s", elapsed.as_secs_f64());
  assert!(elapsed < BOOT_STOP_TIMEOUT, "the process took {elapsed:?} to stop");
  assert!(!lines.iter().any(|line| line.contains("\"event\":\"server_ready\"")), "{lines:?}");
  let shutdowns: Vec<&String> =
    lines.iter().filter(|line| line.contains("\"event\":\"charliebot_shutdown\"")).collect();
  assert_eq!(shutdowns.len(), 1, "the boot stop logs one shutdown line: {lines:?}");
  assert!(shutdowns[0].contains("\"boot\":true"), "the shutdown line marks a boot stop: {shutdowns:?}");
  match signal_to_send {
    Signal::SIGTERM => assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}"),
    Signal::SIGINT => assert_eq!(status.code(), Some(0), "{status:?}"),
    signal => panic!("unexpected test signal {signal}"),
  }

  let (mut restarted, restarted_receiver) = start_server(temporary.path(), &home, false);
  let mut restarted_lines =
    wait_for_event(&mut restarted, &restarted_receiver, "server_ready", Duration::from_secs(30));
  assert!(
    !restarted_lines.iter().any(|line| line.contains("\"event\":\"task_recovery_node_failed\"")),
    "the restarted home recovers without a failed node: {restarted_lines:?}"
  );
  send_signal(&restarted, Signal::SIGTERM);
  let (status, elapsed) = wait_for_exit(&mut restarted, &restarted_receiver, &mut restarted_lines, BOOT_STOP_TIMEOUT);
  drain_logs(&restarted_receiver, &mut restarted_lines);
  assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}");
  assert!(elapsed < BOOT_STOP_TIMEOUT, "the restarted server took {elapsed:?} to stop");
}

// The delay gate `wait_test_recovery_gate` exists only in debug builds, so a release build has no gate to stop in.
#[cfg(debug_assertions)]
#[test]
fn sigterm_stops_task_recovery_during_boot() {
  assert_boot_signal(Signal::SIGTERM);
}

// The delay gate `wait_test_recovery_gate` exists only in debug builds, so a release build has no gate to stop in.
#[cfg(debug_assertions)]
#[test]
fn sigint_stops_task_recovery_during_boot() {
  assert_boot_signal(Signal::SIGINT);
}

/// Task-tree nodes the boot pass walks, and the chat events each node carries. The walk takes at least `WALK_MIN_MS`,
/// so the signal lands between nodes. Each node's events are inert (the fact fold ignores their type), which lengthens
/// the walk without a directory per node. A release build walks an event about six times faster than a debug build, so
/// its nodes carry five times the events.
const WALK_SESSIONS: usize = 200;
const WALK_EVENTS: usize = if cfg!(debug_assertions) { 4_800 } else { 24_000 };
const WALK_MIN_MS: u64 = 2_000;

/// Writes task-tree nodes: a profiled session whose chat events open with a creation boundary. A session without a
/// profile is a v1 session, and the startup check refuses a home that holds one.
fn write_sessions(home: &Path, count: usize) {
  let sessions = home.join("sessions");
  for index in 0..count {
    let id = format!("synthetic-session-{index:05}");
    let data = sessions.join(&id).join("data");
    std::fs::create_dir_all(&data).expect("the synthetic session data directory is created");
    let metadata = serde_json::json!({"id": id, "name": id, "profile": "manager"});
    std::fs::write(sessions.join(&id).join("metadata.json"), metadata.to_string())
      .expect("the synthetic session metadata is written");
    let mut events = format!("{}\n", serde_json::json!({"id": format!("{id}-created"), "type": "task_created"}));
    for event in 0..WALK_EVENTS {
      writeln!(events, "{}", serde_json::json!({"id": format!("{id}-note-{event:05}"), "type": "note"}))
        .expect("the synthetic chat events are formatted");
    }
    std::fs::write(data.join("chat_events.jsonl"), events).expect("the synthetic chat events are written");
  }
}

fn recovery_elapsed_ms(lines: &[String]) -> u64 {
  let line = lines
    .iter()
    .find(|line| line.contains("\"event\":\"task_tree_recovery_done\""))
    .expect("the boot pass logs task_tree_recovery_done");
  let value: serde_json::Value = serde_json::from_str(line).expect("the done line is JSON");
  value["elapsed_ms"].as_u64().expect("the done line has elapsed_ms")
}

/// SIGTERM during a real node walk. Neither boot sets the delay gate, so the walk's own stop checks end it.
#[test]
fn sigterm_stops_the_node_walk_without_the_delay_gate() {
  let temporary = tempfile::tempdir().expect("the synthetic home tempdir is created");
  let home = prepare_home(temporary.path());
  write_sessions(&home, WALK_SESSIONS);

  let (mut server, receiver) = start_server(temporary.path(), &home, false);
  let mut lines = wait_for_event(&mut server, &receiver, "task_tree_recovery_walk_started", Duration::from_secs(20));
  send_signal(&server, Signal::SIGTERM);
  let (status, elapsed) = wait_for_exit(&mut server, &receiver, &mut lines, BOOT_STOP_TIMEOUT);
  drain_logs(&receiver, &mut lines);
  println!("SIGTERM during the node walk exited in {:.3}s", elapsed.as_secs_f64());
  assert!(elapsed < BOOT_STOP_TIMEOUT, "the process took {elapsed:?} to stop");
  // A full walk takes at least WALK_MIN_MS, so an exit sooner than that means the stop ended the walk.
  assert!(elapsed < Duration::from_millis(WALK_MIN_MS), "the stop took {elapsed:?}; the walk ran to its end");
  assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}");
  assert!(
    !lines.iter().any(|line| line.contains("\"event\":\"task_tree_recovery_done\"")),
    "the walk finished before the stop reached it: {lines:?}"
  );
  assert!(!lines.iter().any(|line| line.contains("\"event\":\"server_ready\"")), "{lines:?}");
  let shutdowns: Vec<&String> =
    lines.iter().filter(|line| line.contains("\"event\":\"charliebot_shutdown\"")).collect();
  assert_eq!(shutdowns.len(), 1, "the boot stop logs one shutdown line: {lines:?}");
  assert!(shutdowns[0].contains("\"boot\":true"), "the shutdown line marks a boot stop: {shutdowns:?}");

  let (mut restarted, restarted_receiver) = start_server(temporary.path(), &home, false);
  let mut restarted_lines =
    wait_for_event(&mut restarted, &restarted_receiver, "server_ready", Duration::from_secs(60));
  let walk_ms = recovery_elapsed_ms(&restarted_lines);
  println!("the node walk over {WALK_SESSIONS} sessions took {walk_ms} ms");
  assert!(walk_ms >= WALK_MIN_MS, "the node walk took {walk_ms} ms, under the {WALK_MIN_MS} ms the test needs");
  assert!(
    !restarted_lines.iter().any(|line| line.contains("\"event\":\"task_recovery_node_failed\"")),
    "the restarted home walks every node without a failure: {restarted_lines:?}"
  );
  send_signal(&restarted, Signal::SIGTERM);
  let (status, elapsed) = wait_for_exit(&mut restarted, &restarted_receiver, &mut restarted_lines, BOOT_STOP_TIMEOUT);
  drain_logs(&restarted_receiver, &mut restarted_lines);
  assert_eq!(status.signal(), Some(Signal::SIGTERM as i32), "{status:?}");
  assert!(elapsed < BOOT_STOP_TIMEOUT, "the restarted server took {elapsed:?} to stop");
}
