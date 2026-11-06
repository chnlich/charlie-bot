//! The GitHub draft platform: the server calls the REST API with the operator's own token.
//!
//! The token command comes from the core config's `github.user_token_command`, so no repository content names the
//! operator's host. A pull request opens with `draft: true`; a comment goes to the issues comments endpoint. The
//! sender never retries and never falls back to another account.

use std::sync::Arc;
use std::time::Duration;

use charliebot_compat::http_client::{self, Settings};
use charliebot_registry::draft::{DraftContext, DraftPlatform, DraftPlatformFactory, DraftSendFailure, DraftSent};
use futures_util::future::BoxFuture;
use http::header;
use serde_json::Value;

use crate::target::{self, Target};

const API_BASE: &str = "https://api.github.com/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The GitHub platform: a fresh HTTP client per send request, and the host's token command.
pub struct GithubDraftPlatform {
  http: reqwest::Client,
  api_base: String,
  token_command: Option<String>,
}

impl GithubDraftPlatform {
  /// The platform built from the live host config, with a fresh HTTP client.
  pub fn from_context(context: &DraftContext) -> anyhow::Result<Self> {
    let config = context.config.get()?;
    let token_command = config.github.user_token_command.clone();
    let http = http_client::client(&Settings::from_env())?;
    Ok(Self::with_client(http, API_BASE.to_string(), token_command))
  }

  /// The platform built over an injected client and base URL.
  pub fn with_client(http: reqwest::Client, api_base: String, token_command: Option<String>) -> Self {
    Self { http, api_base, token_command }
  }

  /// The send of `body` in the form the target describes: a draft pull request, or a comment.
  pub async fn send(&self, target: &Value, body: &str) -> Result<DraftSent, DraftSendFailure> {
    let target = Target::parse(target).map_err(DraftSendFailure::rejected)?;
    let token = self.operator_token().await?;
    let (path, payload) = target.request(body);
    let response = self
      .http
      .post(format!("{}{path}", self.api_base))
      .bearer_auth(token)
      .header(header::ACCEPT, "application/vnd.github+json")
      .header(header::CONTENT_TYPE, "application/json")
      .header("X-GitHub-Api-Version", "2022-11-28")
      .header(header::USER_AGENT, "charliebot")
      .timeout(REQUEST_TIMEOUT)
      .body(serde_json::to_vec(&payload).expect("a JSON payload serializes"))
      .send()
      .await
      .map_err(|error| DraftSendFailure::unknown(format!("POST {path}: {error}")))?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| DraftSendFailure::unknown(format!("POST {path}: {error}")))?;
    let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !status.is_success() {
      let message = payload.get("message").and_then(Value::as_str).unwrap_or("no message");
      return Err(DraftSendFailure::rejected(format!("POST {path}: HTTP {status}: {message}")));
    }
    let sender = payload
      .pointer("/user/login")
      .and_then(Value::as_str)
      .filter(|login| !login.is_empty())
      .ok_or_else(|| {
        DraftSendFailure::unknown(format!("POST {path}: the response names no author, so the write may have landed"))
      })?
      .to_string();
    let url = payload
      .get("html_url")
      .and_then(Value::as_str)
      .filter(|url| !url.is_empty())
      .ok_or_else(|| {
        DraftSendFailure::unknown(format!("POST {path}: the response names no URL, so the write may have landed"))
      })?
      .to_string();
    Ok(DraftSent { sender, url })
  }

  /// The token the host's token command prints, from one run of that command.
  async fn operator_token(&self) -> Result<String, DraftSendFailure> {
    let Some(command) = self.token_command.as_deref().filter(|command| !command.trim().is_empty()) else {
      return Err(DraftSendFailure::rejected(
        "github.user_token_command is not configured, so a GitHub draft cannot be sent from the operator's account",
      ));
    };
    let Some(parts) = shlex::split(command).filter(|parts| !parts.is_empty()) else {
      return Err(DraftSendFailure::rejected("github.user_token_command is not a valid command line"));
    };
    let output = tokio::process::Command::new(&parts[0])
      .args(&parts[1..])
      .output()
      .await
      .map_err(|error| DraftSendFailure::rejected(format!("running github.user_token_command: {error}")))?;
    if !output.status.success() {
      return Err(DraftSendFailure::rejected(format!("github.user_token_command exited with {}", output.status)));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
      return Err(DraftSendFailure::rejected("github.user_token_command printed no token"));
    }
    Ok(token)
  }
}

impl DraftPlatform for GithubDraftPlatform {
  fn name(&self) -> &'static str {
    "github"
  }

  fn validate_target(&self, target: &Value) -> Result<Value, String> {
    target::validate_target(target)
  }

  fn target_label(&self, target: &Value) -> String {
    target::target_label(target)
  }

  fn send<'a>(&'a self, target: &'a Value, body: &'a str) -> BoxFuture<'a, Result<DraftSent, DraftSendFailure>> {
    Box::pin(async move { Self::send(self, target, body).await })
  }
}

/// The factory the GitHub package registers under the platform name `github`.
pub fn platform_factory() -> DraftPlatformFactory {
  Arc::new(|context: &DraftContext| {
    let platform = GithubDraftPlatform::from_context(context)?;
    Ok(Arc::new(platform) as Arc<dyn DraftPlatform>)
  })
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::os::unix::fs::PermissionsExt;
  use std::sync::{Arc, Mutex};
  use std::thread;

  use charliebot_registry::draft::DraftFailureOutcome;
  use serde_json::json;

  use super::*;

  struct Recorded {
    path: String,
    authorization: String,
    body: Value,
  }

  /// One stub GitHub API: it answers the first request with `status` and `value`, and records what it saw.
  struct Stub {
    base: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
  }

  impl Stub {
    fn start(status: u16, value: Value) -> Stub {
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let base = format!("http://{}/", listener.local_addr().unwrap());
      let seen = Arc::new(Mutex::new(Vec::new()));
      let recorded = Arc::clone(&seen);
      thread::spawn(move || {
        for stream in listener.incoming() {
          let Ok(mut stream) = stream else { break };
          let Some(request) = read_request(&mut stream) else { continue };
          recorded.lock().unwrap().push(request);
          write_response(&mut stream, status, &value);
        }
      });
      Stub { base, seen }
    }

    fn requests(&self) -> Vec<Recorded> {
      self
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|record| Recorded {
          path: record.path.clone(),
          authorization: record.authorization.clone(),
          body: record.body.clone(),
        })
        .collect()
    }
  }

  fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).ok()?;
      if read == 0 {
        return None;
      }
      request.extend_from_slice(&buffer[..read]);
      if request.windows(4).any(|window| window == b"\r\n\r\n") {
        break;
      }
    }
    let text = String::from_utf8_lossy(&request).to_string();
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let authorization = text
      .lines()
      .find_map(|line| line.to_ascii_lowercase().strip_prefix("authorization:").map(|value| value.trim().to_string()))
      .unwrap_or_default();
    let length: usize = text
      .lines()
      .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|value| value.trim().to_string()))
      .and_then(|value| value.parse().ok())
      .unwrap_or(0);
    let body_start = text.find("\r\n\r\n")? + 4;
    let mut body = request[body_start..].to_vec();
    while body.len() < length {
      let read = stream.read(&mut buffer).ok()?;
      if read == 0 {
        break;
      }
      body.extend_from_slice(&buffer[..read]);
    }
    Some(Recorded { path, authorization, body: serde_json::from_slice(&body).unwrap_or(Value::Null) })
  }

  fn write_response(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = value.to_string();
    let response = format!(
      "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
      body.len()
    );
    let _ = stream.write_all(response.as_bytes());
  }

  fn platform(base: &str, token_command: Option<String>) -> GithubDraftPlatform {
    charliebot_cli_common::http::ensure_crypto_provider();
    GithubDraftPlatform::with_client(reqwest::Client::new(), base.to_string(), token_command)
  }

  fn token_script(dir: &std::path::Path) -> String {
    let path = dir.join("print-token");
    std::fs::write(&path, "#!/bin/sh\necho synthetic-github-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
  }

  fn long_body() -> String {
    (0..100).map(|index| format!("line {index} of the approved body\n")).collect()
  }

  #[tokio::test]
  async fn a_draft_pull_request_posts_its_four_fields_and_the_body() {
    let dir = tempfile::tempdir().unwrap();
    let stub =
      Stub::start(201, json!({"html_url": "https://github.example/o/r/pull/7", "user": {"login": "operator"}}));
    let body = long_body();

    let sent = platform(&stub.base, Some(token_script(dir.path())))
      .send(&json!({"repo": "o/r", "title": "T", "head": "feature", "base": "main"}), &body)
      .await
      .unwrap();

    assert_eq!(sent.sender, "operator");
    assert_eq!(sent.url, "https://github.example/o/r/pull/7");
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/repos/o/r/pulls");
    assert_eq!(requests[0].authorization.to_ascii_lowercase(), "bearer synthetic-github-token");
    assert_eq!(requests[0].body, json!({"title": "T", "head": "feature", "base": "main", "body": body, "draft": true}));
  }

  #[tokio::test]
  async fn a_comment_posts_the_body_to_the_issue_comments_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let stub =
      Stub::start(201, json!({"html_url": "https://github.example/o/r/pull/7#c", "user": {"login": "operator"}}));

    platform(&stub.base, Some(token_script(dir.path())))
      .send(&json!({"repo": "o/r", "number": 7}), "the comment")
      .await
      .unwrap();

    let requests = stub.requests();
    assert_eq!(requests[0].path, "/repos/o/r/issues/7/comments");
    assert_eq!(requests[0].body, json!({"body": "the comment"}));
  }

  #[tokio::test]
  async fn a_platform_error_response_is_a_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start(404, json!({"message": "Not Found"}));

    let failure = platform(&stub.base, Some(token_script(dir.path())))
      .send(&json!({"repo": "o/r", "number": 7}), "the comment")
      .await
      .unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Rejected);
    assert!(failure.detail.contains("Not Found"), "{}", failure.detail);
    assert_eq!(stub.requests().len(), 1, "the sender never retries");
  }

  #[tokio::test]
  async fn a_lost_response_is_unknown() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let holder = thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      drop(stream);
    });
    let dir = tempfile::tempdir().unwrap();

    let failure = platform(&base, Some(token_script(dir.path())))
      .send(&json!({"repo": "o/r", "number": 7}), "the comment")
      .await
      .unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Unknown);
    holder.join().unwrap();
  }

  #[tokio::test]
  async fn an_absent_token_command_is_a_rejection_before_any_request() {
    let stub = Stub::start(201, json!({"html_url": "u", "user": {"login": "operator"}}));

    let failure =
      platform(&stub.base, None).send(&json!({"repo": "o/r", "number": 7}), "the comment").await.unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Rejected);
    assert!(failure.detail.contains("github.user_token_command is not configured"), "{}", failure.detail);
    assert!(stub.requests().is_empty());
  }

  #[tokio::test]
  async fn a_failing_token_command_is_a_rejection_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start(201, json!({"html_url": "u", "user": {"login": "operator"}}));

    let failure = platform(&stub.base, Some("false".to_string()))
      .send(&json!({"repo": "o/r", "number": 7}), "x")
      .await
      .unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Rejected);
    assert!(failure.detail.contains("exited with"), "{}", failure.detail);
    assert!(stub.requests().is_empty());
    drop(dir);
  }

  #[test]
  fn the_registered_platform_validates_and_labels_the_same_target() {
    let platform = platform(API_BASE, None);
    assert_eq!(platform.name(), "github");
    assert_eq!(
      platform.validate_target(&json!({"repo": "o/r", "number": 3})).unwrap(),
      json!({"repo": "o/r", "number": 3})
    );
    assert_eq!(platform.target_label(&json!({"repo": "o/r", "number": 3})), "GitHub comment on o/r#3");
  }
}
