//! The Feishu Open API client: one method per operation the Feishu feature makes, on the app's tenant access token.
//!
//! The token comes from `feishu.app_id` and `feishu.app_secret` and is cached in the server until shortly before it
//! expires. Neither the token nor the secret reaches an error message: a failed call names its method, path, HTTP
//! status, Feishu's code and Feishu's message.

use std::fmt::{self, Display};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use http::{Method, StatusCode, header};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The Open API's domain; every path below is relative to it.
pub const BASE_URL: &str = "https://open.feishu.cn";
const TENANT_TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";
const ENDPOINT_PATH: &str = "/callback/ws/endpoint";
const BOT_INFO_PATH: &str = "/open-apis/bot/v3/info";
/// The code Feishu answers `BOT_INFO_PATH` with while the app has no bot capability, which only the developer console
/// fixes.
pub const BOT_CAPABILITY_CODE: i64 = 11205;
const MESSAGES_PATH: &str = "/open-apis/im/v1/messages";
const CHATS_PATH: &str = "/open-apis/im/v1/chats";
/// A tenant token is replaced this long before Feishu says it expires.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The maximum page size Feishu accepts for list calls.
pub const PAGE_SIZE: usize = 50;

/// The tenant token the server holds, with the app it was minted for.
static TOKEN: LazyLock<Mutex<Option<CachedToken>>> = LazyLock::new(|| Mutex::new(None));

struct CachedToken {
  app_id: String,
  value: String,
  refresh_at: Instant,
}

/// One failed Open API call: its method, path, HTTP status, Feishu's error code (None when the answer has none) and
/// Feishu's message. The request body and the token never appear in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuAPIError {
  pub method: String,
  pub path: String,
  pub status: u16,
  pub code: Option<i64>,
  pub message: String,
}

impl Display for FeishuAPIError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let code = self.code.map_or_else(|| "None".to_string(), |code| code.to_string());
    write!(f, "{} {} failed: HTTP {} code={code} message={:?}", self.method, self.path, self.status, self.message)
  }
}

impl std::error::Error for FeishuAPIError {}

/// The long-connection endpoint Feishu answers with: the WebSocket URL and the interval of the client's pings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
  pub url: String,
  pub ping_interval: Duration,
}

/// One page of a list call: its items, and the token of the next page when Feishu has more.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
  pub items: Vec<Value>,
  pub next_page_token: Option<String>,
}

/// The Feishu REST client of one app. It holds the app secret, so it is never printed.
#[derive(Clone)]
pub struct FeishuClient {
  http: reqwest::Client,
  base_url: String,
  app_id: String,
  app_secret: String,
}

/// What one answer's body holds, as its `Content-Type` announces it: JSON for an Open API answer, and the file's
/// own bytes for a resource download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyKind {
  Json,
  Raw,
}

/// The answer of one HTTP call, before its JSON is read.
struct Answer {
  status: StatusCode,
  body_kind: BodyKind,
  body: Vec<u8>,
}

impl FeishuClient {
  /// A client that calls `base_url` (`BASE_URL` in production) as the app `app_id`.
  pub fn new(
    http: reqwest::Client,
    base_url: impl Into<String>,
    app_id: impl Into<String>,
    app_secret: impl Into<String>,
  ) -> Self {
    Self { http, base_url: base_url.into(), app_id: app_id.into(), app_secret: app_secret.into() }
  }

  /// The app's tenant token, minted once and reused until it nears expiry.
  pub async fn tenant_token(&self) -> anyhow::Result<String> {
    let mut cache = TOKEN.lock().await;
    if let Some(cached) =
      cache.as_ref().filter(|cached| cached.app_id == self.app_id && Instant::now() < cached.refresh_at)
    {
      return Ok(cached.value.clone());
    }
    let body = json!({"app_id": self.app_id, "app_secret": self.app_secret});
    let answer = self.send(Method::POST, TENANT_TOKEN_PATH, &[], None, Some(body)).await?;
    let minted = checked(&Method::POST, TENANT_TOKEN_PATH, answer)?;
    let value = minted["tenant_access_token"].as_str().context("the token answer has no tenant_access_token")?;
    let expire = minted["expire"].as_u64().context("the token answer has no expire")?;
    let lifetime = Duration::from_secs(expire).saturating_sub(TOKEN_REFRESH_MARGIN);
    *cache = Some(CachedToken {
      app_id: self.app_id.clone(),
      value: value.to_string(),
      refresh_at: Instant::now() + lifetime,
    });
    Ok(value.to_string())
  }

  /// One HTTP call. A bearer token goes in the Authorization header when given.
  async fn send(
    &self,
    method: Method,
    path: &str,
    query: &[(&str, String)],
    bearer: Option<&str>,
    body: Option<Value>,
  ) -> anyhow::Result<Answer> {
    let mut url = reqwest::Url::parse(&format!("{}{path}", self.base_url))?;
    if !query.is_empty() {
      url.query_pairs_mut().extend_pairs(query.iter().map(|(name, value)| (*name, value.as_str())));
    }
    let mut request = self.http.request(method, url).timeout(REQUEST_TIMEOUT);
    if let Some(token) = bearer {
      request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(body) = body {
      request =
        request.header(header::CONTENT_TYPE, "application/json; charset=utf-8").body(serde_json::to_vec(&body)?);
    }
    let response = request.send().await?;
    let status = response.status();
    let body_kind = match response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) {
      Some(value) if value.contains("json") => BodyKind::Json,
      Some(_) | None => BodyKind::Raw,
    };
    let body = response.bytes().await?.to_vec();
    Ok(Answer { status, body_kind, body })
  }

  /// One Open API call on the tenant token, answered with its JSON `data` (or the whole answer when it has no `data`).
  pub async fn call(
    &self,
    method: Method,
    path: &str,
    query: &[(&str, String)],
    body: Option<Value>,
  ) -> anyhow::Result<Value> {
    let token = self.tenant_token().await?;
    let answer = self.send(method.clone(), path, query, Some(&token), body).await?;
    checked(&method, path, answer)
  }

  /// The long-connection endpoint of this app (`POST /callback/ws/endpoint`, with the app id and secret in the body).
  pub async fn long_connection_endpoint(&self) -> anyhow::Result<Endpoint> {
    let body = json!({"AppID": self.app_id, "AppSecret": self.app_secret});
    let answer = self.send(Method::POST, ENDPOINT_PATH, &[], None, Some(body)).await?;
    let endpoint = checked(&Method::POST, ENDPOINT_PATH, answer)?;
    let url = endpoint["URL"].as_str().context("the endpoint answer has no URL")?;
    let ping_seconds = endpoint["ClientConfig"]["PingInterval"]
      .as_u64()
      .context("the endpoint answer has no ClientConfig.PingInterval")?;
    Ok(Endpoint { url: url.to_string(), ping_interval: Duration::from_secs(ping_seconds) })
  }

  /// The bot's open_id (`GET /open-apis/bot/v3/info`). Feishu answers `BOT_CAPABILITY_CODE` while the app has no bot
  /// capability.
  pub async fn bot_open_id(&self) -> anyhow::Result<String> {
    let info = self.call(Method::GET, BOT_INFO_PATH, &[], None).await?;
    let open_id = info["bot"]["open_id"].as_str().context("the bot info names no open_id")?;
    Ok(open_id.to_string())
  }

  /// The reply to `root_id` inside its thread (`POST .../messages/:id/reply` with `reply_in_thread`). Feishu keeps
  /// `uuid` for an hour, so a retried post with the same uuid does not repeat the message.
  pub async fn reply_in_thread(&self, root_id: &str, text: &str, uuid: &str) -> anyhow::Result<Value> {
    let body = json!({
      "msg_type": "text",
      "content": json!({"text": text}).to_string(),
      "reply_in_thread": true,
      "uuid": uuid,
    });
    self.call(Method::POST, &format!("{MESSAGES_PATH}/{}/reply", path_segment(root_id)?), &[], Some(body)).await
  }

  /// One page of a thread's messages, oldest first (`container_id_type=thread`).
  pub async fn thread_messages(&self, thread_id: &str, page_token: Option<&str>) -> anyhow::Result<Page> {
    self.message_page("thread", thread_id, "ByCreateTimeAsc", PAGE_SIZE, page_token).await
  }

  /// One page of a thread's messages with a caller-selected page size, oldest first.
  pub async fn thread_messages_sized(
    &self,
    thread_id: &str,
    page_size: usize,
    page_token: Option<&str>,
  ) -> anyhow::Result<Page> {
    self.message_page("thread", thread_id, "ByCreateTimeAsc", page_size, page_token).await
  }

  /// One page of a group's main chat, newest first (`container_id_type=chat`).
  pub async fn chat_messages(&self, chat_id: &str, page_size: usize, page_token: Option<&str>) -> anyhow::Result<Page> {
    self.message_page("chat", chat_id, "ByCreateTimeDesc", page_size, page_token).await
  }

  /// One page of a message container, ordered by creation time and capped at Feishu's page size.
  async fn message_page(
    &self,
    container_id_type: &str,
    container_id: &str,
    sort_type: &str,
    page_size: usize,
    page_token: Option<&str>,
  ) -> anyhow::Result<Page> {
    let mut query = vec![
      ("container_id_type", container_id_type.to_string()),
      ("container_id", path_segment(container_id)?.to_string()),
      ("sort_type", sort_type.to_string()),
      ("page_size", page_size.min(PAGE_SIZE).to_string()),
    ];
    if let Some(token) = page_token {
      query.push(("page_token", token.to_string()));
    }
    let data = self.call(Method::GET, MESSAGES_PATH, &query, None).await?;
    page_of(&data)
  }

  /// One page of a group's members, keyed by open_id (`GET /open-apis/im/v1/chats/:chat_id/members`).
  pub async fn chat_members(&self, chat_id: &str, page_token: Option<&str>) -> anyhow::Result<Page> {
    let mut query = vec![("member_id_type", "open_id".to_string()), ("page_size", PAGE_SIZE.to_string())];
    if let Some(token) = page_token {
      query.push(("page_token", token.to_string()));
    }
    let path = format!("{CHATS_PATH}/{}/members", path_segment(chat_id)?);
    let data = self.call(Method::GET, &path, &query, None).await?;
    page_of(&data)
  }

  /// One message by id (`GET /open-apis/im/v1/messages/:id`), the first of its items.
  pub async fn message(&self, message_id: &str) -> anyhow::Result<Value> {
    let data = self.call(Method::GET, &format!("{MESSAGES_PATH}/{}", path_segment(message_id)?), &[], None).await?;
    let items = data["items"].as_array().context("the message answer has no items")?;
    items.first().cloned().context("the message answer holds no message")
  }

  /// The addition of an emoji reaction to a message (`POST .../messages/:id/reactions`). The answer carries the
  /// `reaction_id`.
  pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> anyhow::Result<Value> {
    let body = json!({"reaction_type": {"emoji_type": emoji_type}});
    self.call(Method::POST, &format!("{MESSAGES_PATH}/{}/reactions", path_segment(message_id)?), &[], Some(body)).await
  }

  /// One page of a message's reactions of one emoji type (`GET .../messages/:id/reactions`).
  pub async fn reactions(&self, message_id: &str, emoji_type: &str, page_token: Option<&str>) -> anyhow::Result<Page> {
    let mut query = vec![("reaction_type", emoji_type.to_string()), ("page_size", PAGE_SIZE.to_string())];
    if let Some(token) = page_token {
      query.push(("page_token", token.to_string()));
    }
    let path = format!("{MESSAGES_PATH}/{}/reactions", path_segment(message_id)?);
    let data = self.call(Method::GET, &path, &query, None).await?;
    page_of(&data)
  }

  /// The deletion of one reaction of a message (`DELETE .../messages/:id/reactions/:reaction_id`).
  pub async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
    let path = format!("{MESSAGES_PATH}/{}/reactions/{}", path_segment(message_id)?, path_segment(reaction_id)?);
    self.call(Method::DELETE, &path, &[], None).await?;
    Ok(())
  }

  /// The bytes of one image or file of a message (`GET .../messages/:id/resources/:key?type=`). A JSON answer is an
  /// error: Feishu answers errors as JSON and the file as its bytes.
  pub async fn download_resource(&self, message_id: &str, file_key: &str, kind: &str) -> anyhow::Result<Vec<u8>> {
    let path = format!("{MESSAGES_PATH}/{}/resources/{}", path_segment(message_id)?, path_segment(file_key)?);
    let token = self.tenant_token().await?;
    let answer = self.send(Method::GET, &path, &[("type", kind.to_string())], Some(&token), None).await?;
    if answer.body_kind == BodyKind::Json || !answer.status.is_success() {
      checked(&Method::GET, &path, answer)?;
      bail!("GET {path} answered without the file");
    }
    Ok(answer.body)
  }
}

/// A list call's page: the `items`, and the `page_token` of the next page while `has_more` holds.
fn page_of(data: &Value) -> anyhow::Result<Page> {
  let items = data["items"].as_array().context("the list answer has no items")?.clone();
  let has_more = data["has_more"].as_bool().context("the list answer has no has_more")?;
  let next_page_token = if has_more {
    Some(data["page_token"].as_str().context("the list answer has more pages but no page_token")?.to_string())
  } else {
    None
  };
  Ok(Page { items, next_page_token })
}

/// The `data` of an answer that succeeded (HTTP 2xx and code 0). Any other answer fails with a `FeishuAPIError`.
fn checked(method: &Method, path: &str, answer: Answer) -> anyhow::Result<Value> {
  let parsed: Option<Value> =
    if answer.body_kind == BodyKind::Json { serde_json::from_slice(&answer.body).ok() } else { None };
  let code = parsed.as_ref().and_then(|value| value.get("code")).and_then(Value::as_i64);
  let succeeded = answer.status.is_success() && code.is_none_or(|code| code == 0);
  if !succeeded {
    let message =
      parsed.as_ref().and_then(|value| value.get("msg")).and_then(Value::as_str).unwrap_or_default().to_string();
    return Err(
      FeishuAPIError {
        method: method.to_string(),
        path: path.to_string(),
        status: answer.status.as_u16(),
        code,
        message,
      }
      .into(),
    );
  }
  match parsed {
    Some(value) => Ok(value.get("data").cloned().unwrap_or(value)),
    None => bail!("{method} {path} answered with no JSON body"),
  }
}

/// An id or key used as one URL path segment: letters, digits, `_` and `-` only, so no id can add a path or query.
pub fn path_segment(id: &str) -> anyhow::Result<&str> {
  if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
    return Ok(id);
  }
  bail!("a Feishu id is letters, digits, '_' or '-': {id:?}")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::{Answer, serve};
  use charliebot_compat::http_client;

  #[test]
  fn a_path_segment_refuses_anything_that_could_rewrite_the_path() {
    assert_eq!(path_segment("om_100-abc").unwrap(), "om_100-abc");
    assert!(path_segment("om/../x").is_err());
    assert!(path_segment("om?x=1").is_err());
    assert!(path_segment("").is_err());
  }

  // Each test takes its own app id: the tenant token cache is keyed by it, so no test reads another test's token.

  fn client(base_url: &str, app_id: &str) -> FeishuClient {
    let http = http_client::client(&http_client::Settings::from_env()).expect("the HTTP client builds");
    FeishuClient::new(http, base_url, app_id, "synthetic-secret")
  }

  #[tokio::test]
  async fn a_reply_posts_its_text_into_the_thread_with_the_uuid() {
    let stub = serve(|_| Answer::json(json!({"code": 0, "data": {"message_id": "om_reply"}}))).await;
    let answer = client(&stub.base_url, "cli_reply").reply_in_thread("om_root", "hello", "uuid-1").await.unwrap();
    assert_eq!(answer["message_id"], "om_reply");
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/open-apis/im/v1/messages/om_root/reply");
    assert_eq!(
      requests[0].body,
      json!({"msg_type": "text", "content": "{\"text\":\"hello\"}", "reply_in_thread": true, "uuid": "uuid-1"})
    );
  }

  #[tokio::test]
  async fn an_ack_reaction_posts_its_emoji_on_the_message() {
    let stub = serve(|_| Answer::json(json!({"code": 0, "data": {"reaction_id": "r1"}}))).await;
    client(&stub.base_url, "cli_react").add_reaction("om_summon", "OnIt").await.unwrap();
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].target, "/open-apis/im/v1/messages/om_summon/reactions");
    assert_eq!(requests[0].body, json!({"reaction_type": {"emoji_type": "OnIt"}}));
  }

  #[tokio::test]
  async fn reactions_report_the_next_page_and_delete_by_reaction_id() {
    let stub = serve(|request| match request.method.as_str() {
      "GET" => Answer::json(json!({"code": 0, "data": {"items": [], "has_more": true, "page_token": "p2"}})),
      _ => Answer::json(json!({"code": 0, "data": {}})),
    })
    .await;
    let client = client(&stub.base_url, "cli_reactions");
    let page = client.reactions("om_summon", "OnIt", None).await.unwrap();
    assert_eq!(page.next_page_token.as_deref(), Some("p2"));
    client.delete_reaction("om_summon", "r1").await.unwrap();
    let requests = stub.requests.lock().unwrap();
    assert!(requests[0].target.starts_with("/open-apis/im/v1/messages/om_summon/reactions?reaction_type=OnIt"));
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].target, "/open-apis/im/v1/messages/om_summon/reactions/r1");
  }

  #[tokio::test]
  async fn a_thread_read_names_the_thread_and_carries_the_page_token_forward() {
    let stub = serve(|_| {
      Answer::json(
        json!({"code": 0, "data": {"items": [{"message_id": "om_1"}], "has_more": true, "page_token": "p2"}}),
      )
    })
    .await;
    let client = client(&stub.base_url, "cli_thread");
    let first = client.thread_messages("omt_1", None).await.unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.next_page_token.as_deref(), Some("p2"));
    client.thread_messages("omt_1", Some("p2")).await.unwrap();
    let requests = stub.requests.lock().unwrap();
    assert!(requests[0].target.contains("container_id_type=thread&container_id=omt_1&sort_type=ByCreateTimeAsc"));
    assert!(!requests[0].target.contains("page_token="));
    assert!(requests[1].target.contains("page_token=p2"));
  }

  #[tokio::test]
  async fn a_resource_download_returns_the_file_bytes() {
    let bytes = b"\x89PNG-synthetic".to_vec();
    let body = bytes.clone();
    let stub =
      serve(move |_| Answer { status: 200, content_type: "application/octet-stream", body: body.clone() }).await;
    let fetched = client(&stub.base_url, "cli_download").download_resource("om_1", "img_k", "image").await.unwrap();
    assert_eq!(fetched, bytes);
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].target, "/open-apis/im/v1/messages/om_1/resources/img_k?type=image");
  }

  #[tokio::test]
  async fn a_json_answer_to_a_download_is_an_error() {
    let stub = serve(|_| Answer::json(json!({"code": 99991663, "msg": "invalid token"}))).await;
    let result = client(&stub.base_url, "cli_download_error").download_resource("om_1", "img_k", "file").await;
    assert!(result.is_err());
  }
}
