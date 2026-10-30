//! `charliebot google`: the Google account table and authenticated Google requests.
//!
//! `request` mints the selected account's access token inside this process, sends one HTTP request
//! and prints the response body. `list` prints the account table. `consent-url` builds the account
//! owner's consent link and `add` exchanges the browser redirect for a refresh token and stores it
//! atomically in `credentials.yaml`. The token never reaches a command line, an output stream or a
//! run record.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::request::{self, Request, RequestArgs};
use charliebot_cli_common::urlencode::urlencode;
use charliebot_core::config;
use charliebot_core::credentials::write_credentials;
use charliebot_core::home;
use charliebot_core::paths;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as Yaml};

const GOOGLE_SECTION: &str = "google";
const ACCOUNT_PREFIX: &str = "google_account_";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const PROFILE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";
const REDIRECT_URI: &str = "http://localhost";
const SCOPES: &str = "https://www.googleapis.com/auth/spreadsheets https://www.googleapis.com/auth/documents \
  https://www.googleapis.com/auth/drive.file https://www.googleapis.com/auth/gmail.readonly \
  https://www.googleapis.com/auth/calendar.readonly";

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
  ConsentUrl(ConsentUrlArgs),
  Add(AddArgs),
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

#[derive(Args)]
struct ConsentUrlArgs {
  /// The account name the consent state carries.
  name: String,
  /// Pre-fill the consent page's email address when the account's address is known.
  #[arg(long)]
  email: Option<String>,
}

#[derive(Args)]
struct AddArgs {
  /// The account name whose section receives the refresh token.
  name: String,
  /// The email address the consenting account must answer with.
  #[arg(long)]
  email: String,
  /// The full browser URL after the redirect to `http://localhost`.
  #[arg(long = "redirect-url", value_name = "URL")]
  redirect_url: String,
  /// One-line description stored with the account section.
  #[arg(long)]
  description: Option<String>,
  /// The person the account belongs to, required when `credentials.google` sets `owner`.
  #[arg(long)]
  owner: Option<String>,
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
    GoogleCommand::ConsentUrl(args) => {
      let url = consent_url(&table, &args.name, args.email.as_deref())?;
      request::write_stdout(format!("{url}\n").as_bytes())
    }
    GoogleCommand::Add(args) => {
      let email = add_account(
        &table,
        &args.name,
        &args.email,
        &args.redirect_url,
        args.description.as_deref(),
        args.owner.as_deref(),
        &endpoints,
      )?;
      request::write_stdout(add_output(&args.name, &email).as_bytes())
    }
  }
}

/// The `add` verb's stdout line.
fn add_output(name: &str, email: &str) -> String {
  format!("{name}: {email}\n")
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
  /// The file the table was read from; `add` writes its edit back to it.
  path: PathBuf,
  raw_bytes: Vec<u8>,
  raw_text: String,
  /// The parsed document, with repeated Google keys resolved the way PyYAML does (last wins).
  document: Mapping,
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
  table_from_bytes(path.to_path_buf(), &raw_bytes)
}

fn table_from_bytes(path: PathBuf, raw_bytes: &[u8]) -> Result<AccountTable, CliExit> {
  let raw_text =
    std::str::from_utf8(raw_bytes).map_err(|_| CliExit::usage("credentials.yaml must use UTF-8"))?.to_string();
  if let Some(section) = duplicate_google_section(&raw_text) {
    return Err(CliExit::usage(format!("credentials.{section} appears more than once")));
  }
  let normalized = drop_repeated_google_keys(&raw_text);
  let parsed =
    serde_yaml::from_str::<Yaml>(&normalized).map_err(|_| CliExit::usage("credentials.yaml is not valid YAML"))?;
  let Yaml::Mapping(document) = parsed else {
    return Err(CliExit::usage("credentials.yaml must contain a mapping"));
  };
  if document.keys().any(|key| key.as_str().is_none()) {
    return Err(CliExit::usage("credentials.yaml has an invalid section name"));
  }
  if !document.contains_key(GOOGLE_SECTION) {
    return Err(CliExit::usage("credentials.google is missing"));
  }
  let google = google_values(&document, GOOGLE_SECTION)?;
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
  for (key, _) in &document {
    let Some(section) = key.as_str() else { continue };
    let Some(name) = section.strip_prefix(ACCOUNT_PREFIX) else { continue };
    if !is_account_name(name) {
      return Err(CliExit::usage(format!("credentials.{section} has an invalid account name")));
    }
    let values = google_values(&document, section)?;
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
  Ok(AccountTable { path, raw_bytes: raw_bytes.to_vec(), raw_text, document, accounts, by_name, google })
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

/// PyYAML's `safe_load` keeps the last of repeated keys, and `add` relies on that when it refuses a
/// repeated `refresh_token` line with its own message; serde_yaml refuses every repeated key, so
/// repeated key lines inside Google sections are dropped (last wins) before the parse. Keys in the
/// other sections keep serde_yaml's refusal.
fn drop_repeated_google_keys(raw_text: &str) -> String {
  let lines = split_source_lines(raw_text);
  let mut dropped: HashSet<usize> = HashSet::new();
  let mut seen: HashMap<String, usize> = HashMap::new();
  let mut section_is_google = false;
  let mut inner_indent: Option<usize> = None;
  for (index, line) in lines.iter().enumerate() {
    if is_blank_line(line.text) || is_comment_line(line.text) {
      continue;
    }
    let indent = line_indent(line.text);
    if indent == 0 {
      seen.clear();
      inner_indent = None;
      section_is_google =
        mapping_key(line.text).is_some_and(|key| key == GOOGLE_SECTION || key.starts_with(ACCOUNT_PREFIX));
      continue;
    }
    let body_indent = *inner_indent.get_or_insert(indent);
    if indent != body_indent {
      continue;
    }
    let Some(key) = mapping_key(line.text) else { continue };
    if !section_is_google {
      continue;
    }
    if let Some(previous) = seen.insert(key.to_string(), index) {
      dropped.insert(previous);
    }
  }
  lines
    .iter()
    .enumerate()
    .filter(|(index, _)| !dropped.contains(index))
    .map(|(_, line)| format!("{}{}", line.text, line.ending))
    .collect()
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
// The consent-url and add verbs
// ---------------------------------------------------------------------------

/// The consent page URL for one account: the state names the account, and an optional login hint
/// pre-fills the email address.
fn consent_url(table: &AccountTable, name: &str, email: Option<&str>) -> Result<String, CliExit> {
  if !is_account_name(name) {
    return Err(CliExit::usage("account name must match [a-z][a-z0-9-]*"));
  }
  if email.is_some_and(|email| email.trim().is_empty()) {
    return Err(CliExit::usage("email address must not be empty"));
  }
  let (client_id, _) = client_for(table, table.by_name.get(name))?;
  let mut parameters = vec![
    ("response_type".to_string(), "code".to_string()),
    ("client_id".to_string(), client_id),
    ("redirect_uri".to_string(), REDIRECT_URI.to_string()),
    ("scope".to_string(), SCOPES.to_string()),
    ("access_type".to_string(), "offline".to_string()),
    ("prompt".to_string(), "consent select_account".to_string()),
    ("state".to_string(), name.to_string()),
  ];
  if let Some(email) = email {
    parameters.push(("login_hint".to_string(), email.trim().to_string()));
  }
  Ok(format!("{AUTH_URL}?{}", urlencode(&parameters)))
}

/// The section name an account's values live under.
fn account_section(account: &Account) -> String {
  if account.is_default { GOOGLE_SECTION.to_string() } else { format!("{ACCOUNT_PREFIX}{}", account.name) }
}

/// `add`: exchange the browser redirect's code for a refresh token, confirm the consenting
/// account's email address and write the account section.
fn add_account(
  table: &AccountTable,
  name: &str,
  email: &str,
  redirect_url: &str,
  description: Option<&str>,
  owner: Option<&str>,
  endpoints: &Endpoints,
) -> Result<String, CliExit> {
  if !is_account_name(name) {
    return Err(CliExit::usage("account name must match [a-z][a-z0-9-]*"));
  }
  if email.trim().is_empty() {
    return Err(CliExit::usage("email address must not be empty"));
  }
  if description.is_some_and(|line| line.trim().is_empty() || line.contains(['\n', '\r'])) {
    return Err(CliExit::usage("description must be one line"));
  }
  if owner.is_some_and(|line| line.trim().is_empty() || line.contains(['\n', '\r'])) {
    return Err(CliExit::usage("owner must be one line"));
  }
  let google_owner = validate_owner(GOOGLE_SECTION, &table.google)?;
  if google_owner.is_some() && owner.is_none() {
    return Err(CliExit::usage("credentials.google sets owner; pass --owner"));
  }
  let account = table.by_name.get(name).cloned().unwrap_or(Account {
    name: name.to_string(),
    is_default: false,
    values: Mapping::new(),
    owner: None,
    description: String::new(),
    is_primary: false,
    person_primary: false,
  });
  // Reject a table that the write would leave invalid before the consent code is spent.
  let (validated_bytes, _) = updated_content(table, &account, "validation-only", description, owner)?;
  table_from_bytes(table.path.clone(), &validated_bytes)?;
  let code = redirect_code(redirect_url, name)?;
  let (client_id, client_secret) = client_for(table, Some(&account))?;
  let form = urlencode(&[
    ("client_id".to_string(), client_id.clone()),
    ("client_secret".to_string(), client_secret.clone()),
    ("code".to_string(), code.clone()),
    ("grant_type".to_string(), "authorization_code".to_string()),
    ("redirect_uri".to_string(), REDIRECT_URI.to_string()),
  ]);
  let exchange = Request::new(reqwest::Method::POST, &endpoints.token_url)
    .header("Content-Type", "application/x-www-form-urlencoded")
    .body(form.into_bytes());
  let payload = open_json(&exchange, "Google authorization-code exchange", &[&client_secret, &code])?;
  let refresh_token = required_response_string(&payload, "refresh_token", "Google authorization-code exchange")?;
  let access_token = required_response_string(&payload, "access_token", "Google authorization-code exchange")?;
  let actual_email = profile_email(&access_token, &endpoints.profile_url)?;
  // Python `str.casefold`; the compared addresses are ASCII.
  if !actual_email.eq_ignore_ascii_case(email.trim()) {
    return Err(CliExit::error(format!("email mismatch: Google returned {actual_email}")));
  }
  write_refresh_token(table, &account, &refresh_token, description, owner)?;
  Ok(actual_email)
}

/// The authorization code a browser redirect carries: the query must hold the account's state
/// exactly once and one non-blank `code`.
fn redirect_code(redirect_url: &str, name: &str) -> Result<String, CliExit> {
  let query = urlsplit_query(redirect_url).map_err(|_| CliExit::error("redirect URL is invalid"))?;
  let values = |key: &str| -> Vec<&str> {
    query.iter().filter(|(parameter, _)| parameter == key).map(|(_, value)| value.as_str()).collect()
  };
  let states = values("state");
  if states.len() != 1 || states[0] != name {
    return Err(CliExit::error("redirect state does not match the account name"));
  }
  let codes = values("code");
  if codes.len() != 1 || codes[0].trim().is_empty() {
    return Err(CliExit::error("redirect URL does not contain an authorization code"));
  }
  Ok(codes[0].to_string())
}

/// `urllib.parse.parse_qs(urllib.parse.urlsplit(url).query, keep_blank_values=True)`: one
/// `(key, value)` pair per query parameter, in order, `+` and percent escapes decoded on both
/// sides. `urlsplit`'s unbalanced-bracket refusal is the only URL error the script can hit.
fn urlsplit_query(url: &str) -> Result<Vec<(String, String)>, ()> {
  let netloc_end = url.find(['/', '?', '#']).unwrap_or(url.len());
  let netloc = &url[..netloc_end];
  if netloc.matches('[').count() != netloc.matches(']').count() {
    return Err(());
  }
  let without_fragment = url.split('#').next().expect("split always yields a first part");
  let Some((_, query)) = without_fragment.split_once('?') else { return Ok(Vec::new()) };
  Ok(
    query
      .split('&')
      .filter(|pair| !pair.is_empty())
      .map(|pair| match pair.split_once('=') {
        Some((key, value)) => (unquote_plus(key), unquote_plus(value)),
        None => (unquote_plus(pair), String::new()),
      })
      .collect(),
  )
}

/// `urllib.parse.unquote_plus`: `+` becomes a space, then `%XX` bytes decode as UTF-8 with
/// undecodable bytes replaced, matching the default `errors='replace'`.
fn unquote_plus(text: &str) -> String {
  fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
      b'0'..=b'9' => Some(byte - b'0'),
      b'a'..=b'f' => Some(byte - b'a' + 10),
      b'A'..=b'F' => Some(byte - b'A' + 10),
      _ => None,
    }
  }
  let source = text.as_bytes();
  let mut bytes = Vec::with_capacity(source.len());
  let mut index = 0;
  while index < source.len() {
    match source[index] {
      b'+' => bytes.push(b' '),
      b'%' if index + 2 < source.len() => match (hex_digit(source[index + 1]), hex_digit(source[index + 2])) {
        (Some(high), Some(low)) => {
          bytes.push(high * 16 + low);
          index += 2;
        }
        _ => bytes.push(b'%'),
      },
      byte => bytes.push(byte),
    }
    index += 1;
  }
  String::from_utf8_lossy(&bytes).into_owned()
}

/// The bytes `add` writes for one account: the edited or appended section, plus the document the
/// written file must parse back to.
fn updated_content(
  table: &AccountTable,
  account: &Account,
  refresh_token: &str,
  description: Option<&str>,
  owner: Option<&str>,
) -> Result<(Vec<u8>, Mapping), CliExit> {
  let mut write_values: Vec<(&str, &str)> = vec![("refresh_token", refresh_token)];
  if let Some(description) = description {
    write_values.push(("description", description));
  }
  if let Some(owner) = owner {
    write_values.push(("owner", owner));
  }
  let section = account_section(account);
  let mut expected = table.document.clone();
  if expected.contains_key(Yaml::String(section.clone())) {
    let plan =
      section_edit_plan(&table.raw_text, &section, &write_values.iter().map(|(key, _)| *key).collect::<Vec<_>>())?;
    let mut lines: Vec<(String, String)> = split_source_lines(&table.raw_text)
      .into_iter()
      .map(|line| (line.text.to_string(), line.ending.to_string()))
      .collect();
    for (key, line_number) in &plan.replacements {
      let value = write_values
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| *value)
        .unwrap_or_else(|| panic!("the section plan only names keys of the write: {key}"));
      let ending = lines[*line_number].1.to_string();
      lines[*line_number] = (format!("{}{}: {}", plan.indentation, key, dump_scalar(value, 0, false, None)), ending);
    }
    let inserted: Vec<&(&str, &str)> =
      write_values.iter().filter(|(key, _)| !plan.replacements.iter().any(|(planned, _)| planned == *key)).collect();
    if !inserted.is_empty() {
      let (following, last) = match lines.get(plan.insert_line) {
        Some(following) => (following.1.clone(), plan.insert_line),
        None => (lines.last().expect("the table holds its section lines").1.clone(), lines.len() - 1),
      };
      let ending = if following.ends_with("\r\n") { "\r\n".to_string() } else { "\n".to_string() };
      if plan.insert_line >= lines.len() && !lines[last].1.ends_with(['\n', '\r']) {
        lines[last].1.push('\n');
      }
      for (offset, (key, value)) in inserted.iter().enumerate() {
        let start_column = plan.indentation.chars().count() + key.chars().count() + 2;
        lines.insert(
          plan.insert_line + offset,
          (format!("{}{}: {}", plan.indentation, key, dump_scalar(value, start_column, false, None)), ending.clone()),
        );
      }
    }
    let slot = section_mapping_mut(&mut expected, &section);
    for (key, value) in write_values {
      slot.insert(Yaml::String(key.to_string()), Yaml::String(value.to_string()));
    }
    return Ok((
      lines.into_iter().map(|(text, ending)| format!("{text}{ending}")).collect::<String>().into_bytes(),
      expected,
    ));
  }
  let appended = dump_section(&section, &write_values);
  let separator = if table.raw_bytes.is_empty() || table.raw_bytes.ends_with(b"\n") || table.raw_bytes.ends_with(b"\r")
  {
    ""
  } else {
    "\n"
  };
  let mut bytes = table.raw_bytes.clone();
  bytes.extend_from_slice(separator.as_bytes());
  bytes.extend_from_slice(appended.as_bytes());
  expected.insert(
    Yaml::String(section),
    Yaml::Mapping(
      write_values
        .iter()
        .map(|(key, value)| (Yaml::String((*key).to_string()), Yaml::String((*value).to_string())))
        .collect(),
    ),
  );
  Ok((bytes, expected))
}

/// The written section's mapping inside the expected document; the loaded table validated every
/// Google section as a mapping.
fn section_mapping_mut<'a>(sections: &'a mut Mapping, section: &str) -> &'a mut Mapping {
  match sections.get_mut(Yaml::String(section.to_string())) {
    Some(Yaml::Mapping(values)) => values,
    _ => unreachable!("the loaded table validated credentials.{section} as a mapping"),
  }
}

/// Exchange `add`'s validated edit for a credentials file: every byte outside the edited lines
/// stays, the file mode stays, and a failure leaves the file unchanged.
fn write_refresh_token(
  table: &AccountTable,
  account: &Account,
  refresh_token: &str,
  description: Option<&str>,
  owner: Option<&str>,
) -> Result<(), CliExit> {
  let (new_bytes, expected) = updated_content(table, account, refresh_token, description, owner)?;
  // The Python script writes the temporary file and reparses it before the rename; the same check
  // on the bytes in memory refuses an edit that would not read back as the expected document.
  let written = std::str::from_utf8(&new_bytes).expect("the edit only rearranges UTF-8 lines");
  let reparsed = serde_yaml::from_str::<Yaml>(&drop_repeated_google_keys(written))
    .map_err(|_| CliExit::error("temporary credentials file is not valid YAML"))?;
  let Yaml::Mapping(reparsed) = reparsed else {
    return Err(CliExit::error("temporary credentials file must contain a mapping"));
  };
  if reparsed != expected {
    return Err(CliExit::error("temporary credentials file did not match the expected change"));
  }
  write_credentials(table.path.parent().expect("a credentials path always names a parent directory"), &new_bytes)
    .map_err(|error| CliExit::error(error.to_string()))
}

/// One physical line of the credentials file: its text without the break, and the break (`\n`,
/// `\r\n` or `\r`).
struct SourceLine<'a> {
  text: &'a str,
  ending: &'a str,
}

/// Split the file into physical lines the way the script's `splitlines(keepends=True)` does for the
/// breaks YAML recognizes.
fn split_source_lines(text: &str) -> Vec<SourceLine<'_>> {
  let mut lines = Vec::new();
  let mut rest = text;
  while !rest.is_empty() {
    let Some(position) = rest.find(['\n', '\r']) else {
      lines.push(SourceLine { text: rest, ending: "" });
      break;
    };
    let double = rest.as_bytes()[position] == b'\r' && rest.as_bytes().get(position + 1) == Some(&b'\n');
    let ending_length = if double { 2 } else { 1 };
    let (line, after) = rest.split_at(position);
    lines.push(SourceLine { text: line, ending: &after[..ending_length] });
    rest = &after[ending_length..];
  }
  lines
}

fn line_indent(text: &str) -> usize {
  text.chars().take_while(|ch| *ch == ' ' || *ch == '\t').count()
}

fn is_blank_line(text: &str) -> bool {
  text.chars().all(|ch| ch == ' ' || ch == '\t')
}

fn is_comment_line(text: &str) -> bool {
  text[line_indent(text)..].starts_with('#')
}

/// The plain mapping key of a line that carries one: the text before the first `:` that a space, a
/// tab or the end of the line follows.
fn mapping_key(text: &str) -> Option<&str> {
  let bytes = text.as_bytes();
  let index = (0..bytes.len())
    .find(|&index| bytes[index] == b':' && (index + 1 == bytes.len() || matches!(bytes[index + 1], b' ' | b'\t')))?;
  Some(&text[..index])
}

/// The value after a key's colon: inline when anything but spaces and a comment follows it.
fn inline_value(text: &str, key_length: usize) -> bool {
  let value = text[key_length + 1..].trim_matches([' ', '\t']);
  !value.is_empty() && !value.starts_with('#')
}

struct SectionPlan {
  /// Each targeted key's line, in file order.
  replacements: Vec<(String, usize)>,
  /// The line untargeted keys insert at.
  insert_line: usize,
  /// The section's key indentation.
  indentation: String,
}

/// The script's `_section_edit_plan`: each targeted key's line inside one block-mapping section,
/// the line the other keys insert at, and the section's key indentation.
fn section_edit_plan(raw_text: &str, section_name: &str, keys: &[&str]) -> Result<SectionPlan, CliExit> {
  let lines = split_source_lines(raw_text);
  let section_lines: Vec<usize> = lines
    .iter()
    .enumerate()
    .filter(|(_, line)| line_indent(line.text) == 0 && mapping_key(line.text) == Some(section_name))
    .map(|(index, _)| index)
    .collect();
  if section_lines.len() != 1 {
    return Err(CliExit::error(format!("credentials.{section_name} cannot be updated")));
  }
  let section_text = lines[section_lines[0]].text;
  if inline_value(section_text, section_name.len()) {
    return Err(CliExit::error(format!("credentials.{section_name} must be a block mapping")));
  }
  let mut cursor = section_lines[0] + 1;
  while cursor < lines.len() && (is_blank_line(lines[cursor].text) || is_comment_line(lines[cursor].text)) {
    cursor += 1;
  }
  if cursor == lines.len() {
    return Err(CliExit::error(format!("credentials.{section_name} cannot be updated")));
  }
  let indentation = lines[cursor].text[..line_indent(lines[cursor].text)].to_string();
  let inner = indentation.len();
  let mut replacements: Vec<(String, usize)> = Vec::new();
  let mut last_content = cursor;
  let mut last_value_inline = false;
  while cursor < lines.len() {
    let line = &lines[cursor];
    if is_blank_line(line.text) || is_comment_line(line.text) {
      cursor += 1;
      continue;
    }
    let indent = line_indent(line.text);
    if indent < inner {
      break;
    }
    if indent == inner
      && let Some(key) = mapping_key(&line.text[indent..])
    {
      if keys.contains(&key) {
        if replacements.iter().any(|(seen, _)| seen == key) {
          return Err(CliExit::error(format!("credentials.{section_name} contains more than one {key} line")));
        }
        replacements.push((key.to_string(), cursor));
      }
      last_value_inline = inline_value(line.text, indent + key.len());
      last_content = cursor;
      cursor += 1;
      continue;
    }
    last_content = cursor;
    cursor += 1;
  }
  // A scalar value ends on its own content line, so the insertion goes one line past it; a nested
  // block ends at the next token, and blank or comment lines above that token stay above the
  // insertion, matching the script's node end marks.
  let insert_line = if last_value_inline {
    last_content + 1
  } else {
    let mut next = last_content + 1;
    while next < lines.len() && (is_blank_line(lines[next].text) || is_comment_line(lines[next].text)) {
      next += 1;
    }
    next
  };
  Ok(SectionPlan { replacements, insert_line, indentation })
}

// ---------------------------------------------------------------------------
// The PyYAML emitter's block-mapping scalars
// ---------------------------------------------------------------------------

/// Render one string the way `yaml.safe_dump` prints it as a block mapping's value: plain when the
/// text reads back as a string, single-quoted when plain style would change the value, double-
/// quoted with escapes when the text needs them. `start_column` is the column the value starts at;
/// `fold_indent` folds long values at single spaces once the column passes the emitter's
/// 80-column width (the script's `width=1_000_000` dumps pass `None` and stay on one line).
fn dump_scalar(value: &str, start_column: usize, allow_unicode: bool, fold_indent: Option<&str>) -> String {
  let analysis = analyze_scalar(value, allow_unicode);
  if !analysis.special && !analysis.edge_space && !analysis.block_indicators && !plain_resolves_to_non_string(value) {
    return write_folded(value, start_column, fold_indent, false);
  }
  if !analysis.special {
    return format!("'{}'", write_folded(value, start_column + 1, fold_indent, true));
  }
  dump_double_quoted(value, allow_unicode)
}

/// `Emitter.analyze_scalar` for the single-line values `add` writes: the flags that forbid plain
/// style. A line break routes the value to double-quoted escapes; the block-mapping values are one
/// line.
fn analyze_scalar(value: &str, allow_unicode: bool) -> AnalyzedScalar {
  let mut analyzed = AnalyzedScalar::default();
  if value.is_empty() {
    return analyzed;
  }
  analyzed.block_indicators = value.starts_with("---") || value.starts_with("...");
  let chars: Vec<char> = value.chars().collect();
  let whitespace_like = |ch: char| matches!(ch, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
  for (index, &ch) in chars.iter().enumerate() {
    let followed_by_whitespace = index + 1 >= chars.len() || whitespace_like(chars[index + 1]);
    let preceded_by_whitespace = index == 0 || whitespace_like(chars[index - 1]);
    if index == 0 {
      if "#,[]{}&*!|>'\"%@`".contains(ch) {
        analyzed.block_indicators = true;
      }
      if (ch == '?' || ch == ':') && followed_by_whitespace {
        analyzed.block_indicators = true;
      }
      if ch == '-' && followed_by_whitespace {
        analyzed.block_indicators = true;
      }
    } else if (ch == ':' && followed_by_whitespace) || (ch == '#' && preceded_by_whitespace) {
      analyzed.block_indicators = true;
    }
    if ch != '\n' && !('\u{20}'..='\u{7E}').contains(&ch) {
      let unicode = (ch == '\u{85}'
        || ('\u{A0}'..='\u{D7FF}').contains(&ch)
        || ('\u{E000}'..='\u{FFFD}').contains(&ch)
        || ('\u{10000}'..'\u{10FFFF}').contains(&ch))
        && ch != '\u{FEFF}';
      if !unicode || !allow_unicode {
        analyzed.special = true;
      }
    }
    if ch == ' ' && (index == 0 || index == chars.len() - 1) {
      analyzed.edge_space = true;
    }
  }
  analyzed
}

#[derive(Default)]
struct AnalyzedScalar {
  /// The value needs double-quoted escapes (control characters, or non-ASCII with `allow_unicode`
  /// off).
  special: bool,
  /// A space at either end forbids plain style.
  edge_space: bool,
  /// An indicator character forbids plain style.
  block_indicators: bool,
}

/// Whether PyYAML's 1.1 implicit resolution would read the plain form back as something other than
/// a string: null, bool, int, float, timestamp, merge or value.
fn plain_resolves_to_non_string(value: &str) -> bool {
  const BOOLS: [&str; 18] = [
    "yes", "Yes", "YES", "no", "No", "NO", "true", "True", "TRUE", "false", "False", "FALSE", "on", "On", "ON", "off",
    "Off", "OFF",
  ];
  const NULLS: [&str; 4] = ["~", "null", "Null", "NULL"];
  if value.is_empty() || matches!(value, " " | "<<" | "=") {
    return true;
  }
  if BOOLS.contains(&value) || NULLS.contains(&value) {
    return true;
  }
  static INT: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
  static FLOAT: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
  static TIMESTAMP: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
  let int =
    INT.get_or_init(|| fancy_regex::Regex::new(r"^[-+]?0b[0-1_]+$|^[-+]?0[0-7_]+$|^[-+]?(0|[1-9][0-9_]*)$|^[-+]?0x[0-9a-fA-F_]+$|^[-+]?[1-9][0-9_]*(:[0-5]?[0-9])+$|^[-+]?(0|[1-9][0-9_]*)(:[0-5]?[0-9])+$").expect("the int pattern is valid"));
  let float = FLOAT.get_or_init(|| {
    fancy_regex::Regex::new(
      r"^[-+]?[0-9][0-9_]*\.[0-9_]*([eE][-+][0-9]+)?$|^\.[0-9][0-9_]*([eE][-+][0-9]+)?$|^[-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*$|^[-+]?\.(inf|Inf|INF)$|^\.(nan|NaN|NAN)$",
    )
    .expect("the float pattern is valid")
  });
  let timestamp = TIMESTAMP.get_or_init(|| {
    fancy_regex::Regex::new(
      r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$|^[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}([Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(\.[0-9]*)?([ \t]*(Z|[-+][0-9]{1,2}(:[0-9]{2})?))?$",
    )
    .expect("the timestamp pattern is valid")
  });
  [int, float, timestamp].iter().any(|pattern| {
    pattern.is_match(value).unwrap_or_else(|error| panic!("the resolver patterns never backtrack: {error}"))
  })
}

/// `Emitter.write_plain` and `Emitter.write_single_quoted` over one line: chunks between spaces,
/// with a single interior space replaced by a line break plus the fold indent once the column
/// passes the emitter's 80-column fold width. `single_quoted` doubles each quote, counts the
/// value's opening quote and skips folding at a leading or trailing space.
fn write_folded(value: &str, start_column: usize, fold_indent: Option<&str>, single_quoted: bool) -> String {
  const FOLD_WIDTH: usize = 80;
  let emit = |folded: &mut String, column: &mut usize, text: &str, single_quoted: bool| {
    let written = if single_quoted { text.replace('\'', "''") } else { text.to_string() };
    *column += written.chars().count();
    folded.push_str(&written);
  };
  let mut folded = String::new();
  let mut column = start_column + usize::from(single_quoted);
  let chars: Vec<char> = value.chars().collect();
  let mut start = 0;
  let mut spaces = false;
  let mut index = 0;
  while index <= chars.len() {
    let ch = chars.get(index).copied();
    if spaces && (ch.is_none() || ch != Some(' ')) {
      let run_end = index;
      let interior = start != 0 && run_end != chars.len();
      if let Some(indent) = fold_indent.filter(|_| run_end - start == 1 && column > FOLD_WIDTH && interior) {
        folded.push('\n');
        folded.push_str(indent);
        column = indent.chars().count();
      } else {
        let run: String = chars[start..run_end].iter().collect();
        emit(&mut folded, &mut column, &run, single_quoted);
      }
      start = run_end;
      spaces = false;
    } else if !spaces && (ch.is_none() || ch == Some(' ')) {
      let chunk: String = chars[start..index].iter().collect();
      emit(&mut folded, &mut column, &chunk, single_quoted);
      start = index;
      spaces = ch == Some(' ');
    }
    index += 1;
  }
  folded
}

/// `yaml.safe_dump({section: values}, sort_keys=False, allow_unicode=True)`: the section key at the
/// margin, each pair on its own line at indent 2, long values folded with continuation indent 4.
fn dump_section(section: &str, write_values: &[(&str, &str)]) -> String {
  let mut dumped = format!("{section}:\n");
  for (key, value) in write_values {
    let start_column = 2 + key.chars().count() + 2;
    dumped.push_str(&format!("  {key}: {}\n", dump_scalar(value, start_column, true, Some("    "))));
  }
  dumped
}

/// `Emitter.write_double_quoted`: the ESCAPE_REPLACEMENTS forms, then `\xXX`, `\uXXXX` and
/// `\UXXXXXXXX` for everything the writer does not print literally. Folding is the emitter's
/// escaped-space behavior and stays unimplemented; the values `add` writes are one line.
fn dump_double_quoted(value: &str, allow_unicode: bool) -> String {
  let mut quoted = String::from("\"");
  for ch in value.chars() {
    let literal = ch != '"'
      && ch != '\\'
      && !matches!(ch, '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}')
      && (('\u{20}'..='\u{7E}').contains(&ch)
        || (allow_unicode && (('\u{A0}'..='\u{D7FF}').contains(&ch) || ('\u{E000}'..='\u{FFFD}').contains(&ch))));
    if literal {
      quoted.push(ch);
      continue;
    }
    let replacement = match ch {
      '\0' => "\\0".to_string(),
      '\u{7}' => "\\a".to_string(),
      '\u{8}' => "\\b".to_string(),
      '\t' => "\\t".to_string(),
      '\n' => "\\n".to_string(),
      '\u{b}' => "\\v".to_string(),
      '\u{c}' => "\\f".to_string(),
      '\r' => "\\r".to_string(),
      '\u{1b}' => "\\e".to_string(),
      '"' => "\\\"".to_string(),
      '\\' => "\\\\".to_string(),
      '\u{85}' => "\\N".to_string(),
      '\u{a0}' => "\\_".to_string(),
      '\u{2028}' => "\\L".to_string(),
      '\u{2029}' => "\\P".to_string(),
      other => {
        let code = other as u32;
        if code <= 0xFF {
          format!("\\x{code:02X}")
        } else if code <= 0xFFFF {
          format!("\\u{code:04X}")
        } else {
          format!("\\U{code:08X}")
        }
      }
    };
    quoted.push_str(&replacement);
  }
  quoted.push('"');
  quoted
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

  // The ported consent-url and add tests: the token exchange and the profile read run against the
  // stub endpoints, and the file-bytes expectations were recorded from the Python script at the
  // port's base commit.

  /// The exact file `add` writes when it appends a section to the default table.
  const APPEND_REFERENCE_BYTES: &str = concat!(
    "google:\n",
    "  client_id: default-client\n",
    "  client_secret: default-secret\n",
    "  refresh_token: default-refresh\n",
    "google_account_personal:\n",
    "  refresh_token: new-refresh\n",
    "  description: 'personal: mailbox'\n",
  );
  /// The exact file `add` writes when it inserts into the pending section between two sections.
  const INSERT_REFERENCE_BYTES: &str = concat!(
    "google:\n",
    "  client_id: default-client\n",
    "  client_secret: default-secret\n",
    "  refresh_token: default-refresh\n",
    "\n",
    "google_account_personal:\n",
    "  client_id: personal-client\n",
    "  client_secret: personal-secret\n",
    "  refresh_token: new-refresh\n",
    "  description: personal mailbox\n",
    "\n",
    "google_account_after:\n",
    "  refresh_token: after-refresh\n",
  );
  /// The exact URL `consent-url` prints for the default account of the default table.
  const CONSENT_URL_REFERENCE: &str = concat!(
    "https://accounts.google.com/o/oauth2/v2/auth",
    "?response_type=code",
    "&client_id=default-client",
    "&redirect_uri=http%3A%2F%2Flocalhost",
    "&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fspreadsheets",
    "+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdocuments",
    "+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file",
    "+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.readonly",
    "+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.readonly",
    "&access_type=offline",
    "&prompt=consent+select_account",
    "&state=default",
    "&login_hint=person%40example.invalid",
  );

  const LONG_DESCRIPTION: &str = "Shared family mailbox for travel bookings, school notices, receipts, \
appointments and the tax paperwork of the household";

  fn credentials_path(home: &tempfile::TempDir) -> PathBuf {
    home.path().join(config::CREDENTIALS_FILENAME)
  }

  fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
  }

  fn set_file_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
  }

  /// The `.credentials.yaml.*.tmp` files a failed or interrupted write would leave behind.
  fn temporary_files(home: &tempfile::TempDir) -> Vec<String> {
    std::fs::read_dir(home.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .filter(|name| name.starts_with(".credentials.yaml.") && name.ends_with(".tmp"))
      .collect()
  }

  fn exchange_response(access_token: &str, refresh_token: &str) -> (u16, Vec<u8>) {
    (200, json!({"access_token": access_token, "refresh_token": refresh_token}).to_string().into_bytes())
  }

  fn add(
    home: &tempfile::TempDir,
    name: &str,
    email: &str,
    redirect_url: &str,
    description: Option<&str>,
    owner: Option<&str>,
    endpoints: &Endpoints,
  ) -> Result<String, CliExit> {
    add_account(&table(home), name, email, redirect_url, description, owner, endpoints)
  }

  /// The form body the stub's recorded exchange request carries.
  fn form_fields(request: &[u8]) -> Vec<(String, String)> {
    let body = String::from_utf8_lossy(request);
    let form = body.split("\r\n\r\n").nth(1).unwrap_or("");
    form
      .split('&')
      .map(|pair| match pair.split_once('=') {
        Some((key, value)) => (key.to_string(), value.to_string()),
        None => (pair.to_string(), String::new()),
      })
      .collect()
  }

  fn consent_query(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').expect("the consent URL carries a query");
    query
      .split('&')
      .map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (unquote_plus(key), unquote_plus(value))
      })
      .collect()
  }

  fn parameter<'a>(query: &'a [(String, String)], name: &str) -> &'a str {
    query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()).expect("the query holds the key")
  }

  #[test]
  fn consent_url_has_scopes_state_and_optional_login_hint() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);

    let url = consent_url(&table, "personal", Some("person@example.invalid")).unwrap();

    let query = consent_query(&url);
    assert_eq!(parameter(&query, "response_type"), "code");
    assert_eq!(parameter(&query, "login_hint"), "person@example.invalid");
    assert_eq!(parameter(&query, "state"), "personal");
    assert_eq!(parameter(&query, "redirect_uri"), "http://localhost");
    assert!(
      parameter(&query, "scope").split(' ').any(|scope| scope == "https://www.googleapis.com/auth/calendar.readonly")
    );
  }

  #[test]
  fn consent_url_omits_login_hint_without_email() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);

    let url = consent_url(&table, "personal", None).unwrap();

    assert!(!consent_query(&url).iter().any(|(key, _)| key == "login_hint"));
  }

  #[test]
  fn the_consent_url_prints_the_recorded_bytes() {
    let home = credentials_home(DEFAULT_TABLE);
    let table = table(&home);

    let url = consent_url(&table, "default", Some("person@example.invalid")).unwrap();

    assert_eq!(url, CONSENT_URL_REFERENCE);
  }

  #[test]
  fn add_appends_section_without_changing_prior_bytes_or_mode() {
    let home = credentials_home(DEFAULT_TABLE);
    let path = credentials_path(&home);
    let original = std::fs::read(&path).unwrap();
    set_file_mode(&path, 0o640);
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    let email = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      Some("personal: mailbox"),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated = std::fs::read(&path).unwrap();
    let parsed: Yaml = serde_yaml::from_slice(&updated).unwrap();
    assert_eq!(email, "person@example.invalid");
    assert_eq!(add_output("personal", &email), "personal: person@example.invalid\n");
    assert_eq!(updated, APPEND_REFERENCE_BYTES.as_bytes());
    assert!(updated.starts_with(&original));
    assert_eq!(parsed["google_account_personal"]["refresh_token"], "new-refresh");
    assert_eq!(parsed["google_account_personal"]["description"], "personal: mailbox");
    assert_eq!(file_mode(&path), 0o640);
    assert!(temporary_files(&home).is_empty());
  }

  #[test]
  fn add_existing_section_keeps_description_without_flag() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}\ngoogle_account_personal:\n  description: keep this\n  refresh_token: old-refresh\n"
    ));
    let original_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      None,
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    assert_eq!(original_lines.len(), updated_lines.len());
    let changes: Vec<(&String, &String)> =
      original_lines.iter().zip(&updated_lines).filter(|(before, after)| before != after).collect();
    assert_eq!(
      changes,
      vec![(&"  refresh_token: old-refresh".to_string(), &"  refresh_token: new-refresh".to_string())]
    );
    assert!(updated_lines.contains(&"  description: keep this".to_string()));
  }

  #[test]
  fn add_pending_section_inserts_refresh_token_and_description() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}\ngoogle_account_personal:\n  client_id: personal-client\n  client_secret: personal-secret\n\ngoogle_account_after:\n  refresh_token: after-refresh\n"
    ));
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    let email = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      Some("personal mailbox"),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated = std::fs::read(credentials_path(&home)).unwrap();
    assert_eq!(email, "person@example.invalid");
    assert_eq!(add_output("personal", &email), "personal: person@example.invalid\n");
    assert_eq!(updated, INSERT_REFERENCE_BYTES.as_bytes());
    let updated_text = String::from_utf8(updated).unwrap();
    let (head, rest) = updated_text.split_once("google_account_personal:\n").expect("the pending section stays");
    let (section, tail) = rest.split_once("\ngoogle_account_after:\n").expect("the after section stays");
    assert_eq!(head, format!("{DEFAULT_TABLE}\n"));
    assert_eq!(
      section.lines().collect::<Vec<_>>(),
      vec![
        "  client_id: personal-client",
        "  client_secret: personal-secret",
        "  refresh_token: new-refresh",
        "  description: personal mailbox",
      ]
    );
    assert_eq!(tail, "  refresh_token: after-refresh\n");
    let fields = form_fields(&stub.next().into_bytes());
    assert_eq!(
      fields.iter().find(|(key, _)| key == "client_id").map(|(_, value)| value.as_str()),
      Some("personal-client")
    );
    assert_eq!(
      fields.iter().find(|(key, _)| key == "client_secret").map(|(_, value)| value.as_str()),
      Some("personal-secret")
    );
  }

  #[test]
  fn add_existing_section_replaces_description_line() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}\ngoogle_account_personal:\n  description: old text\n  refresh_token: old-refresh\n"
    ));
    let original_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      Some("new: text"),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    assert_eq!(original_lines.len(), updated_lines.len());
    let changes: Vec<(&String, &String)> =
      original_lines.iter().zip(&updated_lines).filter(|(before, after)| before != after).collect();
    assert_eq!(
      changes,
      vec![
        (&"  description: old text".to_string(), &"  description: 'new: text'".to_string()),
        (&"  refresh_token: old-refresh".to_string(), &"  refresh_token: new-refresh".to_string()),
      ]
    );
  }

  #[test]
  fn add_default_name_inserts_google_description() {
    let home = credentials_home(DEFAULT_TABLE);
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    add(
      &home,
      "default",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=default",
      Some("work mailbox"),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated = std::fs::read_to_string(credentials_path(&home)).unwrap();
    assert_eq!(
      updated,
      concat!(
        "google:\n",
        "  client_id: default-client\n",
        "  client_secret: default-secret\n",
        "  refresh_token: new-refresh\n",
        "  description: work mailbox\n",
      )
    );
    let parsed: Yaml = serde_yaml::from_str(&updated).unwrap();
    assert_eq!(parsed["google"]["description"], "work mailbox");
  }

  #[test]
  fn add_long_description_on_pending_section_stays_on_one_line() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}\ngoogle_account_personal:\n  description: old text\n  refresh_token: old-refresh\n"
    ));
    let original_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    let email = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      Some(LONG_DESCRIPTION),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    assert_eq!(email, "person@example.invalid");
    assert_eq!(add_output("personal", &email), "personal: person@example.invalid\n");
    assert_eq!(updated_lines.len(), original_lines.len());
    let changes: Vec<(&String, &String)> =
      original_lines.iter().zip(&updated_lines).filter(|(before, after)| before != after).collect();
    assert_eq!(
      changes,
      vec![
        (&"  description: old text".to_string(), &format!("  description: {LONG_DESCRIPTION}")),
        (&"  refresh_token: old-refresh".to_string(), &"  refresh_token: new-refresh".to_string()),
      ]
    );
    let parsed: Yaml = serde_yaml::from_str(&std::fs::read_to_string(credentials_path(&home)).unwrap()).unwrap();
    assert_eq!(parsed["google_account_personal"]["description"], LONG_DESCRIPTION);
  }

  #[test]
  fn add_long_description_on_default_name_stays_on_one_line() {
    let home = credentials_home(DEFAULT_TABLE);
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    add(
      &home,
      "default",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=default",
      Some(LONG_DESCRIPTION),
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated = std::fs::read_to_string(credentials_path(&home)).unwrap();
    assert_eq!(
      updated,
      format!(
        concat!(
          "google:\n",
          "  client_id: default-client\n",
          "  client_secret: default-secret\n",
          "  refresh_token: new-refresh\n",
          "  description: {}\n",
        ),
        LONG_DESCRIPTION
      )
    );
    let parsed: Yaml = serde_yaml::from_str(&updated).unwrap();
    assert_eq!(parsed["google"]["description"], LONG_DESCRIPTION);
  }

  #[test]
  fn add_rejects_inline_section_without_changes() {
    for inline_section in ["{client_id: personal-client, client_secret: personal-secret}", "{}"] {
      let home = credentials_home(&format!("{DEFAULT_TABLE}\ngoogle_account_personal: {inline_section}\n"));
      let original = std::fs::read(credentials_path(&home)).unwrap();
      let endpoints = refused_endpoints();

      let error = add(
        &home,
        "personal",
        "person@example.invalid",
        "http://localhost/?code=auth-code&state=personal",
        Some("personal mailbox"),
        None,
        &endpoints,
      )
      .unwrap_err();

      assert_eq!(error.code, 1);
      assert!(error_text(&error).contains("credentials.google_account_personal must be a block mapping"));
      assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
      assert!(temporary_files(&home).is_empty());
    }
  }

  #[test]
  fn add_duplicate_refresh_token_lines_fail_without_changes() {
    let home = credentials_home(&format!(
      "{DEFAULT_TABLE}\ngoogle_account_personal:\n  refresh_token: one\n  refresh_token: two\n"
    ));
    let original = std::fs::read(credentials_path(&home)).unwrap();
    let endpoints = refused_endpoints();

    let error = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=personal",
      None,
      None,
      &endpoints,
    )
    .unwrap_err();

    assert_eq!(error.code, 1);
    assert!(error_text(&error).contains("more than one refresh_token"));
    assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
    assert!(temporary_files(&home).is_empty());
  }

  #[test]
  fn add_default_name_reauthorizes_google_section() {
    let home = credentials_home(&DEFAULT_TABLE.replace("google:\n", "google:\n  name: work\n"));
    let original_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    let stub = Stub::start(vec![
      exchange_response("temporary-access", "new-refresh"),
      profile_response("person@example.invalid"),
    ]);

    add(
      &home,
      "work",
      "person@example.invalid",
      "http://localhost/?code=auth-code&state=work",
      None,
      None,
      &stub.endpoints(),
    )
    .unwrap();

    let updated_lines: Vec<String> =
      std::fs::read_to_string(credentials_path(&home)).unwrap().lines().map(str::to_string).collect();
    assert_eq!(original_lines.len(), updated_lines.len());
    let changes: Vec<(&String, &String)> =
      original_lines.iter().zip(&updated_lines).filter(|(before, after)| before != after).collect();
    assert_eq!(
      changes,
      vec![(&"  refresh_token: default-refresh".to_string(), &"  refresh_token: new-refresh".to_string())]
    );
    let parsed: Yaml = serde_yaml::from_str(&std::fs::read_to_string(credentials_path(&home)).unwrap()).unwrap();
    assert_eq!(parsed["google"]["refresh_token"], "new-refresh");
  }

  #[test]
  fn failed_add_keeps_file_bytes_and_cleans_temporary_file() {
    for (redirect_url, email, expected_error, responses) in [
      ("http://localhost/?code=auth-code&state=other", "person@example.invalid", "state", Vec::new()),
      (
        "http://localhost/?code=auth-code&state=personal",
        "wrong@example.invalid",
        "email mismatch",
        vec![exchange_response("temporary-access", "new-refresh"), profile_response("person@example.invalid")],
      ),
    ] {
      let home = credentials_home(DEFAULT_TABLE);
      let original = std::fs::read(credentials_path(&home)).unwrap();
      let stub = (!responses.is_empty()).then(|| Stub::start(responses.clone()));
      let endpoints = stub.as_ref().map(Stub::endpoints).unwrap_or_else(refused_endpoints);

      let error = add(&home, "personal", email, redirect_url, None, None, &endpoints).unwrap_err();

      assert_eq!(error.code, 1);
      assert!(error_text(&error).contains(expected_error));
      assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
      assert!(temporary_files(&home).is_empty());
    }
  }

  #[test]
  fn add_owner_writes_into_new_and_existing_sections() {
    let home = credentials_home(concat!(
      "google:\n",
      "  client_id: default-client\n",
      "  client_secret: default-secret\n",
      "  refresh_token: default-refresh\n",
      "  owner: avery\n",
      "google_account_existing:\n",
      "  description: existing\n",
    ));
    let stub = Stub::start(vec![
      exchange_response("new-access", "new-refresh"),
      profile_response("new@example.invalid"),
      exchange_response("existing-access", "existing-refresh"),
      profile_response("existing@example.invalid"),
    ]);

    add(
      &home,
      "fresh",
      "new@example.invalid",
      "http://localhost?state=fresh&code=c",
      None,
      Some("blake"),
      &stub.endpoints(),
    )
    .unwrap();
    add(
      &home,
      "existing",
      "existing@example.invalid",
      "http://localhost?state=existing&code=c",
      None,
      Some("carol"),
      &stub.endpoints(),
    )
    .unwrap();

    let parsed: Yaml = serde_yaml::from_str(&std::fs::read_to_string(credentials_path(&home)).unwrap()).unwrap();
    assert_eq!(parsed["google_account_fresh"]["owner"], "blake");
    assert_eq!(parsed["google_account_existing"]["owner"], "carol");
    assert_eq!(parsed["google_account_existing"]["description"], "existing");
  }

  #[test]
  fn add_without_owner_exits_when_google_sets_owner() {
    let home = credentials_home(
      "google:\n  client_id: c\n  client_secret: s\n  refresh_token: r\n  owner: avery\ngoogle_account_personal:\n  description: personal mailbox\n",
    );
    let original = std::fs::read(credentials_path(&home)).unwrap();
    let stub = Stub::start(Vec::new());

    let error = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost?state=personal&code=c",
      None,
      None,
      &stub.endpoints(),
    )
    .unwrap_err();

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("--owner"));
    assert!(stub.requests.try_recv().is_err());
    assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
    assert!(temporary_files(&home).is_empty());
  }

  #[test]
  fn add_owner_without_google_owner_exits_before_network() {
    let home = credentials_home(DEFAULT_TABLE);
    let original = std::fs::read(credentials_path(&home)).unwrap();
    let stub = Stub::start(Vec::new());

    let error = add(
      &home,
      "personal",
      "person@example.invalid",
      "http://localhost?state=personal&code=c",
      None,
      Some("blake"),
      &stub.endpoints(),
    )
    .unwrap_err();

    assert_eq!(error.code, 2);
    assert!(
      error_text(&error).contains("credentials.google_account_personal sets owner but credentials.google does not")
    );
    assert!(stub.requests.try_recv().is_err());
    assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
  }

  /// The block-mapping scalar renderings `yaml.safe_dump` prints for the values `add` can write,
  /// recorded from PyYAML at the port's base commit.
  #[test]
  fn the_block_mapping_scalar_dumper_prints_pyyaml_forms() {
    for (value, expected) in [
      ("personal mailbox", "personal mailbox"),
      ("new: text", "'new: text'"),
      ("a #b", "'a #b'"),
      ("-x", "-x"),
      ("- x", "'- x'"),
      (":x", ":x"),
      (": x", "': x'"),
      ("a:b", "a:b"),
      ("[x", "'[x'"),
      ("@x", "'@x'"),
      ("x:", "'x:'"),
      ("x: ", "'x: '"),
      (" lead", "' lead'"),
      ("trail ", "'trail '"),
      ("true", "'true'"),
      ("yes", "'yes'"),
      ("123", "'123'"),
      ("1.5", "'1.5'"),
      ("2024-01-01", "'2024-01-01'"),
      ("null", "'null'"),
      ("~", "'~'"),
      ("", "''"),
      ("0x1f", "'0x1f'"),
      ("010", "'010'"),
      ("1:30", "'1:30'"),
      ("+12", "'+12'"),
      ("1_000", "'1_000'"),
      (".inf", "'.inf'"),
      (".NaN", "'.NaN'"),
      ("off", "'off'"),
      ("On", "'On'"),
      ("y", "y"),
      ("<<", "'<<'"),
      ("=", "'='"),
      ("...", "'...'"),
      ("---", "'---'"),
      ("12:30", "'12:30'"),
      ("0o17", "0o17"),
      ("no", "'no'"),
      ("1e5", "1e5"),
      (".5", "'.5'"),
      ("-3.14e+10", "'-3.14e+10'"),
      ("2024-01-02T03:04:05", "'2024-01-02T03:04:05'"),
      ("1.2.3", "1.2.3"),
      ("a''b", "a''b"),
      ("it's", "it's"),
      ("a  b", "a  b"),
      ("#leading", "'#leading'"),
      ("1//0abc", "1//0abc"),
      ("1//0abc_def-xyz.42", "1//0abc_def-xyz.42"),
      ("5:00:00", "'5:00:00'"),
      ("60:00", "'60:00'"),
      ("0", "'0'"),
      ("-0", "'-0'"),
      ("0b101", "'0b101'"),
      ("9223372036854775808", "'9223372036854775808'"),
    ] {
      assert_eq!(dump_scalar(value, 3, false, None), expected, "for {value:?}");
    }
  }

  #[test]
  fn add_leaving_person_without_primary_exits_before_network() {
    let home = credentials_home(
      "google:\n  client_id: c\n  client_secret: s\n  refresh_token: r\n  owner: avery\ngoogle_account_blake-a:\n  refresh_token: ra\n  owner: blake\n",
    );
    let original = std::fs::read(credentials_path(&home)).unwrap();
    let stub = Stub::start(Vec::new());

    let error = add(
      &home,
      "blake-b",
      "person@example.invalid",
      "http://localhost?state=blake-b&code=c",
      None,
      Some("blake"),
      &stub.endpoints(),
    )
    .unwrap_err();

    assert_eq!(error.code, 2);
    assert!(error_text(&error).contains("person blake has several accounts and none sets primary: true"));
    assert!(stub.requests.try_recv().is_err());
    assert_eq!(std::fs::read(credentials_path(&home)).unwrap(), original);
  }
}
