//! The production transport: reqwest on one current-thread tokio runtime, with the failure classes
//! and error messages the Python raw-socket client produces.

use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

use crate::wire::{Method, TransportError, WireRequest, WireResponse};

/// reqwest's workspace feature set ships no default rustls crypto provider; the CLI's
/// `http://localhost` base URLs never negotiate TLS, but the client builder refuses to build
/// without one installed. Failure to install can only mean another provider is already installed,
/// which serves the same purpose.
/// Make a rustls crypto provider available to reqwest clients.
pub fn ensure_crypto_provider() {
  static INSTALLED: OnceLock<()> = OnceLock::new();
  INSTALLED.get_or_init(|| {
    let _ = rustls::crypto::ring::default_provider().install_default();
  });
}

/// One request over the real HTTP client.
pub fn send_http(request: &WireRequest) -> Result<WireResponse, TransportError> {
  static RUNTIME: OnceLock<Runtime> = OnceLock::new();
  let runtime = RUNTIME
    .get_or_init(|| Builder::new_current_thread().enable_all().build().expect("tokio current-thread runtime builds"));
  runtime.block_on(send_async(request))
}

async fn send_async(request: &WireRequest) -> Result<WireResponse, TransportError> {
  ensure_crypto_provider();
  let client = reqwest::Client::builder()
    // The base URL is the config-owned internal server on loopback: no proxy, as Python's raw
    // socket client that ignores the proxy environment entirely.
    .no_proxy()
    .timeout(request.timeout)
    .build()
    .map_err(|error| TransportError::SentButLost(error.to_string()))?;
  let method = match request.method {
    Method::Get => reqwest::Method::GET,
    Method::Post => reqwest::Method::POST,
    Method::Patch => reqwest::Method::PATCH,
  };
  let mut pending = client.request(method, &request.url);
  for (name, value) in &request.headers {
    pending = pending.header(name, value);
  }
  if let Some(body) = &request.body {
    pending = pending.body(body.clone());
  }
  let response = pending.send().await.map_err(map_response_head_error)?;
  let status = response.status();
  let reason = status.canonical_reason().unwrap_or("").to_string();
  let body = response.bytes().await.map_err(|error| TransportError::SentButLost(error.to_string()))?;
  Ok(WireResponse { status: status.as_u16(), reason, body: body.to_vec() })
}

/// Classify a failure of the response-head phase. A connect that never established is the retry
/// class; a connection closed before any status line is Python's missing-status-line parse failure.
fn map_response_head_error(error: reqwest::Error) -> TransportError {
  if error.is_connect() {
    return TransportError::ConnectPhase(cpython_os_error_message(&error));
  }
  if source_chain_contains(&error, "connection closed before message completed") {
    // hyper's message for a connection closed before the response head; Python's raw client
    // reports the same condition as its missing-or-oversized status line parse failure.
    return TransportError::SentButLost("missing or oversized status line".to_string());
  }
  TransportError::SentButLost(error.to_string())
}

/// CPython `str(OSError)`: `[Errno <errno>] <strerror>`, read off the deepest OS error in the chain.
fn cpython_os_error_message(error: &reqwest::Error) -> String {
  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
  while let Some(current) = source {
    if let Some(io_error) = current.downcast_ref::<std::io::Error>()
      && let Some(errno) = io_error.raw_os_error()
    {
      let text = io_error.to_string();
      let suffix = format!(" (os error {errno})");
      let strerror = text.strip_suffix(&suffix).unwrap_or(&text);
      return format!("[Errno {errno}] {strerror}");
    }
    source = current.source();
  }
  error.to_string()
}

fn source_chain_contains(error: &reqwest::Error, needle: &str) -> bool {
  let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
  while let Some(current) = source {
    if current.to_string().contains(needle) {
      return true;
    }
    source = current.source();
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Read, Write};
  use std::net::TcpListener;

  fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
  }

  /// One accept that reads the whole request and closes without an answer; the listener lives
  /// until the acceptor has served the one connection.
  fn drop_after_request_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let mut buffer = [0u8; 4096];
      let _ = stream.read(&mut buffer);
      drop(stream);
    });
    port
  }

  fn get(port: u16, path: &str) -> WireRequest {
    WireRequest {
      method: Method::Get,
      url: format!("http://localhost:{port}{path}"),
      headers: Vec::new(),
      body: None,
      timeout: std::time::Duration::from_secs(5),
    }
  }

  #[test]
  fn a_refused_connection_is_a_connect_phase_error_with_the_cpython_message() {
    let error = send_http(&get(free_port(), "/api/internal/x")).unwrap_err();
    assert_eq!(error, TransportError::ConnectPhase("[Errno 111] Connection refused".to_string()));
  }

  #[test]
  fn a_connection_closed_before_the_status_line_reads_as_the_missing_status_line() {
    let port = drop_after_request_server();
    let error = send_http(&get(port, "/api/internal/x")).unwrap_err();
    assert_eq!(error, TransportError::SentButLost("missing or oversized status line".to_string()));
  }

  #[test]
  fn a_200_answer_round_trips_status_reason_and_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let mut buffer = [0u8; 4096];
      let read = stream.read(&mut buffer).unwrap();
      let request_text = String::from_utf8_lossy(&buffer[..read]).to_string();
      let body = br#"{"ok": true}"#;
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(body).unwrap();
      request_text
    });

    let response = send_http(&get(port, "/api/internal/x?k=v")).unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.reason, "OK");
    assert_eq!(response.body, br#"{"ok": true}"#.to_vec());
  }
}
