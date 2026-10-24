//! The Anthropic proxy route of the cc-openai-compatible backend, checked against the Python route.
//!
//! `openai_compatible_reference.py` runs the Python route over one case matrix and prints what it did for each case.
//! This test serves each case's upstream from a loopback server that answers with the case's status and chunks and
//! records the request it was sent, posts the case's body to the Rust route, and compares: the status, the body with
//! message ids masked, the request sent upstream, and whether a stream reached its message_stop frame. The route's own
//! cases (the backend checks, the missing credential, the type factory, the raw separators) follow, as ports of the
//! Python tests.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::post;
use charliebot_agents::backends::openai_compatible;
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::{BackendOption, CONFIG_FILENAME, CREDENTIALS_FILENAME, CharlieBotConfig};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, LaunchArgs, ResumeStyle,
};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, StartupState};
use charliebot_store::Store;
use futures_util::stream;
use regex::bytes::Regex;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const PACKAGE: &str = "src.backends.openai_compatible";
const ACCESS_KEY: &str = "synthetic-access-key";
const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";
const MESSAGE_STOP_FRAME: &[u8] = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// One case of the matrix, as the Python reference printed it.
struct Case {
  name: String,
  index: usize,
  body: Vec<u8>,
  upstream_status: u16,
  upstream_chunks: Vec<Vec<u8>>,
  credential: Option<String>,
  unreachable: bool,
  expected_status: u16,
  expected_body: Vec<u8>,
  expected_upstream: Option<Seen>,
  expected_complete: bool,
}

impl Case {
  fn from_record(record: &Value) -> Case {
    let input = &record["input"];
    let expected = &record["expected"];
    Case {
      name: record["name"].as_str().unwrap().to_string(),
      index: input["index"].as_u64().unwrap() as usize,
      body: hex(input["body_hex"].as_str().unwrap()),
      upstream_status: input["status"].as_u64().unwrap() as u16,
      upstream_chunks: input["chunks_hex"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| hex(chunk.as_str().unwrap()))
        .collect(),
      credential: input["credential"].as_str().map(str::to_string),
      unreachable: input["unreachable"].as_bool().unwrap(),
      expected_status: expected["status"].as_u64().unwrap() as u16,
      expected_body: hex(expected["body_hex"].as_str().unwrap()),
      expected_upstream: expected["upstream"]
        .as_object()
        .map(|seen| (hex(seen["body_hex"].as_str().unwrap()), seen["authorization"].as_str().map(str::to_string))),
      expected_complete: expected["complete"].as_bool().unwrap(),
    }
  }
}

fn hex(text: &str) -> Vec<u8> {
  (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
}

fn repo_root() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap()
}

/// The matrix the Python route answered, one record per case.
fn python_reference() -> Vec<Case> {
  let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/openai_compatible_reference.py");
  let output = Command::new("uv")
    .args(["run", "python"])
    .arg(&helper)
    .current_dir(repo_root())
    .output()
    .expect("uv runs the Python reference");
  assert!(
    output.status.success(),
    "openai_compatible_reference.py failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  String::from_utf8(output.stdout)
    .expect("the reference prints UTF-8")
    .lines()
    .map(|line| Case::from_record(&serde_json::from_str(line).expect("each reference line is JSON")))
    .collect()
}

/// A type the server does not register, so a test can give a backend a type that is not the proxy's.
struct OtherType;

impl BackendFactory for OtherType {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    bail!("the other-type backend is not built by these tests")
  }
}

fn register_other_type(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "other-type",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(OtherType),
    lifecycle: None,
    translate_fallback: false,
  })
}

/// The registry a server would build for the proxy, plus the other type the type check refuses.
fn test_registry() -> Registry {
  let mut registry = Registry::new();
  registry.register_package(PACKAGE, openai_compatible::register).unwrap();
  registry.register_package("tests.other_type", register_other_type).unwrap();
  registry
}

/// The feature context over `home`, with the config and credentials reloading from it, as the server builds it.
fn features(home: &Path) -> Arc<FeatureContext> {
  let registry = Arc::new(test_registry());
  let cfg = registry.load_config(home).unwrap();
  let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).unwrap();
  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let credentials_home = home.to_path_buf();
  Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      home.join(CONFIG_FILENAME),
      "config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.join(CREDENTIALS_FILENAME),
      "credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    )),
    store: Arc::new(store),
    registry,
    startup: Arc::new(StartupState::default()),
  })
}

/// The proxy route as the server mounts it: the router the proxy's registration gave, on a loopback port.
async fn serve_route(features: &Arc<FeatureContext>) -> SocketAddr {
  let route = features.registry.routers().iter().find(|route| route.package == PACKAGE).unwrap();
  let app: Router = (route.build)(features);
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
  address
}

/// The request the upstream received for one case: its body and its authorization header.
type Seen = (Vec<u8>, Option<String>);

/// One upstream answer: the status and the body chunks, sent as they are.
#[derive(Clone)]
struct Reply {
  status: u16,
  chunks: Vec<Vec<u8>>,
}

/// The fake OpenAI-compatible upstream: the answer for each case index, and the request each index received.
struct Upstream {
  replies: Vec<Reply>,
  seen: Mutex<HashMap<usize, Seen>>,
}

async fn chat(
  State(upstream): State<Arc<Upstream>>,
  UrlPath(index): UrlPath<usize>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  let authorization = headers.get(header::AUTHORIZATION).map(|value| value.to_str().unwrap().to_string());
  upstream.seen.lock().unwrap().insert(index, (body.to_vec(), authorization));
  let reply = upstream.replies[index].clone();
  let chunks = reply.chunks.into_iter().map(|chunk| Ok::<Bytes, io::Error>(Bytes::from(chunk)));
  Response::builder().status(reply.status).body(Body::from_stream(stream::iter(chunks))).unwrap()
}

/// Serves `replies` at `/<index>/v1/chat/completions` on a loopback port.
async fn serve_upstream(replies: Vec<Reply>) -> (u16, Arc<Upstream>) {
  let upstream = Arc::new(Upstream { replies, seen: Mutex::new(HashMap::new()) });
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  let app = Router::new().route("/{index}/v1/chat/completions", post(chat)).with_state(Arc::clone(&upstream));
  tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
  (port, upstream)
}

fn write_home(home: &Path, options: &str, credentials: &str) {
  fs::write(home.join(CONFIG_FILENAME), format!("server:\n  port: 8123\nbackends:\n  options:\n{options}")).unwrap();
  fs::write(home.join(CREDENTIALS_FILENAME), credentials).unwrap();
}

/// The home of the matrix: one proxy option per case, pointed at the upstream on `upstream_port`. An unreachable case
/// points at a closed port instead, and a credential case names its section, which holds a key only when present.
fn write_matrix_home(home: &Path, cases: &[Case], upstream_port: u16) {
  let mut options = String::new();
  for case in cases {
    let base =
      if case.unreachable { "http://127.0.0.1:1".to_string() } else { format!("http://127.0.0.1:{upstream_port}") };
    options.push_str(&format!(
      "    - id: case-{i}\n      label: case {i}\n      type: cc-openai-compatible\n      model: synthetic-model\n      api_base: {base}/{i}/v1\n",
      i = case.index
    ));
    if case.credential.is_some() {
      options.push_str(&format!("      credential: case_{}\n", case.index));
    }
  }
  let mut credentials = format!("charliebot:\n  access_key: {ACCESS_KEY}\n");
  for case in cases.iter().filter(|case| case.credential.as_deref() == Some("present")) {
    credentials.push_str(&format!("case_{}:\n  api_key: key-{}\n", case.index, case.index));
  }
  write_home(home, &options, &credentials);
}

/// The comparison's form of a body: the credentials file's path is `<credentials>` and each message id is `msg_MASKED`,
/// as the Python side writes them.
fn normalize(body: &[u8], credentials_path: &str) -> Vec<u8> {
  let credentials = Regex::new(&regex::escape(credentials_path)).unwrap();
  let message_id = Regex::new("msg_[0-9a-f]{32}").unwrap();
  let body = credentials.replace_all(body, &b"<credentials>"[..]);
  message_id.replace_all(&body, &b"msg_MASKED"[..]).into_owned()
}

/// A client that reaches the loopback servers directly, whatever proxy the environment names.
fn no_proxy_client() -> reqwest::Client {
  http_client::client(&Settings::default()).unwrap()
}

/// Posts `body` to a case's route and reads the answer until it ends or breaks: the status, the bytes received, and
/// the transport failure that cut the body short, if any.
async fn post_case(
  client: &reqwest::Client,
  proxy: SocketAddr,
  index: usize,
  body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
  let url = format!("http://{proxy}/api/anthropic-proxy/openai-compatible/case-{index}/v1/messages");
  let mut response = client.post(url).body(body.to_vec()).send().await.unwrap();
  let status = response.status().as_u16();
  let mut received = Vec::new();
  loop {
    match response.chunk().await {
      Ok(Some(chunk)) => received.extend_from_slice(&chunk),
      Ok(None) => return (status, received, None),
      Err(error) => return (status, received, Some(error.to_string())),
    }
  }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "catch-up d113f2c4d: R337"]
async fn the_route_answers_as_the_python_route_does_over_the_matrix() {
  let cases = python_reference();
  let replies =
    cases.iter().map(|case| Reply { status: case.upstream_status, chunks: case.upstream_chunks.clone() }).collect();
  let (upstream_port, upstream) = serve_upstream(replies).await;
  let home = tempfile::tempdir().unwrap();
  write_matrix_home(home.path(), &cases, upstream_port);
  let proxy = serve_route(&features(home.path())).await;
  let client = no_proxy_client();
  let credentials_path = home.path().join(CREDENTIALS_FILENAME).to_string_lossy().into_owned();

  let mut mismatches = Vec::new();
  for case in &cases {
    upstream.seen.lock().unwrap().remove(&case.index);
    let (status, body, _failure) = post_case(&client, proxy, case.index, &case.body).await;
    let body = normalize(&body, &credentials_path);
    let seen = upstream.seen.lock().unwrap().get(&case.index).cloned();
    let complete = body.ends_with(MESSAGE_STOP_FRAME);
    let expected_upstream = case.expected_upstream.clone();
    if status != case.expected_status
      || body != case.expected_body
      || seen != expected_upstream
      || complete != case.expected_complete
    {
      mismatches.push(format!(
        "{}: status {} (python {}), complete {} (python {}), body matches {}, upstream matches {}",
        case.name,
        status,
        case.expected_status,
        complete,
        case.expected_complete,
        body == case.expected_body,
        seen == expected_upstream,
      ));
    }
  }
  assert!(mismatches.is_empty(), "{} of {} cases differ:\n{}", mismatches.len(), cases.len(), mismatches.join("\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_credential_is_a_400_that_names_the_key() {
  let home = tempfile::tempdir().unwrap();
  write_home(
    home.path(),
    "    - id: cc-glm52\n      label: CC GLM-5.2\n      type: cc-openai-compatible\n      model: nvidia/GLM-5.2-NVFP4\n      api_base: http://upstream.example/v1\n      credential: missing_upstream\n",
    &format!("charliebot:\n  access_key: {ACCESS_KEY}\n"),
  );
  let proxy = serve_route(&features(home.path())).await;
  let response = no_proxy_client()
    .post(format!("http://{proxy}/api/anthropic-proxy/openai-compatible/cc-glm52/v1/messages"))
    .json(&json!({"model": "claude-facing", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 16}))
    .send()
    .await
    .unwrap();

  assert_eq!(response.status(), 400);
  let detail: Value = response.json().await.unwrap();
  assert!(detail["detail"].as_str().unwrap().contains("missing_upstream"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backend_checks_answer_before_the_body_is_read() {
  let home = tempfile::tempdir().unwrap();
  write_home(
    home.path(),
    r#"    - id: wrong-type
      label: wrong
      type: other-type
      model: m
    - id: no-base
      label: base
      type: cc-openai-compatible
      model: m
      api_base: ''
    - id: no-model
      label: model
      type: cc-openai-compatible
      model: ''
      api_base: http://upstream.example/v1
"#,
    &format!("charliebot:\n  access_key: {ACCESS_KEY}\n"),
  );
  let proxy = serve_route(&features(home.path())).await;
  let client = no_proxy_client();
  let post = |id: &str| {
    client
      .post(format!("http://{proxy}/api/anthropic-proxy/openai-compatible/{id}/v1/messages"))
      .body("not json")
      .send()
  };

  let missing = post("missing-id").await.unwrap();
  assert_eq!(missing.status(), 404);
  assert_eq!(missing.json::<Value>().await.unwrap(), json!({"detail": "unknown backend id: missing-id"}));

  let wrong_type = post("wrong-type").await.unwrap();
  assert_eq!(wrong_type.status(), 400);
  assert_eq!(
    wrong_type.json::<Value>().await.unwrap(),
    json!({"detail": "backend 'wrong-type' is not type 'cc-openai-compatible' (got 'other-type')"})
  );

  let no_base = post("no-base").await.unwrap();
  assert_eq!(no_base.status(), 400);
  assert_eq!(no_base.json::<Value>().await.unwrap(), json!({"detail": "backend 'no-base' missing api_base"}));

  let no_model = post("no-model").await.unwrap();
  assert_eq!(no_model.status(), 400);
  assert_eq!(no_model.json::<Value>().await.unwrap(), json!({"detail": "backend 'no-model' missing model"}));
}

#[test]
fn the_factory_builds_the_proxy_backend_for_the_server_port() {
  let home = tempfile::tempdir().unwrap();
  write_home(
    home.path(),
    "    - id: cc-glm52\n      label: CC GLM-5.2\n      type: cc-openai-compatible\n      model: nvidia/GLM-5.2-NVFP4\n      api_base: http://upstream.example/v1\n",
    &format!("charliebot:\n  access_key: {ACCESS_KEY}\n"),
  );
  let registry = test_registry();
  let cfg = registry.load_config(home.path()).unwrap();
  let option = cfg.get_backend_option("cc-glm52").unwrap();
  let factory = &registry.backend_type_of("cc-openai-compatible").unwrap().factory;

  let backend = factory.build(Some(option), &cfg, LaunchArgs::default()).unwrap();

  let env = backend.prepare_env(Default::default()).unwrap();
  assert_eq!(env["ANTHROPIC_BASE_URL"], "http://localhost:8123/api/anthropic-proxy/openai-compatible/cc-glm52");
  assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], ACCESS_KEY);
  assert_eq!(env["ANTHROPIC_MODEL"], "nvidia/GLM-5.2-NVFP4");
}

fn find(haystack: &[u8], needle: &str) -> usize {
  haystack.windows(needle.len()).position(|window| window == needle.as_bytes()).unwrap()
}

/// A stream whose JSON string carries raw U+0085 and U+2028 is translated whole, with each frame's name equal to its
/// `type`, even when the upstream cuts the frame inside those characters.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_with_raw_separator_characters_translates_to_the_same_text() {
  let content = "grep hit: a:48<\u{85}>NEL-CHAR\u{2028}>LS-CHAR";
  let frame = format!("data: {}\n\n", json!({"choices": [{"delta": {"content": content}}]}));
  let frame = frame.as_bytes();
  let nel = find(frame, "\u{85}") + 1;
  let line_separator = find(frame, "\u{2028}");
  let chunks = vec![
    frame[..nel].to_vec(),
    frame[nel..line_separator].to_vec(),
    frame[line_separator..].to_vec(),
    DONE_FRAME.to_vec(),
  ];
  let (upstream_port, _upstream) = serve_upstream(vec![Reply { status: 200, chunks }]).await;
  let home = tempfile::tempdir().unwrap();
  write_home(
    home.path(),
    &format!(
      "    - id: case-0\n      label: case 0\n      type: cc-openai-compatible\n      model: nvidia/GLM-5.2-NVFP4\n      api_base: http://127.0.0.1:{upstream_port}/0/v1\n"
    ),
    &format!("charliebot:\n  access_key: {ACCESS_KEY}\n"),
  );
  let proxy = serve_route(&features(home.path())).await;
  let body = json!({"model": "claude-facing", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 16, "stream": true});
  let (status, received, failure) = post_case(&no_proxy_client(), proxy, 0, body.to_string().as_bytes()).await;
  assert_eq!((status, failure), (200, None));

  let text = String::from_utf8(received).unwrap();
  let mut events = Vec::new();
  for block in text.split("\n\n").filter(|block| !block.is_empty()) {
    let (event, data) = block.split_once('\n').unwrap();
    let event = event.strip_prefix("event: ").unwrap();
    let data: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(event, data["type"], "the frame name is the data's type");
    events.push((event.to_string(), data));
  }
  let streamed: String = events
    .iter()
    .filter(|(event, data)| event == "content_block_delta" && data["delta"]["type"] == "text_delta")
    .map(|(_, data)| data["delta"]["text"].as_str().unwrap())
    .collect();
  assert_eq!(streamed, content);
  assert_eq!(events.last().unwrap().0, "message_stop");
}
