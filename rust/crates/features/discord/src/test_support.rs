//! Fixtures for the Discord tests: a loopback server that answers as Discord's REST API does, and a feature context over a
//! temporary home with this package registered. The client runs over real HTTP, so each request it sends is recorded.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use charliebot_core::config::{BackendOption, CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RunningTurnInputs, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::{CreateTask, Store, TaskCaller};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

/// One request a fake Discord server received.
#[derive(Clone, Debug)]
pub struct Recorded {
  pub method: Method,
  /// The path under the server's root, `/api/v10/...` for the REST calls.
  pub path: String,
  /// The query string as the client wrote it.
  pub query: HashMap<String, String>,
  pub headers: HeaderMap,
  pub body: Vec<u8>,
}

impl Recorded {
  async fn read(request: Request<Body>) -> Self {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX).await.expect("the request body reads");
    let query = parts
      .uri
      .query()
      .unwrap_or_default()
      .split('&')
      .filter_map(|pair| pair.split_once('='))
      .map(|(name, value)| (name.to_string(), value.to_string()));
    Self {
      method: parts.method,
      path: parts.uri.path().to_string(),
      query: query.collect(),
      headers: parts.headers,
      body: body.to_vec(),
    }
  }

  /// The request body as JSON.
  pub fn json(&self) -> Value {
    serde_json::from_slice(&self.body).expect("the request body is JSON")
  }
}

/// One answer a fake Discord server gives.
pub struct Reply {
  status: StatusCode,
  body: Vec<u8>,
}

impl Reply {
  pub fn json(status: u16, body: Value) -> Self {
    Self { status: StatusCode::from_u16(status).expect("a test status is valid"), body: body.to_string().into_bytes() }
  }

  pub fn empty(status: u16) -> Self {
    Self { status: StatusCode::from_u16(status).expect("a test status is valid"), body: Vec::new() }
  }
}

impl IntoResponse for Reply {
  fn into_response(self) -> Response {
    let mut response = Response::new(Body::from(self.body));
    *response.status_mut() = self.status;
    response
  }
}

/// Serve `responder` on a loopback port and return the Discord API base it answers (`http://127.0.0.1:<port>/api/v10`).
pub async fn serve(responder: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> String {
  let responder = Arc::new(responder);
  let app = Router::new().fallback(move |request: Request<Body>| {
    let responder = Arc::clone(&responder);
    async move { responder(&Recorded::read(request).await) }
  });
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a loopback port binds");
  let address = listener.local_addr().expect("the listener has an address");
  tokio::spawn(async move { axum::serve(listener, app).await.expect("the fake Discord serves") });
  format!("http://{address}/api/v10")
}

/// The synthetic account the test config maps to a person.
pub const ALLOWED_USER: &str = "700000000000000001";
pub const ALLOWED_PERSON: &str = "tester";
pub const OTHER_USER: &str = "700000000000000002";
pub const TEST_TOKEN: &str = "test-bot-token";
const FIXTURE_BACKEND: &str = "fixture";

/// The state of the Discord fake a route test drives: channels, the bot's identity and guilds, and what it recorded.
#[derive(Default)]
pub struct FakeDiscord {
  /// Channel id to its messages, oldest first.
  pub channels: HashMap<String, Vec<Value>>,
  /// Channel objects returned by GET /channels/{channel_id}.
  pub channel_objects: HashMap<String, Value>,
  /// Channels whose reads answer 403.
  pub hidden: Vec<String>,
  pub user: Value,
  pub application: Value,
  pub guilds: Value,
  pub gateway_url: Option<String>,
  /// Posted messages as (channel id, content).
  pub posts: Vec<(String, String)>,
  /// Bodies sent to POST /channels/{channel_id}/messages.
  pub post_bodies: Vec<Value>,
  /// Messages accepted by nonce, so the fake server can model Discord's enforce_nonce behavior.
  pub nonce_messages: HashMap<String, Value>,
  /// Accepted posts whose HTTP response is failed, for retry cases.
  pub accepted_post_failures_remaining: usize,
  /// Started threads as (parent channel, source message, thread name).
  pub started_threads: Vec<(String, String, String)>,
  /// Each request as the server saw it: method, path and query.
  pub calls: Vec<String>,
  /// A message that lands in a channel right after its first read answers.
  pub landing: Option<(String, Value)>,
}

impl FakeDiscord {
  /// The fake's answer to one request.
  pub fn answer(&mut self, request: &Recorded) -> Reply {
    let mut pairs: Vec<String> = request.query.iter().map(|(name, value)| format!("{name}={value}")).collect();
    pairs.sort();
    let query = pairs.join("&");
    self.calls.push(format!(
      "{} {}{}",
      request.method,
      request.path,
      if query.is_empty() { String::new() } else { format!("?{query}") }
    ));
    let parts: Vec<&str> = request.path.split('/').collect();
    match (request.method.as_str(), parts.as_slice()) {
      ("GET", ["", "api", "v10", "gateway", "bot"]) => {
        Reply::json(200, json!({"url": self.gateway_url.as_deref().unwrap_or("wss://gateway.discord.gg")}))
      }
      ("GET", ["", "api", "v10", "users", "@me"]) => Reply::json(200, self.user.clone()),
      ("GET", ["", "api", "v10", "applications", "@me"]) => Reply::json(200, self.application.clone()),
      ("GET", ["", "api", "v10", "users", "@me", "guilds"]) => Reply::json(200, self.guilds.clone()),
      ("GET", ["", "api", "v10", "channels", channel]) => self.channel_objects.get(*channel).map_or_else(
        || Reply::json(404, json!({"code": 10003, "message": "Unknown Channel"})),
        |value| Reply::json(200, value.clone()),
      ),
      ("GET", ["", "api", "v10", "channels", channel, "messages", message]) => {
        match self.find_message(channel, message) {
          Some(message) => Reply::json(200, message),
          None => Reply::json(404, json!({"code": 10008, "message": "Unknown Message"})),
        }
      }
      ("GET", ["", "api", "v10", "channels", channel, "messages"]) => self.read_messages(channel, &request.query),
      ("POST", ["", "api", "v10", "channels", channel, "messages"]) => {
        let body = request.json();
        self.post_bodies.push(body.clone());
        let content = body["content"].as_str().unwrap_or_default().to_string();
        if let Some(nonce) = body.get("nonce").and_then(Value::as_str)
          && let Some(existing) = self.nonce_messages.get(nonce)
        {
          return Reply::json(200, existing.clone());
        }
        let message = json!({
          "id": format!("900000000000000{:03}", self.posts.len() + 100),
          "channel_id": channel,
          "content": content,
          "author": {
            "id": self.user["id"],
            "username": self.user["username"],
            "bot": true,
          },
          "timestamp": "2099-01-01T00:00:00Z",
        });
        self.posts.push((channel.to_string(), content.clone()));
        self.channels.entry(channel.to_string()).or_default().push(message.clone());
        if let Some(nonce) = body.get("nonce").and_then(Value::as_str) {
          self.nonce_messages.insert(nonce.to_string(), message.clone());
        }
        if self.accepted_post_failures_remaining > 0 {
          self.accepted_post_failures_remaining -= 1;
          return Reply::json(500, json!({"code": 0, "message": "synthetic response timeout"}));
        }
        Reply::json(200, message)
      }
      ("POST", ["", "api", "v10", "channels", _, "messages", _, "threads"]) => {
        let body = request.json();
        self.started_threads.push((
          parts[4].to_string(),
          parts[6].to_string(),
          body["name"].as_str().unwrap().to_string(),
        ));
        Reply::json(201, json!({"id": "900000000000000003", "type": 11, "name": body["name"]}))
      }
      ("PUT" | "DELETE", ["", "api", "v10", "channels", _, "messages", _, "reactions", _, "@me"]) => Reply::empty(204),
      _ => Reply::json(404, json!({"code": 0, "message": "404: Not Found"})),
    }
  }

  fn find_message(&self, channel: &str, message_id: &str) -> Option<Value> {
    self.channels.get(channel)?.iter().find(|message| message["id"] == message_id).cloned()
  }

  /// Discord's read of a channel: the newest `limit` messages without `after`, else the first `limit` above it.
  fn read_messages(&mut self, channel: &str, query: &HashMap<String, String>) -> Reply {
    if self.hidden.iter().any(|hidden| hidden == channel) {
      return Reply::json(403, json!({"code": 50001, "message": "Missing Access"}));
    }
    let limit: usize = query["limit"].parse().expect("the limit is a number");
    let messages = self.channels.get(channel).cloned().unwrap_or_default();
    let page = match query.get("after") {
      None => messages[messages.len().saturating_sub(limit)..].to_vec(),
      Some(after) => {
        let floor: u64 = after.parse().expect("the after id is a number");
        messages
          .into_iter()
          .filter(|message| message["id"].as_str().unwrap().parse::<u64>().unwrap() > floor)
          .take(limit)
          .collect()
      }
    };
    if let Some((landing_channel, _)) = &self.landing
      && landing_channel == channel
    {
      let (_, message) = self.landing.take().expect("the landing message is pending");
      self.channels.entry(channel.to_string()).or_default().push(message);
    }
    Reply::json(200, Value::Array(page))
  }
}

/// A store-backed feature context over a temporary home, with this package registered and a fake Discord server.
pub struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
  pub api: String,
  pub discord: Arc<Mutex<FakeDiscord>>,
}

impl Home {
  /// A home whose config allows `ALLOWED_USER` and whose credentials hold `TEST_TOKEN` when `token`.
  pub async fn new(token: bool) -> Self {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    fs::write(
      dir.path().join(CONFIG_FILENAME),
      format!("discord:\n  allowed_users:\n    \"{ALLOWED_USER}\": {ALLOWED_PERSON}\n"),
    )
    .expect("the config writes");
    if token {
      fs::write(dir.path().join(CREDENTIALS_FILENAME), format!("discord:\n  bot_token: {TEST_TOKEN}\n"))
        .expect("the credentials write");
    }
    let mut registry = Registry::new();
    registry.register_package("src.features.discord", crate::register).expect("the discord package registers");
    let registry = Arc::new(registry);
    let config_home = dir.path().to_path_buf();
    let config_registry = Arc::clone(&registry);
    let config = Arc::new(HotReload::new(
      dir.path().join(CONFIG_FILENAME),
      "discord_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join(CREDENTIALS_FILENAME),
      "discord_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let mut loaded = registry.load_config(dir.path()).expect("the synthetic config loads");
    loaded.backends.options.push(BackendOption {
      id: FIXTURE_BACKEND.to_string(),
      label: "Synthetic fixture".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    fs::create_dir_all(loaded.sessions_dir()).expect("the synthetic sessions directory exists");
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let startup = StartupState::default();
    startup.insert(RunningTurnInputs::new(|_| Vec::new())).expect("the running-turn stub inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
    features.registry.bind_feature_context(&features).expect("the registry binds its feature context once");
    charliebot_chat_threads::test_support::install_test_dispatcher(&features);
    features
      .startup
      .insert(Arc::new(charliebot_sched::triggers::TriggerManager::new(Arc::clone(&features))))
      .expect("the trigger manager inserts once");
    let fake_discord =
      FakeDiscord { user: json!({"id": "600000000000000001", "username": "synthetic-bot"}), ..FakeDiscord::default() };
    let discord = Arc::new(Mutex::new(fake_discord));
    let fake = Arc::clone(&discord);
    let api = serve(move |request| fake.lock().expect("the fake lock is never poisoned").answer(request)).await;
    Self { _dir: dir, features, api, discord }
  }

  /// The routes of this package, answering on the fake's API base.
  pub fn router(&self) -> Router {
    crate::api::routes_at(&self.features, &self.api)
  }

  /// A Discord-origin manager session `session_id` (or a generated id) in `thread`, with an optional read watermark.
  pub async fn thread_session(&self, session_id: &str, origin: Value, watermark: Option<&str>) -> String {
    let mut slots = Map::new();
    slots.insert("discord_origin".to_string(), origin);
    let meta = self.create(session_id, "discord session", slots).await;
    if let Some(watermark) = watermark {
      let fields = Map::from_iter([("discord_watermark_id".to_string(), json!(watermark))]);
      self
        .features
        .store
        .update_slot_fields_at(&meta.id, "discord", fields, UtcDateTime::now())
        .await
        .expect("the watermark writes")
        .expect("the session exists");
    }
    meta.id
  }

  /// A manager session with no Discord origin.
  pub async fn plain_session(&self, name: &str) -> String {
    self.create(&format!("plain-{name}"), name, Map::new()).await.id
  }

  async fn create(&self, session_id: &str, name: &str, slots: Map<String, Value>) -> SessionMetadata {
    let request = CreateTask {
      request_id: format!("root-{session_id}"),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some(name.to_string()),
      // No backend runs in these tests, so the session names one the config does not define.
      backend: Some(FIXTURE_BACKEND.to_string()),
      group: None,
      session_id: Some(session_id.to_string()),
    };
    self.features.store.create_task(request, TaskCaller::Operator, slots).await.expect("the session is created")
  }

  /// Appends one chat event and returns it with its stamped id.
  pub async fn append(&self, session_id: &str, event: Value) -> Value {
    self.features.store.persist_and_broadcast(session_id, event).await.expect("the chat event appends").to_value()
  }

  /// The session's chat events in log order.
  pub fn events(&self, session_id: &str) -> Vec<Value> {
    self
      .features
      .store
      .load_chat_events_sync(session_id)
      .expect("the chat log reads")
      .iter()
      .map(charliebot_store::ChatEvent::to_value)
      .collect()
  }

  /// The session's metadata as the store reads it.
  pub async fn meta(&self, session_id: &str) -> SessionMetadata {
    self.features.store.get_session(session_id).await.expect("the metadata reads").expect("the session exists")
  }

  /// The fake's recorded state.
  pub fn fake(&self) -> std::sync::MutexGuard<'_, FakeDiscord> {
    self.discord.lock().expect("the fake lock is never poisoned")
  }
}

/// A Discord message as the REST API writes it: number `i` fixes the id and the timestamp, so ids of different digit
/// counts never string-sort.
#[derive(Clone)]
pub struct Msg {
  i: u64,
  id: Option<String>,
  content: String,
  author: String,
  bot: bool,
  global_name: Option<String>,
  attachments: Vec<String>,
}

impl Msg {
  /// A plain message from an allowed account, numbered `i`.
  pub fn new(i: u64, content: &str) -> Self {
    Self {
      i,
      id: None,
      content: content.to_string(),
      author: ALLOWED_USER.to_string(),
      bot: false,
      global_name: None,
      attachments: Vec::new(),
    }
  }

  /// The message's id, when it is not `snowflake(i)` (a thread's starter takes the thread's id).
  pub fn id(mut self, id: &str) -> Self {
    self.id = Some(id.to_string());
    self
  }

  pub fn by(mut self, author: &str) -> Self {
    self.author = author.to_string();
    self
  }

  pub fn bot(mut self) -> Self {
    self.bot = true;
    self
  }

  pub fn global_name(mut self, name: &str) -> Self {
    self.global_name = Some(name.to_string());
    self
  }

  pub fn attachment(mut self, url: &str) -> Self {
    self.attachments.push(url.to_string());
    self
  }

  pub fn value(&self) -> Value {
    let mut author = json!({"id": self.author, "username": "plain-name", "global_name": self.global_name});
    if self.bot {
      author["bot"] = json!(true);
    }
    json!({
      "id": self.id.clone().unwrap_or_else(|| snowflake(self.i)),
      "author": author,
      "timestamp": format!("2026-08-26T00:00:{:02}Z", self.i),
      "content": self.content,
      "attachments": self.attachments.iter().map(|url| json!({"url": url})).collect::<Vec<_>>(),
      "type": 0,
    })
  }
}

/// The synthetic snowflake `900000000000000000 + i`.
pub fn snowflake(i: u64) -> String {
  (900000000000000000 + i).to_string()
}
