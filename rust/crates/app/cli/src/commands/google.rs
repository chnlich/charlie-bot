//! `charliebot google`: the Google account table and authenticated Google requests
//! (`skills/google-oauth/scripts/google_account.py`).
//!
//! `request` mints the selected account's access token inside this process, sends one HTTP request
//! and prints the response body. `list` prints the account table. The token never reaches a
//! command line, an output stream or a run record.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::request::{self, Request, RequestArgs};
use charliebot_cli_common::urlencode::urlencode;
use charliebot_core::config;
use charliebot_core::home;
use charliebot_core::paths;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as Yaml};

const GOOGLE_SECTION: &str = "google";
const ACCOUNT_PREFIX: &str = "google_account_";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const PROFILE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";

/// The two endpoints this command calls. Tests point them at local stubs.
#[derive(Debug, Clone)]
struct Endpoints {
  token_url: String,
  profile_url: String,
}

impl Endpoints {
  fn production() -> Endpoints {
    Endpoints { token_url: TOKEN_URL.to_string(), profile_url: PROFILE_URL.to_string() }
  }
}

#[derive(Parser)]
#[command(name = "google", subcommand_required = true, arg_required_else_help = true)]
struct GoogleCli {
  #[command(subcommand)]
  command: GoogleCommand,
}

#[derive(Subcommand)]
enum GoogleCommand {
  Request(RequestCommand),
  List(ListArgs),
}

#[derive(Args)]
struct RequestCommand {
  #[command(flatten)]
  request: RequestArgs,
  #[arg(long, value_name = "NAME")]
  account: Option<String>,
}

#[derive(Args)]
struct ListArgs {
  #[arg(long)]
  check: bool,
}

/// Run the `google` subcommand.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(GoogleCli::command());
  let argv = std::iter::once(OsString::from("charliebot google")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = GoogleCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let endpoints = Endpoints::production();
  let table = load_table()?;
  match cli.command {
    GoogleCommand::Request(args) => {
      let body = execute_request(&table, args.account.as_deref(), &args.request, &endpoints)?;
      request::write_stdout(&body)
    }
    GoogleCommand::List(args) => {
      let output = list_output(&table, args.check, &endpoints)?;
      request::write_stdout(output.as_bytes())
    }
  }
}

// ---------------------------------------------------------------------------
// The request and list verbs
// ---------------------------------------------------------------------------

/// Mint the selected account's token and send one request. The body is the 2xx response.
fn execute_request(
  table: &AccountTable,
  account_name: Option<&str>,
  args: &RequestArgs,
  endpoints: &Endpoints,
) -> Result<Vec<u8>, CliExit> {
  let request = args.prepare()?;
  let account = account_for(table, account_name)?;
  let token = mint_access_token(table, account, &endpoints.token_url)?;
  request::fetch(&request.bearer(&token), &[token.as_str()])
}

/// The `list` output: one tab-separated row per account, with the checked email address when
/// `--check` succeeds. No token or refresh token reaches the output.
fn list_output(table: &AccountTable, check: bool, endpoints: &Endpoints) -> Result<String, CliExit> {
  let mut checked_emails: HashMap<String, String> = HashMap::new();
  if check {
    for account in &table.accounts {
      if is_pending(account) {
        continue;
      }
      let email = mint_access_token(table, account, &endpoints.token_url)
        .and_then(|token| profile_email(&token, &endpoints.profile_url))
        .map_err(|error| account_failure(&account.name, error))?;
      checked_emails.insert(account.name.clone(), email);
    }
  }
  let mut output = String::new();
  for account in &table.accounts {
    let pending = is_pending(account);
    let mark = if account.is_default {
      "default"
    } else if account.person_primary {
      "primary"
    } else {
      ""
    };
    let mut fields = vec![
      account.name.as_str(),
      mark,
      if pending { "pending" } else { "ready" },
      account.owner.as_deref().unwrap_or(""),
      account.description.as_str(),
    ];
    if check {
      fields.push(if pending { "" } else { checked_emails.get(&account.name).map(String::as_str).unwrap_or("") });
    }
    output.push_str(&fields.join("\t"));
    output.push('\n');
  }
  Ok(output)
}

/// Wrap a per-account failure the way the Python list check does.
fn account_failure(name: &str, error: CliExit) -> CliExit {
  let message =
    error.body.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| error.body.to_string());
  CliExit::error(format!("account {name}: {message}"))
}

// ---------------------------------------------------------------------------
// The account table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Account {
  name: String,
  is_default: bool,
  values: Mapping,
  owner: Option<String>,
  description: String,
  is_primary: bool,
  person_primary: bool,
}

#[derive(Debug)]
struct AccountTable {
  accounts: Vec<Account>,
  by_name: HashMap<String, Account>,
  google: Mapping,
}

fn load_table() -> Result<AccountTable, CliExit> {
  let home = home::charliebot_home_dir().map_err(|error| CliExit::usage(error.to_string()))?;
  load_table_at(&home.join(config::CREDENTIALS_FILENAME))
}

fn load_table_at(path: &Path) -> Result<AccountTable, CliExit> {
  let raw_bytes = std::fs::read(path).map_err(|_| {
    // The Linear table's credential failure: the file a credential would come from is unreadable.
    CliExit::contract(json!({"error": format!("cannot read credentials file: {}", paths::display(path))}), 3)
  })?;
  table_from_bytes(&raw_bytes)
}

fn table_from_bytes(raw_bytes: &[u8]) -> Result<AccountTable, CliExit> {
  let raw_text = std::str::from_utf8(raw_bytes).map_err(|_| CliExit::usage("credentials.yaml must use UTF-8"))?;
  if let Some(section) = duplicate_google_section(raw_text) {
    return Err(CliExit::usage(format!("credentials.{section} appears more than once")));
  }
  let parsed =
    serde_yaml::from_str::<Yaml>(raw_text).map_err(|_| CliExit::usage("credentials.yaml is not valid YAML"))?;
  let Yaml::Mapping(data) = parsed else {
    return Err(CliExit::usage("credentials.yaml must contain a mapping"));
  };
  if data.keys().any(|key| key.as_str().is_none()) {
    return Err(CliExit::usage("credentials.yaml has an invalid section name"));
  }
  if !data.contains_key(GOOGLE_SECTION) {
    return Err(CliExit::usage("credentials.google is missing"));
  }
  let google = google_values(&data, GOOGLE_SECTION)?;
  validate_client_pair(GOOGLE_SECTION, &google)?;
  let google_description = validate_description(GOOGLE_SECTION, &google)?;
  let host_owner = validate_owner(GOOGLE_SECTION, &google)?;

  let default_name = match google.get("name") {
    None => "default".to_string(),
    Some(Yaml::String(name)) if is_account_name(name) => name.clone(),
    Some(_) => return Err(CliExit::usage("credentials.google.name must match [a-z][a-z0-9-]*")),
  };

  let mut accounts = vec![Account {
    name: default_name,
    is_default: true,
    values: google.clone(),
    owner: host_owner.clone(),
    description: google_description,
    is_primary: false,
    person_primary: false,
  }];
  for (key, _) in &data {
    let Some(section) = key.as_str() else { continue };
    let Some(name) = section.strip_prefix(ACCOUNT_PREFIX) else { continue };
    if !is_account_name(name) {
      return Err(CliExit::usage(format!("credentials.{section} has an invalid account name")));
    }
    let values = google_values(&data, section)?;
    validate_client_pair(section, &values)?;
    let description = validate_description(section, &values)?;
    match values.get("refresh_token") {
      None | Some(Yaml::Null) | Some(Yaml::String(_)) => {}
      Some(_) => return Err(CliExit::usage(format!("credentials.{section}.refresh_token must be a string"))),
    }
    let owner = validate_owner(section, &values)?;
    if owner.is_some() && host_owner.is_none() {
      return Err(CliExit::usage(format!("credentials.{section} sets owner but credentials.google does not")));
    }
    let resolved_owner = owner.clone().or_else(|| host_owner.clone());
    let is_primary = validate_primary(section, &values, resolved_owner.as_deref(), host_owner.as_deref())?;
    accounts.push(Account {
      name: name.to_string(),
      is_default: false,
      values,
      owner,
      description,
      is_primary,
      person_primary: false,
    });
  }
  validate_ownership(&accounts, host_owner.as_deref())?;
  mark_person_primary(&mut accounts, host_owner.as_deref());

  let mut by_name = HashMap::new();
  for account in &accounts {
    if by_name.insert(account.name.clone(), account.clone()).is_some() {
      return Err(CliExit::usage(format!("duplicate Google account name: {}", account.name)));
    }
  }
  Ok(AccountTable { accounts, by_name, google })
}

/// The Python script composes the YAML tree and names a repeated `google` section; serde_yaml
/// refuses every repeated key, so the section names are scanned before the parse to keep the
/// specific message.
fn duplicate_google_section(raw_text: &str) -> Option<String> {
  let mut seen: Vec<&str> = Vec::new();
  for line in raw_text.lines() {
    if line.starts_with([' ', '\t']) {
      continue;
    }
    let Some((name, rest)) = line.split_once(':') else { continue };
    if !(name == GOOGLE_SECTION || name.starts_with(ACCOUNT_PREFIX)) {
      continue;
    }
    if !(rest.is_empty() || rest.starts_with([' ', '\t', '#'])) {
      continue;
    }
    if seen.contains(&name) {
      return Some(name.to_string());
    }
    seen.push(name);
  }
  None
}

fn google_values(data: &Mapping, section: &str) -> Result<Mapping, CliExit> {
  let Some(value) = data.get(section) else {
    return Err(CliExit::usage(format!("credentials.{section} must be a mapping")));
  };
  let Yaml::Mapping(values) = value else {
    return Err(CliExit::usage(format!("credentials.{section} must be a mapping")));
  };
  if values.keys().any(|key| key.as_str().is_none()) {
    return Err(CliExit::usage(format!("credentials.{section} has an invalid key")));
  }
  Ok(values.clone())
}

fn validate_client_pair(section: &str, values: &Mapping) -> Result<(), CliExit> {
  let has_client_id = values.contains_key("client_id");
  let has_client_secret = values.contains_key("client_secret");
  if has_client_id != has_client_secret {
    return Err(CliExit::usage(format!("credentials.{section} must set client_id and client_secret together")));
  }
  Ok(())
}

fn validate_owner(section: &str, values: &Mapping) -> Result<Option<String>, CliExit> {
  match values.get("owner") {
    None | Some(Yaml::Null) => Ok(None),
    Some(Yaml::String(owner)) if !owner.trim().is_empty() && !owner.contains(['\n', '\r']) => Ok(Some(owner.clone())),
    Some(_) => Err(CliExit::usage(format!("credentials.{section}.owner must be one line"))),
  }
}

fn validate_primary(
  section: &str,
  values: &Mapping,
  resolved_owner: Option<&str>,
  host_owner: Option<&str>,
) -> Result<bool, CliExit> {
  match values.get("primary") {
    None | Some(Yaml::Null) => Ok(false),
    Some(Yaml::Bool(true)) => {
      if resolved_owner == host_owner {
        Err(CliExit::usage(format!("credentials.{section} belongs to the host owner and cannot set primary")))
      } else {
        Ok(true)
      }
    }
    Some(_) => Err(CliExit::usage(format!("credentials.{section}.primary must be true"))),
  }
}

fn validate_description(section: &str, values: &Mapping) -> Result<String, CliExit> {
  match values.get("description") {
    None | Some(Yaml::Null) => Ok(String::new()),
    Some(Yaml::String(description)) if !description.contains(['\n', '\r']) => Ok(description.clone()),
    Some(_) => Err(CliExit::usage(format!("credentials.{section}.description must be one line"))),
  }
}

fn validate_ownership(accounts: &[Account], host_owner: Option<&str>) -> Result<(), CliExit> {
  let mut groups: Vec<(Option<String>, Vec<&Account>)> = Vec::new();
  for account in accounts {
    match groups.iter_mut().find(|(owner, _)| *owner == account.owner) {
      Some((_, members)) => members.push(account),
      None => groups.push((account.owner.clone(), vec![account])),
    }
  }
  for (owner, sections) in groups {
    let primaries = sections.iter().filter(|account| account.is_primary).count();
    if primaries > 1 {
      return Err(CliExit::usage(format!(
        "person {} has two accounts with primary: true",
        owner.as_deref().unwrap_or("None")
      )));
    }
    if owner.is_some() && owner.as_deref() != host_owner && sections.len() > 1 && primaries == 0 {
      return Err(CliExit::usage(format!(
        "person {} has several accounts and none sets primary: true",
        owner.as_deref().unwrap_or("None")
      )));
    }
  }
  Ok(())
}

fn mark_person_primary(accounts: &mut [Account], host_owner: Option<&str>) {
  let mut counts: Vec<(String, usize)> = Vec::new();
  for account in accounts.iter() {
    let resolved = resolved_owner(account, host_owner);
    match counts.iter_mut().find(|(owner, _)| *owner == resolved) {
      Some((_, count)) => *count += 1,
      None => counts.push((resolved, 1)),
    }
  }
  for account in accounts.iter_mut() {
    let resolved = resolved_owner(account, host_owner);
    let count = counts.iter().find(|(owner, _)| *owner == resolved).map(|(_, count)| *count).unwrap_or(0);
    account.person_primary =
      !account.is_default && Some(resolved.as_str()) != host_owner && (account.is_primary || count == 1);
  }
}

fn resolved_owner(account: &Account, host_owner: Option<&str>) -> String {
  account.owner.clone().unwrap_or_else(|| host_owner.unwrap_or("").to_string())
}

fn is_account_name(name: &str) -> bool {
  let mut chars = name.chars();
  let Some(first) = chars.next() else { return false };
  first.is_ascii_lowercase() && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn account_for<'a>(table: &'a AccountTable, name: Option<&str>) -> Result<&'a Account, CliExit> {
  let name = match name {
    Some(name) => name,
    None => table
      .accounts
      .iter()
      .find(|account| account.is_default)
      .map(|account| account.name.as_str())
      .expect("the table always holds its default account"),
  };
  table.by_name.get(name).ok_or_else(|| {
    let names: Vec<&str> = table.accounts.iter().map(|account| account.name.as_str()).collect();
    CliExit::usage(format!("unknown account. Available accounts: {}", names.join(", ")))
  })
}

fn client_for(table: &AccountTable, account: Option<&Account>) -> Result<(String, String), CliExit> {
  let mut values = account.map(|account| &account.values).unwrap_or(&table.google);
  if !values.contains_key("client_id") {
    values = &table.google;
  }
  let client_id = values.get("client_id").and_then(Yaml::as_str);
  let client_secret = values.get("client_secret").and_then(Yaml::as_str);
  match (client_id, client_secret) {
    (Some(client_id), Some(client_secret)) if !client_id.trim().is_empty() && !client_secret.trim().is_empty() => {
      Ok((client_id.to_string(), client_secret.to_string()))
    }
    _ => Err(CliExit::error(format!(
      "OAuth client is not configured for {}",
      account.map(|account| account.name.as_str()).unwrap_or("new account")
    ))),
  }
}

fn is_pending(account: &Account) -> bool {
  match account.values.get("refresh_token") {
    Some(Yaml::String(refresh_token)) => refresh_token.trim().is_empty(),
    _ => true,
  }
}

// ---------------------------------------------------------------------------
// Google's OAuth calls
// ---------------------------------------------------------------------------

fn mint_access_token(table: &AccountTable, account: &Account, token_url: &str) -> Result<String, CliExit> {
  let refresh_token = match account.values.get("refresh_token") {
    Some(Yaml::String(refresh_token)) if !refresh_token.trim().is_empty() => refresh_token.clone(),
    _ => return Err(CliExit::error(format!("account {} is pending authorization", account.name))),
  };
  let (client_id, client_secret) = client_for(table, Some(account))?;
  let form = urlencode(&[
    ("client_id".to_string(), client_id),
    ("client_secret".to_string(), client_secret.clone()),
    ("refresh_token".to_string(), refresh_token.clone()),
    ("grant_type".to_string(), "refresh_token".to_string()),
  ]);
  let request = Request::new(reqwest::Method::POST, token_url)
    .header("Content-Type", "application/x-www-form-urlencoded")
    .body(form.into_bytes());
  let payload = open_json(&request, "Google token refresh", &[&refresh_token, &client_secret])?;
  required_response_string(&payload, "access_token", "Google token refresh")
}

fn profile_email(access_token: &str, profile_url: &str) -> Result<String, CliExit> {
  let request = Request::new(reqwest::Method::GET, profile_url).bearer(access_token);
  let payload = open_json(&request, "Gmail profile lookup", &[access_token])?;
  required_response_string(&payload, "emailAddress", "Gmail profile lookup")
}

fn open_json(request: &Request, operation: &str, redactions: &[&str]) -> Result<Map<String, Value>, CliExit> {
  let response =
    request::send(request, redactions).map_err(|_| CliExit::error(format!("{operation} failed: network error")))?;
  if !(200..300).contains(&response.status) {
    return Err(CliExit::error(format!("{operation} failed: HTTP {}", response.status)));
  }
  let payload: Value =
    serde_json::from_slice(&response.body).map_err(|_| CliExit::error(format!("{operation} returned invalid JSON")))?;
  let Value::Object(object) = payload else {
    return Err(CliExit::error(format!("{operation} returned an invalid response")));
  };
  if object.contains_key("error") {
    return Err(CliExit::error(format!("{operation} failed: Google returned an error")));
  }
  Ok(object)
}

fn required_response_string(payload: &Map<String, Value>, key: &str, operation: &str) -> Result<String, CliExit> {
  match payload.get(key).and_then(Value::as_str) {
    Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
    _ => Err(CliExit::error(format!("{operation} did not return {key}"))),
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::sync::mpsc::{self, Receiver};
  use std::thread;
  use std::time::Duration;

  use super::*;

  const DEFAULT_TABLE: &str =
    "google:\n  client_id: default-client\n  client_secret: default-secret\n  refresh_token: default-refresh\n";
  const TIMEOUT: Duration = Duration::from_secs(2);

  fn credentials_home(text: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(config::CREDENTIALS_FILENAME), text).unwrap();
    home
  }

  fn table(home: &tempfile::TempDir) -> AccountTable {
    load_table_at(&home.path().join(config::CREDENTIALS_FILENAME)).unwrap()
  }

  fn table_error(home: &tempfile::TempDir) -> CliExit {
    load_table_at(&home.path().join(config::CREDENTIALS_FILENAME)).unwrap_err()
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  /// One endpoint pair that no test reaches: a port with no listener.
  fn refused_endpoints() -> Endpoints {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    Endpoints {
      token_url: format!("http://127.0.0.1:{port}/token"),
      profile_url: format!("http://127.0.0.1:{port}/profile"),
    }
  }

  fn request_args(url: &str) -> RequestArgs {
    RequestArgs { method: "GET".to_string(), url: url.to_string(), data_file: None, json: None, header: Vec::new() }
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
      Endpoints { token_url: format!("{}/token", self.base), profile_url: format!("{}/profile", self.base) }
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

  fn token_response(token: &str) -> (u16, Vec<u8>) {
    (200, json!({"access_token": token}).to_string().into_bytes())
  }

  fn profile_response(email: &str) -> (u16, Vec<u8>) {
    (200, json!({"emailAddress": email}).to_string().into_bytes())
  }

  fn rows(output: &str) -> HashMap<String, Vec<String>> {
    output
      .lines()
      .map(|row| (row.split('\t').next().unwrap().to_string(), row.split('\t').map(str::to_string).collect()))
      .collect()
  }

  // The ported account-selection tests: `token` is now a request against a stub token endpoint and
  // a stub API server.

  #[test]
  fn request_uses_default_account() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);
    let stub = Stub::start(vec![token_response("default-access"), (200, br#"{"ok":true}"#.to_vec())]);

    let body = execute_request(&table, None, &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap();
    let form = stub.next();
    let api = stub.next();

    assert_eq!(body, br#"{"ok":true}"#.to_vec());
    assert!(form.ends_with(
      "client_id=default-client&client_secret=default-secret&refresh_token=default-refresh&grant_type=refresh_token"
    ));
    assert!(api.to_ascii_lowercase().contains("authorization: bearer default-access"));
    assert!(api.starts_with("GET /api HTTP/1.1"));
    assert!(!String::from_utf8_lossy(&body).contains("default-access"));
  }

  #[test]
  fn request_uses_named_account() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: personal-refresh\n"));
    let table = table(&home);
    let stub = Stub::start(vec![token_response("personal-access"), (200, b"{}".to_vec())]);

    execute_request(&table, Some("personal"), &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap();
    let form = stub.next();
    let api = stub.next();

    assert!(form.contains("client_id=default-client"));
    assert!(form.contains("refresh_token=personal-refresh"));
    assert!(api.to_ascii_lowercase().contains("authorization: bearer personal-access"));
  }

  #[test]
  fn unknown_account_lists_names() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: personal-refresh\n"));
    let table = table(&home);

    let error = execute_request(&table, Some("missing"), &request_args("http://127.0.0.1:1/api"), &refused_endpoints())
      .unwrap_err();
    let text = error_text(&error);

    assert_eq!(error.code, 2);
    assert!(text.contains("default"));
    assert!(text.contains("Available accounts:"));
  }

  #[test]
  fn empty_account_name_is_rejected() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);

    let error =
      execute_request(&table, Some(""), &request_args("http://127.0.0.1:1/api"), &refused_endpoints()).unwrap_err();

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("Available accounts: default"));
  }

  #[test]
  fn duplicate_account_names_are_rejected() {
    let home = credentials_home(
      "google:\n  name: personal\n  client_id: client\n  client_secret: secret\n  refresh_token: default-refresh\n\
       google_account_personal:\n  refresh_token: personal-refresh\n",
    );

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("duplicate Google account name"));
  }

  #[test]
  fn pending_account_cannot_mint_token() {
    let home = credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  description: personal mailbox\n"));
    let table = table(&home);

    let error =
      execute_request(&table, Some("personal"), &request_args("http://127.0.0.1:1/api"), &refused_endpoints())
        .unwrap_err();
    let text = error_text(&error);

    assert_eq!(error.code, 1);
    assert!(text.contains("personal"));
    assert!(text.contains("pending authorization"));
  }

  #[test]
  fn account_specific_client_is_used() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}google_account_personal:\n  client_id: personal-client\n  client_secret: personal-secret\n  refresh_token: personal-refresh\n"
    ));
    let table = table(&home);
    let stub = Stub::start(vec![token_response("personal-access"), (200, b"{}".to_vec())]);

    let body =
      execute_request(&table, Some("personal"), &request_args(&format!("{}/api", stub.base)), &stub.endpoints())
        .unwrap();
    let form = stub.next();
    let api = stub.next();

    assert!(form.contains("client_id=personal-client"));
    assert!(form.contains("client_secret=personal-secret"));
    assert!(!String::from_utf8_lossy(&body).contains("personal-secret"));
    assert!(!api.contains("personal-secret"));
  }

  #[test]
  fn list_marks_pending_and_does_not_print_tokens() {
    let home = credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  description: personal mailbox\n"));
    let table = table(&home);

    let output = list_output(&table, false, &refused_endpoints()).unwrap();

    assert!(output.contains("default\tdefault\tready"));
    assert!(output.contains("personal\t\tpending\t\tpersonal mailbox"));
    assert!(!output.contains("default-refresh"));
  }

  #[test]
  fn list_check_reads_email_without_printing_tokens() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: personal-refresh\n"));
    let table = table(&home);
    let stub = Stub::start(vec![
      token_response("default-access"),
      profile_response("work@example.invalid"),
      token_response("personal-access"),
      profile_response("person@example.invalid"),
    ]);

    let output = list_output(&table, true, &stub.endpoints()).unwrap();

    assert!(output.contains("work@example.invalid"));
    assert!(output.contains("person@example.invalid"));
    for secret in ["default-access", "personal-access", "default-refresh", "personal-refresh"] {
      assert!(!output.contains(secret), "the table output contains a token");
    }
  }

  #[test]
  fn list_check_keeps_pending_account_row_with_empty_address() {
    let home = credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  description: personal mailbox\n"));
    let table = table(&home);
    let stub = Stub::start(vec![token_response("default-access"), profile_response("work@example.invalid")]);

    let output = list_output(&table, true, &stub.endpoints()).unwrap();

    assert_eq!(
      output,
      "default\tdefault\tready\t\t\twork@example.invalid\npersonal\t\tpending\t\tpersonal mailbox\t\n"
    );
  }

  #[test]
  fn table_without_owner_keys_selects_same_accounts() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: personal-refresh\n"));
    let table = table(&home);

    let output = list_output(&table, false, &refused_endpoints()).unwrap();
    let rows = rows(&output);

    assert_eq!(rows["default"][1], "default");
    assert_eq!(rows["personal"][1], "");
    assert_eq!(rows["personal"][3], "");
  }

  #[test]
  fn section_without_owner_belongs_to_google_owner() {
    let home = credentials_home(
      "google:\n  client_id: default-client\n  client_secret: default-secret\n  refresh_token: default-refresh\n  owner: avery\n\
       google_account_shared:\n  refresh_token: shared-refresh\n\
       google_account_other:\n  refresh_token: other-refresh\n  owner: blake\n  primary: true\n",
    );
    let table = table(&home);

    let output = list_output(&table, false, &refused_endpoints()).unwrap();
    let rows = rows(&output);

    assert_eq!(rows["shared"][1], "");
    assert_eq!(rows["shared"][3], "");
    assert_eq!(rows["other"][1], "primary");
    assert_eq!(rows["other"][3], "blake");
  }

  #[test]
  fn two_people_each_have_one_primary_account() {
    let home = credentials_home(
      "google:\n  client_id: default-client\n  client_secret: default-secret\n  refresh_token: default-refresh\n  owner: avery\n\
       google_account_blake:\n  owner: blake\n",
    );
    let table = table(&home);
    let stub = Stub::start(vec![token_response("a"), profile_response("avery@example.invalid")]);

    let output = list_output(&table, true, &stub.endpoints()).unwrap();

    assert!(output.starts_with("default\tdefault\tready\tavery\t\tavery@example.invalid\n"));
    assert!(!output.contains("blake\tprimary\tready\tblake\t\tblake@example.invalid\n"));
    assert!(output.ends_with("blake\tprimary\tpending\tblake\t\t\n"));
  }

  #[test]
  fn primary_true_selects_one_of_several_sections() {
    let home = credentials_home(
      "google:\n  client_id: default-client\n  client_secret: default-secret\n  refresh_token: default-refresh\n  owner: avery\n\
       google_account_blake-a:\n  refresh_token: blake-a-refresh\n  owner: blake\n  primary: true\n\
       google_account_blake-b:\n  refresh_token: blake-b-refresh\n  owner: blake\n",
    );
    let table = table(&home);

    let output = list_output(&table, false, &refused_endpoints()).unwrap();
    let rows = rows(&output);

    assert_eq!(rows["blake-a"][1], "primary");
    assert_eq!(rows["blake-b"][1], "");
  }

  #[test]
  fn owner_must_be_one_line() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  owner: \"line one\\nline two\"\n"));

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("credentials.google_account_personal.owner must be one line"));
  }

  #[test]
  fn section_owner_without_google_owner_is_rejected() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: r\n  owner: blake\n"));

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(
      error_text(&error).contains("credentials.google_account_personal sets owner but credentials.google does not")
    );
  }

  #[test]
  fn primary_not_true_is_rejected() {
    let home = credentials_home(
      "google:\n  client_id: c\n  client_secret: s\n  refresh_token: r\n  owner: avery\n\
       google_account_personal:\n  refresh_token: personal-refresh\n  owner: blake\n  primary: yes\n",
    );

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("credentials.google_account_personal.primary must be true"));
  }

  #[test]
  fn primary_on_host_owner_section_is_rejected() {
    let home =
      credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: r\n  primary: true\n"));

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("cannot set primary"));
  }

  #[test]
  fn two_primary_accounts_for_one_person_are_rejected() {
    let home = credentials_home(
      "google:\n  client_id: c\n  client_secret: s\n  refresh_token: r\n  owner: avery\n\
       google_account_blake-a:\n  refresh_token: ra\n  owner: blake\n  primary: true\n\
       google_account_blake-b:\n  refresh_token: rb\n  owner: blake\n  primary: true\n",
    );

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("blake"));
    assert!(error_text(&error).contains("two accounts with primary"));
  }

  #[test]
  fn several_sections_without_primary_are_rejected() {
    let home = credentials_home(
      "google:\n  client_id: c\n  client_secret: s\n  refresh_token: r\n  owner: avery\n\
       google_account_blake-a:\n  refresh_token: ra\n  owner: blake\n\
       google_account_blake-b:\n  refresh_token: rb\n  owner: blake\n",
    );

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("blake"));
    assert!(error_text(&error).contains("none sets primary"));
  }

  #[test]
  fn empty_owner_is_rejected() {
    let home = credentials_home(&format!("{DEFAULT_TABLE}google_account_personal:\n  refresh_token: r\n  owner: ''\n"));

    let error = table_error(&home);

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("credentials.google_account_personal.owner must be one line"));
  }

  #[test]
  fn a_token_refresh_failure_names_the_operation_and_keeps_the_token_out() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);
    let stub = Stub::start(vec![(401, b"denied".to_vec())]);

    let error =
      execute_request(&table, None, &request_args(&format!("{}/api", stub.base)), &stub.endpoints()).unwrap_err();

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("Google token refresh failed: HTTP 401"));
    assert!(!error_text(&error).contains("default-refresh"));
  }
}
