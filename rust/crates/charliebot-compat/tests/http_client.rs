//! The outbound clients send https requests through the proxy `HTTPS_PROXY` names and refuse a certificate bundle
//! with no certificate in it. The client returns redirects without following them.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use charliebot_compat::http_client::{self, ClientError, Settings};

/// A proxy on a local port that refuses the first tunnel it is asked for. The thread returns the request line
/// the client sent.
fn refusing_proxy() -> (String, JoinHandle<String>) {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
  let url = format!("http://{}", listener.local_addr().expect("the listener has an address"));
  let handle = std::thread::spawn(move || {
    let (mut stream, _) = listener.accept().expect("the client connects");
    let mut request_line = String::new();
    BufReader::new(stream.try_clone().expect("clone the connection"))
      .read_line(&mut request_line)
      .expect("read the request line");
    stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").expect("answer the request");
    request_line
  });
  (url, handle)
}

/// The request line of the request on `stream`. The head is read to its blank line before the server answers, so the
/// server never closes a connection with request bytes unread.
fn request_line(stream: &TcpStream) -> String {
  let head: Vec<String> = BufReader::new(stream.try_clone().expect("clone the connection"))
    .lines()
    .map_while(Result::ok)
    .take_while(|line| !line.is_empty())
    .collect();
  head.into_iter().next().expect("the request has a request line")
}

#[tokio::test]
async fn https_requests_go_through_https_proxy() {
  let (proxy_url, proxy) = refusing_proxy();
  let settings = Settings::from_lookup(|name| (name == "HTTPS_PROXY").then(|| proxy_url.clone()));
  let client = http_client::client(&settings).expect("the client builds");
  let error = client.get("https://example.test/").send().await.expect_err("the proxy refuses the tunnel");
  assert!(error.is_connect(), "{error}");
  assert_eq!(proxy.join().expect("the proxy thread finishes"), "CONNECT example.test:443 HTTP/1.1\r\n");
}

#[tokio::test]
async fn a_redirect_comes_back_unfollowed() {
  // The Location names a port that nothing listens on: a followed redirect fails the send instead of answering.
  let closed = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
  let location = format!("http://{}/followed", closed.local_addr().expect("the listener has an address"));
  drop(closed);
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
  let url = format!("http://{}/start", listener.local_addr().expect("the listener has an address"));
  let answer_location = location.clone();
  let server = std::thread::spawn(move || {
    let (mut stream, _) = listener.accept().expect("the client connects");
    let line = request_line(&stream);
    stream
      .write_all(format!("HTTP/1.1 302 Found\r\nLocation: {answer_location}\r\nContent-Length: 0\r\n\r\n").as_bytes())
      .expect("answer the request");
    line
  });
  let client = http_client::client(&Settings::default()).expect("the client builds");
  let response = client.get(url).send().await.expect("the redirect comes back as the response");
  assert_eq!(response.status(), 302);
  assert_eq!(response.headers()["location"].to_str().expect("the location is text"), location);
  assert_eq!(server.join().expect("the server thread finishes"), "GET /start HTTP/1.1");
}

#[test]
fn a_bundle_without_certificates_is_refused() {
  let settings = Settings::from_lookup(|name| (name == "SSL_CERT_FILE").then(|| "/dev/null".to_string()));
  let error = http_client::client(&settings).expect_err("an empty bundle leaves no trust root");
  assert!(matches!(error, ClientError::Tls(_)), "{error}");
}

#[test]
fn the_shared_client_is_open_until_one_close_drops_it() {
  assert!(!http_client::close_shared_client(), "no client is open before the first use");
  http_client::shared_client().expect("the shared client builds");
  assert!(http_client::close_shared_client(), "the first use left a client open");
  assert!(!http_client::close_shared_client(), "a second close finds nothing to drop");
}
