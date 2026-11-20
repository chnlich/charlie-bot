//! A loopback stand-in for the live server, for the CLI tests whose command reads a session route. Each request is
//! answered with `status_of(path)`, which must answer 200 or 404, and each request's header lines come back when the
//! serving thread is joined.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};

/// Serves `count` requests on a loopback port, and returns the port with the thread that serves them.
pub fn serve(count: usize, status_of: impl Fn(&str) -> u16 + Send + 'static) -> (u16, JoinHandle<Vec<Vec<String>>>) {
  let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
  let port = listener.local_addr().expect("the port's address").port();
  let handle = thread::spawn(move || {
    let mut requests = Vec::new();
    for _ in 0..count {
      let (mut stream, _) = listener.accept().expect("a connection");
      let mut reader = BufReader::new(stream.try_clone().expect("the connection's read half"));
      let mut lines = Vec::new();
      loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("a request line");
        if line.trim_end().is_empty() {
          break;
        }
        lines.push(line.trim_end().to_string());
      }
      let path = lines[0].split_whitespace().nth(1).expect("a request target").to_string();
      let status = status_of(&path);
      let reason = if status == 200 { "OK" } else { "Not Found" };
      write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .expect("the answer is sent");
      requests.push(lines);
    }
    requests
  });
  (port, handle)
}
