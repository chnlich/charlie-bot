//! A loopback stand-in for the Feishu Open API. Each request except the tenant-token call is answered by the test's
//! handler and recorded, so a test checks the calls the client made and the answers it read. A [`Home`] is the
//! store-backed feature context the listener tests run against.

use std::sync::{Arc, Mutex};

use charliebot_core::config::BackendOption;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, RunningTurnInputs, StartupState};
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, Store, TaskCaller};
use charliebot_triggers::triggers::TriggerManager;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";
const TEST_BACKEND: &str = "synthetic-feishu-test";

/// One request the stub received.
#[derive(Clone, Debug)]
pub struct Recorded {
  pub method: String,
  /// The request target: the path with its query string, as sent.
  pub target: String,
  /// The JSON body, or `Value::Null` when the request had none.
  pub body: Value,
}

/// One canned answer: a status, a content type, and the body bytes.
pub struct Answer {
  pub status: u16,
  pub content_type: &'static str,
  pub body: Vec<u8>,
}

impl Answer {
  /// A 200 answer whose body is `value` as JSON.
  pub fn json(value: Value) -> Self {
    Answer { status: 200, content_type: "application/json", body: value.to_string().into_bytes() }
  }
}

/// A running stub. Dropping it stops the listener.
pub struct Stub {
  pub base_url: String,
  pub requests: Arc<Mutex<Vec<Recorded>>>,
  task: JoinHandle<()>,
}

impl Drop for Stub {
  fn drop(&mut self) {
    self.task.abort();
  }
}

/// A store and trigger manager over a temporary synthetic home, with the Feishu package registered.
pub struct Home {
  _dir: TempDir,
  pub features: Arc<FeatureContext>,
}

impl Home {
  pub fn new() -> Self {
    let dir = tempfile::tempdir().expect("the synthetic home exists");
    std::fs::write(
      dir.path().join("config.yaml"),
      "feishu:\n  allowed_users:\n    ou_0000000000000000000000000000000000000001: tester\n",
    )
    .expect("the synthetic config writes");
    std::fs::write(dir.path().join("credentials.yaml"), "feishu:\n  app_id: cli_synthetic\n  app_secret: s\n")
      .expect("the synthetic credentials write");

    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("features.feishu", crate::register).expect("the Feishu package registers");
    let registry = Arc::new(registry);
    let mut loaded = registry.load_config(dir.path()).expect("the synthetic Feishu config loads");
    loaded.backends.options.push(BackendOption {
      id: TEST_BACKEND.to_string(),
      label: "Synthetic Feishu fixture".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    std::fs::create_dir_all(loaded.sessions_dir()).expect("the synthetic sessions directory exists");
    let config_reload = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "feishu_test_config_reload_failed",
      Box::new(move || Ok(config_reload.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "feishu_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = StartupState::default();
    startup.insert(RunningTurnInputs::new(|_| Vec::new())).expect("running inputs insert once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });
    features.registry.bind_feature_context(&features).expect("the registry binds its feature context once");
    charliebot_chat_threads::test_support::install_test_dispatcher(&features);
    features
      .startup
      .insert(Arc::new(TriggerManager::new(Arc::clone(&features))))
      .expect("trigger manager inserts once");
    Self { _dir: dir, features }
  }

  /// The creation of a Feishu-origin manager session with an optional read watermark.
  pub async fn thread_session(&self, session_id: &str, origin: Value, watermark: Option<&str>) -> String {
    let mut slots = Map::from_iter([(crate::metadata::FEISHU_ORIGIN.to_string(), origin)]);
    if let Some(watermark) = watermark {
      slots.insert(crate::metadata::FEISHU_WATERMARK_ID.to_string(), json!(watermark));
    }
    let request = CreateTask {
      request_id: format!("root-{session_id}"),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some("Feishu test session".to_string()),
      backend: Some(TEST_BACKEND.to_string()),
      group: None,
      session_id: Some(session_id.to_string()),
    };
    self
      .features
      .store
      .create_task(request, TaskCaller::Operator, slots)
      .await
      .expect("the Feishu test session is created")
      .id
  }

  /// The session metadata as the store reads it.
  pub async fn meta(&self, session_id: &str) -> charliebot_store::metadata::SessionMetadata {
    self
      .features
      .store
      .get_session(session_id)
      .await
      .expect("the Feishu test metadata reads")
      .expect("the Feishu test session exists")
  }
}

/// A stub on a loopback port. It answers the tenant-token call itself, and every other request with `handler`. A
/// request is recorded before its answer is written, so the test reads it once the client returns.
pub async fn serve(handler: impl Fn(&Recorded) -> Answer + Send + Sync + 'static) -> Stub {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("the loopback port binds");
  let address = listener.local_addr().expect("the listener has an address");
  let requests = Arc::new(Mutex::new(Vec::new()));
  let recorded = Arc::clone(&requests);
  let task = tokio::spawn(async move {
    loop {
      let Ok((mut socket, _)) = listener.accept().await else { return };
      let request = read_request(&mut socket).await;
      let answer = if request.target.starts_with(TOKEN_PATH) {
        Answer::json(json!({"code": 0, "tenant_access_token": "t-synthetic", "expire": 7200}))
      } else {
        let answer = handler(&request);
        recorded.lock().expect("the request log is never poisoned").push(request);
        answer
      };
      write_answer(&mut socket, answer).await;
    }
  });
  Stub { base_url: format!("http://{address}"), requests, task }
}

async fn read_request(socket: &mut TcpStream) -> Recorded {
  let mut reader = BufReader::new(socket);
  let mut line = String::new();
  reader.read_line(&mut line).await.expect("a request line arrives");
  let mut parts = line.split_whitespace();
  let method = parts.next().expect("the request line names a method").to_string();
  let target = parts.next().expect("the request line names a target").to_string();
  let mut length = 0;
  loop {
    let mut header = String::new();
    reader.read_line(&mut header).await.expect("a header line arrives");
    let header = header.trim_end();
    if header.is_empty() {
      break;
    }
    if let Some((name, value)) = header.split_once(':')
      && name.eq_ignore_ascii_case("content-length")
    {
      length = value.trim().parse::<usize>().expect("the content length is a number");
    }
  }
  let mut bytes = vec![0; length];
  reader.read_exact(&mut bytes).await.expect("the body arrives in full");
  let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).expect("the body is JSON") };
  Recorded { method, target, body }
}

async fn write_answer(socket: &mut TcpStream, answer: Answer) {
  let head = format!(
    "HTTP/1.1 {} Stub\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    answer.status,
    answer.content_type,
    answer.body.len()
  );
  socket.write_all(head.as_bytes()).await.expect("the answer head writes");
  socket.write_all(&answer.body).await.expect("the answer body writes");
  socket.shutdown().await.expect("the connection closes after the answer");
}
