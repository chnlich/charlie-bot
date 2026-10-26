//! Claude Code's quota accounts for the quota panel (`src/backends/claude_code/usage_quota.py`).
//!
//! One account reads one login directory's usage from the Anthropic OAuth usage endpoint. The accounts are the default
//! login directory (label `main`) followed by the `accounts.claude` pool. Every derivation reads the live config, and an
//! account whose login directory stays keeps its object, and with it its backoff.
//!
//! An account records its panel reading in the shared [`ClaudeAccountState`], keeps the User-Agent probe's result there
//! too, and marks its cached entry with `login_required` and the expired windows.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::timefmt::utc_now_iso;
use charliebot_registry::usage::{
  PANEL_EXPIRED, PANEL_FETCHED_AT, PANEL_PROVIDER, PANEL_RESETS_AT, PANEL_SCOPE_LABEL, PANEL_UTILIZATION,
  PANEL_WINDOW_MINUTES, PANEL_WINDOWS, QuotaAccount,
};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

use super::ClaudeHost;
use super::claude_accounts::{self, ClaudeAccountState, json_truthy};
use super::claude_config::{AccountsConfig, ClaudeAccount};
use super::login_dirs::CREDENTIALS_FILE;

/// The OAuth usage endpoint.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The OAuth token endpoint that renews an access token.
pub const TOKEN_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
/// The User-Agent when the installed CLI's version cannot be probed.
const USER_AGENT_FALLBACK: &str = "claude-code/2.1.219";
const HTTP_OAUTH_TIMEOUT: Duration = Duration::from_secs(30);
const EXT_USAGE_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The backoff ladder after a failed round: 60 seconds first, doubling, capped at 30 minutes.
const BACKOFF_BASE_SECONDS: f64 = 30.0;
const BACKOFF_CAP_SECONDS: f64 = 30.0 * 60.0;

const PROVIDER: &str = "claude";
const PROVIDER_LABEL: &str = "Claude";
const DEFAULT_LABEL: &str = "main";
const NO_DATA: &str = "no data";

/// The plan windows the response names by field: `(camel name, snake name, window length in minutes)`.
const CLAUDE_WINDOW_FIELDS: [(&str, &str, i64); 2] = [("fiveHour", "five_hour", 300), ("sevenDay", "seven_day", 10080)];
/// The window length of each `limits[].group` the response reports. Any other group is skipped with a warning.
const LIMIT_GROUP_WINDOW_MINUTES: [(&str, i64); 2] = [("session", 300), ("weekly", 10080)];

static CLI_VERSION: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("the version pattern is valid"));

/// The HTTP client and the two endpoints the quota accounts call. Production builds it from the environment and the URL
/// constants; tests point the endpoints at a local server.
pub struct QuotaHttp {
  client: reqwest::Client,
  usage_url: String,
  token_url: String,
}

impl QuotaHttp {
  /// The production client: the shared outbound client, configured from the process environment.
  pub fn from_process() -> anyhow::Result<Self> {
    let client = http_client::client(&Settings::from_env())?;
    Ok(Self::with_endpoints(client, USAGE_URL, TOKEN_REFRESH_URL))
  }

  pub fn with_endpoints(client: reqwest::Client, usage_url: &str, token_url: &str) -> Self {
    Self { client, usage_url: usage_url.to_string(), token_url: token_url.to_string() }
  }
}

/// The process-wide state of the quota accounts, kept on the shared [`ClaudeAccountState`]: the User-Agent the probe
/// resolved, and the warnings already logged.
#[derive(Default)]
pub struct QuotaState {
  user_agent: OnceCell<String>,
  warned: Mutex<Warned>,
}

#[derive(Default)]
struct Warned {
  /// `(event, credentials path)` pairs. A read of a path that returns a token forgets its pairs.
  credentials: HashSet<(&'static str, PathBuf)>,
  /// `(account, slot, reason)` triples of unrecognized limit shapes, logged once per process.
  limit_shapes: HashSet<(String, String, String)>,
}

/// What one account carries between rounds: why its last fetch returned nothing, and its backoff.
struct Round {
  last_error: String,
  backoff_seconds: f64,
  backoff_until: f64,
}

impl Default for Round {
  fn default() -> Self {
    Self { last_error: NO_DATA.to_string(), backoff_seconds: 0.0, backoff_until: 0.0 }
  }
}

/// The Claude quota accounts, kept between rounds by login directory.
pub struct QuotaAccounts {
  state: Arc<ClaudeAccountState>,
  http: Arc<QuotaHttp>,
  by_dir: Mutex<HashMap<PathBuf, Arc<ClaudeQuotaAccount>>>,
}

impl QuotaAccounts {
  pub fn new(state: Arc<ClaudeAccountState>, http: Arc<QuotaHttp>) -> Self {
    Self { state, http, by_dir: Mutex::new(HashMap::new()) }
  }

  /// `quota_accounts()`: the Claude logins in panel order, the default login first and then the pool. A login whose
  /// directory stays in the config keeps its account object, and a login the config dropped is forgotten.
  pub fn derive(&self, host: &ClaudeHost, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    let pool: Vec<(String, String)> = cfg
      .section::<AccountsConfig>("accounts")
      .claude
      .iter()
      .map(|account| (account.label.clone(), account.config_dir.clone()))
      .collect();
    let mut pool_dirs = HashMap::new();
    for (label, raw) in &pool {
      pool_dirs.insert(label.clone(), abspath(&host.expand(raw))?.to_string_lossy().into_owned());
    }
    let logins = derive_login_dirs(host, &pool)?;

    let mut by_dir = self.by_dir.lock().expect("the Claude quota accounts are not poisoned");
    let mut accounts: Vec<Arc<dyn QuotaAccount>> = Vec::new();
    let mut live = HashSet::new();
    for (label, dir) in logins {
      let account = match by_dir.get(&dir) {
        Some(existing) if existing.label == label => Arc::clone(existing),
        Some(existing) => Arc::new(existing.relabeled(&label)),
        None => Arc::new(ClaudeQuotaAccount::new(
          &label,
          dir.join(CREDENTIALS_FILE),
          Arc::clone(&self.state),
          Arc::clone(&self.http),
        )),
      };
      account.set_login_dir(pool_dirs.get(&label).cloned());
      accounts.push(Arc::clone(&account) as Arc<dyn QuotaAccount>);
      by_dir.insert(dir.clone(), account);
      live.insert(dir);
    }
    by_dir.retain(|dir, _| live.contains(dir));
    Ok(accounts)
  }
}

/// One Claude login on the quota panel: its label, its credentials file, and the state it carries between rounds.
pub struct ClaudeQuotaAccount {
  label: String,
  credentials_path: PathBuf,
  /// The login directory of a pool entry, or `None` for an account that is not one. Only a pool entry carries the
  /// `login_required` mark.
  login_dir: Mutex<Option<String>>,
  state: Arc<ClaudeAccountState>,
  http: Arc<QuotaHttp>,
  round: Arc<Mutex<Round>>,
}

impl ClaudeQuotaAccount {
  pub fn new(label: &str, credentials_path: PathBuf, state: Arc<ClaudeAccountState>, http: Arc<QuotaHttp>) -> Self {
    Self {
      label: label.to_string(),
      credentials_path,
      login_dir: Mutex::new(None),
      state,
      http,
      round: Arc::new(Mutex::new(Round::default())),
    }
  }

  /// The same login under another label. The backoff and the last error carry over.
  fn relabeled(&self, label: &str) -> Self {
    Self {
      label: label.to_string(),
      credentials_path: self.credentials_path.clone(),
      login_dir: Mutex::new(None),
      state: Arc::clone(&self.state),
      http: Arc::clone(&self.http),
      round: Arc::clone(&self.round),
    }
  }

  fn set_login_dir(&self, login_dir: Option<String>) {
    *self.login_dir.lock().expect("the login directory is not poisoned") = login_dir;
  }

  fn login_dir(&self) -> Option<String> {
    self.login_dir.lock().expect("the login directory is not poisoned").clone()
  }

  fn set_error(&self, message: &str) {
    self.round.lock().expect("the account round is not poisoned").last_error = message.to_string();
  }

  fn in_backoff(&self) -> bool {
    now_seconds() < self.round.lock().expect("the account round is not poisoned").backoff_until
  }

  fn backoff_seconds(&self) -> f64 {
    self.round.lock().expect("the account round is not poisoned").backoff_seconds
  }

  /// Advances the backoff ladder and starts the new backoff now.
  fn arm_backoff(&self) {
    let mut round = self.round.lock().expect("the account round is not poisoned");
    round.backoff_seconds = next_backoff_seconds(round.backoff_seconds);
    round.backoff_until = now_seconds() + round.backoff_seconds;
  }

  fn clear_backoff(&self) {
    self.round.lock().expect("the account round is not poisoned").backoff_seconds = 0.0;
  }

  /// One round. The usage is fetched; a 401 renews the token once and retries once; a second 401 or a 429 backs off.
  async fn fetch_round(&self) -> anyhow::Result<Option<Map<String, Value>>> {
    if self.in_backoff() {
      return Ok(None);
    }
    let Some(credentials) = read_credentials(&self.state.quota, &self.credentials_path)? else {
      self.set_error("credentials not found");
      return Ok(None);
    };
    let mut response = self.get_usage(&credentials.access_token).await?;
    if response.status() == StatusCode::UNAUTHORIZED {
      let Some(access_token) = self.reauthenticate(&credentials.access_token).await? else {
        self.arm_backoff();
        return Ok(None);
      };
      response = self.get_usage(&access_token).await?;
      if response.status() == StatusCode::UNAUTHORIZED {
        self.set_error("auth rejected");
        self.arm_backoff();
        tracing::warn!(
          target: "charliebot_agents",
          account = %self.label,
          backoff_seconds = self.backoff_seconds(),
          "ext_usage_auth_rejected_after_renewal"
        );
        return Ok(None);
      }
    }
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
      self.set_error("rate limited");
      self.arm_backoff();
      tracing::warn!(
        target: "charliebot_agents",
        account = %self.label,
        backoff_seconds = self.backoff_seconds(),
        "ext_usage_rate_limited"
      );
      return Ok(None);
    }
    self.clear_backoff();
    let response = raise_for_status(response)?;
    let raw: Value = response.json().await.context("the usage response is not JSON")?;
    let usage = transform_response(&raw, &self.label, &self.state.quota)?;
    // The account pool ranks logins by their newest reading, and the panel poll is that reading's source.
    claude_accounts::observe_usage_panel(&self.state, &self.label, &Value::Object(usage.clone()));
    Ok(Some(usage))
  }

  async fn get_usage(&self, access_token: &str) -> anyhow::Result<reqwest::Response> {
    let user_agent = user_agent(&self.state.quota).await;
    self
      .http
      .client
      .get(&self.http.usage_url)
      .header(AUTHORIZATION, format!("Bearer {access_token}"))
      .header("anthropic-beta", ANTHROPIC_BETA)
      .header(USER_AGENT, user_agent)
      .timeout(HTTP_OAUTH_TIMEOUT)
      .send()
      .await
      .with_context(|| format!("requesting the usage of {}", self.label))
  }

  /// A usable access token after a 401. The credentials file is shared with the Claude CLI, which rotates the refresh
  /// token whenever it renews. A file that already holds another access token is used before any refresh call.
  async fn reauthenticate(&self, failed_token: &str) -> anyhow::Result<Option<String>> {
    let Some(credentials) = read_credentials(&self.state.quota, &self.credentials_path)? else {
      self.set_error("credentials not found");
      return Ok(None);
    };
    if credentials.access_token != failed_token {
      return Ok(Some(credentials.access_token));
    }
    let Some(refresh_token) = credentials.refresh_token else {
      tracing::warn!(target: "charliebot_agents", account = %self.label, "ext_usage_no_refresh_token");
      self.set_error("token refresh failed");
      return Ok(None);
    };
    match self.refresh_access_token(&refresh_token).await? {
      Some(access_token) => Ok(Some(access_token)),
      None => {
        self.set_error("token refresh failed");
        Ok(None)
      }
    }
  }

  /// Renews the access token and writes the renewed credentials to the account's file. A refusal or a transport failure
  /// is logged and returns `None`. Only the refusal's status and a prefix of its body are logged, never a token.
  async fn refresh_access_token(&self, refresh_token: &str) -> anyhow::Result<Option<String>> {
    let path = self.credentials_path.as_path();
    let user_agent = user_agent(&self.state.quota).await;
    let response = match self
      .http
      .client
      .post(&self.http.token_url)
      .json(&json!({"grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": CLIENT_ID}))
      .header(USER_AGENT, user_agent)
      .timeout(HTTP_OAUTH_TIMEOUT)
      .send()
      .await
    {
      Ok(response) => response,
      Err(error) => {
        tracing::error!(
          target: "charliebot_agents",
          path = %path.display(),
          error = %error,
          "ext_usage_token_refresh_failed"
        );
        return Ok(None);
      }
    };
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
      let body = response.text().await.context("reading the token refusal")?;
      tracing::warn!(
        target: "charliebot_agents",
        path = %path.display(),
        status_code = status.as_u16(),
        body = %body.chars().take(200).collect::<String>(),
        "ext_usage_token_refresh_failed"
      );
      return Ok(None);
    }
    let token: Value = response.json().await.context("the token response is not JSON")?;
    let token = token.as_object().context("the token response is not a JSON object")?;
    let new_access = token
      .get("access_token")
      .and_then(Value::as_str)
      .context("the token response names no access_token")?
      .to_string();
    let new_refresh = match token.get("refresh_token") {
      Some(value) => value.clone(),
      None => json!(refresh_token),
    };
    let new_expires = expires_at_ms(token);
    if new_expires.is_none() {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), "ext_usage_renewal_without_expiry");
    }
    let credentials_path = self.credentials_path.clone();
    let access = new_access.clone();
    tokio::task::spawn_blocking(move || {
      write_renewed_credentials(&credentials_path, &access, new_refresh, new_expires)
    })
    .await
    .context("the credentials write task failed")??;
    tracing::info!(target: "charliebot_agents", path = %path.display(), "ext_usage_token_refreshed");
    Ok(Some(new_access))
  }
}

impl QuotaAccount for ClaudeQuotaAccount {
  fn provider(&self) -> &str {
    PROVIDER
  }

  fn provider_label(&self) -> &str {
    PROVIDER_LABEL
  }

  fn label(&self) -> &str {
    &self.label
  }

  fn last_error(&self) -> String {
    self.round.lock().expect("the account round is not poisoned").last_error.clone()
  }

  fn fetch<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Option<Map<String, Value>>>> {
    Box::pin(async move { self.fetch_round().await })
  }

  fn mark_login_required(&self, entry: &mut Map<String, Value>) {
    let Some(login_dir) = self.login_dir() else {
      return;
    };
    let account = ClaudeAccount { label: self.label.clone(), config_dir: login_dir.clone() };
    if claude_accounts::healthy(&self.state, &account, None) {
      entry.remove("login_required");
    } else {
      entry.insert("login_required".to_string(), Value::String(login_dir));
    }
  }

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> Map<String, Value> {
    let sampled = entry.get(PANEL_FETCHED_AT).and_then(Value::as_str).and_then(|text| UtcDateTime::parse(text).ok());
    mark_expired_windows(entry, sampled, now)
  }
}

/// `_derive_login_dirs`: the default login directory under label `main`, then the pool entries under their labels. An
/// entry that names a directory already listed is skipped, and a label already taken is logged and skipped.
fn derive_login_dirs(host: &ClaudeHost, pool: &[(String, String)]) -> anyhow::Result<Vec<(String, PathBuf)>> {
  let default = abspath(&host.default_dir())?;
  let mut seen = HashSet::from([default.clone()]);
  let mut labels = HashSet::from([DEFAULT_LABEL.to_string()]);
  let mut logins = vec![(DEFAULT_LABEL.to_string(), default)];
  for (label, raw) in pool {
    let expanded = abspath(&host.expand(raw))?;
    if seen.contains(&expanded) {
      continue;
    }
    if labels.contains(label) {
      tracing::error!(
        target: "charliebot_agents",
        provider = PROVIDER,
        dir = %expanded.display(),
        label = label.as_str(),
        "ext_usage_account_label_collision_skip"
      );
      continue;
    }
    seen.insert(expanded.clone());
    labels.insert(label.clone());
    logins.push((label.clone(), expanded));
  }
  Ok(logins)
}

/// `os.path.abspath`: the path joined to the working directory when it is relative, with `.` and `..` resolved
/// lexically.
fn abspath(path: &Path) -> anyhow::Result<PathBuf> {
  let joined = if path.is_absolute() {
    path.to_path_buf()
  } else {
    std::env::current_dir().context("reading the working directory")?.join(path)
  };
  let mut normal = PathBuf::new();
  for component in joined.components() {
    match component {
      Component::ParentDir => {
        normal.pop();
      }
      Component::CurDir => {}
      other => normal.push(other.as_os_str()),
    }
  }
  Ok(normal)
}

/// The OAuth tokens of a credentials file.
struct Credentials {
  access_token: String,
  refresh_token: Option<String>,
}

/// `_read_credentials`: the tokens in a credentials file, or `None` after a logged warning when the file is missing or
/// holds no access token. A file that is not JSON, or whose `claudeAiOauth` is not an object, fails the round.
fn read_credentials(quota: &QuotaState, path: &Path) -> anyhow::Result<Option<Credentials>> {
  if !path.exists() {
    warn_credentials_once(quota, "ext_usage_credentials_not_found", path);
    return Ok(None);
  }
  let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let data: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
  let data = data.as_object().with_context(|| format!("{} is not a JSON object", path.display()))?;
  let no_oauth = Map::new();
  let oauth = match data.get("claudeAiOauth") {
    None => &no_oauth,
    Some(value) => value.as_object().with_context(|| format!("{}: claudeAiOauth is not an object", path.display()))?,
  };
  let access_token = match oauth.get("accessToken") {
    Some(value) if json_truthy(value) => {
      value.as_str().with_context(|| format!("{}: accessToken is not a string", path.display()))?.to_string()
    }
    _ => {
      warn_credentials_once(quota, "ext_usage_no_access_token", path);
      return Ok(None);
    }
  };
  let refresh_token = match oauth.get("refreshToken") {
    Some(value) if json_truthy(value) => {
      Some(value.as_str().with_context(|| format!("{}: refreshToken is not a string", path.display()))?.to_string())
    }
    _ => None,
  };
  // expiresAt is deliberately not read: renewal keys off the server's 401, never a local expiry check.
  quota.warned.lock().expect("the warnings are not poisoned").credentials.retain(|(_, known)| known != path);
  Ok(Some(Credentials { access_token, refresh_token }))
}

/// The status error text returned by Python's `httpx.Response.raise_for_status`.
fn raise_for_status(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
  let status = response.status();
  let category = match status.as_u16() {
    200..=299 => return Ok(response),
    100..=199 => "Informational response",
    300..=399 => "Redirect response",
    400..=499 => "Client error",
    500..=599 => "Server error",
    _ => "Invalid status code",
  };
  let reason = status.canonical_reason().context("the HTTP status has no canonical reason")?;
  bail!(
    "{category} '{} {}' for url '{}'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{}",
    status.as_u16(),
    reason,
    response.url(),
    status.as_u16()
  );
}

/// Logs one credentials warning per event and path until a read of the path returns a token.
fn warn_credentials_once(quota: &QuotaState, event: &'static str, path: &Path) {
  let fresh =
    quota.warned.lock().expect("the warnings are not poisoned").credentials.insert((event, path.to_path_buf()));
  if !fresh {
    return;
  }
  match event {
    "ext_usage_credentials_not_found" => {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), "ext_usage_credentials_not_found");
    }
    _ => tracing::warn!(target: "charliebot_agents", path = %path.display(), "ext_usage_no_access_token"),
  }
}

/// The User-Agent every OAuth request carries: the installed CLI's version. The probe runs on the first request and its
/// answer is cached on the state, so a process pays for the subprocess once.
async fn user_agent(quota: &QuotaState) -> String {
  quota.user_agent.get_or_init(probe_user_agent).await.clone()
}

async fn probe_user_agent() -> String {
  let (user_agent, source) = match probe_cli_version().await {
    Some(version) => (format!("claude-code/{version}"), "probe"),
    None => (USER_AGENT_FALLBACK.to_string(), "fallback"),
  };
  let version = user_agent.strip_prefix("claude-code/").unwrap_or(&user_agent).to_string();
  if source == "fallback" {
    tracing::warn!(target: "charliebot_agents", version = %version, source, "ext_usage_user_agent_resolved");
  } else {
    tracing::info!(target: "charliebot_agents", version = %version, source, "ext_usage_user_agent_resolved");
  }
  user_agent
}

/// One `claude --version` probe. The program is found on PATH and runs without a shell. A missing program, a non-zero
/// exit, a timeout, or output without a version all return `None`.
async fn probe_cli_version() -> Option<String> {
  let child = match tokio::process::Command::new("claude")
    .arg("--version")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
  {
    Ok(child) => child,
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", error = %error, "ext_usage_version_probe_failed");
      return None;
    }
  };
  let output = match tokio::time::timeout(EXT_USAGE_VERSION_PROBE_TIMEOUT, child.wait_with_output()).await {
    Ok(Ok(output)) => output,
    Ok(Err(error)) => {
      tracing::debug!(target: "charliebot_agents", error = %error, "ext_usage_version_probe_failed");
      return None;
    }
    Err(_) => {
      tracing::debug!(target: "charliebot_agents", "ext_usage_version_probe_timed_out");
      return None;
    }
  };
  if !output.status.success() {
    return None;
  }
  CLI_VERSION.find(&String::from_utf8_lossy(&output.stdout)).map(|version| version.as_str().to_string())
}

/// `_expires_at_ms`: the renewed token's expiry as the millisecond stamp the CLI's file uses. An absolute `expires_at` may
/// be in seconds; otherwise `expires_in` is relative to now. Nothing reads the stamp back.
fn expires_at_ms(token: &Map<String, Value>) -> Option<i64> {
  if let Some(absolute) = token.get("expires_at").and_then(Value::as_f64) {
    return Some(if absolute > 1e11 { absolute as i64 } else { (absolute * 1000.0) as i64 });
  }
  token.get("expires_in").and_then(Value::as_f64).map(|relative| ((now_seconds() + relative) * 1000.0) as i64)
}

/// Replaces the renewed tokens in a credentials file: the file is read again, its other keys are kept in place, and the
/// new file replaces the old in one step.
fn write_renewed_credentials(
  path: &Path,
  access_token: &str,
  refresh_token: Value,
  expires_at: Option<i64>,
) -> anyhow::Result<()> {
  let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let mut data: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
  let oauth = data
    .get_mut("claudeAiOauth")
    .and_then(Value::as_object_mut)
    .with_context(|| format!("{} holds no claudeAiOauth object", path.display()))?;
  oauth.insert("accessToken".to_string(), json!(access_token));
  oauth.insert("refreshToken".to_string(), refresh_token);
  if let Some(expires_at) = expires_at {
    oauth.insert("expiresAt".to_string(), json!(expires_at));
  }
  let text = serde_json::to_string_pretty(&data).context("serializing the renewed credentials")?;
  write_private_atomically(path, &text)
}

/// Writes `text` to `path` through a uniquely named sibling that is created 0600 and swapped in by `rename`, so the token
/// never appears at its final path readable by anyone but the owner. The sibling is removed when a step fails.
fn write_private_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
  let name = path.file_name().context("a credentials path names a file")?.to_string_lossy();
  let directory = path.parent().context("a credentials path names a file in a directory")?;
  let mut temporary = tempfile::Builder::new()
    .prefix(&format!(".{name}."))
    .suffix(".tmp")
    .permissions(std::fs::Permissions::from_mode(0o600))
    .tempfile_in(directory)
    .with_context(|| format!("creating a temporary credentials file beside {}", path.display()))?;
  temporary
    .write_all(text.as_bytes())
    .with_context(|| format!("writing the temporary credentials for {}", path.display()))?;
  temporary.persist(path).map_err(|error| anyhow::anyhow!("writing {}: {}", path.display(), error.error))?;
  Ok(())
}

/// The window length of a plan window, or of a scoped one, with the scope label it sorts by (empty for a plan window).
struct Window {
  window_minutes: i64,
  scope_label: String,
  entry: Value,
}

/// `_transform_response`: the usage the panel stores. The plan windows come first in their fixed order, then the scoped
/// windows, all sorted by window length and scope label. A reading timestamp and the provider complete it.
pub fn transform_response(raw: &Value, account: &str, quota: &QuotaState) -> anyhow::Result<Map<String, Value>> {
  let raw = raw.as_object().context("the usage response is not a JSON object")?;
  let mut windows = Vec::new();
  for (camel, snake, window_minutes) in CLAUDE_WINDOW_FIELDS {
    windows.push(plan_window(raw, camel, snake, window_minutes)?);
  }
  windows.extend(scoped_windows(raw, account, quota)?);
  windows.sort_by_key(|window| (window.window_minutes, window.scope_label.clone()));

  let known = |key: &str| CLAUDE_WINDOW_FIELDS.iter().any(|(camel, snake, _)| key == *camel || key == *snake);
  for (key, value) in raw {
    if !known(key) && value.as_object().is_some_and(|object| object.contains_key("utilization")) {
      warn_limit_shape(quota, account, LimitSlot::Name(key), "unrecognized window field");
    }
  }

  let mut usage = Map::new();
  usage.insert(PANEL_WINDOWS.to_string(), Value::Array(windows.into_iter().map(|window| window.entry).collect()));
  usage.insert(PANEL_FETCHED_AT.to_string(), json!(utc_now_iso()));
  usage.insert(PANEL_PROVIDER.to_string(), json!(PROVIDER));
  Ok(usage)
}

/// One plan window, read from `raw.get(camel, raw.get(snake)) or {}`.
fn plan_window(raw: &Map<String, Value>, camel: &str, snake: &str, window_minutes: i64) -> anyhow::Result<Window> {
  let no_bucket = Map::new();
  let bucket = match raw.get(camel).or_else(|| raw.get(snake)) {
    Some(value) if json_truthy(value) => {
      value.as_object().with_context(|| format!("the {camel} window is not an object"))?
    }
    _ => &no_bucket,
  };
  let resets_at = match bucket.get("resetsAt") {
    Some(value) => value.clone(),
    None => bucket.get("resets_at").cloned().unwrap_or_else(|| json!("")),
  };
  let mut entry = Map::new();
  entry.insert(PANEL_WINDOW_MINUTES.to_string(), json!(window_minutes));
  entry.insert(PANEL_UTILIZATION.to_string(), json!(as_utilization(bucket.get("utilization"))));
  entry.insert(PANEL_RESETS_AT.to_string(), resets_at);
  Ok(Window { window_minutes, scope_label: String::new(), entry: Value::Object(entry) })
}

/// The windows of the per-model `limits` entries. An entry without a scope is the plan-wide limit that the top-level
/// fields cover, so it is skipped silently. An entry that cannot be identified is skipped with a warning.
fn scoped_windows(raw: &Map<String, Value>, account: &str, quota: &QuotaState) -> anyhow::Result<Vec<Window>> {
  let entries = match raw.get("limits") {
    None | Some(Value::Null) => return Ok(Vec::new()),
    Some(Value::Array(entries)) => entries,
    Some(_) => {
      warn_limit_shape(quota, account, LimitSlot::Name("limits"), "limits is not a list");
      return Ok(Vec::new());
    }
  };
  let mut windows = Vec::new();
  for (index, entry) in entries.iter().enumerate() {
    let slot = entry
      .as_object()
      .and_then(|object| object.get("kind"))
      .and_then(Value::as_str)
      .map_or(LimitSlot::Index(index), LimitSlot::Name);
    let Some(entry) = entry.as_object() else {
      warn_limit_shape(quota, account, slot, "entry is not an object");
      continue;
    };
    let scope = match entry.get("scope") {
      None | Some(Value::Null) => continue,
      Some(scope) => scope,
    };
    let group = entry.get("group").and_then(Value::as_str);
    let Some(window_minutes) =
      LIMIT_GROUP_WINDOW_MINUTES.iter().find(|(known, _)| Some(*known) == group).map(|(_, minutes)| *minutes)
    else {
      warn_limit_shape(quota, account, slot, "unknown limit group");
      continue;
    };
    let display_name = scope
      .as_object()
      .and_then(|scope| scope.get("model"))
      .and_then(Value::as_object)
      .and_then(|model| model.get("display_name"))
      .and_then(Value::as_str)
      .filter(|name| !name.is_empty());
    let Some(display_name) = display_name else {
      warn_limit_shape(quota, account, slot, "missing model display_name");
      continue;
    };
    let utilization = as_utilization(entry.get("percent"));
    if utilization.is_none() {
      warn_limit_shape(quota, account, slot, "missing percent");
    }
    let resets_at = match entry.get("resets_at") {
      Some(value) if json_truthy(value) => value.clone(),
      _ => json!(""),
    };
    let mut scoped = Map::new();
    scoped.insert(PANEL_WINDOW_MINUTES.to_string(), json!(window_minutes));
    scoped.insert(PANEL_SCOPE_LABEL.to_string(), json!(display_name));
    scoped.insert(PANEL_UTILIZATION.to_string(), json!(utilization));
    scoped.insert(PANEL_RESETS_AT.to_string(), resets_at);
    windows.push(Window { window_minutes, scope_label: display_name.to_string(), entry: Value::Object(scoped) });
  }
  Ok(windows)
}

/// `_as_utilization`: a percentage, or `None` for a missing value or a non-number.
fn as_utilization(value: Option<&Value>) -> Option<f64> {
  match value {
    Some(Value::Number(number)) => number.as_f64(),
    _ => None,
  }
}

/// Logs one unrecognized limit shape the first time the process sees it for an account.
#[derive(Clone, Copy)]
enum LimitSlot<'a> {
  Name(&'a str),
  Index(usize),
}

impl LimitSlot<'_> {
  fn key(self) -> String {
    match self {
      Self::Name(name) => name.to_string(),
      Self::Index(index) => index.to_string(),
    }
  }
}

fn warn_limit_shape(quota: &QuotaState, account: &str, slot: LimitSlot<'_>, reason: &str) {
  let key = (account.to_string(), slot.key(), reason.to_string());
  let fresh = quota.warned.lock().expect("the warnings are not poisoned").limit_shapes.insert(key);
  if fresh {
    match slot {
      LimitSlot::Name(slot) => tracing::warn!(
        target: "charliebot_agents",
        provider = PROVIDER,
        account = account,
        slot = slot,
        reason = reason,
        "ext_usage_unknown_limit_shape"
      ),
      LimitSlot::Index(slot) => tracing::warn!(
        target: "charliebot_agents",
        provider = PROVIDER,
        account = account,
        slot = slot,
        reason = reason,
        "ext_usage_unknown_limit_shape"
      ),
    }
  }
}

/// `usage_sources.mark_expired_windows`: a copy of the entry whose expired windows carry `expired: true`. An entry with
/// no sample time, or no window list, comes back as it is.
fn mark_expired_windows(
  entry: &Map<String, Value>,
  sampled: Option<UtcDateTime>,
  now: UtcDateTime,
) -> Map<String, Value> {
  let (Some(windows), Some(sampled)) = (entry.get(PANEL_WINDOWS).and_then(Value::as_array), sampled) else {
    return entry.clone();
  };
  let annotated = windows
    .iter()
    .map(|window| match window.as_object() {
      Some(object) if claude_accounts::panel_window_expired(window, sampled, now) => {
        let mut marked = object.clone();
        marked.insert(PANEL_EXPIRED.to_string(), Value::Bool(true));
        Value::Object(marked)
      }
      _ => window.clone(),
    })
    .collect();
  let mut marked = entry.clone();
  marked.insert(PANEL_WINDOWS.to_string(), Value::Array(annotated));
  marked
}

/// The next backoff: `min((previous or 30) * 2, 1800)`, so 60 seconds first, doubling, capped at 30 minutes.
fn next_backoff_seconds(previous: f64) -> f64 {
  let base = if previous == 0.0 { BACKOFF_BASE_SECONDS } else { previous };
  (base * 2.0).min(BACKOFF_CAP_SECONDS)
}

fn now_seconds() -> f64 {
  SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is after the Unix epoch").as_secs_f64()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backends::claude_code::test_support;

  #[test]
  fn backoff_starts_at_sixty_seconds_doubles_and_caps_at_thirty_minutes() {
    let mut seconds = 0.0;
    let mut ladder = Vec::new();
    for _ in 0..7 {
      seconds = next_backoff_seconds(seconds);
      ladder.push(seconds);
    }
    assert_eq!(ladder, [60.0, 120.0, 240.0, 480.0, 960.0, 1800.0, 1800.0]);
  }

  #[test]
  fn renewed_credentials_publish_privately_in_one_step() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(".credentials.json");
    fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"r","expiresAt":1},"kept":true}"#)
      .unwrap();

    write_renewed_credentials(&path, "new-access", Value::String("new-refresh".to_string()), Some(42)).unwrap();

    let published: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(published["claudeAiOauth"]["accessToken"], "new-access");
    assert_eq!(published["claudeAiOauth"]["refreshToken"], "new-refresh");
    assert_eq!(published["claudeAiOauth"]["expiresAt"], 42);
    assert_eq!(published["kept"], true);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // No staged sibling survives the swap.
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
  }

  #[test]
  fn a_renewed_expiry_reads_absolute_seconds_milliseconds_or_a_relative_span() {
    let seconds: Map<String, Value> = serde_json::from_str(r#"{"expires_at": 1700000000}"#).unwrap();
    assert_eq!(expires_at_ms(&seconds), Some(1_700_000_000_000));
    let millis: Map<String, Value> = serde_json::from_str(r#"{"expires_at": 1700000000000}"#).unwrap();
    assert_eq!(expires_at_ms(&millis), Some(1_700_000_000_000));
    let flag: Map<String, Value> = serde_json::from_str(r#"{"expires_at": true}"#).unwrap();
    assert_eq!(expires_at_ms(&flag), None);
    let relative: Map<String, Value> = serde_json::from_str(r#"{"expires_in": 3600}"#).unwrap();
    let stamp = expires_at_ms(&relative).unwrap();
    assert!((stamp as f64 - (now_seconds() + 3600.0) * 1000.0).abs() < 60_000.0);
  }

  #[test]
  fn abspath_resolves_dot_segments_lexically() {
    assert_eq!(abspath(Path::new("/a/b/../c/./d/")).unwrap(), PathBuf::from("/a/c/d"));
    assert_eq!(abspath(Path::new("/../a")).unwrap(), PathBuf::from("/a"));
  }

  #[test]
  fn the_transform_sorts_scoped_windows_and_skips_what_it_cannot_identify() {
    let raw = json!({
      "five_hour": {"utilization": 12.0, "resets_at": "2026-07-29T23:00:00+00:00"},
      "sevenDay": {"utilization": 34, "resetsAt": "2026-08-03T19:00:00+00:00"},
      "limits": [
        {"kind": "weekly-opus", "group": "weekly", "scope": {"model": {"display_name": "Opus"}}, "percent": 9.5, "resets_at": "2026-08-03T19:00:00+00:00"},
        {"group": "session", "scope": {"model": {"display_name": "Sonnet"}}, "percent": 4},
        {"group": "monthly", "scope": {"model": {"display_name": "Haiku"}}, "percent": 1},
        {"group": "session", "scope": {"model": {}}, "percent": 2},
        {"group": "session", "scope": null, "percent": 50},
        "not an entry"
      ],
      "extra": {"utilization": 3}
    });
    let state = QuotaState::default();

    let usage = transform_response(&raw, "main", &state).unwrap();

    let windows = usage[PANEL_WINDOWS].as_array().unwrap();
    let shape: Vec<(i64, Option<&str>, Option<f64>)> = windows
      .iter()
      .map(|window| {
        (
          window[PANEL_WINDOW_MINUTES].as_i64().unwrap(),
          window.get(PANEL_SCOPE_LABEL).and_then(Value::as_str),
          window[PANEL_UTILIZATION].as_f64(),
        )
      })
      .collect();
    assert_eq!(
      shape,
      vec![
        (300, None, Some(12.0)),
        (300, Some("Sonnet"), Some(4.0)),
        (10080, None, Some(34.0)),
        (10080, Some("Opus"), Some(9.5)),
      ]
    );
    assert_eq!(windows[0][PANEL_RESETS_AT], json!("2026-07-29T23:00:00+00:00"));
    assert_eq!(windows[1].get(PANEL_RESETS_AT), Some(&json!("")));
    assert_eq!(usage[PANEL_PROVIDER], json!("claude"));
    assert!(usage[PANEL_FETCHED_AT].as_str().unwrap().ends_with("+00:00"));
  }

  #[test]
  fn a_window_whose_reset_passed_since_the_sample_is_marked_expired_on_the_copy_only() {
    let entry = json!({
      PANEL_FETCHED_AT: "2026-01-01T00:00:00+00:00",
      PANEL_WINDOWS: [
        {PANEL_WINDOW_MINUTES: 300, PANEL_UTILIZATION: 5.0, PANEL_RESETS_AT: "2026-01-01T03:00:00+00:00"},
        {PANEL_WINDOW_MINUTES: 10080, PANEL_UTILIZATION: 6.0, PANEL_RESETS_AT: "2026-01-07T00:00:00+00:00"},
      ],
    });
    let entry = entry.as_object().unwrap();
    let sampled = UtcDateTime::parse("2026-01-01T00:00:00+00:00").unwrap();
    let now = UtcDateTime::parse("2026-01-02T00:00:00+00:00").unwrap();

    let emitted = mark_expired_windows(entry, Some(sampled), now);

    let marks: Vec<Option<&Value>> =
      emitted[PANEL_WINDOWS].as_array().unwrap().iter().map(|window| window.get(PANEL_EXPIRED)).collect();
    assert_eq!(marks, vec![Some(&Value::Bool(true)), None]);
    assert!(entry[PANEL_WINDOWS].as_array().unwrap().iter().all(|window| window.get(PANEL_EXPIRED).is_none()));
  }

  fn host_at(home: &Path) -> ClaudeHost {
    ClaudeHost { home: home.to_path_buf(), claude_config_dir: home.join(".claude") }
  }

  fn accounts() -> QuotaAccounts {
    QuotaAccounts::new(Arc::new(ClaudeAccountState::default()), Arc::new(test_support::quota_http()))
  }

  fn pool_config(home: &Path, pool: &[(&str, &Path)]) -> CharlieBotConfig {
    test_support::load_config(home, pool, &home.join("worktrees"))
  }

  #[test]
  fn a_shared_label_skips_the_later_pool_entry() {
    let root = tempfile::tempdir().unwrap();
    let host = host_at(root.path());
    let first = root.path().join(".claude-invite-1");
    let second = root.path().join("accounts/invite-1");
    let cfg = pool_config(root.path(), &[("invite-1", &first), ("invite-1", &second)]);

    let labels: Vec<String> =
      accounts().derive(&host, &cfg).unwrap().iter().map(|account| account.label().to_string()).collect();

    assert_eq!(labels, ["main", "invite-1"]);
  }

  #[test]
  fn a_login_that_stays_keeps_its_account_and_backoff_and_a_dropped_one_is_forgotten() {
    let root = tempfile::tempdir().unwrap();
    let host = host_at(root.path());
    let invite_dir = root.path().join(".claude-invite-1");
    let accounts = accounts();
    let with_invite = pool_config(root.path(), &[("invite-1", &invite_dir)]);
    accounts.derive(&host, &with_invite).unwrap();
    let invite = accounts.by_dir.lock().unwrap().get(&invite_dir).cloned().unwrap();
    invite.arm_backoff();
    let armed_until = invite.round.lock().unwrap().backoff_until;

    accounts.derive(&host, &with_invite).unwrap();
    let again = accounts.by_dir.lock().unwrap().get(&invite_dir).cloned().unwrap();
    assert!(Arc::ptr_eq(&invite, &again));
    assert!(armed_until > 0.0);
    assert_eq!(again.round.lock().unwrap().backoff_until, armed_until);

    accounts.derive(&host, &pool_config(root.path(), &[])).unwrap();
    assert!(!accounts.by_dir.lock().unwrap().contains_key(&invite_dir));
    accounts.derive(&host, &with_invite).unwrap();
    let readded = accounts.by_dir.lock().unwrap().get(&invite_dir).cloned().unwrap();
    assert!(!Arc::ptr_eq(&invite, &readded));
    assert_eq!(readded.round.lock().unwrap().backoff_until, 0.0);
  }

  #[test]
  fn a_pool_login_without_credentials_is_marked_and_expired_windows_mark_only_the_emitted_copy() {
    let root = tempfile::tempdir().unwrap();
    let host = host_at(root.path());
    let pool_dir = root.path().join(".claude-invite-1");
    let cfg = pool_config(root.path(), &[("invite-1", &pool_dir)]);
    let accounts = accounts();
    let logins = accounts.derive(&host, &cfg).unwrap();
    let (main, invite) = (&logins[0], &logins[1]);
    let entry_json = json!({
      "provider": "claude",
      "account": "invite-1",
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "windows": [
        {"window_minutes": 300, "utilization": 5.0, "resets_at": "2026-01-01T03:00:00+00:00"},
        {"window_minutes": 10080, "utilization": 6.0, "resets_at": "2026-01-07T00:00:00+00:00"},
      ],
    });
    let mut entry = entry_json.as_object().unwrap().clone();

    invite.mark_login_required(&mut entry);
    assert_eq!(entry["login_required"], json!(pool_dir.display().to_string()));
    fs::create_dir_all(&pool_dir).unwrap();
    fs::write(pool_dir.join(CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#).unwrap();
    invite.mark_login_required(&mut entry);
    assert!(!entry.contains_key("login_required"));

    let emitted = invite.mark_expired(&entry, UtcDateTime::parse("2026-01-02T00:00:00+00:00").unwrap());
    let marks: Vec<Option<&Value>> =
      emitted[PANEL_WINDOWS].as_array().unwrap().iter().map(|window| window.get(PANEL_EXPIRED)).collect();
    assert_eq!(marks, vec![Some(&Value::Bool(true)), None]);
    assert!(entry[PANEL_WINDOWS].as_array().unwrap().iter().all(|window| window.get(PANEL_EXPIRED).is_none()));

    main.mark_login_required(&mut entry);
    assert!(!entry.contains_key("login_required"));
  }
}
