//! The host-auth routes: the page, the status JSON, the manual probe, and each browser login's readback and start routes.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::{get_only, post_only};
use charliebot_registry::FeatureContext;
use http::{StatusCode, header};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::browser_login;
use crate::config::{BrowserLoginConfig, HostAuthConfig};
use crate::host_auth::{self, RoundSlot};
use crate::login_page;
use crate::page;
use crate::timestamps;

/// Register host-auth routes with the application feature context as shared state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/host-auth", get_only(host_auth_page))
    .route("/api/host-auth/status", get_only(host_auth_status))
    .route("/api/host-auth/probe", post_only(host_auth_probe))
    .route("/api/host-auth/logins/{name}/readback", get_only(browser_login_readback))
    .route("/api/host-auth/logins/{name}/start", post_only(browser_login_start))
    .with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect: a slashed form of one of them redirects to it.
pub fn matches(path: &str) -> bool {
  if matches!(path, "/host-auth" | "/api/host-auth/status" | "/api/host-auth/probe") {
    return true;
  }
  login_action_name(path, "readback").or_else(|| login_action_name(path, "start")).is_some()
}

/// The login name of `/api/host-auth/logins/{name}/<action>`: one non-empty segment, as matchit's parameter captures.
fn login_action_name<'a>(path: &'a str, action: &str) -> Option<&'a str> {
  let name = path.strip_prefix("/api/host-auth/logins/")?.strip_suffix(action)?.strip_suffix('/')?;
  (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// `GET /host-auth`: the standing page, rendered from the state file, the config and the live attempts. It awaits no
/// login readback: each idle login's row asks for its own.
async fn host_auth_page(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let rendered = tokio::task::spawn_blocking(move || {
    let state_path = host_auth::default_state_path().map_err(|error| ("Exception".to_string(), error.to_string()))?;
    let state = host_auth::load_state(&state_path);
    page::render(&features, &state, timestamps::utc_now(), &request)
  })
  .await
  .expect("rendering the host-auth page does not panic");
  match rendered {
    Ok(response) => response,
    Err((kind, detail)) => responses::internal_error(&kind, detail),
  }
}

/// `GET /api/host-auth/status`: the state file, each host with its derived estimate after its stored keys.
async fn host_auth_status() -> Response {
  let body = tokio::task::spawn_blocking(|| -> anyhow::Result<Value> {
    status_body(&host_auth::default_state_path()?, timestamps::utc_now())
  })
  .await
  .expect("reading the host-auth status does not panic");
  match body {
    Ok(value) => responses::json(StatusCode::OK, &value),
    Err(error) => failure(error),
  }
}

/// The status body: `{**state, "hosts": [{**entry, **estimate}]}`.
pub fn status_body(state_path: &std::path::Path, now: OffsetDateTime) -> anyhow::Result<Value> {
  let mut state = host_auth::load_state(state_path);
  let mut hosts = Vec::new();
  for entry in host_auth::host_entries(&state)? {
    let estimate = host_auth::derive_estimate(entry, now)?;
    let mut host = entry.clone();
    host.insert("estimated_expires_at".to_string(), json!(estimate.expires_at));
    host.insert("remaining_sec".to_string(), json!(estimate.remaining_sec));
    hosts.push(Value::Object(host));
  }
  state.insert("hosts".to_string(), Value::Array(hosts));
  Ok(Value::Object(state))
}

/// `POST /api/host-auth/probe`: start a forced round unless one runs, and return to the page either way.
async fn host_auth_probe() -> Response {
  if let Some(slot) = RoundSlot::claim() {
    tokio::spawn(manual_round(slot));
  }
  see_other()
}

/// `GET /api/host-auth/logins/{name}/readback`: the login's readback, run now and answered as `{"exit", "output"}`. The
/// `exit` is null when the timeout ended the readback. A readback that cannot start answers 500 with its reason in
/// `detail`.
async fn browser_login_readback(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(name): RoutePath<String>,
) -> Response {
  let login = match login_named(&features, &name) {
    Ok(Some(login)) => login,
    Ok(None) => return no_such_login(&name),
    Err(error) => return responses::internal_error("RuntimeError", error),
  };
  match browser_login::page_readback(&login).await {
    Ok(readback) => responses::json(StatusCode::OK, &login_page::readback_json(readback)),
    Err(error) => {
      tracing::error!(login = %name, error = %error, "browser_login_readback_failed");
      detail(StatusCode::INTERNAL_SERVER_ERROR, format!("the readback did not run: {error}"))
    }
  }
}

/// `POST /api/host-auth/logins/{name}/start`: start the named login and redirect to its approval link. It answers 502
/// with the failure body when the login gives none.
async fn browser_login_start(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(name): RoutePath<String>,
) -> Response {
  let login = match login_named(&features, &name) {
    Ok(Some(login)) => login,
    Ok(None) => return no_such_login(&name),
    Err(error) => return responses::internal_error("RuntimeError", error),
  };
  match browser_login::start_login(&login).await {
    Ok(link) => see_other_to(&link),
    Err(failed) => responses::plain_text(StatusCode::BAD_GATEWAY, &failed.0),
  }
}

/// The login `name` names in the current config, copied out, or `None` when no login has that name.
fn login_named(features: &FeatureContext, name: &str) -> Result<Option<BrowserLoginConfig>, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  Ok(cfg.section::<HostAuthConfig>("host_auth").browser_logins.iter().find(|login| login.name == name).cloned())
}

/// The 404 answer for a login name the config does not hold.
fn no_such_login(name: &str) -> Response {
  detail(StatusCode::NOT_FOUND, format!("no browser login named '{name}'"))
}

/// The error body of a status-coded answer: a JSON object whose `detail` holds the message.
fn detail(status: StatusCode, detail: String) -> Response {
  responses::fast_json(status, &json!({ "detail": detail }))
}

/// The manual round: forced, with its failure logged. The slot is released when the round ends.
async fn manual_round(_slot: RoundSlot) {
  let outcome = async {
    let (state_path, config_path) = host_auth::default_paths()?;
    host_auth::run_round(true, &state_path, &config_path).await?;
    anyhow::Ok(())
  }
  .await;
  if let Err(error) = outcome {
    tracing::error!(error = %format!("{error:#}"), "host_auth_manual_round_failed");
  }
}

/// A 303 redirect to `location`: an empty body, its length and its location.
fn see_other_to(location: &str) -> Response {
  Response::builder()
    .status(StatusCode::SEE_OTHER)
    .header(header::CONTENT_LENGTH, "0")
    .header(header::LOCATION, location)
    .body(Body::empty())
    .expect("the redirect builds")
}

/// The redirect back to the standing page.
fn see_other() -> Response {
  see_other_to("/host-auth")
}

/// `ServerErrorMiddleware`'s answer to a handler that raised.
fn failure(error: anyhow::Error) -> Response {
  responses::internal_error("Exception", format!("{error:#}"))
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::Path;
  use std::time::Duration;

  use axum::body::{Body, to_bytes};
  use axum::http::{HeaderMap, Method, Request, StatusCode};
  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME, CharlieBotConfig};
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks::store_hooks;
  use charliebot_registry::templating::Templating;
  use charliebot_registry::{FeatureContext, Registry, StartupState};
  use charliebot_store::Store;
  use serde_json::Value;
  use tempfile::TempDir;
  use tower::ServiceExt;

  use super::*;
  use crate::process_tree::is_running;

  const LINK: &str = "https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21";
  /// The link a pty login's fake command prints, once its user code is appended.
  const ROUTER_LINK: &str = "https://example.invalid/activate?user_code=ABCD-EFGH";

  fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate lies under the repo")
  }

  /// A stub the config's commands point at: one executable shell script under the home.
  fn write_script(home: &Path, name: &str, body: &str) -> String {
    let path = home.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("the stub script writes");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the stub becomes executable");
    path.to_str().expect("the temp path is UTF-8").to_string()
  }

  /// A home whose `host_auth:` section names these logins. One command prints the approval link, one fails after a
  /// line of output, one renews `host1` under a pseudo-terminal, one sleeps past the readback timeout, one starts a
  /// background child that holds its output and then sleeps, and one cannot start. The names carry the test's prefix,
  /// so parallel tests share no record.
  struct Home {
    _dir: TempDir,
    router: Router,
    features: Arc<FeatureContext>,
    ok_name: String,
    failing_name: String,
    router_name: String,
    slow_name: String,
    tree_name: String,
    missing_name: String,
  }

  fn home(prefix: &str, templating: bool) -> Home {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let ok_name = format!("{prefix}-aws-sso");
    let failing_name = format!("{prefix}-failing-sso");
    let router_name = format!("{prefix}-router-host1");
    let slow_name = format!("{prefix}-slow");
    let tree_name = format!("{prefix}-tree");
    let missing_name = format!("{prefix}-missing");
    let missing_readback = dir.path().join(format!("{prefix}-no-such-readback")).display().to_string();
    let login = write_script(dir.path(), &format!("{prefix}-login"), &format!("printf '%s\\n' '{LINK}'\n"));
    let failing = write_script(dir.path(), &format!("{prefix}-failing"), "echo starting; exit 3\n");
    let readback = write_script(dir.path(), &format!("{prefix}-readback"), "echo valid\n");
    let failing_readback = write_script(dir.path(), &format!("{prefix}-failing-readback"), "echo failed\nexit 1\n");
    let slow_readback = write_script(dir.path(), &format!("{prefix}-slow-readback"), "sleep 60\n");
    // The shell records its pid and its background child's pid beside itself, then both wait. The background child holds
    // the output pipe, so only the timeout's tree kill ends the readback.
    let tree_readback = write_script(
      dir.path(),
      &format!("{prefix}-tree-readback"),
      "echo $$ > \"$0.root\"\nsleep 600 & echo $! > \"$0.child\"\nsleep 600\n",
    );
    // The login router's flow: a link with no code, the code on its own line, then an Enter prompt with no newline.
    // The script ends approved only when the newline it reads from its terminal is the one the driver wrote.
    let router_login = write_script(
      dir.path(),
      &format!("{prefix}-router-login"),
      r#"printf 'Open: https://example.invalid/activate\n'
printf 'Code: ABCD-EFGH\n'
printf 'Press Enter after approval: '
if read -r answer < /dev/tty; then :; else exit 7; fi
"#,
    );
    let config = format!(
      "\
host_auth:
  browser_logins:
    - name: {ok_name}
      label: AWS SSO
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{readback}]
    - name: {failing_name}
      label: Failing SSO
      login_command: [{failing}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{failing_readback}]
    - name: {router_name}
      label: host1 login router
      host: host1
      login_command: [{router_login}]
      approval_url_pattern: 'https://example\\.invalid/activate\\S*'
      readback_command: [{readback}]
      use_pty: true
      enter_after_pattern: 'Press Enter after approval'
      user_code_pattern: 'Code:\\s*(\\S+)'
    - name: {slow_name}
      label: Slow SSO
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{slow_readback}]
    - name: {tree_name}
      label: Tree SSO
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{tree_readback}]
    - name: {missing_name}
      label: Missing SSO
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{missing_readback}]
"
    );
    fs::write(dir.path().join(CONFIG_FILENAME), config).expect("the config writes");
    let mut registry = Registry::new();
    registry.register_package("features.host_auth", crate::register).expect("the package registers");
    let registry = Arc::new(registry);
    let config_registry = Arc::clone(&registry);
    let config_home = dir.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      dir.path().join(CONFIG_FILENAME),
      "config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join(CREDENTIALS_FILENAME),
      "credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    ));
    let cfg: CharlieBotConfig = registry.load_config(dir.path()).expect("the config loads");
    let store =
      Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens");
    let features = Arc::new(FeatureContext {
      config,
      credentials,
      store: Arc::new(store),
      registry,
      startup: Arc::new(StartupState::default()),
    });
    if templating {
      features
        .startup
        .insert(Templating::new(repo_root(), &["features.host_auth"]))
        .expect("the templating inserts once");
    }
    let router = routes(&features);
    Home { _dir: dir, router, features, ok_name, failing_name, router_name, slow_name, tree_name, missing_name }
  }

  /// One route call: its status, headers and body text.
  async fn call(router: Router, method: Method, path: &str) -> (StatusCode, HeaderMap, String) {
    let response = router
      .oneshot(Request::builder().method(method).uri(path).body(Body::empty()).expect("the request builds"))
      .await
      .expect("the route answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_start_route_redirects_to_the_approval_link() {
    let home = home("start-ok", false);
    let (status, headers, body) =
      call(home.router, Method::POST, &format!("/api/host-auth/logins/{}/start", home.ok_name)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
      headers.get(header::LOCATION),
      Some(&header::HeaderValue::from_str(LINK).expect("the link is header-safe"))
    );
    assert_eq!(body, "", "the redirect carries an empty body");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_start_route_answers_502_with_the_exit_code_and_the_output_tail() {
    let home = home("start-fail", false);
    let (status, headers, body) =
      call(home.router, Method::POST, &format!("/api/host-auth/logins/{}/start", home.failing_name)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
      headers.get(header::CONTENT_TYPE).expect("a plain-text body names its type"),
      "text/plain; charset=utf-8"
    );
    assert_eq!(body, "exit 3\nstarting");
  }

  #[tokio::test]
  async fn an_unknown_login_name_answers_404_with_its_detail() {
    let home = home("start-unknown", false);
    let (status, _, body) = call(home.router, Method::POST, "/api/host-auth/logins/nope/start").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let detail: Value = serde_json::from_str(&body).expect("the detail is JSON");
    assert_eq!(detail["detail"], Value::String("no browser login named 'nope'".to_string()));
  }

  #[tokio::test]
  async fn the_readback_route_answers_the_exit_and_output_and_a_readback_that_cannot_start_fails() {
    let home = home("readback-route", false);
    let readback = |name: &str| format!("/api/host-auth/logins/{name}/readback");

    let (status, _, body) = call(home.router.clone(), Method::GET, &readback(&home.ok_name)).await;
    assert_eq!(status, StatusCode::OK);
    let answer: Value = serde_json::from_str(&body).expect("the answer is JSON");
    assert_eq!(answer, json!({"exit": 0, "output": "valid"}));

    let (_, _, body) = call(home.router.clone(), Method::GET, &readback(&home.failing_name)).await;
    let answer: Value = serde_json::from_str(&body).expect("the answer is JSON");
    assert_eq!(answer, json!({"exit": 1, "output": "failed"}));

    let (status, _, body) = call(home.router.clone(), Method::GET, &readback(&home.missing_name)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let detail: Value = serde_json::from_str(&body).expect("the error names its reason as JSON");
    assert!(detail["detail"].as_str().expect("the detail is a string").starts_with("the readback did not run:"));

    let (status, _, _) = call(home.router, Method::GET, &readback("nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_page_answers_while_a_readback_runs_and_lists_each_login_with_its_readback_and_start_route() {
    let home = home("page-frame", true);
    // The page reads the state file the process's home names, and the pty test below names its own home through the
    // same process-wide variable. Both hold the round-slot lock while they run, so neither reads the other's home.
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    // SAFETY: every test that sets the variable holds the round-slot lock, so no two of them write it at once.
    unsafe { std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, home._dir.path()) };
    // The first render loads the templates; the timed render below is the frame a browser waits on.
    call(home.router.clone(), Method::GET, "/host-auth").await;
    // The browser's readback for the sleeping login is in flight; the page must not wait for it.
    let slow_path = format!("/api/host-auth/logins/{}/readback", home.slow_name);
    let slow_router = home.router.clone();
    let slow = tokio::spawn(async move { call(slow_router, Method::GET, &slow_path).await });
    let started = tokio::time::Instant::now();
    let (status, _, body) = call(home.router.clone(), Method::GET, "/host-auth").await;
    let frame = started.elapsed();
    assert!(frame < Duration::from_secs(1), "the page answers within a second, took {frame:?}");
    assert_eq!(status, StatusCode::OK);
    for name in [&home.ok_name, &home.failing_name, &home.router_name, &home.slow_name] {
      assert!(body.contains(&format!("data-readback-url=\"/api/host-auth/logins/{name}/readback\"")));
      assert!(body.contains(&format!("action=\"/api/host-auth/logins/{name}/start\"")));
    }
    let (status, _, body) = slow.await.expect("the readback task finishes");
    assert_eq!(status, StatusCode::OK);
    let answer: Value = serde_json::from_str(&body).expect("the answer is JSON");
    assert_eq!(answer["exit"], Value::Null, "the timeout ended the sleeping readback");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_readback_past_its_timeout_leaves_no_process_of_its_tree_and_answers_within_the_margin() {
    let home = home("tree", false);
    let started = tokio::time::Instant::now();
    let (status, _, body) =
      call(home.router, Method::GET, &format!("/api/host-auth/logins/{}/readback", home.tree_name)).await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    let answer: Value = serde_json::from_str(&body).expect("the answer is JSON");
    assert_eq!(answer["exit"], Value::Null);
    assert!(elapsed <= Duration::from_secs(22), "the readback answers within its timeout plus 2 s, took {elapsed:?}");

    let script = home._dir.path().join(format!("{}-readback", home.tree_name));
    for pid_file in [script.with_extension("root"), script.with_extension("child")] {
      let pid: u32 =
        fs::read_to_string(&pid_file).expect("the readback recorded its pid").trim().parse().expect("the pid parses");
      let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
      while is_running(pid) {
        assert!(tokio::time::Instant::now() < deadline, "process {pid} of the timed-out tree survived");
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    }
  }

  #[tokio::test]
  async fn the_logins_page_is_gone_with_no_redirect() {
    let home = home("logins-gone", false);
    let (status, _, _) = call(home.router, Method::GET, "/host-auth/logins").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pty_login_answers_the_enter_prompt_and_the_start_route_redirects_to_the_code_link() {
    let home = home("pty", false);
    // The renewal claims the process-wide round slot, which a poller test's round skips while it is held.
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    // The renewal records through the state file the server home names, so this process's home is the fixture's and
    // holds a host1 entry. Nextest runs each test in its own process, and no other test of this binary reads it.
    let dir = home._dir.path();
    // SAFETY: a nextest test owns its process, and the tests of this binary that could run alongside read no
    // environment variable.
    unsafe { std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, dir) };
    let state_path = dir.join(host_auth::STATE_FILENAME);
    let mut state = host_auth::empty_state();
    state.insert("hosts".to_string(), json!([{ "alias": "host1", "hostname": "host1.example.internal" }]));
    host_auth::save_state(&state, &state_path).expect("the state seeds");

    let path = format!("/api/host-auth/logins/{}/start", home.router_name);
    let (status, headers, body) = call(home.router, Method::POST, &path).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
      headers.get(header::LOCATION),
      Some(&header::HeaderValue::from_str(ROUTER_LINK).expect("the link is header-safe"))
    );
    assert_eq!(body, "", "the redirect carries an empty body");
    assert_eq!(ended_outcome(&home.router_name).await, browser_login::Outcome::Approved);

    let state = host_auth::read_state(&state_path).expect("the state reads");
    let entry = host_auth::host_entries(&state).expect("the hosts read")[0];
    let baseline = host_auth::opt_str(entry, "enrolled_observed_at").expect("the baseline field reads");
    assert!(baseline.is_some(), "the approved renewal recorded the baseline");
  }

  /// The outcome the login's attempt ended with, once it ends.
  async fn ended_outcome(name: &str) -> browser_login::Outcome {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
      if let Some(snapshot) = browser_login::last_attempt(name) {
        return snapshot.outcome.expect("a recorded attempt holds its outcome");
      }
      assert!(tokio::time::Instant::now() < deadline, "the attempt did not end in time");
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  }

  /// The rendered `/host-auth` page for the given `(alias, status)` rows.
  async fn page_body(home: &Home, rows: &[(&str, &str)]) -> String {
    let mut state = host_auth::empty_state();
    let hosts = rows
      .iter()
      .map(|(alias, status)| {
        json!({
          "alias": alias,
          "hostname": format!("{alias}.example.internal"),
          "status": status,
          "detail": "",
          "last_probe_at": "2026-09-15T07:30:00+00:00",
          "last_change_at": null,
          "last_ok_at": null,
          "enrolled_observed_at": null,
        })
      })
      .collect();
    state.insert("hosts".to_string(), Value::Array(hosts));
    let request = Request::builder().uri("/host-auth").body(()).expect("the request builds").into_parts().0;
    let response = page::render(&home.features, &state, timestamps::utc_now(), &request).expect("the page renders");
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    String::from_utf8_lossy(&body).into_owned()
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_page_offers_the_renewal_login_to_a_router_rejected_row_and_names_a_missing_one() {
    let home = home("page-rows", true);
    let form = format!("action=\"/api/host-auth/logins/{}/start\"", home.router_name);

    // The browser-login cards follow the host rows and carry their own start forms, so the host rows are read alone.
    let host_rows = |body: &str| body.split("<section class=\"login\">").next().expect("the page has text").to_string();

    let mapped = page_body(&home, &[("host1", "router-rejected")]).await;
    assert!(host_rows(&mapped).contains(&form), "a router-rejected row with a mapped login offers its start route");
    assert!(mapped.contains("Needs renewal"), "the state is labelled as needing renewal");
    assert!(!mapped.contains("No renewal login is configured"));

    let unmapped = page_body(&home, &[("host2", "router-rejected")]).await;
    assert!(unmapped.contains("No renewal login is configured for this host"));
    assert!(!host_rows(&unmapped).contains(&form), "a row with no mapped login shows no start form");

    let unreachable = page_body(&home, &[("host2", "unreachable")]).await;
    assert!(!host_rows(&unreachable).contains(&form), "an unreachable row takes no renewal login");
    assert!(!unreachable.contains("No renewal login is configured"));
    assert!(unreachable.contains("Nothing to renew"), "an unreachable row keeps its current next step");
  }

  #[tokio::test]
  async fn the_page_and_the_login_routes_claim_their_slashed_forms_for_the_redirect() {
    assert!(!matches("/host-auth/logins"), "the removed logins page is no longer a route");
    assert!(matches("/api/host-auth/logins/aws-sso/start"));
    assert!(matches("/api/host-auth/logins/aws-sso/readback"));
    assert!(matches("/api/host-auth/logins/a-b-c/start"));
    for path in [
      "/host-auth/logins/",
      "/api/host-auth/logins//start",
      "/api/host-auth/logins//readback",
      "/api/host-auth/logins/a/b/start",
      "/api/host-auth/logins/a/b/readback",
    ] {
      assert!(!matches(path), "{path} should not be claimed");
    }
  }
}
