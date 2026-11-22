//! The stand-in server of the held-port trial test. It logs `server_ready` the way the server does, then waits without
//! binding a port, so the test can take the port the trial picked once that readiness line is in the trial's log.

use std::thread;
use std::time::Duration;

fn main() {
  println!(
    "2026-11-22 11:31:05.017Z [info     ] server/main.rs:340             server_ready                   ready_in_ms=1"
  );
  // The trial's cleanup stops this process with SIGTERM, which ends the sleep.
  thread::sleep(Duration::from_secs(120));
}
