//! The loopback HTTP server that a test puts in front of the shared client. The test scripts the answers the server
//! gives. The server records each request it receives, in order, so the test checks the bytes the client sent.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

/// How long a test waits for a request, and a socket read.
const WAIT: Duration = Duration::from_secs(2);

/// A loopback server that answers the scripted responses in order, one connection each, and records every request.
pub struct LoopbackServer {
  base: String,
  requests: Receiver<Vec<u8>>,
}

impl LoopbackServer {
  /// Binds a free loopback port and answers `responses` in order, each as a status and a body.
  pub fn start(responses: Vec<(u16, Vec<u8>)>) -> LoopbackServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port binds");
    let base = format!("http://{}", listener.local_addr().expect("the bound listener names its address"));
    let (sender, requests) = mpsc::channel();
    thread::spawn(move || {
      for (status, body) in responses {
        let (mut stream, _) = listener.accept().expect("the client connects once per scripted response");
        stream.set_nodelay(true).expect("the accepted stream sets nodelay");
        sender.send(read_http_request(&mut stream)).expect("the recording channel stays open");
        let response = format!(
          "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&body);
      }
    });
    LoopbackServer { base, requests }
  }

  /// The server's base URL, `http://<address>`.
  pub fn base(&self) -> &str {
    &self.base
  }

  /// The text of the next recorded request, as the client sent it.
  pub fn next_request(&self) -> String {
    let request = self.requests.recv_timeout(WAIT).expect("a request reaches the loopback server within the wait");
    String::from_utf8(request).expect("a recorded request is UTF-8")
  }

  /// Whether no request arrived.
  pub fn no_request(&self) -> bool {
    self.requests.try_recv().is_err()
  }
}

/// The bytes of one request: everything read up to the body its `content-length` announces.
fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
  stream.set_read_timeout(Some(WAIT)).expect("the accepted stream sets its read timeout");
  let mut request = Vec::new();
  let mut buffer = [0_u8; 4096];
  loop {
    let read = stream.read(&mut buffer).expect("the client's request reads to its end");
    if read == 0 {
      return request;
    }
    request.extend_from_slice(&buffer[..read]);
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4) else {
      continue;
    };
    let head = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    let content_length = head
      .lines()
      .find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("a numeric length"))
      })
      .unwrap_or(0);
    if request.len() >= header_end + content_length {
      return request;
    }
  }
}
