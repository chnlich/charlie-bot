//! The stand-in server of the SIGTERM serve test. It binds the trial's port, logs `server_ready`, and ignores SIGTERM,
//! so only the trial's SIGKILL ends it. It answers every connection with 200: the scheduled task list lists each cron
//! task file of the trial home as disabled, as the started server reads them, and any other route gets an empty run
//! page. The port is the trial's own range, which the test sets as TRIAL_PORT_MIN and the trial sets as its chosen port.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;

use serde_json::json;

const CRON_TASKS_REQUEST: &[u8] = b"GET /api/cron/tasks ";

fn main() {
  let port: u16 =
    std::env::var("TRIAL_PORT_MIN").expect("the trial's port range").parse().expect("a numeric trial port");
  let home = std::env::var("CHARLIEBOT_HOME").expect("the trial's home");
  // SAFETY: SIG_IGN takes no pointer; the call only changes SIGTERM's disposition for this process.
  unsafe {
    libc::signal(libc::SIGTERM, libc::SIG_IGN);
  }
  let listener = TcpListener::bind(("127.0.0.1", port)).expect("the trial's port");
  println!(
    "2026-11-22 11:31:05.017Z [info     ] server/main.rs:340             server_ready                   ready_in_ms=1"
  );
  let run_page = r#"{"items":[],"next_cursor":null}"#;
  for stream in listener.incoming() {
    let mut stream = stream.expect("a probe connection");
    let mut request = [0u8; 1024];
    let read = stream.read(&mut request).expect("the probe request");
    assert!(read > 0, "the probe closed before it sent a request");
    let body =
      if request[..read].starts_with(CRON_TASKS_REQUEST) { cron_tasks(Path::new(&home)) } else { run_page.to_owned() };
    write!(
      stream,
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
      body.len()
    )
    .expect("the probe answer");
  }
}

/// The trial home's cron tasks as the started server lists them: one disabled entry per task file.
fn cron_tasks(home: &Path) -> String {
  let mut tasks = Vec::new();
  for entry in fs::read_dir(home.join("config.d/cron.d")).expect("the cron task directory") {
    let path = entry.expect("a cron task entry").path();
    let name = path.file_stem().expect("a task name").to_string_lossy().into_owned();
    tasks.push(json!({"name": name, "enabled": false}));
  }
  serde_json::to_string(&tasks).expect("the task list")
}
