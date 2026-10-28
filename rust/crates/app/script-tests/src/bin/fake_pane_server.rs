//! The stand-in for a server under a pane in the escalation tests. The escalation finds it by its argv, which names a
//! `charliebot-server` binary, and it runs until it is killed. With the argument `stubborn` it ignores SIGTERM.

use std::env;
use std::thread;
use std::time::Duration;

fn main() {
  if env::args().nth(1).as_deref() == Some("stubborn") {
    // SAFETY: SIG_IGN installs no handler, so no code runs in the signal path.
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
  }
  thread::sleep(Duration::from_secs(300));
}
