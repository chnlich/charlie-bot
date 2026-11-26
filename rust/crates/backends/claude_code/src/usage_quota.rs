//! Claude Code's quota accounts for the quota panel.
//!
//! One account reads one login directory's usage from the Anthropic OAuth usage endpoint. The accounts are the default
//! login directory (label `main`) followed by the `accounts.claude` pool. Every derivation reads the live config, and an
//! account whose login directory stays keeps its object, and with it its backoff.
//!
//! An account records its panel reading in the shared [`ClaudeAccountState`], keeps the User-Agent probe's result there
//! too, and marks its cached entry with `login_required` and the expired windows.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use backon::{BackoffBuilder, ExponentialBuilder};
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::fsutil;
use charliebot_core::headers::bearer_value;
use charliebot_core::oauth::{
  ACCESS_TOKEN_FIELD, CLIENT_ID_FIELD, EXPIRES_IN_FIELD, GRANT_TYPE_FIELD, REFRESH_TOKEN_FIELD, REFRESH_TOKEN_GRANT,
};
use charliebot_core::text_repr::is_truthy;
use charliebot_core::timefmt::utc_now_iso;
use charliebot_registry::usage::{
  DEFAULT_ACCOUNT_LABEL, FIVE_HOUR_WINDOW_MINUTES, PANEL_FETCHED_AT, PANEL_PROVIDER, PANEL_RESETS_AT,
  PANEL_SCOPE_LABEL, PANEL_UTILIZATION, PANEL_WINDOW_MINUTES, PANEL_WINDOWS, QuotaAccount, UNKNOWN_LIMIT_SHAPE_EVENT,
  WEEKLY_WINDOW_MINUTES, as_utilization, mark_expired_windows,
};
use charliebot_store::metadata::UtcDateTime;
use futures_util::future::BoxFuture;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde_json::{Map, Value, json};

pub use super::claude_accounts::QuotaState;
use super::claude_accounts::{self, ClaudeAccountState};
use super::claude_config::{ACCOUNTS_SECTION_KEY, AccountsConfig, ClaudeAccount};
use super::login_dirs::{CREDENTIALS_FILE, ClaudeHost};

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
const BACKOFF_MIN_DELAY: Duration = Duration::from_secs(60);
const BACKOFF_CAP_SECONDS: f64 = 30.0 * 60.0;
/// How long a renewal waits for the Claude CLI's refresh lock before it gives up with `lock_busy`.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// A lock untouched for this long is stale, as the Claude CLI's refresh lock treats it.
const LOCK_STALE: Duration = Duration::from_secs(60);
/// How often the lock's holder refreshes its modification time while it holds the lock.
const LOCK_UPDATE: Duration = Duration::from_secs(5);
/// The Claude CLI's refresh lock, inside the login directory.
const REFRESH_LOCK: &str = ".oauth_refresh.lock";
/// The request and response fields the renewal reads and writes, beside the shared OAuth vocabulary.
const SCOPE_FIELD: &str = "scope";
const SCOPES_FIELD: &str = "scopes";
/// The credentials file's own name for the refresh token; the wire name is [`REFRESH_TOKEN_FIELD`].
const REFRESH_TOKEN_FILE_FIELD: &str = "refreshToken";
const REFRESH_TOKEN_EXPIRES_IN_FIELD: &str = "refresh_token_expires_in";
const REFRESH_TOKEN_EXPIRES_AT_FIELD: &str = "refreshTokenExpiresAt";

const PROVIDER: &str = "claude";
const PROVIDER_LABEL: &str = "Claude";
const NO_DATA: &str = "no data";

/// The plan windows the response names by field: `(camel name, snake name, window length in minutes)`.
const CLAUDE_WINDOW_FIELDS: [(&str, &str, i64); 2] =
  [("fiveHour", "five_hour", FIVE_HOUR_WINDOW_MINUTES as i64), ("sevenDay", "seven_day", WEEKLY_WINDOW_MINUTES)];
/// The window length of each `limits[].group` the response reports. Any other group is skipped with a warning.
const LIMIT_GROUP_WINDOW_MINUTES: [(&str, i64); 2] =
  [("session", FIVE_HOUR_WINDOW_MINUTES as i64), ("weekly", WEEKLY_WINDOW_MINUTES)];

static CLI_VERSION: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("the version pattern is valid"));

/// The HTTP client and the two endpoints the quota accounts call. Production builds it from the environment and the URL
/// constants. Tests point the endpoints at a local server.
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

  /// The Claude logins in panel order, the default login first and then the pool. A login whose directory stays in
  /// the config keeps its account object, and a login the config dropped is forgotten. `quota_accounts` returns this
  /// list.
  pub fn derive(&self, host: &ClaudeHost, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    let pool: Vec<(String, String)> = cfg
      .section::<AccountsConfig>(ACCOUNTS_SECTION_KEY)
      .claude
      .iter()
      .map(|account| (account.label.clone(), account.config_dir.clone()))
      .collect();
    let mut pool_dirs = HashMap::new();
    for (label, raw) in &pool {
      pool_dirs.insert(label.clone(), abspath(&host.expand(raw))?.to_string_lossy().into_owned());
    }
    let logins = derive_login_dirs(host, &pool)?;
    // The improve hosts whose launch directories a renewal's hold check reads, once for every account of this derive.
    let hold_hosts = claude_accounts::improve_hosts(cfg);

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
      account.set_hold_hosts(hold_hosts.clone());
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
  /// The improve hosts a renewal's hold check reads, from `host_auth` as the last `derive` read it.
  hold_hosts: Mutex<Vec<String>>,
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
      hold_hosts: Mutex::new(Vec::new()),
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
      hold_hosts: Mutex::new(self.hold_hosts()),
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

  fn set_hold_hosts(&self, hosts: Vec<String>) {
    *self.hold_hosts.lock().expect("the hold hosts are not poisoned") = hosts;
  }

  fn hold_hosts(&self) -> Vec<String> {
    self.hold_hosts.lock().expect("the hold hosts are not poisoned").clone()
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

  /// One round. The usage is fetched. A 401 renews the token once and retries once. A second 401 or a 429 backs off.
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
      .header(AUTHORIZATION, bearer_value(access_token))
      .header("anthropic-beta", ANTHROPIC_BETA)
      .header(USER_AGENT, user_agent)
      .timeout(HTTP_OAUTH_TIMEOUT)
      .send()
      .await
      .with_context(|| format!("requesting the usage of {}", self.label))
  }

  /// A usable access token after a 401. The credentials file is shared with the Claude CLI, which rotates the refresh
  /// token whenever it renews. A file that already holds another access token is used before any refresh call. A file
  /// that still holds the refused token is renewed only under the renewal's own rules: a short life, and no live keyed
  /// round holding the login.
  async fn reauthenticate(&self, failed_token: &str) -> anyhow::Result<Option<String>> {
    let Some(credentials) = read_credentials(&self.state.quota, &self.credentials_path)? else {
      self.set_error("credentials not found");
      return Ok(None);
    };
    if credentials.access_token != failed_token {
      return Ok(Some(credentials.access_token));
    }
    let login_dir = self.credentials_path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let renewal = renew_login(
      &self.state.quota,
      &self.http,
      &self.label,
      &login_dir,
      &self.hold_hosts(),
      failed_token,
      &RenewalTrigger::Usage401,
    )
    .await?;
    match renewal {
      Renewal::Usable(access_token) => Ok(Some(access_token)),
      Renewal::Skipped { .. } | Renewal::Failed { .. } => {
        self.set_error("token refresh failed");
        Ok(None)
      }
    }
  }
}

/// The two callers of a renewal, as the renewal's log lines name them.
#[derive(Clone, Debug, PartialEq)]
pub enum RenewalTrigger {
  /// The usage endpoint refused the login's access token.
  Usage401,
  /// A keyed round's placement renews a login's token before it starts the round.
  KeyedRound { run_id: String },
}

impl RenewalTrigger {
  fn label(&self) -> &'static str {
    match self {
      Self::Usage401 => "usage_401",
      Self::KeyedRound { .. } => "keyed_round",
    }
  }

  fn run_id(&self) -> &str {
    match self {
      Self::Usage401 => "",
      Self::KeyedRound { run_id } => run_id,
    }
  }
}

/// A renewal's answer.
#[derive(PartialEq)]
pub enum Renewal {
  /// The login holds a usable access token: renewed now, or renewed by another writer while this call waited.
  Usable(String),
  /// Nothing was renewed. The skip event carries the reason (`band`, `held`, `unreadable` or `lock_busy`) and, for a
  /// hold, the holder's name.
  Skipped { reason: &'static str, holder: Option<String> },
  /// The renewal was attempted and failed. The text names the failure for a refusal and carries no credential.
  Failed { detail: String },
}

impl std::fmt::Debug for Renewal {
  /// The access token is left out: a renewal's answer reaches logs and panic messages.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Usable(_) => f.write_str("Usable(<access token>)"),
      Self::Skipped { reason, holder } => {
        f.debug_struct("Skipped").field("reason", reason).field("holder", holder).finish()
      }
      Self::Failed { detail } => f.debug_struct("Failed").field("detail", detail).finish(),
    }
  }
}

/// The OAuth tokens and their life in a login's credentials file, as a renewal reads them.
struct LoginFile {
  access_token: String,
  refresh_token: Option<String>,
  expires_at_ms: i64,
  scopes: Option<Vec<String>>,
}

/// Renews one login's access token for `trigger`. The token is renewed only while it lives under
/// `RENEWABLE_TOKEN_MAX_SECONDS` and no live keyed round holds the login; the Claude CLI's two refresh locks serialize
/// the renewal with the CLI's own, and the login file is read again inside them. `observed_token` is the access token
/// the caller saw before it asked. This is the only code besides the Claude CLI that writes a login's tokens.
pub async fn renew_login(
  quota: &QuotaState,
  http: &QuotaHttp,
  label: &str,
  login_dir: &Path,
  hosts: &[String],
  observed_token: &str,
  trigger: &RenewalTrigger,
) -> anyhow::Result<Renewal> {
  let path = login_dir.join(CREDENTIALS_FILE);
  let Some(file) = read_login_file(&path)? else {
    return Ok(skipped(label, trigger, "unreadable", Some("unreadable:login_file".to_string())));
  };
  if file.expires_at_ms - now_millis() >= claude_accounts::RENEWABLE_TOKEN_MAX_SECONDS * 1000 {
    return Ok(skipped(label, trigger, "band", None));
  }
  let hold_dir = login_dir.to_path_buf();
  let check = claude_accounts::HoldCheck::new(hosts.to_vec(), claude_accounts::now_or(None));
  let hold = tokio::task::spawn_blocking(move || check.hold(&hold_dir)).await.context("the hold check task failed")?;
  match hold {
    claude_accounts::Hold::Free => {}
    claude_accounts::Hold::Held(holder) => return Ok(skipped(label, trigger, "held", Some(holder))),
    claude_accounts::Hold::Unreadable(source) => {
      return Ok(skipped(label, trigger, "unreadable", Some(format!("unreadable:{source}"))));
    }
  }
  let lock_dir = login_dir.to_path_buf();
  let locks = tokio::task::spawn_blocking(move || RefreshLocks::acquire(&lock_dir, LOCK_WAIT))
    .await
    .context("the refresh lock task failed")??;
  let Some(_locks) = locks else {
    return Ok(skipped(label, trigger, "lock_busy", None));
  };
  // Another writer had the lock: the file may already hold a token that works.
  let Some(current) = read_login_file(&path)? else {
    return Ok(skipped(label, trigger, "unreadable", Some("unreadable:login_file".to_string())));
  };
  if !current.access_token.is_empty() && current.access_token != observed_token {
    return Ok(Renewal::Usable(current.access_token));
  }
  let Some(refresh_token) = current.refresh_token.clone() else {
    return Ok(failed(label, trigger, &path, "the login file holds no refresh token"));
  };
  let Some(scopes) = current.scopes.as_ref().filter(|scopes| !scopes.is_empty()) else {
    return Ok(failed(label, trigger, &path, "the login file holds no scopes"));
  };
  let user_agent = user_agent(quota).await;
  let response = match http
    .client
    .post(&http.token_url)
    .json(&json!({
      (GRANT_TYPE_FIELD): REFRESH_TOKEN_GRANT,
      (REFRESH_TOKEN_FIELD): refresh_token,
      (CLIENT_ID_FIELD): CLIENT_ID,
      (SCOPE_FIELD): scopes.join(" "),
    }))
    .header(USER_AGENT, user_agent)
    .timeout(HTTP_OAUTH_TIMEOUT)
    .send()
    .await
  {
    Ok(response) => response,
    Err(error) => return Ok(failed(label, trigger, &path, &format!("the request failed: {error}"))),
  };
  let status = response.status();
  if status.is_client_error() || status.is_server_error() {
    let body = response.text().await.unwrap_or_default();
    tracing::warn!(
      target: "charliebot_agents",
      account = label,
      trigger = trigger.label(),
      status_code = status.as_u16(),
      body = %body.chars().take(200).collect::<String>(),
      "ext_usage_token_refresh_failed"
    );
    return Ok(Renewal::Failed { detail: format!("refused ({})", status.as_u16()) });
  }
  let Ok(token) = response.json::<Value>().await else {
    return Ok(failed(label, trigger, &path, "the token response is not JSON"));
  };
  let Some(token) = token.as_object() else {
    return Ok(failed(label, trigger, &path, "the token response is not a JSON object"));
  };
  let Some(new_access) = token.get(ACCESS_TOKEN_FIELD).and_then(Value::as_str).filter(|token| !token.is_empty()) else {
    return Ok(failed(label, trigger, &path, "the token response names no access_token"));
  };
  let new_access = new_access.to_string();
  let Some(new_expires) = expires_at_ms(token) else {
    return Ok(failed(label, trigger, &path, "returned neither expires_in nor expires_at"));
  };
  let new_refresh = token.get(REFRESH_TOKEN_FIELD).cloned().unwrap_or_else(|| json!(refresh_token));
  let new_scopes =
    token.get(SCOPE_FIELD).and_then(Value::as_str).map(|scope| scope.split_whitespace().map(str::to_string).collect());
  let refresh_expires = token
    .get(REFRESH_TOKEN_EXPIRES_IN_FIELD)
    .and_then(Value::as_f64)
    .map(|seconds| now_millis() + (seconds * 1000.0) as i64);
  let write_path = path.clone();
  let access = new_access.clone();
  tokio::task::spawn_blocking(move || {
    write_renewed_credentials(&write_path, &access, new_refresh, new_expires, refresh_expires, new_scopes)
  })
  .await
  .context("the credentials write task failed")??;
  tracing::info!(
    target: "charliebot_agents",
    account = label,
    trigger = trigger.label(),
    path = %path.display(),
    "ext_usage_token_refreshed"
  );
  Ok(Renewal::Usable(new_access))
}

fn skipped(label: &str, trigger: &RenewalTrigger, reason: &'static str, holder: Option<String>) -> Renewal {
  tracing::info!(
    target: "charliebot_agents",
    account = label,
    trigger = trigger.label(),
    run_id = trigger.run_id(),
    reason = reason,
    holder = holder.as_deref().unwrap_or(""),
    "claude_login_renewal_skipped"
  );
  Renewal::Skipped { reason, holder }
}

fn failed(label: &str, trigger: &RenewalTrigger, path: &Path, detail: &str) -> Renewal {
  tracing::warn!(
    target: "charliebot_agents",
    account = label,
    trigger = trigger.label(),
    run_id = trigger.run_id(),
    path = %path.display(),
    reason = detail,
    "ext_usage_token_refresh_failed"
  );
  Renewal::Failed { detail: detail.to_string() }
}

/// The Claude CLI's two refresh locks, taken in its order: `<login>/.oauth_refresh.lock`, then `<login>.lock`. Both
/// are directories whose modification time shows a live holder; a lock untouched for [`LOCK_STALE`] may be taken over,
/// and the holder refreshes the time every [`LOCK_UPDATE`]. Dropping the value removes both locks and stops the
/// toucher at once.
struct RefreshLocks {
  paths: [PathBuf; 2],
  /// Dropping this sender ends the toucher's wait.
  stop: Option<std::sync::mpsc::Sender<()>>,
  toucher: Option<std::thread::JoinHandle<()>>,
}

impl RefreshLocks {
  /// Takes both locks for `login_dir`, waiting up to `wait` for the first. `Ok(None)` is the CLI's own give-up: the
  /// wait ran out, or the second lock is held.
  fn acquire(login_dir: &Path, wait: Duration) -> anyhow::Result<Option<Self>> {
    let first = login_dir.join(REFRESH_LOCK);
    let second = PathBuf::from(format!("{}.lock", login_dir.display()));
    let deadline = Instant::now() + wait;
    loop {
      match fs::create_dir(&first) {
        Ok(()) => break,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
          if lock_is_stale(&first)? {
            let _ = fs::remove_dir_all(&first);
            continue;
          }
          let remaining = deadline.saturating_duration_since(Instant::now());
          if remaining.is_zero() {
            return Ok(None);
          }
          std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
        Err(error) => return Err(error).with_context(|| format!("taking {}", first.display())),
      }
    }
    match fs::create_dir(&second) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
        let _ = fs::remove_dir(&first);
        return Ok(None);
      }
      Err(error) => {
        let _ = fs::remove_dir(&first);
        return Err(error).with_context(|| format!("taking {}", second.display()));
      }
    }
    let locks = Self { paths: [first, second], stop: None, toucher: None };
    Ok(Some(locks.touched()))
  }

  /// Starts the thread that keeps both locks fresh until the sender drops.
  fn touched(mut self) -> Self {
    let paths = self.paths.clone();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    self.stop = Some(stop);
    self.toucher = Some(std::thread::spawn(move || {
      while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(LOCK_UPDATE) {
        for path in &paths {
          if let Ok(file) = fs::File::open(path) {
            let _ = file.set_modified(SystemTime::now());
          }
        }
      }
    }));
    self
  }
}

impl Drop for RefreshLocks {
  fn drop(&mut self) {
    self.stop.take();
    if let Some(toucher) = self.toucher.take() {
      let _ = toucher.join();
    }
    for path in self.paths.iter().rev() {
      let _ = fs::remove_dir(path);
    }
  }
}

/// Whether a held lock's modification time is older than [`LOCK_STALE`]. A lock that vanished between its observation
/// and this read is not stale: the next attempt takes the free name, or waits again.
fn lock_is_stale(path: &Path) -> anyhow::Result<bool> {
  let metadata = match fs::metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(error) => {
      return Err(error).with_context(|| format!("reading the modification time of {}", path.display()));
    }
  };
  let modified = metadata.modified().with_context(|| format!("reading the modification time of {}", path.display()))?;
  Ok(modified.elapsed().is_ok_and(|age| age > LOCK_STALE))
}

/// The login file's tokens and life, or `None` when it is missing, is not JSON, or names no access token with a
/// numeric `expiresAt`. A renewal calls that unreadable and writes nothing.
fn read_login_file(path: &Path) -> anyhow::Result<Option<LoginFile>> {
  let Ok(text) = fs::read_to_string(path) else { return Ok(None) };
  let Ok(data) = serde_json::from_str::<Value>(&text) else { return Ok(None) };
  let Some(oauth) = data.get("claudeAiOauth").and_then(Value::as_object) else { return Ok(None) };
  let Some(access_token) = oauth.get("accessToken").filter(|value| is_truthy(value)).and_then(Value::as_str) else {
    return Ok(None);
  };
  let Some(expires_at_ms) = oauth.get("expiresAt").and_then(Value::as_i64) else { return Ok(None) };
  let scopes = oauth
    .get(SCOPES_FIELD)
    .and_then(Value::as_array)
    .map(|values| values.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<String>>());
  let refresh_token =
    oauth.get(REFRESH_TOKEN_FILE_FIELD).filter(|value| is_truthy(value)).and_then(Value::as_str).map(str::to_string);
  Ok(Some(LoginFile { access_token: access_token.to_string(), refresh_token, expires_at_ms, scopes }))
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

  fn mark_expired(&self, entry: &Map<String, Value>, now: UtcDateTime) -> anyhow::Result<Map<String, Value>> {
    let sampled = entry.get(PANEL_FETCHED_AT).and_then(Value::as_str).and_then(|text| UtcDateTime::parse(text).ok());
    Ok(mark_expired_windows(entry, sampled, now))
  }
}

/// The logins the quota panel lists: the default login directory under label `main`, then the pool entries under
/// their labels. An entry that names a directory already listed is skipped, and a label already taken is logged and
/// skipped.
fn derive_login_dirs(host: &ClaudeHost, pool: &[(String, String)]) -> anyhow::Result<Vec<(String, PathBuf)>> {
  let default = abspath(&host.default_dir())?;
  let mut seen = HashSet::from([default.clone()]);
  let mut labels = HashSet::from([DEFAULT_ACCOUNT_LABEL.to_string()]);
  let mut logins = vec![(DEFAULT_ACCOUNT_LABEL.to_string(), default)];
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

/// The path made absolute: joined to the working directory when it is relative, with `.` and `..` resolved
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

/// The access token of a credentials file.
struct Credentials {
  access_token: String,
}

/// The access token in a credentials file, or `None` after a logged warning when the file is missing or holds no
/// access token. A file that is not JSON, or whose `claudeAiOauth` is not an object, fails the round.
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
    Some(value) if is_truthy(value) => {
      value.as_str().with_context(|| format!("{}: accessToken is not a string", path.display()))?.to_string()
    }
    Some(_) | None => {
      warn_credentials_once(quota, "ext_usage_no_access_token", path);
      return Ok(None);
    }
  };
  // The expiry and the refresh token are not read here. A renewal reads the whole file (`read_login_file`) after it
  // takes the CLI's locks, so it renews on the file's current life and never on a stale copy of it.
  quota.warned.lock().expect("the warnings are not poisoned").credentials.retain(|(_, known)| known != path);
  Ok(Some(Credentials { access_token }))
}

/// The error for a non-success status: its class, the code and reason, the URL, and the MDN page for the code.
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
    "ext_usage_no_access_token" => {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), "ext_usage_no_access_token");
    }
    _ => unreachable!("warn_credentials_once has no warning for the event {event:?}"),
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

/// The renewed token's expiry as the millisecond stamp the CLI's file uses. An absolute `expires_at` may be in
/// seconds. Otherwise `expires_in` is relative to now. A response that carries neither has no expiry, and the renewal
/// writes nothing.
fn expires_at_ms(token: &Map<String, Value>) -> Option<i64> {
  if let Some(absolute) = token.get("expires_at").and_then(Value::as_f64) {
    return Some(if absolute > 1e11 { absolute as i64 } else { (absolute * 1000.0) as i64 });
  }
  token.get(EXPIRES_IN_FIELD).and_then(Value::as_f64).map(|relative| now_millis() + (relative * 1000.0) as i64)
}

/// Replaces the renewed tokens in a credentials file, in the fields the Claude CLI writes: the access token, the
/// refresh token, the access token's expiry, and the refresh token's expiry and scopes when the response names them.
/// The file is read again and its other keys are kept in place. The new file replaces the old in one step.
fn write_renewed_credentials(
  path: &Path,
  access_token: &str,
  refresh_token: Value,
  expires_at: i64,
  refresh_token_expires_at: Option<i64>,
  scopes: Option<Vec<String>>,
) -> anyhow::Result<()> {
  let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let mut data: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
  let oauth = data
    .get_mut("claudeAiOauth")
    .and_then(Value::as_object_mut)
    .with_context(|| format!("{} holds no claudeAiOauth object", path.display()))?;
  oauth.insert("accessToken".to_string(), json!(access_token));
  oauth.insert(REFRESH_TOKEN_FILE_FIELD.to_string(), refresh_token);
  oauth.insert("expiresAt".to_string(), json!(expires_at));
  if let Some(refresh_token_expires_at) = refresh_token_expires_at {
    oauth.insert(REFRESH_TOKEN_EXPIRES_AT_FIELD.to_string(), json!(refresh_token_expires_at));
  }
  if let Some(scopes) = scopes {
    oauth.insert(SCOPES_FIELD.to_string(), json!(scopes));
  }
  let text = serde_json::to_string_pretty(&data).context("serializing the renewed credentials")?;
  // The token never sits at its final path readable by anyone but the owner.
  fsutil::atomic_write_mode(path, text.as_bytes(), 0o600).with_context(|| format!("writing {}", path.display()))
}

/// The window length of a plan window, or of a scoped one, with the scope label it sorts by (empty for a plan window).
struct Window {
  window_minutes: i64,
  scope_label: String,
  entry: Value,
}

/// The usage the panel stores, built from one response. The plan windows come first in their fixed order, then the
/// scoped windows, all sorted by window length and scope label. A reading timestamp and the provider complete it.
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

/// One plan window, read from the `camel` key, or from `snake` when `camel` is absent. A falsy value reads as an
/// empty object.
fn plan_window(raw: &Map<String, Value>, camel: &str, snake: &str, window_minutes: i64) -> anyhow::Result<Window> {
  let no_bucket = Map::new();
  let bucket = match raw.get(camel).or_else(|| raw.get(snake)) {
    Some(value) if is_truthy(value) => {
      value.as_object().with_context(|| format!("the {camel} window is not an object"))?
    }
    Some(_) | None => &no_bucket,
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
      Some(value) if is_truthy(value) => value.clone(),
      Some(_) | None => json!(""),
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
        "{}",
        UNKNOWN_LIMIT_SHAPE_EVENT
      ),
      LimitSlot::Index(slot) => tracing::warn!(
        target: "charliebot_agents",
        provider = PROVIDER,
        account = account,
        slot = slot,
        reason = reason,
        "{}",
        UNKNOWN_LIMIT_SHAPE_EVENT
      ),
    }
  }
}

/// The next backoff: the ladder's first delay above `previous`, so 60 seconds first, doubling, capped at 30 minutes.
fn next_backoff_seconds(previous: f64) -> f64 {
  if previous >= BACKOFF_CAP_SECONDS {
    return BACKOFF_CAP_SECONDS;
  }
  ExponentialBuilder::default()
    .with_min_delay(BACKOFF_MIN_DELAY)
    .with_max_delay(Duration::from_secs_f64(BACKOFF_CAP_SECONDS))
    .with_factor(2.0)
    .without_max_times()
    .build()
    .map(|delay| delay.as_secs_f64())
    .find(|delay| *delay > previous)
    .expect("the delay ladder has a step above any earlier delay below its cap")
}

fn now_seconds() -> f64 {
  SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is after the Unix epoch").as_secs_f64()
}

fn now_millis() -> i64 {
  SystemTime::now().duration_since(UNIX_EPOCH).expect("the system clock is after the Unix epoch").as_millis() as i64
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use charliebot_registry::usage::PANEL_EXPIRED;

  use super::*;
  use crate::test_support;

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

    write_renewed_credentials(
      &path,
      "new-access",
      Value::String("new-refresh".to_string()),
      42,
      Some(43),
      Some(vec!["user:inference".to_string()]),
    )
    .unwrap();

    let published: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(published["claudeAiOauth"]["accessToken"], "new-access");
    assert_eq!(published["claudeAiOauth"]["refreshToken"], "new-refresh");
    assert_eq!(published["claudeAiOauth"]["expiresAt"], 42);
    assert_eq!(published["claudeAiOauth"]["refreshTokenExpiresAt"], 43);
    assert_eq!(published["claudeAiOauth"]["scopes"], json!(["user:inference"]));
    assert_eq!(published["kept"], true);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // No staged sibling survives the swap.
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
  }

  #[test]
  fn a_lock_holder_refreshes_its_time_and_a_stale_lock_is_taken_over() {
    let root = tempfile::tempdir().unwrap();
    let login = root.path().join("login");
    fs::create_dir(&login).unwrap();
    let locks = RefreshLocks::acquire(&login, Duration::ZERO).unwrap().expect("the locks are free");
    assert!(login.join(REFRESH_LOCK).is_dir());
    assert!(PathBuf::from(format!("{}.lock", login.display())).is_dir());
    // The second lock is held: another writer is told to give up at once, as the CLI's ELOCKED does.
    assert!(RefreshLocks::acquire(&login, Duration::ZERO).unwrap().is_none());
    drop(locks);
    assert!(!login.join(REFRESH_LOCK).exists());
    assert!(!PathBuf::from(format!("{}.lock", login.display())).exists());

    // A lock untouched past LOCK_STALE may be taken over.
    fs::create_dir(login.join(REFRESH_LOCK)).unwrap();
    let old = SystemTime::now() - LOCK_STALE - Duration::from_secs(1);
    fs::File::open(login.join(REFRESH_LOCK)).unwrap().set_modified(old).unwrap();
    let taken = RefreshLocks::acquire(&login, Duration::ZERO).unwrap();
    assert!(taken.is_some(), "a stale lock was not taken over");
  }

  #[test]
  fn a_lock_held_by_a_live_writer_wins_the_wait_and_reports_busy() {
    let root = tempfile::tempdir().unwrap();
    let login = root.path().join("login");
    fs::create_dir(&login).unwrap();
    fs::create_dir(login.join(REFRESH_LOCK)).unwrap();

    let started = Instant::now();
    let locks = RefreshLocks::acquire(&login, Duration::from_millis(50)).unwrap();

    assert!(locks.is_none(), "a live lock was taken over");
    assert!(started.elapsed() >= Duration::from_millis(50), "the wait ended before its bound");
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

    let emitted = invite.mark_expired(&entry, UtcDateTime::parse("2026-01-02T00:00:00+00:00").unwrap()).unwrap();
    let marks: Vec<Option<&Value>> =
      emitted[PANEL_WINDOWS].as_array().unwrap().iter().map(|window| window.get(PANEL_EXPIRED)).collect();
    assert_eq!(marks, vec![Some(&Value::Bool(true)), None]);
    assert!(entry[PANEL_WINDOWS].as_array().unwrap().iter().all(|window| window.get(PANEL_EXPIRED).is_none()));

    main.mark_login_required(&mut entry);
    assert!(!entry.contains_key("login_required"));
  }

  /// The `accounts.claude` pool and the improve hosts of the live config.
  #[derive(Default, serde::Deserialize)]
  struct LiveConfig {
    #[serde(default)]
    accounts: LiveAccounts,
    #[serde(default)]
    host_auth: LiveHostAuth,
  }

  #[derive(Default, serde::Deserialize)]
  struct LiveAccounts {
    #[serde(default)]
    claude: Vec<LiveAccount>,
  }

  #[derive(serde::Deserialize)]
  struct LiveAccount {
    label: String,
    config_dir: String,
  }

  #[derive(Default, serde::Deserialize)]
  struct LiveHostAuth {
    #[serde(default)]
    browser_logins: Vec<LiveHostLogin>,
  }

  #[derive(serde::Deserialize)]
  struct LiveHostLogin {
    #[serde(default)]
    host: Option<String>,
  }

  /// A live login's config directory: `CHARLIEBOT_LIVE_RENEW_LOGIN` names its pool alias in the live config.
  fn live_login() -> (String, PathBuf, Vec<String>) {
    let alias = std::env::var("CHARLIEBOT_LIVE_RENEW_LOGIN")
      .expect("set CHARLIEBOT_LIVE_RENEW_LOGIN to the pool alias of an idle login under 3 h 50 min of life");
    let home = std::env::var_os("CHARLIEBOT_HOME")
      .filter(|value| !value.is_empty())
      .map_or_else(|| PathBuf::from(std::env::var("HOME").expect("HOME is set")).join(".charliebot"), PathBuf::from);
    let text = fs::read_to_string(home.join("config.yaml")).expect("the live config.yaml is readable");
    let config: LiveConfig = serde_yaml::from_str(&text).expect("the live config.yaml parses");
    let account = config
      .accounts
      .claude
      .iter()
      .find(|account| account.label == alias)
      .unwrap_or_else(|| panic!("the live config names no login {alias}"));
    let dir = match account.config_dir.strip_prefix("~/") {
      Some(rest) => home.parent().expect("the profile home has a parent").join(rest),
      None => PathBuf::from(&account.config_dir),
    };
    let hosts: Vec<String> = config.host_auth.browser_logins.iter().filter_map(|login| login.host.clone()).collect();
    (alias, dir, hosts)
  }

  /// A Claude CLI process that uses `config_dir`'s login, other than the process `except`, described by its PID and
  /// executable. Only a Claude CLI uses the token: a process that merely inherited `CLAUDE_CONFIG_DIR`, such as a
  /// shell, `sleep` or `kubectl`, does not.
  fn claude_cli_using(config_dir: &Path, except: Option<u32>) -> Option<String> {
    let entries = fs::read_dir("/proc").ok()?;
    let prefix = format!("{}=", crate::login_dirs::CLAUDE_CONFIG_DIR_ENV);
    for entry in entries.flatten() {
      let name = entry.file_name();
      let Some(pid) = name.to_str().filter(|name| name.bytes().all(|byte| byte.is_ascii_digit())) else { continue };
      let Ok(pid_number) = pid.parse::<u32>() else { continue };
      if except == Some(pid_number) {
        continue;
      }
      let Ok(data) = fs::read(entry.path().join("environ")) else { continue };
      let carries = data.split(|byte| *byte == 0).any(|item| {
        std::str::from_utf8(item)
          .ok()
          .and_then(|text| text.strip_prefix(&prefix))
          .is_some_and(|value| Path::new(value) == config_dir)
      });
      if carries && is_claude_cli(&entry.path()) {
        let exe = fs::read_link(entry.path().join("exe"))
          .map_or_else(|_| "<executable unreadable>".to_string(), |path| path.display().to_string());
        return Some(format!("pid {pid_number} ({exe})"));
      }
    }
    None
  }

  /// Whether the process at `proc_dir` is a Claude CLI: its argv[0] is `claude`, or its executable sits in a
  /// `claude/versions` directory, where the native CLI's files are named for their version.
  fn is_claude_cli(proc_dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let argv0_is_claude = fs::read(proc_dir.join("cmdline")).ok().is_some_and(|data| {
      data.split(|byte| *byte == 0).next().is_some_and(|first| {
        Path::new(std::ffi::OsStr::from_bytes(first)).file_name().is_some_and(|name| name == "claude")
      })
    });
    let in_versions = fs::read_link(proc_dir.join("exe"))
      .ok()
      .is_some_and(|exe| exe.parent().is_some_and(|dir| dir.ends_with("claude/versions")));
    argv0_is_claude || in_versions
  }

  fn token_of(dir: &Path) -> String {
    let text = fs::read_to_string(dir.join(CREDENTIALS_FILE)).expect("the login file is readable");
    let data: Value = serde_json::from_str(&text).expect("the login file parses");
    data["claudeAiOauth"]["accessToken"].as_str().expect("the login file names an access token").to_string()
  }

  /// The idle check names a Claude CLI that uses a login, skips the process it is told to skip, and ignores a process
  /// that only inherits the login's config directory. A silent miss would let a live run renew a login a CLI uses.
  #[test]
  #[ignore = "integration"]
  fn the_idle_check_counts_only_a_claude_cli_using_the_login() {
    let root = tempfile::tempdir().unwrap();
    let busy = root.path().join("busy");
    let idle = root.path().join("idle");
    fs::create_dir(&busy).unwrap();
    fs::create_dir(&idle).unwrap();
    let fake_cli = root.path().join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &fake_cli).unwrap();
    let env = crate::login_dirs::CLAUDE_CONFIG_DIR_ENV;
    let mut cli = std::process::Command::new(&fake_cli).arg("30").env(env, &busy).spawn().unwrap();
    let mut inherited = std::process::Command::new("/bin/sleep").arg("30").env(env, &idle).spawn().unwrap();

    let named = claude_cli_using(&busy, None);
    let skipped = claude_cli_using(&busy, Some(cli.id()));
    let inherits_only = claude_cli_using(&idle, None);
    cli.kill().unwrap();
    inherited.kill().unwrap();
    cli.wait().unwrap();
    inherited.wait().unwrap();

    let named = named.expect("a Claude CLI that uses the login is named");
    assert!(named.starts_with(&format!("pid {} (", cli.id())), "the named process was {named}");
    assert_eq!(skipped, None, "the process the check skips was still named");
    assert_eq!(inherits_only, None, "a process that only inherits the config directory was counted");
  }

  /// The live test of a renewal: one real login that is short and idle is renewed while the test's own Claude CLI
  /// process uses it, and the CLI's next request must still authenticate. Run it on the host that holds the login
  /// files, before step 1's switch:
  ///
  ///   CHARLIEBOT_LIVE_RENEW_LOGIN=<alias> cargo test --ignored a_live_cli_survives_a_renewal_of_its_login_live
  #[test]
  #[ignore = "live: renews one real login's token, once, on the host that holds the login files"]
  fn a_live_cli_survives_a_renewal_of_its_login_live() {
    let (alias, dir, hosts) = live_login();
    let before = token_of(&dir);
    let file: Value = serde_json::from_str(&fs::read_to_string(dir.join(CREDENTIALS_FILE)).unwrap()).unwrap();
    let expires = file["claudeAiOauth"]["expiresAt"].as_i64().expect("the login file names its expiry");
    let remaining = expires - now_millis();
    assert!(
      remaining < claude_accounts::RENEWABLE_TOKEN_MAX_SECONDS * 1000,
      "the login {alias} lives {remaining} ms, so it is not under the renewal's cap"
    );
    if let Some(user) = claude_cli_using(&dir, None) {
      panic!("a Claude CLI already uses the login {alias}: {user}; the test needs an idle login");
    }
    let check = claude_accounts::HoldCheck::new(hosts.clone(), claude_accounts::now_or(None));
    assert_eq!(check.hold(&dir), claude_accounts::Hold::Free, "a live round holds the login {alias}");

    let log = tempfile::tempdir().unwrap();
    let out = log.path().join("cli.out");
    let err = log.path().join("cli.err");
    let mut child = std::process::Command::new("claude")
      .args([
        "-p",
        "Use the Bash tool to run the shell command `sleep 60`, then answer with the single word ready.",
        "--allowedTools",
        "Bash",
      ])
      .env("CLAUDE_CONFIG_DIR", &dir)
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::from(fs::File::create(&out).unwrap()))
      .stderr(std::process::Stdio::from(fs::File::create(&err).unwrap()))
      .spawn()
      .expect("the claude CLI starts");
    // The CLI reads its login at start and runs the tool call for a minute; the renewal lands in its middle.
    std::thread::sleep(Duration::from_secs(20));

    // The second idle check, right before the renewal: a run that started after the selection stops the test here,
    // before any renewal. The test's own CLI is the one process it allows to use the login.
    let mut stops = Vec::new();
    if let Some(user) = claude_cli_using(&dir, Some(child.id())) {
      stops.push(format!("a Claude CLI other than the test's own uses the login: {user}"));
    }
    let hold = claude_accounts::HoldCheck::new(hosts.clone(), claude_accounts::now_or(None)).hold(&dir);
    if hold != claude_accounts::Hold::Free {
      stops.push(format!("the hold check reads {hold:?}"));
    }
    if !stops.is_empty() {
      let _ = child.kill();
      let _ = child.wait();
      panic!(
        "the second idle check failed before the renewal of {alias}: {}; the login was not renewed",
        stops.join("; ")
      );
    }

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let quota = QuotaState::default();
    let http = test_support::quota_http();
    let trigger = RenewalTrigger::KeyedRound { run_id: "live-renewal-test".to_string() };
    let renewal = runtime
      .block_on(renew_login(&quota, &http, &alias, &dir, &hosts, &before, &trigger))
      .expect("the renewal call runs");
    let answer = format!("{renewal:?}");

    let deadline = Instant::now() + Duration::from_secs(240);
    let status = loop {
      if let Some(status) = child.try_wait().unwrap() {
        break status;
      }
      if Instant::now() >= deadline {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the CLI did not end within its bound; renewal was {answer}");
      }
      std::thread::sleep(Duration::from_millis(200));
    };
    let stdout = fs::read_to_string(&out).unwrap_or_default();
    let stderr = fs::read_to_string(&err).unwrap_or_default();
    let after = token_of(&dir);

    assert!(matches!(renewal, Renewal::Usable(_)), "the renewal was {answer}");
    // Not assert_ne!: its failure message would print both access tokens.
    assert!(after != before, "the renewal left the access token unchanged; renewal was {answer}");
    assert!(status.success(), "the CLI ended with {status} after the renewal; renewal was {answer}; stderr: {stderr}");
    assert!(!stdout.trim().is_empty(), "the CLI gave no answer; renewal was {answer}; stderr: {stderr}");
  }
}
