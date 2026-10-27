//! The Claude quota account against a loopback fake of the usage and token endpoints. The fake serves each route's
//! scripted responses in order and records every request. A fake `claude` on `PATH` answers the version probe, so no
//! real CLI runs and no real login directory is read.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use charliebot_agents::backends::claude_code::ClaudeHost;
use charliebot_agents::backends::claude_code::claude_accounts::{self, ClaudeAccountState};
use charliebot_agents::backends::claude_code::usage_logs::ClaudeLogs;
use charliebot_agents::backends::claude_code::usage_quota::{ClaudeQuotaAccount, QuotaAccounts, QuotaHttp};
use charliebot_compat::http_client::{self, Settings};
use charliebot_registry::Registry;
use charliebot_registry::usage::{QuotaAccount, UsageSourceImpl};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const FRESH: &str =
  r#"{"claudeAiOauth":{"accessToken":"tok-fresh","refreshToken":"ref-fresh","expiresAt":4102444800000}}"#;
const STALE: &str = r#"{"claudeAiOauth":{"accessToken":"tok-stored","refreshToken":"ref-stored","expiresAt":1000,"scopes":["user:inference"]}}"#;
const PANEL: &str = r#"{"fiveHour":{"utilization":12,"resetsAt":"2026-10-18T15:00:00Z"},"sevenDay":{"utilization":34,"resetsAt":"2026-10-20T00:00:00Z"}}"#;
const RENEWAL: &str = r#"{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800}"#;
const REFUSAL: &str = r#"{"error":"invalid_grant"}"#;

/// Serializes the tests that run a process: `PATH` is process-wide, so one test at a time names the fake `claude`.
static PATH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
struct Recorded {
  headers: HashMap<String, String>,
  body: String,
}

type Routes = HashMap<&'static str, VecDeque<(u16, String)>>;

/// The fake server and its files for one test. Every test gets its own root, login directory and bin directory.
struct Fixture {
  root: TempDir,
  base: String,
  recorded: Arc<Mutex<Vec<(String, Recorded)>>>,
  state: Arc<ClaudeAccountState>,
  login_dir: PathBuf,
  credentials: PathBuf,
  bin: PathBuf,
  claude_runs: PathBuf,
}

impl Fixture {
  fn new(credentials: &str, usage: Vec<(u16, &str)>, token: Vec<(u16, &str)>) -> Self {
    let root = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes: Routes = HashMap::from([("/usage", scripted(usage)), ("/token", scripted(token))]);
    let routes = Arc::new(Mutex::new(routes));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    {
      let routes = Arc::clone(&routes);
      let recorded = Arc::clone(&recorded);
      thread::spawn(move || {
        for stream in listener.incoming() {
          serve(stream.expect("the fake server accepts"), &routes, &recorded);
        }
      });
    }

    let login_dir = root.path().join("login");
    fs::create_dir_all(&login_dir).unwrap();
    let credentials_path = login_dir.join(".credentials.json");
    fs::write(&credentials_path, credentials).unwrap();

    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let claude_runs = root.path().join("claude-runs");
    symlink(env!("CARGO_BIN_EXE_fake_program"), bin.join("claude")).unwrap();
    let probe = json!({"claude": [
      {"append": {"path": claude_runs, "text": "run\n"}},
      {"stdout": "2.1.300 (Claude Code)\n"},
      {"exit": 0},
    ]});
    fs::write(bin.join("probe.json"), probe.to_string()).unwrap();

    Self {
      root,
      base,
      recorded,
      state: Arc::new(ClaudeAccountState::default()),
      login_dir,
      credentials: credentials_path,
      bin,
      claude_runs,
    }
  }

  fn http(&self) -> Arc<QuotaHttp> {
    let client = http_client::client(&Settings::default()).unwrap();
    Arc::new(QuotaHttp::with_endpoints(client, &format!("{}/usage", self.base), &format!("{}/token", self.base)))
  }

  fn account(&self, label: &str) -> ClaudeQuotaAccount {
    ClaudeQuotaAccount::new(label, self.credentials.clone(), Arc::clone(&self.state), self.http())
  }

  /// Runs `body` with `PATH` naming only the fake `claude`, so the version probe never starts a real CLI.
  fn with_claude<T>(&self, body: impl FnOnce() -> T) -> T {
    let _guard: MutexGuard<'_, ()> = PATH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let previous = env::var_os("PATH");
    // SAFETY: PATH_LOCK serializes the tests that run a process, and nothing else in this binary reads PATH.
    unsafe { env::set_var("PATH", &self.bin) };
    let result = body();
    // SAFETY: as above; the lock is still held.
    unsafe {
      match previous {
        Some(path) => env::set_var("PATH", path),
        None => env::remove_var("PATH"),
      }
    }
    result
  }

  fn requests(&self, route: &str) -> Vec<Recorded> {
    self.recorded.lock().unwrap().iter().filter(|(path, _)| path == route).map(|(_, request)| request.clone()).collect()
  }

  fn claude_runs(&self) -> usize {
    fs::read_to_string(&self.claude_runs).map_or(0, |text| text.lines().count())
  }

  fn credentials_text(&self) -> String {
    fs::read_to_string(&self.credentials).unwrap()
  }

  fn credentials_mode(&self) -> u32 {
    fs::metadata(&self.credentials).unwrap().permissions().mode() & 0o777
  }

  /// A config with one pool login whose directory is this fixture's login directory.
  fn config_with_pool_dir(&self, config_dir: &str) -> charliebot_core::config::CharlieBotConfig {
    let home = self.root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
      home.join("config.yaml"),
      format!(
        "paths:\n  worktree_dir: '{}'\naccounts:\n  claude:\n    - label: invite-1\n      config_dir: '{config_dir}'\n",
        self.root.path().join("worktrees").display(),
      ),
    )
    .unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", charliebot_agents::backends::claude_code::register).unwrap();
    registry.load_config(&home).unwrap()
  }

  fn pool_config(&self) -> charliebot_core::config::CharlieBotConfig {
    self.config_with_pool_dir(&self.login_dir.display().to_string())
  }

  fn host(&self) -> ClaudeHost {
    ClaudeHost { home: self.root.path().to_path_buf(), claude_config_dir: self.root.path().join("claude-home") }
  }
}

struct WorkingDirectoryRestore(PathBuf);

impl Drop for WorkingDirectoryRestore {
  fn drop(&mut self) {
    env::set_current_dir(&self.0).expect("the original working directory is restored");
  }
}

fn scripted(responses: Vec<(u16, &str)>) -> VecDeque<(u16, String)> {
  responses.into_iter().map(|(status, body)| (status, body.to_string())).collect()
}

fn serve(mut stream: TcpStream, routes: &Mutex<Routes>, recorded: &Mutex<Vec<(String, Recorded)>>) {
  let mut reader = BufReader::new(stream.try_clone().unwrap());
  let mut request_line = String::new();
  reader.read_line(&mut request_line).unwrap();
  let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
  let mut headers = HashMap::new();
  loop {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let line = line.trim_end();
    if line.is_empty() {
      break;
    }
    if let Some((name, value)) = line.split_once(':') {
      headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
  }
  let length: usize = headers.get("content-length").map_or(0, |length| length.parse().unwrap());
  let mut body = vec![0; length];
  reader.read_exact(&mut body).unwrap();
  let body = String::from_utf8(body).unwrap();
  recorded.lock().unwrap().push((path.clone(), Recorded { headers, body }));

  let (status, payload) = routes
    .lock()
    .unwrap()
    .get_mut(path.as_str())
    .and_then(VecDeque::pop_front)
    .unwrap_or_else(|| (599, format!("no scripted response for {path}")));
  let response = format!(
    "HTTP/1.1 {status} scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
    payload.len()
  );
  stream.write_all(response.as_bytes()).unwrap();
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
  tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}

fn now_ms() -> i64 {
  SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

#[test]
fn a_fresh_token_fetches_the_panel_without_renewal_and_probes_the_cli_once() {
  let fixture = Fixture::new(FRESH, vec![(200, PANEL), (200, PANEL)], vec![]);
  let account = fixture.account("work (default)");

  let (first, second) = fixture.with_claude(|| block_on(async { (account.fetch().await, account.fetch().await) }));

  let first = first.expect("the usage request succeeds").expect("a fresh token fetches the panel");
  assert!(second.expect("the second usage request succeeds").is_some());
  assert_eq!(first["windows"][0]["utilization"], json!(12.0));
  let usage = fixture.requests("/usage");
  assert_eq!(usage.len(), 2);
  assert_eq!(usage[0].headers["authorization"], "Bearer tok-fresh");
  assert_eq!(usage[0].headers["anthropic-beta"], "oauth-2025-04-20");
  assert_eq!(usage[0].headers["user-agent"], "claude-code/2.1.300");
  assert!(fixture.requests("/token").is_empty(), "a fresh token is not renewed");
  assert_eq!(fixture.claude_runs(), 1, "the version probe runs once per state");
  assert_eq!(fixture.credentials_text(), FRESH);
}

#[test]
fn an_expired_token_renews_once_and_rewrites_the_credentials_file() {
  let fixture = Fixture::new(STALE, vec![(401, "{}"), (200, PANEL)], vec![(200, RENEWAL)]);
  let account = fixture.account("work (default)");
  let before = now_ms();

  let result = fixture.with_claude(|| block_on(account.fetch()));

  assert!(result.expect("the usage request succeeds").is_some());
  let usage = fixture.requests("/usage");
  assert_eq!(usage.len(), 2);
  assert_eq!(usage[0].headers["authorization"], "Bearer tok-stored");
  assert_eq!(usage[1].headers["authorization"], "Bearer tok-new");
  let renewals = fixture.requests("/token");
  assert_eq!(renewals.len(), 1);
  let renewal: Value = serde_json::from_str(&renewals[0].body).unwrap();
  assert_eq!(renewal["grant_type"], "refresh_token");
  assert_eq!(renewal["refresh_token"], "ref-stored");

  let file: Value = serde_json::from_str(&fixture.credentials_text()).unwrap();
  let oauth = &file["claudeAiOauth"];
  assert_eq!(oauth["accessToken"], "tok-new");
  assert_eq!(oauth["refreshToken"], "ref-new");
  let expires = oauth["expiresAt"].as_i64().unwrap();
  assert!((expires - before - 28_800_000).abs() < 5_000, "expiresAt is the renewal's span from now, got {expires}");
  let keys: Vec<&str> = oauth.as_object().unwrap().keys().map(String::as_str).collect();
  assert_eq!(keys, ["accessToken", "refreshToken", "expiresAt", "scopes"]);
  assert_eq!(fixture.credentials_mode(), 0o600);
}

#[test]
fn a_refused_renewal_leaves_the_credentials_and_reports_a_failed_refresh() {
  let fixture = Fixture::new(STALE, vec![(401, "{}")], vec![(400, REFUSAL)]);
  let account = fixture.account("work (default)");

  let result = fixture.with_claude(|| block_on(account.fetch()));

  assert!(result.expect("a refused renewal returns no reading").is_none());
  assert_eq!(account.last_error(), "token refresh failed");
  assert_eq!(fixture.requests("/usage").len(), 1);
  assert_eq!(fixture.requests("/token").len(), 1);
  assert_eq!(fixture.credentials_text(), STALE);
}

#[test]
fn a_pool_login_whose_renewal_was_refused_is_marked_only_after_an_auth_failure_is_recorded() {
  let fixture = Fixture::new(STALE, vec![(401, "{}")], vec![(400, REFUSAL)]);
  let config = fixture.pool_config();
  let accounts = QuotaAccounts::new(Arc::clone(&fixture.state), fixture.http());
  let logins = accounts.derive(&fixture.host(), &config).unwrap();
  let invite = logins.iter().find(|login| login.label() == "invite-1").unwrap();

  let result = fixture.with_claude(|| block_on(invite.fetch()));
  assert!(result.expect("a refused renewal returns no reading").is_none());
  assert_eq!(invite.last_error(), "token refresh failed");

  // The refused login keeps its access token on disk, so the pool still judges it healthy.
  let mut entry = Map::new();
  invite.mark_login_required(&mut entry);
  assert!(!entry.contains_key("login_required"));

  claude_accounts::record_auth_failure(&fixture.state, "invite-1", None);
  invite.mark_login_required(&mut entry);
  assert_eq!(entry["login_required"], json!(fixture.login_dir.display().to_string()));
}

#[test]
fn a_rate_limited_account_backs_off_without_a_second_request() {
  let fixture = Fixture::new(STALE, vec![(429, "{}")], vec![]);
  let account = fixture.account("work (default)");

  let (first, second) = fixture.with_claude(|| block_on(async { (account.fetch().await, account.fetch().await) }));

  assert!(first.expect("a rate limit returns no reading").is_none());
  assert!(second.expect("backoff returns no reading").is_none());
  assert_eq!(account.last_error(), "rate limited");
  assert_eq!(fixture.requests("/usage").len(), 1, "the backoff holds the second round back");
}

#[test]
fn an_http_error_returns_the_python_status_error_text() {
  let fixture = Fixture::new(FRESH, vec![(500, "{}")], vec![]);
  let account = fixture.account("work (default)");

  let error = fixture.with_claude(|| block_on(account.fetch())).unwrap_err();

  assert_eq!(
    error.to_string(),
    format!(
      "Server error '500 Internal Server Error' for url '{}/usage'\nFor more information check: \
       https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/500",
      fixture.base
    )
  );
}

#[test]
fn quota_accounts_returns_the_config_derivation_error() {
  let _path_guard: MutexGuard<'_, ()> = PATH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
  let fixture = Fixture::new(FRESH, vec![], vec![]);
  let config = fixture.config_with_pool_dir("relative-login");
  let source = ClaudeLogs::new(fixture.host(), Arc::clone(&fixture.state), fixture.http());
  let original_dir = env::current_dir().unwrap();
  let deleted_dir = fixture.root.path().join("deleted-working-directory");
  fs::create_dir(&deleted_dir).unwrap();
  env::set_current_dir(&deleted_dir).unwrap();
  let restore = WorkingDirectoryRestore(original_dir);
  fs::remove_dir(&deleted_dir).unwrap();

  let error = source.quota_accounts(&config).err().unwrap();

  drop(restore);
  assert!(error.to_string().contains("reading the working directory"));
}
