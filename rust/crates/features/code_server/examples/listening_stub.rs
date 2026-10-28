//! The listening stub that the code-server tests start as the configured executable. It listens on the loopback port
//! that its first argument names, and it exits once its parent process changes, so it outlives no test run.

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn main() {
  let port: u16 = std::env::args().nth(1).expect("the port argument").parse().expect("the port is a number");
  let parent = nix::unistd::getppid();
  let _listener = TcpListener::bind(("127.0.0.1", port)).expect("the port binds");
  while nix::unistd::getppid() == parent {
    thread::sleep(Duration::from_millis(50));
  }
}
