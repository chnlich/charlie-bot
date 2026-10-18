//! The outbound client sends https requests through the proxy `HTTPS_PROXY` names, and refuses a certificate
//! bundle with no certificate in it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
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

#[tokio::test]
async fn https_requests_go_through_https_proxy() {
  let (proxy_url, proxy) = refusing_proxy();
  let settings = Settings::from_lookup(|name| (name == "HTTPS_PROXY").then(|| proxy_url.clone()));
  let client = http_client::client(&settings).expect("the client builds");
  let error = client.get("https://example.test/").send().await.expect_err("the proxy refuses the tunnel");
  assert!(error.is_connect(), "{error}");
  assert_eq!(proxy.join().expect("the proxy thread finishes"), "CONNECT example.test:443 HTTP/1.1\r\n");
}

#[test]
fn a_bundle_without_certificates_is_refused() {
  let settings = Settings::from_lookup(|name| (name == "SSL_CERT_FILE").then(|| "/dev/null".to_string()));
  let error = http_client::client(&settings).expect_err("an empty bundle leaves no trust root");
  assert!(matches!(error, ClientError::Tls(_)), "{error}");
}
