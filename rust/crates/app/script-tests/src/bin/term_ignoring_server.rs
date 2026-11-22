//! The stand-in server of the SIGTERM serve test. It binds the trial's port, logs `server_ready`, answers every
//! connection with 200 and an empty run page, and ignores SIGTERM, so only the trial's SIGKILL ends it. The port is the
//! trial's own range, which the test sets as TRIAL_PORT_MIN and the trial sets as its chosen port.

use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
  let port: u16 =
    std::env::var("TRIAL_PORT_MIN").expect("the trial's port range").parse().expect("a numeric trial port");
  // SAFETY: SIG_IGN takes no pointer; the call only changes SIGTERM's disposition for this process.
  unsafe {
    libc::signal(libc::SIGTERM, libc::SIG_IGN);
  }
  let listener = TcpListener::bind(("127.0.0.1", port)).expect("the trial's port");
  println!(
    "2026-11-22 11:31:05.017Z [info     ] server/main.rs:340             server_ready                   ready_in_ms=1"
  );
  let body = r#"{"items":[],"next_cursor":null}"#;
  for stream in listener.incoming() {
    let mut stream = stream.expect("a probe connection");
    let mut request = [0u8; 1024];
    let read = stream.read(&mut request).expect("the probe request");
    assert!(read > 0, "the probe closed before it sent a request");
    write!(
      stream,
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
      body.len()
    )
    .expect("the probe answer");
  }
}
