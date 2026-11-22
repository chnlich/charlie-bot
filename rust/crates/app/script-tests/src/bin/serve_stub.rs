//! The stand-in server of the held-port trial test. It logs `server_ready` and binds no port, so the trial's readiness
//! check has a readiness line with no listener behind it. The test takes the port the trial picked after that line.

use std::thread;
use std::time::Duration;

fn main() {
  println!(
    "2026-11-22 11:31:05.017Z [info     ] server/main.rs:340             server_ready                   ready_in_ms=1"
  );
  // The trial's cleanup stops this process with SIGTERM, which ends the sleep.
  thread::sleep(Duration::from_secs(120));
}
