mod comment_guard;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::credentials::unavailable;
use charliebot_cli_common::exit::{self, CliExit};
use charliebot_cli_common::http;
use charliebot_compat::mimetypes_table::APPLICATION_JSON;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::home;
use clap::{Args, Parser, Subcommand};
use comment_guard::PendingCommentGuard;
use serde_json::{Value, json};

const GRAPHQL_ENDPOINT: &str = "https://api.linear.app/graphql";
/// How long an identical `commentCreate` on the same issue is refused, blocking double-posts within one shell
/// command, parallel tool calls, or quick retries.
const COMMENT_DEDUP_WINDOW: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(name = "linear", subcommand_required = true, arg_required_else_help = true)]
struct LinearCli {
  #[command(subcommand)]
  command: LinearCommand,
}

#[derive(Subcommand)]
enum LinearCommand {
  Request(RequestArgs),
}

#[derive(Args)]
struct RequestArgs {
  #[arg(long, value_name = "PATH")]
  file: Option<PathBuf>,
}

/// Run the `linear` subcommand.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: LinearCli = cli_args::parse("charliebot linear", args);
  match cli.command {
    LinearCommand::Request(args) => {
      let request_body = read_request(args.file.as_deref())?;
      let home = home::charliebot_home_dir().map_err(|_| unavailable("linear", credentials::API_KEY_FIELD, None))?;
      let response = execute_graphql(&request_body, GRAPHQL_ENDPOINT, &home)?;
      std::io::stdout().write_all(&response).map_err(|_| CliExit::error("could not write the Linear response"))?;
      Ok(())
    }
  }
}

fn read_request(file: Option<&Path>) -> Result<Vec<u8>, CliExit> {
  let mut body = Vec::new();
  match file {
    Some(path) => std::fs::File::open(path)
      .and_then(|mut input| input.read_to_end(&mut body))
      .map_err(|_| CliExit::usage("could not read the request body"))?,
    None => std::io::stdin().read_to_end(&mut body).map_err(|_| CliExit::usage("could not read the request body"))?,
  };
  Ok(body)
}

fn validate_request_body(body: &[u8]) -> Result<Value, CliExit> {
  let value: Value = serde_json::from_slice(body).map_err(|_| invalid_request_body())?;
  let Some(object) = value.as_object() else {
    return Err(invalid_request_body());
  };
  if !object.get("query").is_some_and(Value::is_string)
    || object.get("variables").is_some_and(|variables| !variables.is_object())
  {
    return Err(invalid_request_body());
  }
  Ok(value)
}

fn invalid_request_body() -> CliExit {
  CliExit::usage("request body must be a JSON object with a string query and an optional variables object")
}

fn execute_graphql(body: &[u8], endpoint: &str, home: &Path) -> Result<Vec<u8>, CliExit> {
  execute_graphql_with_window(body, endpoint, home, COMMENT_DEDUP_WINDOW)
}

fn execute_graphql_with_window(
  body: &[u8],
  endpoint: &str,
  home: &Path,
  dedup_window: Duration,
) -> Result<Vec<u8>, CliExit> {
  let request_value = validate_request_body(body)?;
  let api_key = read_api_key(home)?;
  let comment_guard = PendingCommentGuard::acquire(&request_value, home, dedup_window)?;
  http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| response_failure(None, "request failed", Some(&error), &api_key))?;
  let (response_bytes, response_value) = runtime.block_on(async {
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .map_err(|error| response_failure(None, "request failed", Some(&error), &api_key))?;
    let response = client
      .post(endpoint)
      .header(reqwest::header::CONTENT_TYPE, APPLICATION_JSON)
      .header(reqwest::header::AUTHORIZATION, api_key.as_str())
      .body(body.to_vec())
      .send()
      .await
      .map_err(|error| response_failure(None, "request failed", Some(&error), &api_key))?;
    let status = response.status().as_u16();
    let response_body = response
      .bytes()
      .await
      .map_err(|error| response_failure(Some(status), "response body read failed", Some(&error), &api_key))?;
    if status != 200 {
      return Err(response_failure(Some(status), &String::from_utf8_lossy(&response_body), None, &api_key));
    }
    let value: Value = serde_json::from_slice(&response_body)
      .map_err(|_| response_failure(Some(status), &String::from_utf8_lossy(&response_body), None, &api_key))?;
    if value.as_object().is_some_and(|object| object.contains_key("errors")) {
      return Err(response_failure(Some(status), &String::from_utf8_lossy(&response_body), None, &api_key));
    }
    Ok((response_body.to_vec(), value))
  })?;
  if let Some(guard) = comment_guard {
    guard.record_created(&response_value)?;
  }
  Ok(response_bytes)
}

fn read_api_key(home: &Path) -> Result<String, CliExit> {
  let path = home.join(config::CREDENTIALS_FILENAME);
  let loaded =
    credentials::load_credentials(home).map_err(|_| unavailable("linear", credentials::API_KEY_FIELD, Some(&path)))?;
  loaded
    .require("linear", credentials::API_KEY_FIELD)
    .map_err(|_| unavailable("linear", credentials::API_KEY_FIELD, Some(&path)))
}

fn response_failure(
  status: Option<u16>,
  response: &str,
  cause: Option<&dyn std::error::Error>,
  api_key: &str,
) -> CliExit {
  let mut body = json!({"status": status, "response": redact_key(response, api_key)});
  if let Some(cause) = cause {
    body["error"] = json!(redact_key(&exit::source_chain(&cause), api_key));
  }
  CliExit::contract(body, 1)
}

fn redact_key(text: &str, api_key: &str) -> String {
  if api_key.is_empty() { text.to_string() } else { text.replace(api_key, credentials::REDACTED) }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::net::TcpListener;

  use charliebot_cli_common::test_support::LoopbackServer;

  const API_KEY: &str = "synthetic-linear-key";
  const REQUEST_BODY: &[u8] = br#"{"query":"query { viewer { id } }","variables":{"id":"synthetic-id"}}"#;

  fn credentials_home(key: Option<&str>) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    if let Some(key) = key {
      std::fs::write(home.path().join(config::CREDENTIALS_FILENAME), format!("linear:\n  api_key: {key}\n")).unwrap();
    }
    home
  }

  fn start_stub(status: u16, response_body: Vec<u8>) -> (String, LoopbackServer) {
    let server = LoopbackServer::start(vec![(status, response_body)]);
    (format!("{}/graphql", server.base()), server)
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  fn assert_no_key(text: &str) {
    assert!(!text.contains(API_KEY), "an output contains a synthetic credential");
  }

  fn assert_authorized(request: &[u8]) {
    let request = String::from_utf8_lossy(request).to_ascii_lowercase();
    assert!(request.contains("authorization: synthetic-linear-key"));
  }

  #[test]
  fn graphql_posts_the_key_without_bearer_and_returns_the_response_unchanged() {
    let home = credentials_home(Some(API_KEY));
    let response_body = br#"{"data":{"viewer":{"id":"synthetic-id"}}}"#.to_vec();
    let (endpoint, server) = start_stub(200, response_body.clone());

    let response = execute_graphql(REQUEST_BODY, &endpoint, home.path()).unwrap();
    let request_text = server.next_request();
    let lowered = request_text.to_ascii_lowercase();

    assert_eq!(response, response_body);
    assert!(lowered.contains("authorization: synthetic-linear-key"));
    assert!(!lowered.contains("authorization: bearer"));
    assert!(lowered.contains("content-type: application/json"));
    assert!(request_text.ends_with(std::str::from_utf8(REQUEST_BODY).unwrap()));
    assert_no_key(std::str::from_utf8(&response).unwrap());
  }

  #[test]
  fn graphql_errors_payload_includes_status_and_redacts_the_key() {
    assert_graphql_error(200, br#"{"errors":[{"message":"failed"}]}"#.to_vec());
  }

  #[test]
  fn graphql_http_error_includes_status_and_redacts_the_key() {
    assert_graphql_error(400, API_KEY.as_bytes().to_vec());
  }

  #[test]
  fn graphql_malformed_response_includes_status_and_redacts_the_key() {
    assert_graphql_error(200, b"not-json".to_vec());
  }

  fn assert_graphql_error(status: u16, response_body: Vec<u8>) {
    let home = credentials_home(Some(API_KEY));
    let (endpoint, server) = start_stub(status, response_body);
    let error = execute_graphql(REQUEST_BODY, &endpoint, home.path()).unwrap_err();
    assert_authorized(server.next_request().as_bytes());
    let text = error_text(&error);
    assert_eq!(error.code, 1);
    assert_no_key(&text);
    assert!(text.contains(&format!("\"status\":{status}")));
    assert!(error.body.get("error").is_none());
  }

  #[test]
  fn graphql_network_and_credential_failures_do_not_include_the_key() {
    let home = credentials_home(Some(API_KEY));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = execute_graphql(REQUEST_BODY, &format!("http://127.0.0.1:{port}/graphql"), home.path()).unwrap_err();
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().is_some_and(|cause| !cause.is_empty()));
    assert_no_key(&error_text(&error));

    let missing_home = credentials_home(None);
    let error = execute_graphql(REQUEST_BODY, "http://127.0.0.1:1/graphql", missing_home.path()).unwrap_err();
    assert_eq!(error.code, 3);
    assert_no_key(&error_text(&error));
    assert!(error_text(&error).contains("credentials.linear.api_key"));
    assert!(error_text(&error).contains("credentials.yaml"));
  }

  #[test]
  fn invalid_request_bodies_exit_two_without_echoing_input() {
    let home = credentials_home(None);
    let invalid = format!("{{not-json {API_KEY}");
    let error = execute_graphql(invalid.as_bytes(), "http://127.0.0.1:1/graphql", home.path()).unwrap_err();

    assert_eq!(error.code, 2);
    assert_no_key(&error_text(&error));
  }

  #[test]
  fn request_files_are_read_and_variables_must_be_an_object() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("request.json");
    std::fs::write(&path, REQUEST_BODY).unwrap();
    assert_eq!(read_request(Some(&path)).unwrap(), REQUEST_BODY);
    assert!(validate_request_body(br#"{"query":"query { viewer { id } }","variables":[]}"#).is_err());
  }

  #[test]
  fn comment_create_reproduces_double_post_without_window_and_refuses_duplicate_with_fix() {
    let payload = br#"{"query":"mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success comment { id } } }","variables":{"input":{"issueId":"a2cf6453-bbc7-413d-adef-fac1eea99998","body":"Teacher temperature tables"}}}"#;
    let ok_one =
      br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"71b46966-851d-40d8-a87c-6865d883f905"}}}}"#
        .to_vec();
    let ok_two =
      br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"596a5c80-54c0-43be-b649-7c4b3e9dff9c"}}}}"#
        .to_vec();

    // Without a dedup window (Duration::ZERO), two consecutive posts hit the server twice.
    let unguarded_home = credentials_home(Some(API_KEY));
    let unguarded_server = LoopbackServer::start(vec![(200, ok_one.clone()), (200, ok_two.clone())]);
    let unguarded_endpoint = format!("{}/graphql", unguarded_server.base());
    assert!(execute_graphql_with_window(payload, &unguarded_endpoint, unguarded_home.path(), Duration::ZERO).is_ok());
    assert!(execute_graphql_with_window(payload, &unguarded_endpoint, unguarded_home.path(), Duration::ZERO).is_ok());
    let _ = unguarded_server.next_request();
    let _ = unguarded_server.next_request();
    assert!(unguarded_server.no_request());

    // With COMMENT_DEDUP_WINDOW, the second identical post is refused before hitting Linear.
    let home = credentials_home(Some(API_KEY));
    let server = LoopbackServer::start(vec![(200, ok_one.clone()), (200, ok_two)]);
    let endpoint = format!("{}/graphql", server.base());
    let first = execute_graphql(payload, &endpoint, home.path()).unwrap();
    assert_eq!(first, ok_one);
    let second = execute_graphql(payload, &endpoint, home.path()).unwrap_err();
    assert_eq!(second.code, 1);
    assert_eq!(second.body["issueId"], "a2cf6453-bbc7-413d-adef-fac1eea99998");
    assert_eq!(second.body["commentId"], "71b46966-851d-40d8-a87c-6865d883f905");
    assert_no_key(&error_text(&second));
    let _ = server.next_request();
    assert!(server.no_request(), "duplicate commentCreate must not send a second HTTP request");
  }

  #[test]
  fn concurrent_comment_create_serializes_and_posts_only_once() {
    use std::sync::{Arc, Barrier};

    let payload = br#"{"query":"mutation($i: String!, $b: String!) { commentCreate(input: {issueId: $i, body: $b}) { success comment { id } } }","variables":{"i":"issue-1","b":"concurrent body"}}"#;
    let ok = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-1"}}}}"#.to_vec();
    let home = Arc::new(credentials_home(Some(API_KEY)));
    let server = LoopbackServer::start(vec![(200, ok)]);
    let endpoint = format!("{}/graphql", server.base());
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
      .map(|_| {
        let home = Arc::clone(&home);
        let endpoint = endpoint.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
          barrier.wait();
          execute_graphql(payload, &endpoint, home.path())
        })
      })
      .collect();

    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    let ok_count = results.iter().filter(|result| result.is_ok()).count();
    let err_count = results.iter().filter(|result| result.is_err()).count();
    assert_eq!(ok_count, 1);
    assert_eq!(err_count, 1);
    let _ = server.next_request();
    assert!(server.no_request());
  }

  #[test]
  fn comment_create_allows_distinct_body_or_issue_and_retry_after_error() {
    let home = credentials_home(Some(API_KEY));
    let archived_err = br#"{"errors":[{"message":"Entity not found: Issue"}]}"#.to_vec();
    let ok_1 = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-1"}}}}"#.to_vec();
    let ok_2 = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-2"}}}}"#.to_vec();
    let ok_3 = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-3"}}}}"#.to_vec();
    let server = LoopbackServer::start(vec![(200, archived_err), (200, ok_1), (200, ok_2), (200, ok_3)]);
    let endpoint = format!("{}/graphql", server.base());

    let req_issue1_a = br#"{"query":"mutation { commentCreate(input: {issueId: \"issue-1\", body: \"body A\"}) { success comment { id } } }"}"#;
    let req_issue1_b = br#"{"query":"mutation { commentCreate(input: {issueId: \"issue-1\", body: \"body B\"}) { success comment { id } } }"}"#;
    let req_issue2_a = br#"{"query":"mutation { commentCreate(input: {issueId: \"issue-2\", body: \"body A\"}) { success comment { id } } }"}"#;

    // First attempt fails with GraphQL error -> not recorded, so retry succeeds.
    assert!(execute_graphql(req_issue1_a, &endpoint, home.path()).is_err());
    assert!(execute_graphql(req_issue1_a, &endpoint, home.path()).is_ok());
    // Distinct body and distinct issue are both allowed.
    assert!(execute_graphql(req_issue1_b, &endpoint, home.path()).is_ok());
    assert!(execute_graphql(req_issue2_a, &endpoint, home.path()).is_ok());
    for _ in 0..4 {
      let _ = server.next_request();
    }
    assert!(server.no_request());
  }

  #[test]
  fn extracts_comment_creates_ignores_strings_and_refuses_intra_request_duplicate() {
    let query_with_string = serde_json::json!({
      "query": "query { issue(id: \"commentCreate(input: {issueId: \\\"x\\\", body: \\\"y\\\"})\") { id } }"
    });
    assert!(comment_guard::extract_comment_creates(&query_with_string).is_empty());

    let aliased_block = serde_json::json!({
      "query": "mutation { # commentCreate(input: $x)\n  c1: commentCreate(input: {issueId: \"id-1\", body: \"\"\"\n    line1\n    line2\n  \"\"\"}) { success }\n}"
    });
    let targets = comment_guard::extract_comment_creates(&aliased_block);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].response_field, "c1");
    assert_eq!(targets[0].issue_id, "id-1");
    assert_eq!(targets[0].body, "line1\nline2");

    // Intra-request duplicate is refused without sending any HTTP request.
    let home = credentials_home(Some(API_KEY));
    let server = LoopbackServer::start(vec![]);
    let endpoint = format!("{}/graphql", server.base());
    let dup_req = br#"{"query":"mutation { a: commentCreate(input: {issueId: \"id-1\", body: \"same\"}) { success } b: commentCreate(input: {issueId: \"id-1\", body: \"same\"}) { success } }"}"#;
    let err = execute_graphql(dup_req, &endpoint, home.path()).unwrap_err();
    assert_eq!(err.code, 1);
    assert!(server.no_request());
  }

  #[test]
  fn corrupt_comment_ledger_fails_fast_without_sending() {
    let home = credentials_home(Some(API_KEY));
    let state = charliebot_core::home::state_dir(home.path());
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("linear_recent_comments.json"), "{not-an-array").unwrap();
    let server = LoopbackServer::start(vec![]);
    let endpoint = format!("{}/graphql", server.base());
    let req = br#"{"query":"mutation { commentCreate(input: {issueId: \"id-1\", body: \"hi\"}) { success } }"}"#;
    let err = execute_graphql(req, &endpoint, home.path()).unwrap_err();
    assert_eq!(err.code, 1);
    assert!(server.no_request());
  }

  #[test]
  fn comment_create_allows_repost_after_dedup_window_expires() {
    let home = credentials_home(Some(API_KEY));
    let state = charliebot_core::home::state_dir(home.path());
    std::fs::create_dir_all(&state).unwrap();
    let expired_entry = serde_json::json!([{
      "key": "stale-or-matching-key",
      "issue_id": "id-1",
      "posted_at_ms": 1,
      "comment_id": "c-old"
    }]);
    std::fs::write(state.join("linear_recent_comments.json"), expired_entry.to_string()).unwrap();

    // Even a matching key written with an expired timestamp (1 ms since Unix epoch) is pruned.
    let ok_1 = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-1"}}}}"#.to_vec();
    let ok_2 = br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-2"}}}}"#.to_vec();
    let server = LoopbackServer::start(vec![(200, ok_1), (200, ok_2)]);
    let endpoint = format!("{}/graphql", server.base());
    let req = br#"{"query":"mutation { commentCreate(input: {issueId: \"id-1\", body: \"same\"}) { success } }"}"#;

    assert!(execute_graphql_with_window(req, &endpoint, home.path(), Duration::from_secs(60)).is_ok());
    // Rewrite the ledger entry with an expired timestamp to verify real-key expiry without sleeping.
    let ledger_path = state.join("linear_recent_comments.json");
    let mut entries: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    entries[0]["posted_at_ms"] = serde_json::json!(1);
    std::fs::write(&ledger_path, serde_json::to_string(&entries).unwrap()).unwrap();

    assert!(execute_graphql_with_window(req, &endpoint, home.path(), Duration::from_secs(60)).is_ok());
    let _ = server.next_request();
    let _ = server.next_request();
    assert!(server.no_request());
  }
}
