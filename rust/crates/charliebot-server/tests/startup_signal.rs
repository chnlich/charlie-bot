use std::io::{self, BufRead, BufReader};
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
  std::fs::create_dir_all(root.join("venv/bin")).expect("the synthetic venv bin directory is created");
  std::fs::write(home.join("config.yaml"), config_text()).expect("the synthetic config is written");
  std::fs::write(home.join("credentials.yaml"), "").expect("the synthetic credentials file is written");
  home
}

fn start_server(root: &Path, home: &Path, gate_recovery: bool) -> (ServerChild, Receiver<io::Result<String>>) {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
  let mut command = Command::new(env!("CARGO_BIN_EXE_charliebot-server"));
  command
    .current_dir(&repo_root)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", root)
    .env("CHARLIEBOT_HOME", home)
    .env("CHARLIEBOT_REPO_ROOT", &repo_root)
    .env("CHARLIEBOT_VENV_BIN", root.join("venv/bin"))
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

#[test]
fn sigterm_stops_task_recovery_during_boot() {
  assert_boot_signal(Signal::SIGTERM);
}

#[test]
fn sigint_stops_task_recovery_during_boot() {
  assert_boot_signal(Signal::SIGINT);
}
