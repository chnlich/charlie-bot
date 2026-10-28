//! Route tests against a fake Google endpoint: the MCP handshake, the tool listing, a resolved and an unresolved
//! redirect, and the tool errors.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use charliebot_compat::routing::post_only;
use charliebot_core::credentials::{Credentials, load_credentials};
use charliebot_core::hot_reload::HotReload;
use charliebot_core::web_search::MCP_PATH;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;

use super::endpoint::WebSearch;
use super::{matches, mcp};

const API_KEY_HEADER: &str = "x-goog-api-key";
const CREDENTIALS: &str = "gemini:\n  api_key: synthetic-gemini-key\n";
/// The path Google serves `generateContent` on; the model name carries a colon, which axum's router cannot spell.
const GENERATE_PATH: &str = "/v1beta/models/gemini-3.8-flash:generateContent";

/// One request the fake Google endpoint received.
struct Recorded {
  path: String,
  api_key: Option<String>,
  body: Value,
}

#[derive(Clone)]
struct FakeGoogle {
  base: String,
  requests: Arc<Mutex<Vec<Recorded>>>,
}

async fn start_fake_google() -> FakeGoogle {
  let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake Google endpoint");
  let base = format!("http://{}", listener.local_addr().expect("read the fake endpoint address"));
  let fake = FakeGoogle { base: base.clone(), requests: Arc::new(Mutex::new(Vec::new())) };
  let app = Router::new()
    .route("/redirect/ok", get(resolved_redirect))
    .route("/redirect/no-location", get(no_location))
    .fallback(generate)
    .with_state(fake.clone());
  tokio::spawn(async move {
    axum::serve(listener, app).await.expect("serve the fake Google endpoint");
  });
  fake
}

async fn generate(
  State(fake): State<FakeGoogle>,
  method: Method,
  uri: axum::http::Uri,
  headers: HeaderMap,
  body: axum::body::Bytes,
) -> Response {
  if method != Method::POST || uri.path() != GENERATE_PATH {
    return (StatusCode::NOT_FOUND, "the fake Google endpoint has no such route").into_response();
  }
  let Ok(body) = serde_json::from_slice::<Value>(&body) else {
    return (StatusCode::BAD_REQUEST, "the request body is not JSON").into_response();
  };
  fake.requests.lock().expect("the request list is not poisoned").push(Recorded {
    path: uri.path().to_string(),
    api_key: headers.get(API_KEY_HEADER).and_then(|value| value.to_str().ok()).map(str::to_string),
    body: body.clone(),
  });
  let query = body.pointer("/contents/0/parts/0/text").and_then(Value::as_str).unwrap_or("");
  if query.contains("no-candidate") {
    return (StatusCode::OK, axum::Json(json!({"candidates": []}))).into_response();
  }
  if query.contains("refused") {
    return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "synthetic refusal"}}))).into_response();
  }
  let answer = json!({"candidates": [{
    "content": {"parts": [{"text": "Pixi v0.83.0 is the latest release."}]},
    "groundingMetadata": {
      "webSearchQueries": ["prefix-dev pixi release"],
      "groundingChunks": [
        {"web": {"uri": format!("{}/redirect/ok", fake.base), "title": "prefix.dev"}},
        {"web": {"uri": format!("{}/redirect/no-location", fake.base), "title": "github.com"}},
      ],
    },
  }]});
  (StatusCode::OK, axum::Json(answer)).into_response()
}

async fn resolved_redirect() -> Response {
  (StatusCode::FOUND, [(header::LOCATION, "https://pixi.prefix.dev/latest/install/")]).into_response()
}

async fn no_location() -> Response {
  (StatusCode::OK, "not a redirect").into_response()
}

/// A credentials file with `yaml` and its hot-reloaded cache, over a home the caller keeps alive.
fn credentials(yaml: &str) -> (TempDir, Arc<HotReload<Credentials>>) {
  let home = tempfile::tempdir().expect("a temporary home");
  std::fs::write(home.path().join("credentials.yaml"), yaml).expect("the credentials file");
  let path = home.path().to_path_buf();
  let reload = Arc::new(HotReload::new(
    home.path().join("credentials.yaml"),
    "test_credentials_reload_failed",
    Box::new(move || load_credentials(&path)),
  ));
  (home, reload)
}

/// The endpoint over `fake`, answering with `credentials`.
fn app(credentials: Arc<HotReload<Credentials>>, fake: &FakeGoogle) -> Router {
  // reqwest is built with `rustls-no-provider`, and the production client supplies its provider through
  // `charliebot_compat::http_client`; this bare client installs the process provider instead.
  let _ = rustls::crypto::ring::default_provider().install_default();
  let client = reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .build()
    .expect("build the test client");
  Router::new()
    .route(MCP_PATH, post_only(mcp::post))
    .with_state(WebSearch::new(credentials).against(&format!("{}/v1beta", fake.base), client))
}

/// One JSON-RPC request to the endpoint.
async fn rpc(app: Router, request: Value) -> (StatusCode, Value) {
  let request = Request::builder()
    .method(Method::POST)
    .uri(MCP_PATH)
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(request.to_string()))
    .expect("build the request");
  let response = app.oneshot(request).await.expect("the endpoint answers");
  let status = response.status();
  let body = to_bytes(response.into_body(), usize::MAX).await.expect("read the answer");
  let body = match body.is_empty() {
    true => Value::Null,
    false => serde_json::from_slice(&body).expect("the answer is JSON"),
  };
  (status, body)
}

#[test]
fn the_router_claims_only_the_mcp_path() {
  assert!(matches(MCP_PATH));
  assert!(!matches("/api/web-search"));
  assert!(!matches("/api/web-search/mcp/extra"));
}

#[tokio::test]
async fn initialize_and_ping_answer_the_mcp_handshake() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let app = app(credentials, &fake);

  let (status, body) = rpc(
    app.clone(),
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["jsonrpc"], "2.0");
  assert_eq!(body["id"], json!(1));
  assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
  assert_eq!(body["result"]["capabilities"], json!({"tools": {}}));
  assert_eq!(body["result"]["serverInfo"]["name"], "charliebot-web");

  let (status, body) = rpc(app, json!({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"})).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, json!({"jsonrpc": "2.0", "id": "ping-1", "result": {}}));
}

#[tokio::test]
async fn tools_list_names_the_web_search_tool() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (status, body) = rpc(app(credentials, &fake), json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
  assert_eq!(status, StatusCode::OK);
  let tools = body["result"]["tools"].as_array().expect("the answer lists tools");
  assert_eq!(tools.len(), 1);
  assert_eq!(tools[0]["name"], "web_search");
  assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
  assert_eq!(tools[0]["inputSchema"]["properties"]["query"]["type"], "string");
}

#[tokio::test]
async fn tools_call_answers_with_the_answer_the_queries_and_the_real_source_urls() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (status, body) = rpc(
    app(credentials, &fake),
    json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
           "params": {"name": "web_search", "arguments": {"query": "prefix-dev pixi latest release"}}}),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["result"]["isError"], false);
  let text = body["result"]["content"][0]["text"].as_str().expect("the result carries text");
  let expected = format!(
    "Pixi v0.83.0 is the latest release.\nSearch queries: prefix-dev pixi release\nSources:\n\
     [1] prefix.dev — https://pixi.prefix.dev/latest/install/\n\
     [2] github.com — {}/redirect/no-location (redirect resolution failed: HTTP 200 without a Location header)",
    fake.base
  );
  assert_eq!(text, expected);

  let requests = fake.requests.lock().expect("the request list is not poisoned");
  let request = requests.first().expect("the fake Google endpoint received the search");
  assert_eq!(request.path, GENERATE_PATH, "the request names the search model");
  assert_eq!(request.api_key.as_deref(), Some("synthetic-gemini-key"));
  assert_eq!(request.body["tools"], json!([{"google_search": {}}]));
  assert_eq!(request.body["contents"][0]["parts"][0]["text"], "prefix-dev pixi latest release");
}

#[tokio::test]
async fn a_google_refusal_is_a_tool_error_with_the_status_and_the_body_start() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (status, body) = rpc(
    app(credentials, &fake),
    json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
           "params": {"name": "web_search", "arguments": {"query": "refused"}}}),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["result"]["isError"], true);
  let text = body["result"]["content"][0]["text"].as_str().expect("the error carries text");
  assert!(text.starts_with("Google search failed with HTTP 400: "), "{text}");
  assert!(text.contains("synthetic refusal"), "{text}");
}

#[tokio::test]
async fn a_google_answer_without_a_candidate_is_a_tool_error() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (_status, body) = rpc(
    app(credentials, &fake),
    json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
           "params": {"name": "web_search", "arguments": {"query": "no-candidate"}}}),
  )
  .await;
  assert_eq!(body["result"]["isError"], true);
  let text = body["result"]["content"][0]["text"].as_str().expect("the error carries text");
  assert!(text.starts_with("Google search returned no candidate (HTTP 200): "), "{text}");
}

#[tokio::test]
async fn a_missing_gemini_api_key_is_a_tool_error() {
  let (_home, credentials) = credentials("charliebot:\n  access_key: synthetic-access-key\n");
  let fake = start_fake_google().await;
  let (_status, body) = rpc(
    app(credentials, &fake),
    json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
           "params": {"name": "web_search", "arguments": {"query": "prefix-dev pixi latest release"}}}),
  )
  .await;
  assert_eq!(body["result"]["isError"], true);
  let text = body["result"]["content"][0]["text"].as_str().expect("the error carries text");
  assert!(text.contains("gemini.api_key"), "{text}");
  assert!(fake.requests.lock().expect("the request list is not poisoned").is_empty());
}

#[tokio::test]
async fn server_discover_answers_method_not_found() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (status, body) = rpc(
    app(credentials, &fake),
    json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover"}),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["id"], "server-discover-probe-1");
  assert_eq!(body["error"]["code"], -32601);
  assert!(body["result"].is_null(), "a method-not-found answer carries no result");
}

#[tokio::test]
async fn a_notification_is_accepted_with_an_empty_answer() {
  let (_home, credentials) = credentials(CREDENTIALS);
  let fake = start_fake_google().await;
  let (status, body) =
    rpc(app(credentials, &fake), json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
  assert_eq!(status, StatusCode::ACCEPTED);
  assert_eq!(body, Value::Null);
}
