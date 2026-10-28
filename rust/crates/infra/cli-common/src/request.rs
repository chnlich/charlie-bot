//! The `<service> request` verb: one HTTP request whose credential is minted inside the CLI process.
//!
//! `charliebot google request`, `charliebot feishu request` and `charliebot slack request` all share
//! this module: argument parsing for the method, URL, body and headers, the send, the response
//! printing, the exit codes and the redaction. A 2xx response prints its body on stdout and exits 0.
//! Another status or a transport failure exits 1 with a JSON object on stderr; a usage error exits 2.
//! The service's credential failure keeps its own code (the Linear table's 3).

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde_json::json;

use crate::exit::{self, CliExit};

/// The request timeout, matching the Python clients' `urlopen(..., timeout=30)`.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The arguments every `<service> request` verb shares.
#[derive(Args, Debug, Clone)]
pub struct RequestArgs {
  /// The HTTP method.
  pub method: String,
  /// The absolute URL.
  pub url: String,
  /// Read the request body from this file.
  #[arg(long, value_name = "PATH", conflicts_with = "json")]
  pub data_file: Option<PathBuf>,
  /// Send this text as the request body, with a JSON content type.
  #[arg(long, value_name = "TEXT", conflicts_with = "data_file")]
  pub json: Option<String>,
  /// Add one request header, `NAME:VALUE`. Repeat for more.
  #[arg(long = "header", value_name = "NAME:VALUE")]
  pub header: Vec<String>,
}

impl RequestArgs {
  /// Validate the arguments and read the body. The credential's bearer header is added later, so a
  /// usage error costs no token minting.
  pub fn prepare(&self) -> Result<Request, CliExit> {
    let method = parse_method(&self.method)?;
    let url = parse_url(&self.url)?;
    let headers = self.headers()?;
    let body = match (&self.data_file, &self.json) {
      (Some(path), _) => Some(std::fs::read(path).map_err(|_| CliExit::usage("could not read the request body"))?),
      (None, Some(text)) => Some(text.clone().into_bytes()),
      (None, None) => None,
    };
    Ok(Request { method, url, headers, body })
  }

  /// The user's headers, plus a JSON content type when `--json` supplied the body and the user did
  /// not set one.
  fn headers(&self) -> Result<Vec<(String, String)>, CliExit> {
    let mut headers = Vec::new();
    for header in &self.header {
      let Some((name, value)) = header.split_once(':') else {
        return Err(CliExit::usage("each --header must be NAME:VALUE"));
      };
      let (name, value) = (name.trim(), value.trim());
      if name.is_empty()
        || value.contains(['\r', '\n'])
        || name.contains(['\r', '\n'])
        || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
        || reqwest::header::HeaderValue::from_str(value).is_err()
      {
        return Err(CliExit::usage("each --header must be one NAME:VALUE line"));
      }
      if name.eq_ignore_ascii_case("authorization") {
        return Err(CliExit::usage("the Authorization header is set by the command"));
      }
      headers.push((name.to_string(), value.to_string()));
    }
    let has_content_type = headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    if self.json.is_some() && !has_content_type {
      headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    Ok(headers)
  }
}

/// One request ready for the transport.
#[derive(Debug, Clone)]
pub struct Request {
  method: reqwest::Method,
  url: String,
  headers: Vec<(String, String)>,
  body: Option<Vec<u8>>,
}

impl Request {
  /// One request with no header and no body.
  pub fn new(method: reqwest::Method, url: impl Into<String>) -> Request {
    Request { method, url: url.into(), headers: Vec::new(), body: None }
  }

  /// Add one request header.
  pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Request {
    self.headers.push((name.into(), value.into()));
    self
  }

  /// Send this body.
  pub fn body(mut self, body: Vec<u8>) -> Request {
    self.body = Some(body);
    self
  }

  /// Add the credential's bearer header. Nothing else carries the token out of the process.
  pub fn bearer(self, token: &str) -> Request {
    self.header("Authorization", format!("Bearer {token}"))
  }
}

/// One response as the request verb sees it.
#[derive(Debug, Clone)]
pub struct Response {
  pub status: u16,
  pub body: Vec<u8>,
}

/// Send one prepared request.
pub fn send(request: &Request, redactions: &[&str]) -> Result<Response, CliExit> {
  crate::http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| transport_failure(&error.to_string(), redactions))?;
  runtime.block_on(async {
    let client = reqwest::Client::builder()
      // curl without `-L` does not follow a redirect, and the command replaced curl in the skills:
      // an unexpected redirect then surfaces as its status instead of silently changing the target.
      .redirect(reqwest::redirect::Policy::none())
      .timeout(TIMEOUT)
      .build()
      .map_err(|error| transport_failure(&error.to_string(), redactions))?;
    let mut pending = client.request(request.method.clone(), &request.url);
    for (name, value) in &request.headers {
      pending = pending.header(name.as_str(), value.as_str());
    }
    if let Some(body) = &request.body {
      pending = pending.body(body.clone());
    }
    let response = pending.send().await.map_err(|error| transport_failure(&exit::source_chain(&error), redactions))?;
    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(|error| transport_failure(&exit::source_chain(&error), redactions))?;
    Ok(Response { status, body: body.to_vec() })
  })
}

/// The response body of a 2xx answer; another status is an HTTP failure on stderr with exit 1.
pub fn fetch(request: &Request, redactions: &[&str]) -> Result<Vec<u8>, CliExit> {
  let response = send(request, redactions)?;
  if !(200..300).contains(&response.status) {
    return Err(http_failure(response.status, &response.body, redactions));
  }
  Ok(redact_bytes(&response.body, redactions))
}

/// The whole verb for a service whose credential is one bearer token: prepare, send, print.
pub fn run(args: &RequestArgs, token: &str) -> Result<(), CliExit> {
  let body = fetch(&args.prepare()?.bearer(token), &[token])?;
  write_stdout(&body)
}

/// Print a successful answer on stdout.
pub fn write_stdout(body: &[u8]) -> Result<(), CliExit> {
  io::stdout().write_all(body).map_err(|_| CliExit::error("could not write the response"))
}

/// The exit-1 failure of a non-2xx status: the status and the redacted response body on stderr.
pub fn http_failure(status: u16, body: &[u8], redactions: &[&str]) -> CliExit {
  let response = redact(&String::from_utf8_lossy(body), redactions);
  CliExit::contract(json!({ "status": status, "response": response }), 1)
}

/// The exit-1 failure of a transport error: the redacted cause on stderr.
pub fn transport_failure(cause: &str, redactions: &[&str]) -> CliExit {
  CliExit::contract(json!({ "status": null, "response": "request failed", "error": redact(cause, redactions) }), 1)
}

/// Replace every non-empty secret in `text`, so a credential never reaches a log or a transcript.
pub fn redact(text: &str, redactions: &[&str]) -> String {
  let mut text = text.to_string();
  for secret in redactions.iter().filter(|secret| !secret.is_empty()) {
    text = text.replace(secret, "[redacted]");
  }
  text
}

/// Replace every non-empty secret in a response body, preserving bytes that are not a secret.
pub fn redact_bytes(body: &[u8], redactions: &[&str]) -> Vec<u8> {
  let mut body = body.to_vec();
  for secret in redactions.iter().filter(|secret| !secret.is_empty()) {
    let secret = secret.as_bytes();
    let mut redacted = Vec::with_capacity(body.len());
    let mut position = 0;
    while position < body.len() {
      if body[position..].starts_with(secret) {
        redacted.extend_from_slice(b"[redacted]");
        position += secret.len();
      } else {
        redacted.push(body[position]);
        position += 1;
      }
    }
    body = redacted;
  }
  body
}

fn parse_method(method: &str) -> Result<reqwest::Method, CliExit> {
  let upper = method.to_ascii_uppercase();
  reqwest::Method::from_bytes(upper.as_bytes()).map_err(|_| CliExit::usage("METHOD must be an HTTP method name"))
}

fn parse_url(url: &str) -> Result<String, CliExit> {
  let invalid = || CliExit::usage("URL must be an absolute http or https URL");
  let parsed = reqwest::Url::parse(url).map_err(|_| invalid())?;
  if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
    return Err(invalid());
  }
  Ok(parsed.into())
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::thread;

  use super::*;

  const TOKEN: &str = "synthetic-access-token";

  fn args(method: &str, url: &str) -> RequestArgs {
    RequestArgs { method: method.to_string(), url: url.to_string(), data_file: None, json: None, header: Vec::new() }
  }

  fn stub(status: u16, body: &[u8]) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    let body = body.to_vec();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      sender.send(read_request(&mut stream)).unwrap();
      let response = format!(
        "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      let _ = stream.write_all(response.as_bytes());
      let _ = stream.write_all(&body);
    });
    (url, receiver)
  }

  fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).unwrap();
      if read == 0 {
        return request;
      }
      request.extend_from_slice(&buffer[..read]);
      if request.windows(4).any(|window| window == b"\r\n\r\n") {
        return request;
      }
    }
  }

  fn text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  #[test]
  fn prepare_sends_the_json_body_headers_and_bearer() {
    let (url, receiver) = stub(200, br#"{"ok":true}"#);
    let mut arguments = args("post", &url);
    arguments.json = Some(r#"{"title":"x"}"#.to_string());
    arguments.header = vec!["Accept: application/json".to_string()];

    let body = fetch(&arguments.prepare().unwrap().bearer(TOKEN), &[TOKEN]).unwrap();
    let request = String::from_utf8(receiver.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    let lowered = request.to_ascii_lowercase();

    assert_eq!(body, br#"{"ok":true}"#.to_vec());
    assert!(request.starts_with("POST /api HTTP/1.1\r\n"));
    assert!(lowered.contains("authorization: bearer synthetic-access-token"));
    assert!(lowered.contains("content-type: application/json"));
    assert!(lowered.contains("accept: application/json"));
    assert!(request.ends_with(r#"{"title":"x"}"#));
  }

  #[test]
  fn a_user_authorization_header_is_a_usage_error() {
    let mut arguments = args("GET", "http://127.0.0.1:1/api");
    arguments.header = vec![format!("Authorization: Bearer {TOKEN}")];
    let error = arguments.prepare().unwrap_err();
    assert_eq!(error.code, 2);
    assert!(!text(&error).contains(TOKEN));
  }

  #[test]
  fn usage_errors_are_exit_two_and_echo_no_secret() {
    for arguments in [
      args("GET", "relative/path"),
      args("G ET", "http://127.0.0.1:1/api"),
      RequestArgs { data_file: Some(PathBuf::from("/nonexistent/body")), ..args("POST", "http://127.0.0.1:1/api") },
    ] {
      let error = arguments.prepare().unwrap_err();
      assert_eq!(error.code, 2);
      assert!(!text(&error).contains(TOKEN));
    }
  }

  #[test]
  fn a_non_2xx_status_reports_the_redacted_body_and_exits_one() {
    let (url, receiver) = stub(401, format!("denied {TOKEN}").as_bytes());
    let error = fetch(&args("GET", &url).prepare().unwrap().bearer(TOKEN), &[TOKEN]).unwrap_err();
    let _ = receiver.recv_timeout(Duration::from_secs(2));

    assert_eq!(error.code, 1);
    assert_eq!(error.body["status"], 401);
    assert_eq!(error.body["response"], "denied [redacted]");
    assert!(!text(&error).contains(TOKEN));
  }

  #[test]
  fn a_success_body_redacts_the_bearer_token_and_preserves_binary_bytes() {
    let response = [b"echo ".as_slice(), TOKEN.as_bytes(), b"\xff".as_slice()].concat();
    let (url, receiver) = stub(200, &response);
    let body = fetch(&args("GET", &url).prepare().unwrap().bearer(TOKEN), &[TOKEN]).unwrap();
    let _ = receiver.recv_timeout(Duration::from_secs(2));

    assert_eq!(body, [b"echo [redacted]".as_slice(), b"\xff".as_slice()].concat());
    assert!(!body.windows(TOKEN.len()).any(|window| window == TOKEN.as_bytes()));
  }

  #[test]
  fn a_refused_connection_reports_a_redacted_cause() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = fetch(&args("GET", &format!("http://127.0.0.1:{port}/api")).prepare().unwrap().bearer(TOKEN), &[TOKEN])
      .unwrap_err();

    assert_eq!(error.code, 1);
    assert!(error.body["status"].is_null());
    assert!(error.body["error"].as_str().is_some_and(|cause| !cause.is_empty()));
    assert!(!text(&error).contains(TOKEN));
  }
}
