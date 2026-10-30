//! `charliebot feishu`: authenticated Feishu Open API requests, token refresh and re-authorization.
//!
//! `request` reads `feishu.user_access_token` inside this process, sends one HTTP request and prints
//! the response body. A 401 or an expired-token code refreshes the token, writes both new tokens to
//! `credentials.yaml` and retries once. `consent-url` and `exchange-code` port the skill's
//! re-authorization steps. No command or output carries a token.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::request::{self, Request, RequestArgs};
use charliebot_cli_common::urlencode::urlencode;
use charliebot_core::config;
use charliebot_core::credentials::{self, Credentials};
use charliebot_core::home;
use charliebot_core::paths;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

const FEISHU_SECTION: &str = "feishu";
const AUTHORIZE_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/authorize";
const APP_TOKEN_URL: &str = "https://open.feishu.cn/open-apis/auth/v3/app_access_token/internal";
const REFRESH_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/oidc/refresh_access_token";
const EXCHANGE_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/oidc/access_token";
const REDIRECT_URI: &str = "http://localhost:9999/callback";
/// The open API's token-expired code.
const TOKEN_EXPIRED_CODE: i64 = 99991663;
/// The user-info endpoint reports the same expired token as an invalid access token.
const INVALID_ACCESS_TOKEN_CODE: i64 = 20005;

/// The endpoints the command calls. Tests point them at local stubs.
#[derive(Debug, Clone)]
struct Endpoints {
  authorize_url: String,
  app_token_url: String,
  refresh_url: String,
  exchange_url: String,
}

impl Endpoints {
  fn production() -> Endpoints {
    Endpoints {
      authorize_url: AUTHORIZE_URL.to_string(),
      app_token_url: APP_TOKEN_URL.to_string(),
      refresh_url: REFRESH_URL.to_string(),
      exchange_url: EXCHANGE_URL.to_string(),
    }
  }
}

#[derive(Parser)]
#[command(name = "feishu", subcommand_required = true, arg_required_else_help = true)]
struct FeishuCli {
  #[command(subcommand)]
  command: FeishuCommand,
}

#[derive(Subcommand)]
enum FeishuCommand {
  /// Send one authenticated Feishu Open API request.
  Request(RequestCommand),
  /// Print the browser URL that starts a re-authorization.
  ConsentUrl(ConsentUrlArgs),
  /// Exchange an authorization code for tokens and store them.
  ExchangeCode(ExchangeCodeArgs),
}

#[derive(Args)]
struct RequestCommand {
  #[command(flatten)]
  request: RequestArgs,
}

#[derive(Args)]
struct ConsentUrlArgs {
  /// One granted scope; repeat or list several.
  #[arg(long = "scope", required = true, num_args = 1.., value_name = "SCOPE")]
  scope: Vec<String>,
}

#[derive(Args)]
struct ExchangeCodeArgs {
  /// The `code` query parameter of the browser redirect.
  code: String,
}

/// Run the `feishu` subcommand.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(FeishuCli::command());
  let argv = std::iter::once(OsString::from("charliebot feishu")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = FeishuCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let endpoints = Endpoints::production();
  let home_dir = home_dir()?;
  match cli.command {
    FeishuCommand::Request(args) => request::write_stdout(&execute_request(&home_dir, &args.request, &endpoints)?),
    FeishuCommand::ConsentUrl(args) => {
      println!("{}", consent_url(&home_dir, &args.scope, &endpoints)?);
      Ok(())
    }
    FeishuCommand::ExchangeCode(args) => exchange_code(&home_dir, &args.code, &endpoints),
  }
}

fn home_dir() -> Result<PathBuf, CliExit> {
  home::charliebot_home_dir().map_err(|_| credential_failure("user_access_token", None))
}

fn credentials_path(home: &Path) -> PathBuf {
  home.join(config::CREDENTIALS_FILENAME)
}

fn credential_failure(key: &str, path: Option<&Path>) -> CliExit {
  let where_text = match path {
    Some(path) => paths::display(path),
    None => format!("$CHARLIEBOT_HOME/{}", config::CREDENTIALS_FILENAME),
  };
  CliExit::contract(json!({"error": format!("credentials.feishu.{key} is unavailable in {where_text}")}), 3)
}

fn load(home: &Path) -> Result<Credentials, CliExit> {
  credentials::load_credentials(home)
    .map_err(|_| credential_failure("user_access_token", Some(&credentials_path(home))))
}

/// One required credential value. An empty value cannot authenticate, so it counts as unset.
fn required(credentials: &Credentials, key: &str, home: &Path) -> Result<String, CliExit> {
  let failure = || credential_failure(key, Some(&credentials_path(home)));
  credentials
    .require(FEISHU_SECTION, key)
    .map_err(|_| failure())
    .and_then(|value| if value.trim().is_empty() { Err(failure()) } else { Ok(value) })
}

/// Send one request with the stored token, refreshing and retrying once when it expired.
fn execute_request(home: &Path, args: &RequestArgs, endpoints: &Endpoints) -> Result<Vec<u8>, CliExit> {
  let request = args.prepare()?;
  let credentials = load(home)?;
  let access = required(&credentials, "user_access_token", home)?;
  let response = request::send(&request.clone().bearer(&access), &[access.as_str()])?;
  if token_expired(response.status, &response.body) {
    let refreshed = refresh(home, &credentials, endpoints)?;
    let redactions = [refreshed.access.as_str(), refreshed.refresh.as_str(), access.as_str()];
    let retried = request::send(&request.bearer(&refreshed.access), &redactions)?;
    return finish(retried, &redactions);
  }
  finish(response, &[access.as_str()])
}

/// A 2xx body without an error code; every other answer is an HTTP-style failure with exit 1.
fn finish(response: request::Response, redactions: &[&str]) -> Result<Vec<u8>, CliExit> {
  if !(200..300).contains(&response.status) {
    return Err(request::http_failure(response.status, &response.body, redactions));
  }
  if response_code(&response.body).is_some_and(|code| code != 0) {
    return Err(request::http_failure(response.status, &response.body, redactions));
  }
  Ok(request::redact_bytes(&response.body, redactions))
}

fn token_expired(status: u16, body: &[u8]) -> bool {
  status == 401 || matches!(response_code(body), Some(TOKEN_EXPIRED_CODE) | Some(INVALID_ACCESS_TOKEN_CODE))
}

fn response_code(body: &[u8]) -> Option<i64> {
  serde_json::from_slice::<Value>(body).ok()?.get("code")?.as_i64()
}

struct Refreshed {
  access: String,
  refresh: String,
}

/// The skill's Token Refresh steps, inside this process: an app token mints a new user token, which
/// is written before the request retries.
fn refresh(home: &Path, credentials: &Credentials, endpoints: &Endpoints) -> Result<Refreshed, CliExit> {
  let app_id = required(credentials, "app_id", home)?;
  let app_secret = required(credentials, "app_secret", home)?;
  let current_refresh = required(credentials, "refresh_token", home)?;
  let app_token = app_access_token(&app_id, &app_secret, endpoints, &[app_secret.as_str()])?;
  let redactions = [app_secret.as_str(), current_refresh.as_str(), app_token.as_str()];
  let body = json!({"grant_type": "refresh_token", "refresh_token": current_refresh}).to_string();
  let request = Request::new(reqwest::Method::POST, &endpoints.refresh_url)
    .header("Content-Type", "application/json")
    .bearer(&app_token)
    .body(body.into_bytes());
  let payload = open_json(&request, "Feishu token refresh", &redactions)?;
  let access = data_string(&payload, "access_token", "Feishu token refresh")?;
  let refresh = data_string(&payload, "refresh_token", "Feishu token refresh")?;
  store_tokens(home, &access, &refresh)?;
  Ok(Refreshed { access, refresh })
}

fn app_access_token(
  app_id: &str,
  app_secret: &str,
  endpoints: &Endpoints,
  redactions: &[&str],
) -> Result<String, CliExit> {
  let body = json!({"app_id": app_id, "app_secret": app_secret}).to_string();
  let request = Request::new(reqwest::Method::POST, &endpoints.app_token_url)
    .header("Content-Type", "application/json")
    .body(body.into_bytes());
  let payload = open_json(&request, "Feishu app token", redactions)?;
  string_field(&payload, "app_access_token", "Feishu app token")
}

/// One token endpoint's JSON answer; a non-2xx status or a nonzero `code` is an exit-1 failure.
fn open_json(request: &Request, operation: &str, redactions: &[&str]) -> Result<Value, CliExit> {
  let response = request::send(request, redactions)?;
  if !(200..300).contains(&response.status) {
    return Err(request::http_failure(response.status, &response.body, redactions));
  }
  let payload: Value =
    serde_json::from_slice(&response.body).map_err(|_| CliExit::error(format!("{operation} returned invalid JSON")))?;
  if payload.get("code").and_then(Value::as_i64).is_some_and(|code| code != 0) {
    return Err(request::http_failure(response.status, &response.body, redactions));
  }
  Ok(payload)
}

fn string_field(payload: &Value, field: &str, operation: &str) -> Result<String, CliExit> {
  payload
    .get(field)
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
    .ok_or_else(|| CliExit::error(format!("{operation} did not return {field}")))
}

fn data_string(payload: &Value, field: &str, operation: &str) -> Result<String, CliExit> {
  let data = payload.get("data").ok_or_else(|| CliExit::error(format!("{operation} did not return data")))?;
  string_field(data, field, operation)
}

/// Write the new `user_access_token` and `refresh_token`, keeping every other byte of the file.
fn store_tokens(home: &Path, access: &str, refresh: &str) -> Result<(), CliExit> {
  let path = credentials_path(home);
  let original =
    std::fs::read(&path).map_err(|_| CliExit::error(format!("could not read {}", paths::display(&path))))?;
  let text =
    String::from_utf8(original).map_err(|_| CliExit::error(format!("{} must use UTF-8", paths::display(&path))))?;
  let updated = credentials::with_credential_values(
    &text,
    FEISHU_SECTION,
    &[("user_access_token", access), ("refresh_token", refresh)],
  )
  .map_err(|error| CliExit::error(error.to_string()))?;
  credentials::write_credentials(home, updated.as_bytes()).map_err(|error| CliExit::error(error.to_string()))
}

fn consent_url(home: &Path, scopes: &[String], endpoints: &Endpoints) -> Result<String, CliExit> {
  let credentials = load(home)?;
  let app_id = required(&credentials, "app_id", home)?;
  let query = urlencode(&[
    ("app_id".to_string(), app_id),
    ("redirect_uri".to_string(), REDIRECT_URI.to_string()),
    ("scope".to_string(), scopes.join(" ")),
  ]);
  Ok(format!("{}?{query}", endpoints.authorize_url))
}

fn exchange_code(home: &Path, code: &str, endpoints: &Endpoints) -> Result<(), CliExit> {
  let credentials = load(home)?;
  let app_id = required(&credentials, "app_id", home)?;
  let app_secret = required(&credentials, "app_secret", home)?;
  let app_token = app_access_token(&app_id, &app_secret, endpoints, &[app_secret.as_str()])?;
  let redactions = [app_secret.as_str(), app_token.as_str(), code];
  let body = json!({"grant_type": "authorization_code", "code": code}).to_string();
  let request = Request::new(reqwest::Method::POST, &endpoints.exchange_url)
    .header("Content-Type", "application/json")
    .bearer(&app_token)
    .body(body.into_bytes());
  let payload = open_json(&request, "Feishu code exchange", &redactions)?;
  let access = data_string(&payload, "access_token", "Feishu code exchange")?;
  let refresh = data_string(&payload, "refresh_token", "Feishu code exchange")?;
  store_tokens(home, &access, &refresh)
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::os::unix::fs::PermissionsExt;
  use std::sync::mpsc::{self, Receiver};
  use std::thread;
  use std::time::Duration;

  use super::*;

  const ACCESS: &str = "synthetic-access-token";
  const REFRESH: &str = "synthetic-refresh-token";
  const NEW_ACCESS: &str = "synthetic-new-access-token";
  const NEW_REFRESH: &str = "synthetic-new-refresh-token";
  const APP_TOKEN: &str = "synthetic-app-token";
  const APP_SECRET: &str = "synthetic-app-secret";
  const TIMEOUT: Duration = Duration::from_secs(2);

  const TABLE: &str = concat!(
    "charliebot:\n  access_key: k\n",
    "feishu:\n  app_id: synthetic-app-id\n  app_secret: synthetic-app-secret\n",
    "  user_access_token: synthetic-access-token\n  refresh_token: synthetic-refresh-token\n",
    "linear:\n  api_key: l\n",
  );

  fn credentials_home(text: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::CREDENTIALS_FILENAME);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    home
  }

  fn request_args(url: &str) -> RequestArgs {
    RequestArgs { method: "GET".to_string(), url: url.to_string(), data_file: None, json: None, header: Vec::new() }
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  fn assert_no_token(text: &str) {
    for token in [ACCESS, REFRESH, NEW_ACCESS, NEW_REFRESH, APP_TOKEN, APP_SECRET] {
      assert!(!text.contains(token), "an output contains a synthetic token");
    }
  }

  /// A local server that answers the scripted responses in order and records every request.
  struct Stub {
    base: String,
    requests: Receiver<Vec<u8>>,
  }

  impl Stub {
    fn start(responses: Vec<(u16, Vec<u8>)>) -> Stub {
      let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
      let base = format!("http://{}", listener.local_addr().unwrap());
      let (sender, requests) = mpsc::channel();
      thread::spawn(move || {
        for (status, body) in responses {
          let (mut stream, _) = listener.accept().unwrap();
          sender.send(read_http_request(&mut stream)).unwrap();
          let response = format!(
            "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
          let _ = stream.write_all(response.as_bytes());
          let _ = stream.write_all(&body);
        }
      });
      Stub { base, requests }
    }

    fn endpoints(&self) -> Endpoints {
      Endpoints {
        authorize_url: format!("{}/authorize", self.base),
        app_token_url: format!("{}/app-token", self.base),
        refresh_url: format!("{}/refresh", self.base),
        exchange_url: format!("{}/exchange", self.base),
      }
    }

    fn next(&self) -> String {
      String::from_utf8(self.requests.recv_timeout(TIMEOUT).unwrap()).unwrap()
    }
  }

  fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
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
          name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
      if request.len() >= header_end + content_length {
        return request;
      }
    }
  }

  fn json_response(value: Value) -> (u16, Vec<u8>) {
    (200, value.to_string().into_bytes())
  }

  #[test]
  fn request_uses_the_user_token_and_prints_the_body() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![json_response(json!({"code": 0, "data": {"ok": true}}))]);

    let body = execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap();
    let request = stub.next();
    let lowered = request.to_ascii_lowercase();

    assert_eq!(body, br#"{"code":0,"data":{"ok":true}}"#.to_vec());
    assert!(lowered.contains(&format!("authorization: bearer {ACCESS}")));
    assert!(request.starts_with("GET /api HTTP/1.1"));
    assert_no_token(&String::from_utf8_lossy(&body));
  }

  #[test]
  fn a_feishu_error_code_exits_one_and_redacts_the_token() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![json_response(json!({"code": 10003, "msg": format!("denied {ACCESS}")}))]);

    let error =
      execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap_err();
    let _ = stub.next();
    let text = error_text(&error);

    assert_eq!(error.code, 1);
    assert_eq!(error.body["status"], 200);
    assert!(error.body["response"].as_str().unwrap().contains("[redacted]"));
    assert_no_token(&text);
  }

  #[test]
  fn a_non_2xx_status_exits_one() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![(403, br#"{"msg":"denied"}"#.to_vec())]);

    let error =
      execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap_err();
    let _ = stub.next();

    assert_eq!(error.code, 1);
    assert_eq!(error.body["status"], 403);
    assert_no_token(&error_text(&error));
  }

  #[test]
  fn a_missing_token_is_a_credential_failure() {
    let home = credentials_home("feishu:\n  app_id: a\n  app_secret: synthetic-app-secret\n");

    let error =
      execute_request(home.path(), &request_args("http://127.0.0.1:1/api"), &Endpoints::production()).unwrap_err();

    assert_eq!(error.code, 3);
    assert_no_token(&error_text(&error));
  }

  #[test]
  fn an_expired_token_refreshes_writes_both_tokens_and_retries() {
    for code in [TOKEN_EXPIRED_CODE, INVALID_ACCESS_TOKEN_CODE] {
      let home = credentials_home(TABLE);
      let stub = Stub::start(vec![
        json_response(json!({"code": code})),
        json_response(json!({"code": 0, "app_access_token": APP_TOKEN})),
        json_response(json!({"code": 0, "data": {"access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH}})),
        json_response(json!({"code": 0, "data": {"ok": true}})),
      ]);

      let body = execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap();
      let first = stub.next();
      let app_request = stub.next();
      let refresh_request = stub.next();
      let retried = stub.next();

      assert!(first.to_ascii_lowercase().contains(&format!("authorization: bearer {ACCESS}")));
      assert!(app_request.starts_with("POST /app-token HTTP/1.1"));
      assert!(app_request.ends_with(r#"{"app_id":"synthetic-app-id","app_secret":"synthetic-app-secret"}"#));
      assert!(refresh_request.to_ascii_lowercase().contains(&format!("authorization: bearer {APP_TOKEN}")));
      assert!(refresh_request.ends_with(&format!(r#"{{"grant_type":"refresh_token","refresh_token":"{REFRESH}"}}"#)));
      assert!(retried.to_ascii_lowercase().contains(&format!("authorization: bearer {NEW_ACCESS}")));
      assert_eq!(body, br#"{"code":0,"data":{"ok":true}}"#.to_vec());

      let saved = std::fs::read_to_string(home.path().join(config::CREDENTIALS_FILENAME)).unwrap();
      assert_eq!(saved, TABLE.replace(ACCESS, NEW_ACCESS).replace(REFRESH, NEW_REFRESH));
      assert_eq!(
        std::fs::metadata(home.path().join(config::CREDENTIALS_FILENAME)).unwrap().permissions().mode() & 0o7777,
        0o600
      );
      assert_no_token(&String::from_utf8_lossy(&body));
    }
  }

  #[test]
  fn a_401_also_refreshes_and_retries_once() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![
      (401, br#"{"msg":"expired"}"#.to_vec()),
      json_response(json!({"code": 0, "app_access_token": APP_TOKEN})),
      json_response(json!({"code": 0, "data": {"access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH}})),
      json_response(json!({"code": 0})),
    ]);

    execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap();
    for _ in 0..4 {
      let _ = stub.next();
    }

    let saved = std::fs::read_to_string(home.path().join(config::CREDENTIALS_FILENAME)).unwrap();
    assert!(saved.contains(&format!("user_access_token: {NEW_ACCESS}")));
  }

  #[test]
  fn a_failed_refresh_exits_one_and_keeps_the_file_unchanged() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![
      (401, br#"{"msg":"expired"}"#.to_vec()),
      json_response(json!({"code": 0, "app_access_token": APP_TOKEN})),
      (400, json!({"msg": format!("bad {REFRESH}")}).to_string().into_bytes()),
    ]);

    let error =
      execute_request(home.path(), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap_err();
    for _ in 0..3 {
      let _ = stub.next();
    }

    assert_eq!(error.code, 1);
    assert_no_token(&error_text(&error));
    assert_eq!(std::fs::read_to_string(home.path().join(config::CREDENTIALS_FILENAME)).unwrap(), TABLE);
  }

  #[test]
  fn consent_url_names_the_app_and_scopes_without_a_token() {
    let home = credentials_home(TABLE);
    let url = consent_url(
      home.path(),
      &["docs:permission.setting:read".to_string(), "search:docs:read".to_string()],
      &Endpoints::production(),
    )
    .unwrap();

    assert!(url.starts_with("https://open.feishu.cn/open-apis/authen/v1/authorize?app_id=synthetic-app-id"));
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A9999%2Fcallback"));
    assert!(url.contains("scope=docs%3Apermission.setting%3Aread+search%3Adocs%3Aread"));
    assert_no_token(&url);
  }

  #[test]
  fn exchange_code_stores_the_new_tokens() {
    let home = credentials_home(TABLE);
    let stub = Stub::start(vec![
      json_response(json!({"code": 0, "app_access_token": APP_TOKEN})),
      json_response(json!({"code": 0, "data": {"access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH}})),
    ]);

    exchange_code(home.path(), "synthetic-code", &stub.endpoints()).unwrap();
    let app_request = stub.next();
    let exchange_request = stub.next();

    assert!(app_request.starts_with("POST /app-token HTTP/1.1"));
    assert!(exchange_request.ends_with(r#"{"grant_type":"authorization_code","code":"synthetic-code"}"#));
    let saved = std::fs::read_to_string(home.path().join(config::CREDENTIALS_FILENAME)).unwrap();
    assert_eq!(saved, TABLE.replace(ACCESS, NEW_ACCESS).replace(REFRESH, NEW_REFRESH));
  }

  #[test]
  fn usage_errors_exit_two_without_a_token() {
    let home = credentials_home(TABLE);
    let error = execute_request(home.path(), &request_args("relative/path"), &Endpoints::production()).unwrap_err();
    assert_eq!(error.code, 2);
    assert_no_token(&error_text(&error));
  }
}
