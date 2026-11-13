//! The Slack draft platform: `auth.test` for the sending user, `conversations.open` for a DM or group DM, then
//! `chat.postMessage` with the registered body.
//!
//! Every call authenticates with `slack.user_token`. A DM or group DM opens the conversation first; a thread reply
//! carries `thread_ts`. The sender never retries and never falls back to the bot token.

use std::time::Duration;

use charliebot_compat::http_client::{self, Settings};
use charliebot_registry::draft::{DraftContext, DraftPlatform, DraftPlatformFactory, DraftSendFailure, DraftSent};
use futures_util::future::BoxFuture;
use http::header;
use serde_json::{Value, json};

use crate::draft_target;
use crate::slack_client::{
  API_BASE, AUTH_TEST, CHANNEL, CHAT_POST_MESSAGE, CONVERSATIONS_OPEN, TEXT, THREAD_TS, USERS,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The Slack platform's sender, built for one send from the live credentials.
pub struct SlackDraftSender {
  http: reqwest::Client,
  api_base: String,
  token: String,
  timeout: Duration,
}

impl SlackDraftSender {
  /// The sender built from the live Slack user token, over a fresh HTTP client.
  pub fn from_context(context: &DraftContext) -> anyhow::Result<Self> {
    let credentials = context.credentials.get()?;
    let token = credentials.require("slack", "user_token")?;
    let http = http_client::client(&Settings::from_env())?;
    Ok(Self::with_client(http, API_BASE.to_string(), token, REQUEST_TIMEOUT))
  }

  /// The sender built over an injected client and base URL.
  pub fn with_client(http: reqwest::Client, api_base: String, token: String, timeout: Duration) -> Self {
    Self { http, api_base, token, timeout }
  }

  /// The send of `body` to the target exactly as registered.
  ///
  /// `auth.test` runs first, so a missing or refused user-token scope fails before a DM is opened or a message posts.
  pub async fn send(&self, target: &Value, body: &str) -> Result<DraftSent, DraftSendFailure> {
    let auth = self.post(AUTH_TEST, json!({})).await?;
    let sender = auth
      .get("user_id")
      .and_then(Value::as_str)
      .filter(|user| !user.is_empty())
      .ok_or_else(|| DraftSendFailure::rejected(format!("{AUTH_TEST}: no user_id")))?
      .to_string();
    let channel = self.channel(target).await?;
    let mut payload = json!({(CHANNEL): channel, (TEXT): body});
    if let Some(thread) = target.get("thread").and_then(Value::as_str) {
      payload[THREAD_TS] = json!(thread);
    }
    let posted = self.post(CHAT_POST_MESSAGE, payload).await?;
    let ts = posted
      .get("ts")
      .and_then(Value::as_str)
      .filter(|ts| !ts.is_empty())
      .ok_or_else(|| DraftSendFailure::rejected(format!("{CHAT_POST_MESSAGE}: no ts")))?;
    let url = format!("https://slack.com/archives/{channel}/p{}", ts.replace('.', ""));
    Ok(DraftSent { sender, url })
  }

  /// The channel a post goes to: the named channel, or the one `conversations.open` resolves for the users.
  async fn channel(&self, target: &Value) -> Result<String, DraftSendFailure> {
    if let Some(channel) = target.get("channel").and_then(Value::as_str) {
      if channel.is_empty() {
        return Err(DraftSendFailure::rejected("the Slack target names no channel"));
      }
      return Ok(channel.to_string());
    }
    let users: Vec<&str> = target
      .get("users")
      .and_then(Value::as_array)
      .map(|users| users.iter().filter_map(Value::as_str).collect())
      .unwrap_or_default();
    if users.is_empty() {
      return Err(DraftSendFailure::rejected("the Slack target names no channel and no users"));
    }
    let opened = self.post(CONVERSATIONS_OPEN, json!({(USERS): users.join(",")})).await?;
    opened
      .pointer("/channel/id")
      .and_then(Value::as_str)
      .filter(|channel| !channel.is_empty())
      .map(str::to_string)
      .ok_or_else(|| DraftSendFailure::rejected(format!("{CONVERSATIONS_OPEN}: no channel id")))
  }

  async fn post(&self, method: &str, payload: Value) -> Result<Value, DraftSendFailure> {
    let response = self
      .http
      .post(format!("{}{method}", self.api_base))
      .bearer_auth(&self.token)
      .header(header::CONTENT_TYPE, "application/json")
      .timeout(self.timeout)
      .body(serde_json::to_vec(&payload).expect("a JSON payload serializes"))
      .send()
      .await
      .map_err(|error| DraftSendFailure::unknown(format!("{method}: {error}")))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| DraftSendFailure::unknown(format!("{method}: {error}")))?;
    if !status.is_success() {
      return Err(DraftSendFailure::rejected(format!("{method}: HTTP {status}")));
    }
    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
      DraftSendFailure::unknown(format!("{method}: the response is not JSON, so the message may have gone out"))
    })?;
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
      let error = payload.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
      return Err(DraftSendFailure::rejected(format!("{method}: {error}")));
    }
    Ok(payload)
  }
}

/// The Slack entry in the registry's draft platforms.
pub struct SlackDraftPlatform {
  sender: SlackDraftSender,
}

impl DraftPlatform for SlackDraftPlatform {
  fn name(&self) -> &'static str {
    "slack"
  }

  fn validate_target(&self, target: &Value) -> Result<Value, String> {
    draft_target::validate_target(target)
  }

  fn target_label(&self, target: &Value) -> String {
    draft_target::target_label(target)
  }

  fn send<'a>(&'a self, target: &'a Value, body: &'a str) -> BoxFuture<'a, Result<DraftSent, DraftSendFailure>> {
    Box::pin(async move { self.sender.send(target, body).await })
  }
}

/// The factory the Slack package registers under the platform name `slack`.
pub fn platform_factory() -> DraftPlatformFactory {
  std::sync::Arc::new(|context: &DraftContext| {
    let sender = SlackDraftSender::from_context(context)?;
    Ok(std::sync::Arc::new(SlackDraftPlatform { sender }) as std::sync::Arc<dyn DraftPlatform>)
  })
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::sync::{Arc, Mutex};
  use std::thread;
  use std::time::Duration;

  use charliebot_registry::draft::DraftFailureOutcome;

  use super::*;

  /// One stub Slack Web API: it answers each recorded request by the method in its path.
  struct Stub {
    base: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
  }

  impl Stub {
    fn start(answers: Vec<(&'static str, u16, Value)>) -> Stub {
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let base = format!("http://{}/api/", listener.local_addr().unwrap());
      let requests = Arc::new(Mutex::new(Vec::new()));
      let recorded = Arc::clone(&requests);
      let answers: Vec<(String, u16, Value)> =
        answers.into_iter().map(|(method, status, value)| (method.to_string(), status, value)).collect();
      // The thread lives as long as the test binary; every response closes its connection, and a test's later stubs
      // bind their own ports.
      thread::spawn(move || {
        for stream in listener.incoming() {
          let Ok(mut stream) = stream else { break };
          let Some((method, body)) = read_request(&mut stream) else { continue };
          recorded.lock().unwrap().push((method.clone(), body));
          let Some((_, status, value)) = answers.iter().find(|(name, _, _)| *name == method) else {
            write_response(&mut stream, 500, &json!({"ok": false, "error": "no_scripted_answer"}));
            continue;
          };
          write_response(&mut stream, *status, value);
        }
      });
      Stub { base, requests }
    }

    fn sender(&self) -> SlackDraftSender {
      charliebot_cli_common::http::ensure_crypto_provider();
      SlackDraftSender::with_client(
        reqwest::Client::new(),
        self.base.clone(),
        "synthetic-user-token".to_string(),
        Duration::from_secs(2),
      )
    }

    fn requests(&self) -> Vec<(String, Value)> {
      self.requests.lock().unwrap().clone()
    }

    fn bodies(&self, method: &str) -> Vec<Value> {
      self.requests().into_iter().filter(|(name, _)| name == method).map(|(_, body)| body).collect()
    }

    fn posts(&self) -> Vec<Value> {
      self.bodies("chat.postMessage")
    }
  }

  fn read_request(stream: &mut TcpStream) -> Option<(String, Value)> {
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
    let path = request_line.split_whitespace().nth(1)?;
    let method = path.rsplit('/').next()?.to_string();
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
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Some((method, body))
  }

  fn write_response(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = value.to_string();
    let response = format!(
      "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
      body.len()
    );
    let _ = stream.write_all(response.as_bytes());
  }

  fn ok_answers() -> Vec<(&'static str, u16, Value)> {
    vec![
      ("auth.test", 200, json!({"ok": true, "user_id": "U0SYNTHETIC"})),
      ("conversations.open", 200, json!({"ok": true, "channel": {"id": "D0SYNTHETIC"}})),
      ("chat.postMessage", 200, json!({"ok": true, "channel": "C0SYNTHETIC", "ts": "1700000000.000200"})),
    ]
  }

  fn body_text() -> String {
    (0..120).map(|index| format!("line {index} of the approved message\n")).collect()
  }

  #[tokio::test]
  async fn a_channel_post_sends_the_registered_body_without_opening_a_conversation() {
    let stub = Stub::start(ok_answers());
    let body = body_text();

    let sent = stub.sender().send(&json!({"channel": "C0SYNTHETIC", "thread": null}), &body).await.unwrap();

    assert_eq!(sent.sender, "U0SYNTHETIC");
    assert_eq!(sent.url, "https://slack.com/archives/C0SYNTHETIC/p1700000000000200");
    let posts = stub.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0], json!({"channel": "C0SYNTHETIC", "text": body}));
    assert!(stub.bodies("conversations.open").is_empty());
  }

  #[tokio::test]
  async fn a_thread_reply_carries_the_thread_timestamp() {
    let stub = Stub::start(ok_answers());

    stub
      .sender()
      .send(&json!({"channel": "C0SYNTHETIC", "thread": "1700000000.000100"}), "thread answer")
      .await
      .unwrap();

    assert_eq!(
      stub.posts()[0],
      json!({"channel": "C0SYNTHETIC", "text": "thread answer", "thread_ts": "1700000000.000100"})
    );
  }

  #[tokio::test]
  async fn a_dm_opens_the_conversation_first_and_posts_into_it() {
    let stub = Stub::start(ok_answers());

    let sent = stub.sender().send(&json!({"users": ["U0SYNTHETIC"]}), "private answer").await.unwrap();

    assert_eq!(stub.bodies("conversations.open"), [json!({"users": "U0SYNTHETIC"})]);
    assert_eq!(stub.posts(), [json!({"channel": "D0SYNTHETIC", "text": "private answer"})]);
    assert_eq!(sent.url, "https://slack.com/archives/D0SYNTHETIC/p1700000000000200");
  }

  #[tokio::test]
  async fn a_group_dm_opens_the_conversation_with_every_user() {
    let stub = Stub::start(ok_answers());

    stub.sender().send(&json!({"users": ["U0SYNTHETIC", "U1SYNTHETIC"]}), "group answer").await.unwrap();

    assert_eq!(stub.bodies("conversations.open"), [json!({"users": "U0SYNTHETIC,U1SYNTHETIC"})]);
    assert_eq!(stub.posts(), [json!({"channel": "D0SYNTHETIC", "text": "group answer"})]);
  }

  #[tokio::test]
  async fn a_missing_chat_write_scope_is_a_rejection_and_posts_nothing() {
    let stub = Stub::start(vec![
      ("auth.test", 200, json!({"ok": true, "user_id": "U0SYNTHETIC"})),
      ("chat.postMessage", 200, json!({"ok": false, "error": "missing_scope"})),
    ]);

    let failure = stub.sender().send(&json!({"channel": "C0SYNTHETIC", "thread": null}), "body").await.unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Rejected);
    assert!(failure.detail.contains("missing_scope"), "{}", failure.detail);
    assert_eq!(stub.posts().len(), 1, "the sender never retries");
  }

  #[tokio::test]
  async fn a_timeout_is_unknown() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}/api/", listener.local_addr().unwrap());
    let (request_seen, wait_for_request) = tokio::sync::oneshot::channel();
    let holder = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      assert!(read_request(&mut stream).is_some(), "the timed-out request reaches the stub");
      request_seen.send(()).unwrap();
      let mut buffer = [0_u8; 1024];
      while stream.read(&mut buffer).expect("the client closes its timed-out request") != 0 {}
    });
    charliebot_cli_common::http::ensure_crypto_provider();
    let sender = SlackDraftSender::with_client(
      reqwest::Client::new(),
      base,
      "synthetic-user-token".to_string(),
      Duration::from_secs(2),
    );
    let request =
      tokio::spawn(async move { sender.send(&json!({"channel": "C0SYNTHETIC", "thread": null}), "body").await });
    wait_for_request.await.unwrap();
    tokio::time::pause();

    let failure = request.await.unwrap().unwrap_err();

    assert_eq!(failure.outcome, DraftFailureOutcome::Unknown);
    holder.join().unwrap();
  }

  #[test]
  fn the_registered_platform_validates_and_labels_the_same_target() {
    charliebot_cli_common::http::ensure_crypto_provider();
    let sender = SlackDraftSender::with_client(
      reqwest::Client::new(),
      API_BASE.to_string(),
      "synthetic-user-token".to_string(),
      REQUEST_TIMEOUT,
    );
    let platform = SlackDraftPlatform { sender };
    assert_eq!(platform.name(), "slack");
    assert_eq!(platform.validate_target(&json!({"channel": "C1"})).unwrap(), json!({"channel": "C1", "thread": null}));
    assert_eq!(platform.target_label(&json!({"users": ["U1"]})), "Slack DM with U1");
  }
}
