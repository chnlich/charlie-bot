//! The stand-in holder of a serve trial's port in the unnamed-holder test. It makes itself non-dumpable, so the fd table
//! of this process is root's to read and `ss -p` names no pid for its listener. It binds the port it is given and
//! answers every connection with 200, as a foreign server answers the trial's readiness probe.

use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
  let port: u16 = std::env::args()
    .nth(1)
    .expect("the port to hold is the first argument")
    .parse()
    .expect("the port to hold is a number");
  // SAFETY: PR_SET_DUMPABLE with 0 reads and writes no memory.
  if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0) } != 0 {
    panic!("prctl(PR_SET_DUMPABLE, 0) failed: {}", std::io::Error::last_os_error());
  }
  let listener = TcpListener::bind(("127.0.0.1", port)).expect("the port is free for the holder");
  for stream in listener.incoming() {
    let mut stream = stream.expect("a probe connection");
    let mut request = [0u8; 1024];
    let read = stream.read(&mut request).expect("the probe request");
    assert!(read > 0, "the probe closed before it sent a request");
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("the probe answer");
  }
}
