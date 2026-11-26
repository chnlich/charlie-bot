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

use charliebot_claude_code::ClaudeHost;
use charliebot_claude_code::claude_accounts::{self, ClaudeAccountState, QuotaState};
use charliebot_claude_code::test_support::fake_program_bin;
use charliebot_claude_code::usage_logs::ClaudeLogs;
use charliebot_claude_code::usage_quota::{
  ClaudeQuotaAccount, QuotaAccounts, QuotaHttp, Renewal, RenewalTrigger, renew_login,
};
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
const RENEWAL_WITH_SCOPES: &str = r#"{"access_token":"tok-new","refresh_token":"ref-new","expires_in":28800,"refresh_token_expires_in":86400,"scope":"user:inference user:profile"}"#;
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
    symlink(fake_program_bin(), bin.join("claude")).unwrap();
    let probe = json!({"claude": [
      {"append": {"path": claude_runs, "text": "run\n"}},
      {"stdout": "2.1.300 (Claude Code)\n"},
      {"exit": 0},
    ]});
    fs::write(bin.join(charliebot_core::paths::PROBE_FILE_NAME), probe.to_string()).unwrap();

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
    ClaudeQuotaAccount::new(label, self.credentials.clone(), self.cron_dir(), Arc::clone(&self.state), self.http())
  }

  /// The cron task directory of this fixture: absent, so no improve host is named and no keyed round holds a login.
  fn cron_dir(&self) -> PathBuf {
    self.login_dir.join("cron.d")
  }

  /// Runs `body` with `PATH` naming only the fake `claude`, so the version probe never starts a real CLI.
  fn with_claude<T>(&self, body: impl FnOnce() -> T) -> T {
    let _guard: MutexGuard<'_, ()> = PATH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let previous = env::var_os("PATH");
    // SAFETY: PATH_LOCK serializes the tests that run a process, and nothing else in this binary reads PATH.
    unsafe { env::set_var("PATH", &self.bin) };
    let result = body();
    // SAFETY: as above. The lock is still held.
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
    registry.register_package("backends.claude_code", charliebot_claude_code::register).unwrap();
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

#[ignore = "integration"]
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

#[ignore = "integration"]
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
  assert_eq!(renewal["scope"], "user:inference", "the request carries the file's scopes joined by spaces");

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

#[ignore = "integration"]
#[test]
fn a_renewal_writes_the_refresh_tokens_expiry_and_the_scopes_the_response_names() {
  let fixture = Fixture::new(STALE, vec![(401, "{}"), (200, PANEL)], vec![(200, RENEWAL_WITH_SCOPES)]);
  let account = fixture.account("work (default)");
  let before = now_ms();

  let result = fixture.with_claude(|| block_on(account.fetch()));

  assert!(result.expect("the usage request succeeds").is_some());
  let file: Value = serde_json::from_str(&fixture.credentials_text()).unwrap();
  let oauth = &file["claudeAiOauth"];
  assert_eq!(oauth["scopes"], json!(["user:inference", "user:profile"]));
  let refresh_expires = oauth["refreshTokenExpiresAt"].as_i64().unwrap();
  assert!(
    (refresh_expires - before - 86_400_000).abs() < 5_000,
    "refreshTokenExpiresAt is the response's span from now, got {refresh_expires}"
  );
}

#[ignore = "integration"]
#[test]
fn a_renewal_response_with_no_expiry_writes_nothing_and_fails_the_round() {
  let fixture = Fixture::new(STALE, vec![(401, "{}")], vec![(200, r#"{"access_token":"tok-new"}"#)]);
  let account = fixture.account("work (default)");

  let result = fixture.with_claude(|| block_on(account.fetch()));

  assert!(result.expect("a renewal without an expiry returns no reading").is_none());
  assert_eq!(account.last_error(), "token refresh failed");
  assert_eq!(fixture.credentials_text(), STALE, "a response without an expiry changed the file");
}

#[ignore = "integration"]
#[test]
fn the_401_path_renews_a_login_only_under_three_hours_fifty_minutes_of_life() {
  for (life_ms, renews) in [(13_802_000_i64, false), (13_740_000_i64, true)] {
    let credentials = format!(
      r#"{{"claudeAiOauth":{{"accessToken":"tok-stored","refreshToken":"ref-stored","expiresAt":{},"scopes":["user:inference"]}}}}"#,
      now_ms() + life_ms
    );
    let fixture = Fixture::new(&credentials, vec![(401, "{}"), (200, PANEL)], vec![(200, RENEWAL)]);
    let account = fixture.account("work (default)");

    let result = fixture.with_claude(|| block_on(account.fetch()));

    if renews {
      assert!(result.expect("the usage request succeeds").is_some(), "a login under the cap was not renewed");
      assert_eq!(fixture.requests("/token").len(), 1);
    } else {
      assert!(result.expect("a skipped renewal returns no reading").is_none());
      assert!(fixture.requests("/token").is_empty(), "a login at the cap was renewed");
      assert_eq!(fixture.credentials_text(), credentials);
    }
  }
}

#[ignore = "integration"]
#[test]
fn a_login_without_scopes_is_not_renewed_and_makes_no_request() {
  let credentials =
    r#"{"claudeAiOauth":{"accessToken":"tok-stored","refreshToken":"ref-stored","expiresAt":1000}}"#.to_string();
  let fixture = Fixture::new(&credentials, vec![(401, "{}")], vec![(200, RENEWAL)]);
  let account = fixture.account("work (default)");

  let result = fixture.with_claude(|| block_on(account.fetch()));

  assert!(result.expect("a skipped renewal returns no reading").is_none());
  assert!(fixture.requests("/token").is_empty(), "a login with no scopes was renewed");
  assert_eq!(fixture.credentials_text(), credentials);
}

#[ignore = "integration"]
#[test]
fn a_login_a_live_launch_holds_is_not_renewed() {
  let fixture = Fixture::new(STALE, vec![(401, "{}")], vec![(200, RENEWAL)]);
  let account = fixture.account("work (default)");
  let mut live = std::process::Command::new("/bin/sleep")
    .arg("30")
    .env("IMPROVE_REMOTE_HOST", "host3")
    .env("CLAUDE_CONFIG_DIR", &fixture.login_dir)
    .spawn()
    .unwrap();
  std::thread::sleep(std::time::Duration::from_millis(200));

  let result = fixture.with_claude(|| block_on(account.fetch()));

  live.kill().unwrap();
  live.wait().unwrap();
  assert!(result.expect("a held login returns no reading").is_none());
  assert!(fixture.requests("/token").is_empty(), "a login a launch held was renewed");
  assert_eq!(fixture.credentials_text(), STALE);
}

/// Asks `renew_login` for one renewal of the fixture's login, the call the 401 path makes.
fn renew_directly(fixture: &Fixture) -> Renewal {
  let http = fixture.http();
  block_on(renew_login(
    &QuotaState::default(),
    &http,
    "work",
    &fixture.login_dir,
    &fixture.cron_dir(),
    "tok-stored",
    &RenewalTrigger::Usage401,
  ))
  .expect("the renewal call runs")
}

#[ignore = "integration"]
#[test]
fn a_cron_file_that_does_not_parse_holds_every_login() {
  let fixture = Fixture::new(STALE, vec![], vec![(200, RENEWAL)]);
  fs::create_dir_all(fixture.cron_dir()).unwrap();
  fs::write(fixture.cron_dir().join("improve-ux.yaml"), "improve_host: [unclosed\n").unwrap();

  let renewal = renew_directly(&fixture);

  assert_eq!(renewal, Renewal::Skipped { reason: "unreadable", holder: Some("unreadable:cron_tasks".to_string()) });
  assert!(fixture.requests("/token").is_empty(), "a login held by an unparsable cron file was renewed");
  assert_eq!(fixture.credentials_text(), STALE);
}

#[ignore = "integration"]
#[test]
fn an_unreadable_cron_directory_holds_every_login() {
  let fixture = Fixture::new(STALE, vec![], vec![(200, RENEWAL)]);
  // A regular file where the directory belongs fails the read with ENOTDIR, which is not a missing directory.
  fs::write(fixture.cron_dir(), "not a directory\n").unwrap();

  let renewal = renew_directly(&fixture);

  assert_eq!(renewal, Renewal::Skipped { reason: "unreadable", holder: Some("unreadable:cron_tasks".to_string()) });
  assert!(fixture.requests("/token").is_empty(), "a login held by an unreadable cron directory was renewed");
  assert_eq!(fixture.credentials_text(), STALE);
}

#[ignore = "integration"]
#[test]
fn a_renewal_uses_the_token_another_writer_left_in_the_file() {
  let fixture = Fixture::new(STALE, vec![], vec![]);
  fs::write(
    &fixture.credentials,
    r#"{"claudeAiOauth":{"accessToken":"tok-other","refreshToken":"ref-other","expiresAt":1000,"scopes":["user:inference"]}}"#,
  )
  .unwrap();
  // The caller saw `tok-stored` and asks for a renewal, but another writer holds the CLI's refresh lock and has already
  // left a working token in the file. The renewal reads the file again inside the lock and uses that token, sending no
  // request of its own.
  let lock = fixture.login_dir.join(".oauth_refresh.lock");
  fs::create_dir(&lock).unwrap();
  let writer = thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_millis(200));
    fs::remove_dir(&lock).unwrap();
  });
  let http = fixture.http();

  let outcome = fixture.with_claude(|| {
    block_on(renew_login(
      &QuotaState::default(),
      &http,
      "work",
      &fixture.login_dir,
      &fixture.cron_dir(),
      "tok-stored",
      &RenewalTrigger::Usage401,
    ))
  });
  writer.join().unwrap();

  assert_eq!(outcome.expect("the renewal call runs"), Renewal::Usable("tok-other".to_string()));
  assert!(fixture.requests("/token").is_empty(), "a renewal sent a request for a token another writer replaced");
}

#[ignore = "integration"]
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

#[ignore = "integration"]
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

#[ignore = "integration"]
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

#[ignore = "integration"]
#[test]
fn an_http_error_returns_the_status_error_text() {
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
