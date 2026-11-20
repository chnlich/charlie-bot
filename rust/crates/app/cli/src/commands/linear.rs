use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::credentials::unavailable;
use charliebot_cli_common::exit::{self, CliExit};
use charliebot_cli_common::http;
use charliebot_cli_common::urlencode::urlencode;
use charliebot_compat::mimetypes_table::APPLICATION_JSON;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::headers::{AUTHORIZATION, CONTENT_TYPE};
use charliebot_core::home;
use charliebot_core::oauth::{
  ACCESS_TOKEN_FIELD, CLIENT_CREDENTIALS_GRANT, CLIENT_ID_FIELD, CLIENT_SECRET_FIELD, EXPIRES_IN_FIELD,
  GRANT_TYPE_FIELD,
};
use charliebot_core::paths;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const GRAPHQL_ENDPOINT: &str = "https://api.linear.app/graphql";
const TOKEN_ENDPOINT: &str = "https://api.linear.app/oauth/token";
const LINEAR_OAUTH_SCOPE: &str = "read,write,initiative:write";
const REFRESH_MARGIN_SECS: u64 = 3600;

/// The form field naming the permissions an OAuth request asks for.
const SCOPE_FIELD: &str = "scope";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const SCRIPT_SECTION: &str = "linear";
const CLIENT_ID_KEY: &str = "client_id";
const CLIENT_SECRET_KEY: &str = "client_secret";
const TOKEN_FILE_NAME: &str = "linear_oauth_token.json";
const LOCK_FILE_NAME: &str = "linear_oauth_token.lock";

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
      let home = home::charliebot_home_dir().map_err(|_| unavailable(SCRIPT_SECTION, CLIENT_ID_KEY, None))?;
      let response = execute_graphql(&request_body, GRAPHQL_ENDPOINT, TOKEN_ENDPOINT, &home)?;
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

fn validate_request_body(body: &[u8]) -> Result<(), CliExit> {
  let value: Value = serde_json::from_slice(body).map_err(|_| invalid_request_body())?;
  let Some(object) = value.as_object() else {
    return Err(invalid_request_body());
  };
  if !object.get("query").is_some_and(Value::is_string)
    || object.get("variables").is_some_and(|variables| !variables.is_object())
  {
    return Err(invalid_request_body());
  }
  Ok(())
}

fn invalid_request_body() -> CliExit {
  CliExit::usage("request body must be a JSON object with a string query and an optional variables object")
}

/// The OAuth application's credentials, as the flat `linear.client_id` / `linear.client_secret` of `credentials.yaml`.
struct LinearCredentials {
  client_id: String,
  client_secret: String,
}

/// The cached access token and the facts that make it reusable: the application it was minted for and the permissions
/// it carries. A token from another application or another permission set is stale.
#[derive(Debug, Serialize, Deserialize)]
struct CachedToken {
  access_token: String,
  expires_at: u64,
  client_id: String,
  scope: String,
}

/// The exclusive `flock` on the token cache's lock file, held for the duration of one read-exchange-write.
struct StateLock {
  _file: File,
}

impl StateLock {
  fn acquire(home: &Path) -> Result<Self, CliExit> {
    let path = home::state_dir(home).join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(&path)
      .map_err(|error| CliExit::error(format!("could not open {}: {error}", paths::display(&path))))?;
    // SAFETY: `file` owns a valid descriptor for the duration of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
      return Err(CliExit::error(format!(
        "could not lock {}: {}",
        paths::display(&path),
        std::io::Error::last_os_error()
      )));
    }
    Ok(Self { _file: file })
  }
}

fn execute_graphql(body: &[u8], graphql_endpoint: &str, token_endpoint: &str, home: &Path) -> Result<Vec<u8>, CliExit> {
  validate_request_body(body)?;
  let credentials = read_credentials(home)?;
  let secret = credentials.client_secret.as_str();
  http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| response_failure(None, "request failed", Some(&error), &[secret]))?;
  runtime.block_on(async {
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .map_err(|error| response_failure(None, "request failed", Some(&error), &[secret]))?;
    let token = acquire_token(&client, home, &credentials, token_endpoint).await?;
    let (status, response_body) = send_graphql(&client, body, graphql_endpoint, &token, &credentials).await?;
    if status != 401 {
      return finish(status, &response_body, &token, &credentials);
    }
    let retried_token = refresh_after_unauthorized(&client, home, &credentials, token_endpoint, &token).await?;
    let (status, response_body) = send_graphql(&client, body, graphql_endpoint, &retried_token, &credentials).await?;
    finish(status, &response_body, &retried_token, &credentials)
  })
}

fn read_credentials(home: &Path) -> Result<LinearCredentials, CliExit> {
  let path = home.join(config::CREDENTIALS_FILENAME);
  let loaded =
    credentials::load_credentials(home).map_err(|_| unavailable(SCRIPT_SECTION, CLIENT_ID_KEY, Some(&path)))?;
  let client_id = required(&loaded, CLIENT_ID_KEY, &path)?;
  let client_secret = required(&loaded, CLIENT_SECRET_KEY, &path)?;
  Ok(LinearCredentials { client_id, client_secret })
}

/// One credential value. An empty value cannot authenticate, so it counts as unset.
fn required(loaded: &credentials::Credentials, key: &str, path: &Path) -> Result<String, CliExit> {
  let failure = || unavailable(SCRIPT_SECTION, key, Some(path));
  loaded
    .require(SCRIPT_SECTION, key)
    .map_err(|_| failure())
    .and_then(|value| if value.trim().is_empty() { Err(failure()) } else { Ok(value) })
}

/// The access token to send: the cached one while it is reusable, otherwise one minted under the state lock.
async fn acquire_token(
  client: &reqwest::Client,
  home: &Path,
  credentials: &LinearCredentials,
  token_endpoint: &str,
) -> Result<String, CliExit> {
  match read_cache(home)? {
    Some(cached) if cache_usable(&cached, credentials) => Ok(cached.access_token),
    _ => exchange_token(client, home, credentials, token_endpoint).await,
  }
}

/// Mint a token under the state lock, re-reading the cache first: a process that waited for the lock may find the token
/// another process just minted.
async fn exchange_token(
  client: &reqwest::Client,
  home: &Path,
  credentials: &LinearCredentials,
  token_endpoint: &str,
) -> Result<String, CliExit> {
  create_state_dir(home)?;
  let _lock = StateLock::acquire(home)?;
  if let Some(cached) = read_cache(home)?
    && cache_usable(&cached, credentials)
  {
    return Ok(cached.access_token);
  }
  mint_token(client, home, credentials, token_endpoint).await
}

/// After a `401`: reuse the token another process already replaced, otherwise mint a fresh one and cache it.
async fn refresh_after_unauthorized(
  client: &reqwest::Client,
  home: &Path,
  credentials: &LinearCredentials,
  token_endpoint: &str,
  failed_token: &str,
) -> Result<String, CliExit> {
  create_state_dir(home)?;
  let _lock = StateLock::acquire(home)?;
  if let Some(cached) = read_cache(home)?
    && cache_usable(&cached, credentials)
    && cached.access_token != failed_token
  {
    return Ok(cached.access_token);
  }
  mint_token(client, home, credentials, token_endpoint).await
}

/// Trade the application credentials for an access token and write it to the cache.
async fn mint_token(
  client: &reqwest::Client,
  home: &Path,
  credentials: &LinearCredentials,
  token_endpoint: &str,
) -> Result<String, CliExit> {
  let secret = credentials.client_secret.as_str();
  let form = urlencode(&[
    (GRANT_TYPE_FIELD.to_string(), CLIENT_CREDENTIALS_GRANT.to_string()),
    (CLIENT_ID_FIELD.to_string(), credentials.client_id.clone()),
    (CLIENT_SECRET_FIELD.to_string(), credentials.client_secret.clone()),
    (SCOPE_FIELD.to_string(), LINEAR_OAUTH_SCOPE.to_string()),
  ]);
  let response = client
    .post(token_endpoint)
    .header(CONTENT_TYPE, FORM_CONTENT_TYPE)
    .body(form.into_bytes())
    .send()
    .await
    .map_err(|error| response_failure(None, "request failed", Some(&error), &[secret]))?;
  let status = response.status().as_u16();
  let bytes = response
    .bytes()
    .await
    .map_err(|error| response_failure(Some(status), "response body read failed", Some(&error), &[secret]))?;
  let text = String::from_utf8_lossy(&bytes);
  if status != 200 {
    return Err(response_failure(Some(status), &text, None, &[secret]));
  }
  let value: Value =
    serde_json::from_slice(&bytes).map_err(|_| response_failure(Some(status), &text, None, &[secret]))?;
  let Some(access_token) = value.get(ACCESS_TOKEN_FIELD).and_then(Value::as_str).filter(|token| !token.is_empty())
  else {
    return Err(response_failure(Some(status), &text, None, &[secret]));
  };
  let Some(expires_in) = value.get(EXPIRES_IN_FIELD).and_then(Value::as_u64).filter(|seconds| *seconds > 0) else {
    return Err(response_failure(Some(status), &text, None, &[secret, access_token]));
  };
  let cached = CachedToken {
    access_token: access_token.to_string(),
    expires_at: now_unix_secs().saturating_add(expires_in),
    client_id: credentials.client_id.clone(),
    scope: LINEAR_OAUTH_SCOPE.to_string(),
  };
  write_cache(home, &cached)?;
  Ok(cached.access_token)
}

async fn send_graphql(
  client: &reqwest::Client,
  body: &[u8],
  endpoint: &str,
  token: &str,
  credentials: &LinearCredentials,
) -> Result<(u16, Vec<u8>), CliExit> {
  let secrets = [credentials.client_secret.as_str(), token];
  let response = client
    .post(endpoint)
    .header(CONTENT_TYPE, APPLICATION_JSON)
    .header(AUTHORIZATION, format!("Bearer {token}"))
    .body(body.to_vec())
    .send()
    .await
    .map_err(|error| response_failure(None, "request failed", Some(&error), &secrets))?;
  let status = response.status().as_u16();
  let bytes = response
    .bytes()
    .await
    .map_err(|error| response_failure(Some(status), "response body read failed", Some(&error), &secrets))?;
  Ok((status, bytes.to_vec()))
}

fn finish(status: u16, body: &[u8], token: &str, credentials: &LinearCredentials) -> Result<Vec<u8>, CliExit> {
  let secrets = [credentials.client_secret.as_str(), token];
  let text = String::from_utf8_lossy(body);
  if status != 200 {
    return Err(response_failure(Some(status), &text, None, &secrets));
  }
  let value: Value = serde_json::from_slice(body).map_err(|_| response_failure(Some(status), &text, None, &secrets))?;
  if value.as_object().is_some_and(|object| object.contains_key("errors")) {
    return Err(response_failure(Some(status), &text, None, &secrets));
  }
  Ok(body.to_vec())
}

fn token_path(home: &Path) -> PathBuf {
  home::state_dir(home).join(TOKEN_FILE_NAME)
}

fn read_cache(home: &Path) -> Result<Option<CachedToken>, CliExit> {
  let path = token_path(home);
  match std::fs::read(&path) {
    Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
      CliExit::error(format!("could not read the Linear token cache {}: {error}", paths::display(&path)))
    }),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => {
      Err(CliExit::error(format!("could not read the Linear token cache {}: {error}", paths::display(&path))))
    }
  }
}

fn cache_usable(cached: &CachedToken, credentials: &LinearCredentials) -> bool {
  cached.client_id == credentials.client_id
    && cached.scope == LINEAR_OAUTH_SCOPE
    && now_unix_secs().saturating_add(REFRESH_MARGIN_SECS) < cached.expires_at
}

fn create_state_dir(home: &Path) -> Result<(), CliExit> {
  let state = home::state_dir(home);
  std::fs::create_dir_all(&state)
    .map_err(|error| CliExit::error(format!("could not create {}: {error}", paths::display(&state))))
}

/// Write the cache: a temporary file in the same directory with mode `0o600`, then one rename puts it in place.
fn write_cache(home: &Path, cached: &CachedToken) -> Result<(), CliExit> {
  static COUNTER: AtomicU64 = AtomicU64::new(0);
  let directory = home::state_dir(home);
  let path = directory.join(TOKEN_FILE_NAME);
  let path_text = paths::display(&path);
  let bytes = serde_json::to_vec(cached)
    .map_err(|error| CliExit::error(format!("could not encode the Linear token cache {path_text}: {error}")))?;
  let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
  let temp = directory.join(format!(
    ".{TOKEN_FILE_NAME}.{}.{stamp}.{}.tmp",
    std::process::id(),
    COUNTER.fetch_add(1, Ordering::Relaxed)
  ));
  let write = || -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, &path)
  };
  write().map_err(|error| {
    let _ = std::fs::remove_file(&temp);
    CliExit::error(format!("could not write the Linear token cache {path_text}: {error}"))
  })
}

fn now_unix_secs() -> u64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

fn response_failure(
  status: Option<u16>,
  response: &str,
  cause: Option<&dyn std::error::Error>,
  secrets: &[&str],
) -> CliExit {
  let mut body = json!({"status": status, "response": redact(response, secrets)});
  if let Some(cause) = cause {
    body["error"] = json!(redact(&exit::source_chain(&cause), secrets));
  }
  CliExit::contract(body, 1)
}

fn redact(text: &str, secrets: &[&str]) -> String {
  let mut text = text.to_string();
  for secret in secrets {
    if !secret.is_empty() {
      text = text.replace(secret, credentials::REDACTED);
    }
  }
  text
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream};
  use std::os::unix::fs::PermissionsExt;
  use std::sync::mpsc;

  use charliebot_cli_common::test_support::LoopbackServer;

  const CLIENT_ID: &str = "synthetic-linear-client-id";
  const CLIENT_SECRET: &str = "synthetic-linear-client-secret";
  const ACCESS_TOKEN: &str = "synthetic-linear-access-token";
  const NEXT_ACCESS_TOKEN: &str = "synthetic-linear-access-token-next";
  const REQUEST_BODY: &[u8] = br#"{"query":"query { viewer { id } }","variables":{"id":"synthetic-id"}}"#;
  const GRAPHQL_OK: &[u8] = br#"{"data":{"viewer":{"id":"synthetic-id"}}}"#;
  const DAY_SECS: u64 = 86_400;

  fn credentials_home(client_id: Option<&str>, client_secret: Option<&str>) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let mut text = String::from("linear:\n");
    if let Some(client_id) = client_id {
      text.push_str(&credential_line(CLIENT_ID_KEY, client_id));
    }
    if let Some(client_secret) = client_secret {
      text.push_str(&credential_line(CLIENT_SECRET_KEY, client_secret));
    }
    std::fs::write(home.path().join(config::CREDENTIALS_FILENAME), text).unwrap();
    home
  }

  fn credential_line(key: &str, value: &str) -> String {
    if value.is_empty() { format!("  {key}: \"\"\n") } else { format!("  {key}: {value}\n") }
  }

  fn token_body(token: &str, expires_in: u64) -> Vec<u8> {
    format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#).into_bytes()
  }

  fn endpoints(server: &LoopbackServer) -> (String, String) {
    (format!("{}/graphql", server.base()), format!("{}/oauth/token", server.base()))
  }

  fn make_state_dir(home: &Path) {
    std::fs::create_dir_all(home::state_dir(home)).unwrap();
  }

  fn cache_valid_for(home: &Path, token: &str, client_id: &str, scope: &str) {
    make_state_dir(home);
    let cached = CachedToken {
      access_token: token.to_string(),
      expires_at: now_unix_secs() + 30 * DAY_SECS,
      client_id: client_id.to_string(),
      scope: scope.to_string(),
    };
    write_cache(home, &cached).unwrap();
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  fn assert_no_secrets(text: &str) {
    assert!(!text.contains(CLIENT_SECRET), "an output contains the synthetic client secret");
    assert!(!text.contains(ACCESS_TOKEN), "an output contains the synthetic access token");
    assert!(!text.contains(NEXT_ACCESS_TOKEN), "an output contains the synthetic access token");
  }

  #[test]
  fn a_fresh_exchange_caches_a_private_four_field_token_and_sends_bearer() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    let server =
      LoopbackServer::start(vec![(200, token_body(ACCESS_TOKEN, 30 * DAY_SECS)), (200, GRAPHQL_OK.to_vec())]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let response = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap();
    assert_eq!(response, GRAPHQL_OK);

    let token_request = server.next_request();
    let lowered = token_request.to_ascii_lowercase();
    assert!(token_request.starts_with("POST /oauth/token "), "{token_request}");
    assert!(lowered.contains("content-type: application/x-www-form-urlencoded"), "{token_request}");
    assert!(token_request.contains("grant_type=client_credentials"), "{token_request}");
    assert!(token_request.contains(&format!("client_id={CLIENT_ID}")), "{token_request}");
    assert!(token_request.contains(&format!("client_secret={CLIENT_SECRET}")), "{token_request}");
    assert!(token_request.contains("scope=read%2Cwrite%2Cinitiative%3Awrite"), "{token_request}");

    let graphql_request = server.next_request();
    let lowered = graphql_request.to_ascii_lowercase();
    assert!(graphql_request.starts_with("POST /graphql "), "{graphql_request}");
    assert!(lowered.contains(&format!("authorization: bearer {ACCESS_TOKEN}")), "{graphql_request}");
    assert!(lowered.contains("content-type: application/json"), "{graphql_request}");

    let path = token_path(home.path());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let cached: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(cached["access_token"], ACCESS_TOKEN);
    assert_eq!(cached["client_id"], CLIENT_ID);
    assert_eq!(cached["scope"], LINEAR_OAUTH_SCOPE);
    assert!(cached["expires_at"].as_u64().unwrap() > now_unix_secs() + REFRESH_MARGIN_SECS);
    assert_eq!(cached.as_object().unwrap().len(), 4);
  }

  #[test]
  fn a_reusable_cache_skips_the_token_endpoint() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    cache_valid_for(home.path(), ACCESS_TOKEN, CLIENT_ID, LINEAR_OAUTH_SCOPE);
    let server = LoopbackServer::start(vec![(200, GRAPHQL_OK.to_vec())]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let response = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap();
    assert_eq!(response, GRAPHQL_OK);
    let request = server.next_request();
    assert!(request.starts_with("POST /graphql "), "{request}");
    assert!(request.to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS_TOKEN}")));
    assert!(server.no_request(), "the token endpoint was called for a reusable cache");
  }

  fn assert_re_exchange(cached: CachedToken) {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    make_state_dir(home.path());
    write_cache(home.path(), &cached).unwrap();
    let server =
      LoopbackServer::start(vec![(200, token_body(ACCESS_TOKEN, 30 * DAY_SECS)), (200, GRAPHQL_OK.to_vec())]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap();
    assert!(server.next_request().starts_with("POST /oauth/token "));
    assert!(server.next_request().starts_with("POST /graphql "));
  }

  #[test]
  fn a_near_expiry_cache_re_exchanges() {
    assert_re_exchange(CachedToken {
      access_token: ACCESS_TOKEN.to_string(),
      expires_at: now_unix_secs() + REFRESH_MARGIN_SECS,
      client_id: CLIENT_ID.to_string(),
      scope: LINEAR_OAUTH_SCOPE.to_string(),
    });
  }

  #[test]
  fn a_cache_for_another_client_re_exchanges() {
    assert_re_exchange(CachedToken {
      access_token: ACCESS_TOKEN.to_string(),
      expires_at: now_unix_secs() + 30 * DAY_SECS,
      client_id: "another-client".to_string(),
      scope: LINEAR_OAUTH_SCOPE.to_string(),
    });
  }

  #[test]
  fn a_cache_for_another_scope_re_exchanges() {
    assert_re_exchange(CachedToken {
      access_token: ACCESS_TOKEN.to_string(),
      expires_at: now_unix_secs() + 30 * DAY_SECS,
      client_id: CLIENT_ID.to_string(),
      scope: "read".to_string(),
    });
  }

  #[test]
  fn a_corrupt_cache_exits_one_naming_the_path() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    make_state_dir(home.path());
    let path = token_path(home.path());
    std::fs::write(&path, "not-json").unwrap();

    let error =
      execute_graphql(REQUEST_BODY, "http://127.0.0.1:1/graphql", "http://127.0.0.1:1/oauth/token", home.path())
        .unwrap_err();
    assert_eq!(error.code, 1);
    let text = error_text(&error);
    assert!(text.contains(&paths::display(&path)), "{text}");
    assert_no_secrets(&text);
  }

  #[test]
  fn a_401_exchanges_once_and_retries_with_the_new_token() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    cache_valid_for(home.path(), ACCESS_TOKEN, CLIENT_ID, LINEAR_OAUTH_SCOPE);
    let server = LoopbackServer::start(vec![
      (401, b"{}".to_vec()),
      (200, token_body(NEXT_ACCESS_TOKEN, 30 * DAY_SECS)),
      (200, GRAPHQL_OK.to_vec()),
    ]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let response = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap();
    assert_eq!(response, GRAPHQL_OK);
    let first = server.next_request();
    assert!(first.to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS_TOKEN}")), "{first}");
    assert!(server.next_request().starts_with("POST /oauth/token "));
    let retried = server.next_request();
    assert!(retried.starts_with("POST /graphql "), "{retried}");
    assert!(retried.to_ascii_lowercase().contains(&format!("authorization: bearer {NEXT_ACCESS_TOKEN}")), "{retried}");
  }

  #[test]
  fn a_second_401_exits_one_without_leaking_a_token() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    cache_valid_for(home.path(), ACCESS_TOKEN, CLIENT_ID, LINEAR_OAUTH_SCOPE);
    let server = LoopbackServer::start(vec![
      (401, b"{}".to_vec()),
      (200, token_body(NEXT_ACCESS_TOKEN, 30 * DAY_SECS)),
      (401, format!(r#"{{"message":"{NEXT_ACCESS_TOKEN}"}}"#).into_bytes()),
    ]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let error = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap_err();
    assert_eq!(error.code, 1);
    let text = error_text(&error);
    assert!(text.contains("\"status\":401"), "{text}");
    assert!(text.contains(credentials::REDACTED), "{text}");
    assert_no_secrets(&text);
  }

  #[test]
  fn a_concurrent_401_reuses_a_newer_cached_token_without_exchanging() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    cache_valid_for(home.path(), ACCESS_TOKEN, CLIENT_ID, LINEAR_OAUTH_SCOPE);
    let cache_path = token_path(home.path());
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sender, requests) = mpsc::channel();
    let server = std::thread::spawn(move || {
      for (index, status) in [401_u16, 200].into_iter().enumerate() {
        let (mut stream, _) = listener.accept().unwrap();
        sender.send(String::from_utf8(read_test_request(&mut stream)).unwrap()).unwrap();
        if index == 0 {
          let cached = CachedToken {
            access_token: NEXT_ACCESS_TOKEN.to_string(),
            expires_at: now_unix_secs() + 30 * DAY_SECS,
            client_id: CLIENT_ID.to_string(),
            scope: LINEAR_OAUTH_SCOPE.to_string(),
          };
          std::fs::write(&cache_path, serde_json::to_vec(&cached).unwrap()).unwrap();
        }
        write_test_response(&mut stream, status, if index == 0 { b"{}" } else { GRAPHQL_OK });
      }
    });

    let response =
      execute_graphql(REQUEST_BODY, &format!("{base}/graphql"), &format!("{base}/oauth/token"), home.path()).unwrap();
    assert_eq!(response, GRAPHQL_OK);
    let first = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    server.join().unwrap();

    assert!(first.to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS_TOKEN}")), "{first}");
    assert!(second.starts_with("POST /graphql "), "{second}");
    assert!(second.to_ascii_lowercase().contains(&format!("authorization: bearer {NEXT_ACCESS_TOKEN}")), "{second}");
  }

  fn read_test_request(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).unwrap();
      if read == 0 {
        return request;
      }
      request.extend_from_slice(&buffer[..read]);
      let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4) else {
        continue;
      };
      let head = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
      let content_length = head
        .lines()
        .find_map(|line| {
          let (name, value) = line.split_once(':')?;
          name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
      if request.len() >= header_end + content_length {
        return request;
      }
    }
  }

  fn write_test_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let response = format!(
      "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
  }

  #[test]
  fn a_missing_or_empty_client_id_or_secret_exits_three() {
    let cases = [
      (None, Some(CLIENT_SECRET), CLIENT_ID_KEY),
      (Some(CLIENT_ID), None, CLIENT_SECRET_KEY),
      (Some(""), Some(CLIENT_SECRET), CLIENT_ID_KEY),
      (Some(CLIENT_ID), Some(""), CLIENT_SECRET_KEY),
    ];
    for (client_id, client_secret, missing) in cases {
      let home = credentials_home(client_id, client_secret);
      let error =
        execute_graphql(REQUEST_BODY, "http://127.0.0.1:1/graphql", "http://127.0.0.1:1/oauth/token", home.path())
          .unwrap_err();
      assert_eq!(error.code, 3);
      let text = error_text(&error);
      assert!(text.contains(&format!("credentials.linear.{missing}")), "{text}");
      assert!(text.contains("credentials.yaml"), "{text}");
      assert_no_secrets(&text);
    }
  }

  #[test]
  fn error_outputs_redact_the_client_secret_and_the_access_token() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    let server = LoopbackServer::start(vec![(200, token_body(ACCESS_TOKEN, 0))]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let error = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap_err();
    assert_eq!(error.code, 1);
    let text = error_text(&error);
    assert!(text.contains(credentials::REDACTED), "{text}");
    assert_no_secrets(&text);

    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    cache_valid_for(home.path(), ACCESS_TOKEN, CLIENT_ID, LINEAR_OAUTH_SCOPE);
    let echoed = format!(r#"{{"message":"{CLIENT_SECRET} {ACCESS_TOKEN}"}}"#).into_bytes();
    let server = LoopbackServer::start(vec![(400, echoed)]);
    let (graphql_endpoint, token_endpoint) = endpoints(&server);

    let error = execute_graphql(REQUEST_BODY, &graphql_endpoint, &token_endpoint, home.path()).unwrap_err();
    assert_eq!(error.code, 1);
    let text = error_text(&error);
    assert!(text.contains(credentials::REDACTED), "{text}");
    assert_no_secrets(&text);
  }

  #[test]
  fn invalid_request_bodies_exit_two_without_echoing_input() {
    let home = credentials_home(None, None);
    let invalid = format!("{{not-json {CLIENT_SECRET}");
    let error =
      execute_graphql(invalid.as_bytes(), "http://127.0.0.1:1/graphql", "http://127.0.0.1:1/oauth/token", home.path())
        .unwrap_err();

    assert_eq!(error.code, 2);
    assert_no_secrets(&error_text(&error));
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
  fn a_network_failure_reports_a_cause_without_secrets() {
    let home = credentials_home(Some(CLIENT_ID), Some(CLIENT_SECRET));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = execute_graphql(
      REQUEST_BODY,
      &format!("http://127.0.0.1:{port}/graphql"),
      &format!("http://127.0.0.1:{port}/oauth/token"),
      home.path(),
    )
    .unwrap_err();
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().is_some_and(|cause| !cause.is_empty()));
    assert_no_secrets(&error_text(&error));
  }
}
