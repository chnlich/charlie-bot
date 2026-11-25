//! The host-auth routes: the status page, the status and row JSON, the manual probe, and each login's start, row check and
//! input routes.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::{get_only, post_only};
use charliebot_registry::FeatureContext;
use http::{StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::browser_login::{self, InputRefused, StartFailed};
use crate::config::{self, BrowserLoginConfig};
use crate::host_auth::{self, RoundSlot};
use crate::page;
use crate::rows;
use crate::timestamps;

#[cfg(not(test))]
use crate::probe::probe_host_fresh as host_probe;
#[cfg(test)]
use crate::test_support::bound_probe as host_probe;

/// The host-auth routes, registered with the application feature context as shared state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/host-auth", get_only(host_auth_page))
    .route("/api/host-auth/status", get_only(host_auth_status))
    .route("/api/host-auth/probe", post_only(host_auth_probe))
    .route("/api/host-auth/rows/{id}", get_only(host_auth_row))
    .route("/api/host-auth/rows/{id}/check", post_only(host_auth_row_check))
    .route("/api/host-auth/logins/{name}/start", post_only(browser_login_start))
    .route("/api/host-auth/logins/{name}/input", post_only(browser_login_input))
    .with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect: a slashed form of one of them redirects to it.
pub fn matches(path: &str) -> bool {
  if matches!(path, "/host-auth" | "/api/host-auth/status" | "/api/host-auth/probe") {
    return true;
  }
  login_action_name(path, "start").or_else(|| login_action_name(path, "input")).is_some() || row_id_of(path).is_some()
}

/// The login name of `/api/host-auth/logins/{name}/<action>`: one non-empty segment, as matchit's parameter captures.
fn login_action_name<'a>(path: &'a str, action: &str) -> Option<&'a str> {
  let name = path.strip_prefix("/api/host-auth/logins/")?.strip_suffix(action)?.strip_suffix('/')?;
  (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// The row id of `/api/host-auth/rows/{id}` or `/api/host-auth/rows/{id}/check`: one non-empty segment.
fn row_id_of(path: &str) -> Option<&str> {
  let rest = path.strip_prefix("/api/host-auth/rows/")?;
  let rest = rest.strip_suffix('/').unwrap_or(rest);
  let id = rest.strip_suffix("/check").unwrap_or(rest);
  (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// The browser logins of the config, in config order.
fn load_logins(features: &FeatureContext) -> anyhow::Result<Vec<BrowserLoginConfig>> {
  let cfg = features.config.get()?;
  Ok(config::browser_logins(&cfg))
}

/// `GET /host-auth`: the standing page, rendered from the state file and the config. It runs no check: each login's row
/// is checked by the browser after the page is shown.
async fn host_auth_page(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let rendered = tokio::task::spawn_blocking(move || {
    let state_path =
      host_auth::default_state_path().map_err(|error| responses::Failure::new("Exception", error.to_string()))?;
    let state = host_auth::load_state(&state_path);
    page::render(&features, &state, timestamps::utc_now(), &request)
  })
  .await
  .expect("rendering the host-auth page does not panic");
  match rendered {
    Ok(response) => response,
    Err(failure) => responses::internal_error(&failure.kind, failure.detail),
  }
}

/// `GET /api/host-auth/status`: the state file, with the table's `rows` and the `summary` of the rows that need the user
/// and the broken rows.
async fn host_auth_status(State(features): State<Arc<FeatureContext>>) -> Response {
  let logins = match load_logins(&features) {
    Ok(logins) => logins,
    Err(error) => return failure(error),
  };
  let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
    status_body(&host_auth::default_state_path()?, &logins, timestamps::utc_now())
  })
  .await
  .expect("reading the host-auth status does not panic");
  match body {
    Ok(value) => responses::json(StatusCode::OK, &value),
    Err(error) => failure(error),
  }
}

/// The status body: the state, with its `rows` and `summary` added.
pub fn status_body(state_path: &Path, logins: &[BrowserLoginConfig], now: OffsetDateTime) -> anyhow::Result<Value> {
  let mut state = host_auth::load_state(state_path);
  let rows = rows::rows(&state, logins, now)?;
  let summary = rows::summary(&rows);
  state.insert("rows".to_string(), Value::Array(rows));
  state.insert("summary".to_string(), summary);
  Ok(Value::Object(state))
}

/// `GET /api/host-auth/rows/{id}`: one row as the table shows it, from the state and the live attempt.
async fn host_auth_row(State(features): State<Arc<FeatureContext>>, RoutePath(id): RoutePath<String>) -> Response {
  let logins = match load_logins(&features) {
    Ok(logins) => logins,
    Err(error) => return failure(error),
  };
  let lookup = id.clone();
  let found = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
    let state = host_auth::load_state(&host_auth::default_state_path()?);
    rows::row(&state, &logins, &lookup, timestamps::utc_now())
  })
  .await
  .expect("reading a host-auth row does not panic");
  match found {
    Ok(Some(row)) => responses::json(StatusCode::OK, &row),
    Ok(None) => no_such_row(&id),
    Err(error) => failure(error),
  }
}

/// The query of a row check. `link_probe=true` also runs the login's link probe, which the self-check asks for, and
/// only for a login whose row shows that it needs approval. The page's row refresh sends no query, so it checks the
/// readback only.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckQuery {
  #[serde(default)]
  link_probe: bool,
}

/// `POST /api/host-auth/rows/{id}/check`: run the row's check, persist its result, and answer the row.
async fn host_auth_row_check(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(id): RoutePath<String>,
  Query(query): Query<CheckQuery>,
) -> Response {
  let logins = match load_logins(&features) {
    Ok(logins) => logins,
    Err(error) => return failure(error),
  };
  let outcome: anyhow::Result<Option<Value>> = async {
    let (state_path, config_path) = host_auth::default_paths()?;
    if !rows::check_row(&state_path, &config_path, &logins, &id, query.link_probe, host_probe).await? {
      return Ok(None);
    }
    rows::row(&host_auth::load_state(&state_path), &logins, &id, timestamps::utc_now())
  }
  .await;
  match outcome {
    Ok(Some(row)) => responses::json(StatusCode::OK, &row),
    Ok(None) => no_such_row(&id),
    Err(error) => failure(error),
  }
}

/// `POST /api/host-auth/probe`: start a forced round unless one runs, and return to the page either way.
async fn host_auth_probe(State(features): State<Arc<FeatureContext>>) -> Response {
  let logins = match load_logins(&features) {
    Ok(logins) => logins,
    Err(error) => return failure(error),
  };
  if let Some(slot) = RoundSlot::claim() {
    tokio::spawn(manual_round(slot, logins));
  }
  see_other()
}

/// The manual round: forced, with its failure logged. The slot is released when the round ends.
async fn manual_round(_slot: RoundSlot, logins: Vec<BrowserLoginConfig>) {
  let outcome = async {
    let (state_path, config_path) = host_auth::default_paths()?;
    host_auth::run_round(true, &state_path, &config_path, &logins).await?;
    anyhow::Ok(())
  }
  .await;
  if let Err(error) = outcome {
    tracing::error!(error = %format!("{error:#}"), "host_auth_manual_round_failed");
  }
}

/// `POST /api/host-auth/logins/{name}/start`: start the named login and redirect to its approval link. It answers 409
/// with the concurrent-wait page when another Okta authorization holds the router, and 502 with the failure body when
/// the login gives no link for any other reason.
async fn browser_login_start(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(name): RoutePath<String>,
) -> Response {
  let login = match login_named(&features, &name) {
    Ok(Some(login)) => login,
    Ok(None) => return no_such_login(&name),
    Err(error) => return failure(error),
  };
  match browser_login::start_login(&login).await {
    Ok(link) => see_other_to(&link),
    Err(StartFailed::ConcurrentWait) => concurrent_wait_page(),
    Err(StartFailed::Failed(body)) => responses::plain_text(StatusCode::BAD_GATEWAY, &body),
  }
}

/// The text of the 409 answer. It names the places another authorization can have started from, so the user knows where
/// to look.
const CONCURRENT_WAIT_TEXT: &str = concat!(
  "A previous Okta authorization is still waiting for browser confirmation. It was started by another session, ",
  "a terminal, an earlier Start on this row, or a link check of this row. Complete it where its link is shown, ",
  "or wait up to 5 minutes, then click Start again."
);

/// The 409 answer to a start that met a concurrent wait: the wait's text, and a link back to the page.
fn concurrent_wait_page() -> Response {
  let body = format!("<!doctype html>\n<p>{CONCURRENT_WAIT_TEXT}</p>\n<p><a href=\"/host-auth\">/host-auth</a></p>\n");
  Response::builder()
    .status(StatusCode::CONFLICT)
    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
    .body(Body::from(body))
    .expect("the conflict page builds")
}

/// `POST /api/host-auth/logins/{name}/input`: write the body's `value` to the login that waits for it, once. The value
/// goes to the login and nowhere else: no log line, state file or response carries it.
async fn browser_login_input(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(name): RoutePath<String>,
  body: String,
) -> Response {
  let login = match login_named(&features, &name) {
    Ok(Some(login)) => login,
    Ok(None) => return no_such_login(&name),
    Err(error) => return failure(error),
  };
  if login.input_prompt.is_none() {
    return responses::http_exception_with_detail(StatusCode::CONFLICT, &format!("login '{name}' takes no input"));
  }
  let Some(value) = input_value(&body) else {
    return responses::http_exception_with_detail(
      StatusCode::BAD_REQUEST,
      "the body must be {\"value\": one non-empty line}",
    );
  };
  match browser_login::give_input(&login.name, value).await {
    Ok(()) => responses::json(StatusCode::OK, &json!({ "ok": true })),
    Err(InputRefused::NotAwaiting) => {
      responses::http_exception_with_detail(StatusCode::CONFLICT, &format!("login '{name}' is not waiting for a value"))
    }
    Err(InputRefused::Write(error)) => {
      tracing::error!(login = %name, error = %error, "browser_login_input_failed");
      responses::internal_error("OSError", "the value could not be written to the login".to_string())
    }
  }
}

/// The value of an input body, `{"value": string}`: one non-empty line with its surrounding spaces trimmed. A body that
/// does not parse, or a value that is empty or holds a line break, is refused with no part of the body in the answer.
fn input_value(body: &str) -> Option<String> {
  let parsed: Value = serde_json::from_str(body).ok()?;
  let value = parsed.get("value")?.as_str()?.trim();
  (!value.is_empty() && !value.contains(['\n', '\r'])).then(|| value.to_string())
}

/// The login `name` names in the current config, copied out, or `None` when no login has that name.
fn login_named(features: &FeatureContext, name: &str) -> anyhow::Result<Option<BrowserLoginConfig>> {
  Ok(load_logins(features)?.into_iter().find(|login| login.name == name))
}

/// The 404 answer for a login name the config does not hold.
fn no_such_login(name: &str) -> Response {
  responses::http_exception_with_detail(StatusCode::NOT_FOUND, &format!("no browser login named '{name}'"))
}

/// The 404 answer for a row id the table does not hold.
fn no_such_row(id: &str) -> Response {
  responses::http_exception_with_detail(StatusCode::NOT_FOUND, &format!("no host-auth row named '{id}'"))
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
  use std::ffi::OsString;
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::Path;
  use std::time::Duration;

  use axum::body::{Body, to_bytes};
  use axum::http::{HeaderMap, Method, Request, StatusCode};
  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME, CharlieBotConfig};
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::event_types::{CONFIG_RELOAD_FAILED_EVENT, CREDENTIALS_RELOAD_FAILED_EVENT};
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks::store_hooks;
  use charliebot_registry::templating::Templating;
  use charliebot_registry::{FeatureContext, Registry, StartupState};
  use charliebot_store::Store;
  use tempfile::TempDir;
  use tower::ServiceExt;

  use super::*;
  use crate::browser_login::Phase;
  use crate::login_state::{self, LinkMatch, LoginStatus};
  use crate::process_tree::is_running;

  const LINK: &str = "https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21";
  const SSH_CONFIG: &str =
    "Host host0\n  HostName host0.example.internal\n\nHost host1\n  HostName host1.example.internal\n";

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

  /// A home whose `host_auth:` section names these logins, with the router's host and a fake `HOME` whose ssh config
  /// names `host0` and `host1`. The names carry the test's prefix, so parallel tests share no record.
  struct Home {
    dir: TempDir,
    prefix: String,
    router: Router,
    ok_name: String,
    failing_name: String,
    held_name: String,
    router_name: String,
    tree_name: String,
    missing_name: String,
    input_name: String,
    link_name: String,
  }

  fn home(prefix: &str, templating: bool) -> Home {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let names = |suffix: &str| format!("{prefix}-{suffix}");
    let (ok_name, failing_name, router_name) = (names("aws-sso"), names("failing-sso"), names("router-host1"));
    let (tree_name, missing_name, input_name, marked_name) =
      (names("tree"), names("missing"), names("input"), names("marked"));
    let link_name = names("needing-link");
    let held_name = names("held-sso");
    let missing_readback = dir.path().join(format!("{prefix}-no-such-readback")).display().to_string();
    // The login leaves a marker when it runs, so a test shows whether a request started it.
    let login =
      write_script(dir.path(), &format!("{prefix}-login"), &format!("touch \"$0.started\"\nprintf '%s\\n' '{LINK}'\n"));
    let failing = write_script(dir.path(), &format!("{prefix}-failing"), "echo starting; exit 3\n");
    let readback = write_script(dir.path(), &format!("{prefix}-readback"), "echo valid\n");
    let failing_readback = write_script(dir.path(), &format!("{prefix}-failing-readback"), "echo failed\nexit 1\n");
    // A router that another authorization holds: it prints the wait and exits with the router's refusal.
    let held = write_script(
      dir.path(),
      &format!("{prefix}-held"),
      concat!(
        "printf '%s\\n' 'A previous Okta authorization is still waiting for browser confirmation: ",
        "complete it, or wait up to 5 minutes, then reconnect.'\nexit 255\n"
      ),
    );
    // The shell records its pid and its background child's pid beside itself, then both wait. The background child holds
    // the output pipe, so only the timeout's tree kill ends the readback.
    let tree_readback = write_script(
      dir.path(),
      &format!("{prefix}-tree-readback"),
      "echo $$ > \"$0.root\"\nsleep 600 & echo $! > \"$0.child\"\nsleep 600\n",
    );
    // The login router's flow: a link with no code, the code on its own line, then an Enter prompt with no newline.
    let router_login = write_script(
      dir.path(),
      &format!("{prefix}-router-login"),
      r#"printf 'Open: https://example.invalid/activate\n'
printf 'Code: ABCD-EFGH\n'
printf 'Press Enter after approval: '
if read -r answer < /dev/tty; then :; else exit 7; fi
"#,
    );
    // The router's check runs only when the row is not host-bound, so a marker shows it ran.
    let router_marker = write_script(dir.path(), &format!("{prefix}-router-marker"), "touch \"$0.ran\"\nexit 1\n");
    let input_login = write_script(
      dir.path(),
      &format!("{prefix}-input-login"),
      &format!("printf '%s\\n' '{LINK}'\nread -r typed\nprintf '%s' \"$typed\" > \"$0.got\"\nexit 0\n"),
    );
    // A readback that touches its marker, so a test shows whether a check ran it.
    let marked_readback = write_script(dir.path(), &format!("{prefix}-marked-readback"), "touch \"$0.ran\"\nexit 0\n");
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
      readback_command: [{router_marker}]
      use_pty: true
      enter_after_pattern: 'Press Enter after approval'
      user_code_pattern: 'Code:\\s*(\\S+)'
      lifetime_sec: 86400
    - name: {held_name}
      label: Held SSO
      login_command: [{held}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{readback}]
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
    - name: {input_name}
      label: Input SSO
      login_command: [{input_login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{readback}]
      input_prompt: Paste the code
    - name: {marked_name}
      label: Marked SSO
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{marked_readback}]
    - name: {link_name}
      label: Needing SSO with a link
      login_command: [{login}]
      approval_url_pattern: 'https://device\\.sso\\S*'
      readback_command: [{failing_readback}]
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
      CONFIG_RELOAD_FAILED_EVENT,
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join(CREDENTIALS_FILENAME),
      CREDENTIALS_RELOAD_FAILED_EVENT,
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
    Home {
      dir,
      prefix: prefix.to_string(),
      router,
      ok_name,
      failing_name,
      held_name,
      router_name,
      tree_name,
      missing_name,
      input_name,
      link_name,
    }
  }

  /// The two variables a test points at its home, as they were before the test. Dropping the guard puts them back, so no
  /// later test reads a home this one has deleted.
  struct HomeEnv {
    charliebot_home: Option<OsString>,
    home: Option<OsString>,
  }

  impl Drop for HomeEnv {
    fn drop(&mut self) {
      // SAFETY: the test holds the round-slot lock until this guard drops, so no other test touches these variables.
      unsafe {
        match self.charliebot_home.take() {
          Some(value) => std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, value),
          None => std::env::remove_var(charliebot_core::home::CHARLIEBOT_HOME_ENV),
        }
        match self.home.take() {
          Some(value) => std::env::set_var("HOME", value),
          None => std::env::remove_var("HOME"),
        }
      }
    }
  }

  /// The guard that points the process at the home: its state file through `CHARLIEBOT_HOME`, its ssh config through
  /// `HOME`. The caller holds the round-slot lock, which every test that sets these variables takes, and keeps the
  /// returned guard under it.
  fn use_home(home: &Home) -> HomeEnv {
    let ssh = home.dir.path().join(".ssh");
    fs::create_dir_all(&ssh).expect("the ssh directory creates");
    fs::write(ssh.join("config"), SSH_CONFIG).expect("the ssh config writes");
    let before = HomeEnv {
      charliebot_home: std::env::var_os(charliebot_core::home::CHARLIEBOT_HOME_ENV),
      home: std::env::var_os("HOME"),
    };
    // SAFETY: every test that sets these variables holds the round-slot lock, so no two of them write them at once.
    unsafe {
      std::env::set_var(charliebot_core::home::CHARLIEBOT_HOME_ENV, home.dir.path());
      std::env::set_var("HOME", home.dir.path());
    }
    before
  }

  /// One route call: its status, headers and body text.
  async fn call(router: Router, method: Method, path: &str, body: &str) -> (StatusCode, HeaderMap, String) {
    let request =
      Request::builder().method(method).uri(path).body(Body::from(body.to_string())).expect("the request builds");
    let response = router.oneshot(request).await.expect("the route answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
  }

  async fn get_json(router: Router, path: &str) -> (StatusCode, Value) {
    let (status, _, body) = call(router, Method::GET, path, "").await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
  }

  /// The row of `id`, polled until its login is no longer in progress.
  async fn settle(router: &Router, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
      let (_, row) = get_json(router.clone(), &format!("/api/host-auth/rows/{id}")).await;
      if row["busy"] == json!("") {
        return row;
      }
      assert!(tokio::time::Instant::now() < deadline, "the row {id} stayed busy");
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_start_route_redirects_to_the_approval_link() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("start-ok", false);
    let _home_env = use_home(&home);
    let (status, headers, body) =
      call(home.router, Method::POST, &format!("/api/host-auth/logins/{}/start", home.ok_name), "").await;
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
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("start-fail", false);
    let _home_env = use_home(&home);
    let (status, headers, body) =
      call(home.router, Method::POST, &format!("/api/host-auth/logins/{}/start", home.failing_name), "").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
      headers.get(header::CONTENT_TYPE).expect("a plain-text body names its type"),
      "text/plain; charset=utf-8"
    );
    assert_eq!(body, "exit 3\nstarting");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_start_route_answers_409_html_while_another_authorization_holds_the_router() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("start-held", false);
    let _home_env = use_home(&home);
    let (status, headers, body) =
      call(home.router, Method::POST, &format!("/api/host-auth/logins/{}/start", home.held_name), "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(headers.get(header::CONTENT_TYPE).expect("the page names its type"), "text/html; charset=utf-8");
    assert!(body.contains(CONCURRENT_WAIT_TEXT), "{body}");
    assert!(body.contains("href=\"/host-auth\""), "the page links back to /host-auth: {body}");
  }

  #[tokio::test]
  async fn an_unknown_login_name_answers_404_with_its_detail() {
    let home = home("start-unknown", false);
    let (status, _, body) = call(home.router, Method::POST, "/api/host-auth/logins/nope/start", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let detail: Value = serde_json::from_str(&body).expect("the detail is JSON");
    assert_eq!(detail["detail"], Value::String("no browser login named 'nope'".to_string()));
  }

  #[tokio::test]
  async fn the_readback_route_is_gone() {
    let home = home("readback-gone", false);
    let (status, _, _) =
      call(home.router, Method::GET, &format!("/api/host-auth/logins/{}/readback", home.ok_name), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn a_row_check_persists_its_result_and_answers_the_row() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("row-check", false);
    let _home_env = use_home(&home);
    let check = |id: &str| format!("/api/host-auth/rows/{id}/check");

    let (status, _, body) =
      call(home.router.clone(), Method::POST, &check(&format!("login:{}", home.ok_name)), "").await;
    assert_eq!(status, StatusCode::OK);
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("ok"));
    assert_eq!(row["detail"], json!("valid"));

    let (_, row) = get_json(home.router.clone(), &format!("/api/host-auth/rows/login:{}", home.failing_name)).await;
    assert_eq!(row["state"], json!("unchecked"), "a row no check has answered reads unchecked");
    let (status, _, _) =
      call(home.router.clone(), Method::POST, &check(&format!("login:{}", home.failing_name)), "").await;
    assert_eq!(status, StatusCode::OK);
    let (_, row) = get_json(home.router.clone(), &format!("/api/host-auth/rows/login:{}", home.failing_name)).await;
    assert_eq!(row["state"], json!("needs_user"), "a failed check asks the user");

    let (_, _, body) =
      call(home.router.clone(), Method::POST, &check(&format!("login:{}", home.missing_name)), "").await;
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("broken"), "a readback whose script is gone is broken");
    assert!(row["detail"].as_str().expect("the detail is text").starts_with("missing file: "));

    let (status, _, _) = call(home.router.clone(), Method::POST, &check("login:nope"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown row has no check");
    let (status, _) = get_json(home.router.clone(), "/api/host-auth/rows/host:nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let state =
      host_auth::read_state(&host_auth::default_state_path().expect("the state path")).expect("the state reads");
    assert_eq!(
      login_state::login_state(&state, &home.ok_name).expect("the record reads").status,
      Some(LoginStatus::Ok)
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_link_probe_runs_only_for_a_login_that_needs_approval() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("link-probe", false);
    let _home_env = use_home(&home);
    let started = home.dir.path().join(format!("{}-login.started", home.prefix));
    let check = |id: &str, query: &str| format!("/api/host-auth/rows/{id}/check{query}");
    let ok_id = format!("login:{}", home.ok_name);
    let failing_id = format!("login:{}", home.failing_name);
    let needing_id = format!("login:{}", home.link_name);
    let state_path = host_auth::default_state_path().expect("the state path");

    // The page's row refresh sends no query: it checks the readback and starts no login.
    let (status, _, _) = call(home.router.clone(), Method::POST, &check(&ok_id, ""), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!started.exists(), "a row refresh starts no login process");

    // A row that shows as working has no approval to find a link for: its probe is refused before any login starts, and
    // records nothing.
    let (status, _, _) = call(home.router.clone(), Method::POST, &check(&ok_id, "?link_probe=true"), "").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "a working login's link probe is refused");
    assert!(!started.exists(), "a refused probe starts no login");
    let state = host_auth::read_state(&state_path).expect("the state reads");
    assert_eq!(login_state::login_state(&state, &home.ok_name).expect("the record reads").link_match, None);

    // A login that needs approval and prints its link records a pass, and its row still asks the user. No answer or
    // state file holds the link.
    let (status, _, body) = call(home.router.clone(), Method::POST, &check(&needing_id, "?link_probe=true"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(started.exists(), "the link probe starts the login");
    assert!(!body.contains(LINK), "the answer holds no link");
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("needs_user"), "a link that prints leaves the row asking the user");
    assert!(!fs::read_to_string(&state_path).expect("the state reads").contains(LINK), "the state file holds no link");
    let state = host_auth::read_state(&state_path).expect("the state reads");
    let record = login_state::login_state(&state, &home.link_name).expect("the record reads");
    assert_eq!(record.link_match, Some(LinkMatch::Pass));

    // A login that needs approval and prints no link breaks its row, and a readback refresh that still needs approval
    // keeps that.
    let (status, _, body) = call(home.router.clone(), Method::POST, &check(&failing_id, "?link_probe=true"), "").await;
    assert_eq!(status, StatusCode::OK);
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("broken"), "a link probe with no link breaks the row, whatever the readback said");
    let (_, _, body) = call(home.router.clone(), Method::POST, &check(&failing_id, ""), "").await;
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("broken"), "a readback refresh keeps the link failure");
    let (_, status_body) = get_json(home.router.clone(), "/api/host-auth/status").await;
    let broken = status_body["summary"]["broken"].as_array().expect("the broken list");
    assert!(broken.contains(&json!(failing_id)), "the summary lists the broken row");

    // A stored link failure that outlived a passing check does not break the working row, and the next passing check
    // clears it.
    login_state::record_link_match(&state_path, &home.ok_name, LinkMatch::Fail).await.expect("the link result records");
    let (_, row) = get_json(home.router.clone(), &format!("/api/host-auth/rows/{ok_id}")).await;
    assert_eq!(row["state"], json!("ok"), "a stored link failure does not break a row whose check passes");
    let (status, _, _) = call(home.router.clone(), Method::POST, &check(&ok_id, ""), "").await;
    assert_eq!(status, StatusCode::OK);
    let state = host_auth::read_state(&state_path).expect("the state reads");
    let record = login_state::login_state(&state, &home.ok_name).expect("the record reads");
    assert_eq!(record.link_match, None, "a passing check clears the stored link result");

    // Only a login has a link to probe, and the route refuses a query key it does not know.
    let (status, _, _) = call(home.router.clone(), Method::POST, &check("host:host0", "?link_probe=true"), "").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "a host row has no link to probe");
    let (status, _, _) = call(home.router.clone(), Method::POST, &check(&ok_id, "?link-probe=true"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an unknown query key is refused");
  }

  #[tokio::test]
  async fn a_host_bound_row_check_runs_the_single_host_probe_and_no_readback() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("bound-check", false);
    let _home_env = use_home(&home);
    crate::test_support::set_bound_answer(0, "exit 0");
    let state_path = host_auth::default_state_path().expect("the state path");
    login_state::record_link_match(&state_path, &home.router_name, LinkMatch::Fail)
      .await
      .expect("the link result records");
    let (status, _, body) =
      call(home.router.clone(), Method::POST, &format!("/api/host-auth/rows/login:{}/check", home.router_name), "")
        .await;
    assert_eq!(status, StatusCode::OK);
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("ok"), "the probe answered ok");
    assert!(!home.dir.path().join(format!("{}-router-marker.ran", home.prefix)).exists(), "no readback ran");
    let state = host_auth::read_state(&state_path).expect("the state reads");
    let host = host_auth::host_entries(&state).expect("the hosts read");
    assert!(host.iter().any(|entry| entry["alias"] == json!("host1") && entry["status"] == json!("ok")));
    let record = login_state::login_state(&state, &home.router_name).expect("the record reads");
    assert_eq!(record.link_match, None, "a passing host probe clears the bound login's stored link result");
    assert!(row["expiry_text"].as_str().expect("the expiry text").is_empty(), "a row never approved shows no expiry");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_check_past_its_timeout_leaves_no_process_of_its_tree_and_answers_within_the_margin() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("tree", false);
    let _home_env = use_home(&home);
    let started = tokio::time::Instant::now();
    let (status, _, body) =
      call(home.router.clone(), Method::POST, &format!("/api/host-auth/rows/login:{}/check", home.tree_name), "").await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    let row: Value = serde_json::from_str(&body).expect("the row is JSON");
    assert_eq!(row["state"], json!("needs_user"), "a check its timeout ended asks the user");
    assert!(elapsed <= Duration::from_secs(32), "the 30 s readback timeout ends the check by 32 s, took {elapsed:?}");

    let script = home.dir.path().join(format!("{}-tree-readback", home.prefix));
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
  async fn the_input_route_answers_its_status_codes_and_gives_the_value_once() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("input-route", false);
    let _home_env = use_home(&home);
    let input = |name: &str| format!("/api/host-auth/logins/{name}/input");

    let (status, _, _) = call(home.router.clone(), Method::POST, &input("nope"), r#"{"value":"x"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(home.router.clone(), Method::POST, &input(&home.ok_name), r#"{"value":"x"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "a login with no input_prompt takes no input");
    let (status, _, _) = call(home.router.clone(), Method::POST, &input(&home.input_name), r#"{"value":"x"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "a login with no attempt waiting for approval takes no input");

    let (status, _, _) =
      call(home.router.clone(), Method::POST, &format!("/api/host-auth/logins/{}/start", home.input_name), "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !live_input_open(&home.input_name) {
      assert!(tokio::time::Instant::now() < deadline, "the attempt waits for its value");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for body in [r#"{"value":""}"#, r#"{"value":"   "}"#, "{\"value\":\"a\\nb\"}", r#"{"value":3}"#, "not json"] {
      let (status, _, _) = call(home.router.clone(), Method::POST, &input(&home.input_name), body).await;
      assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    let value = "CODE-ROUTE-5A6B";
    let (status, _, answer) =
      call(home.router.clone(), Method::POST, &input(&home.input_name), &format!(r#"{{"value":"  {value}  "}}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, r#"{"ok":true}"#, "the answer carries no part of the value");
    let (status, _, _) =
      call(home.router.clone(), Method::POST, &input(&home.input_name), r#"{"value":"again"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "a second value finds no sink");

    let row = settle(&home.router, &format!("login:{}", home.input_name)).await;
    assert_eq!(row["state"], json!("ok"));
    let got =
      fs::read_to_string(home.dir.path().join(format!("{}-input-login.got", home.prefix))).expect("the login read it");
    assert_eq!(got, value);
    let state = fs::read_to_string(host_auth::default_state_path().expect("the state path")).expect("the state reads");
    assert!(!state.contains(value), "the value is in no state file");
  }

  fn live_input_open(name: &str) -> bool {
    browser_login::live_attempt(name)
      .is_some_and(|attempt| attempt.input_open && attempt.phase == Phase::WaitingForApproval)
  }

  #[tokio::test]
  async fn the_status_route_lists_every_row_once_and_its_summary_lists_are_disjoint() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("status-route", false);
    let _home_env = use_home(&home);
    let mut state = host_auth::empty_state();
    state.insert(
      "hosts".to_string(),
      json!([
        {"alias": "host0", "hostname": "host0.example.internal", "status": "unreachable", "detail": "exit 255"},
        {"alias": "host1", "hostname": "host1.example.internal", "status": "router-rejected", "detail": ""},
      ]),
    );
    host_auth::save_state(&state, &host_auth::default_state_path().expect("the state path")).expect("the state writes");
    let (status, body) = get_json(home.router.clone(), "/api/host-auth/status").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().expect("the rows are a list");
    // Nine logins, and the one ssh host without a login: host0. host1 is renewed by a login, so it has no row.
    assert_eq!(rows.len(), 10);
    assert!(rows.iter().any(|row| row["id"] == json!("host:host0")));
    let needs = body["summary"]["needs_user"].as_array().expect("the needs-user list");
    let broken = body["summary"]["broken"].as_array().expect("the broken list");
    assert!(needs.iter().all(|id| !broken.contains(id)), "no row is in both lists");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_page_renders_from_the_persisted_state_and_runs_no_check() {
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    let home = home("page-persisted", true);
    let _home_env = use_home(&home);
    let state_path = host_auth::default_state_path().expect("the state path");
    fs::write(
      &state_path,
      format!(
        "{}\n",
        json!({
          "probed_at": "2026-11-18T08:19:00.676000+00:00",
          "probe_running": false,
          "interval_sec": 1620,
          "hosts": [{
            "alias": "host0",
            "hostname": "host0.example.internal",
            "status": "ok",
            "detail": "",
            "last_probe_at": "2026-11-18T08:19:00+00:00",
            "last_change_at": null,
            "last_ok_at": null,
          }, {
            "alias": "host1",
            "hostname": "host1.example.internal",
            "status": "router-rejected",
            "detail": "Permission denied (keyboard-interactive).",
            "last_probe_at": "2026-11-18T08:19:00+00:00",
            "last_change_at": null,
            "last_ok_at": null,
          }],
          "logins": {
            (home.router_name.clone()): {"last_approved_at": "2026-11-18T08:16:29.928976+00:00"},
          },
        })
      ),
    )
    .expect("the fixture writes");
    let started = tokio::time::Instant::now();
    let (status, _, body) = call(home.router.clone(), Method::GET, "/host-auth", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(1), "the page answers at once");
    assert!(body.contains(&format!("data-row=\"login:{}\"", home.router_name)), "the login has a row");
    assert!(body.contains("data-row=\"host:host0\""), "the host without a login has a row");
    assert!(!home.dir.path().join(format!("{}-marked-readback.ran", home.prefix)).exists(), "no check ran");
  }

  #[tokio::test]
  async fn the_page_and_the_row_routes_claim_their_slashed_forms_for_the_redirect() {
    assert!(!matches("/host-auth/logins"), "the removed logins page is no longer a route");
    assert!(!matches("/api/host-auth/logins/aws-sso/readback"), "the removed readback route is not claimed");
    assert!(matches("/api/host-auth/logins/aws-sso/start"));
    assert!(matches("/api/host-auth/logins/aws-sso/input"));
    assert!(matches("/api/host-auth/rows/login:aws-sso/check"));
    assert!(matches("/api/host-auth/rows/login:aws-sso/"));
    for path in [
      "/host-auth/logins/",
      "/api/host-auth/logins//start",
      "/api/host-auth/logins/a/b/start",
      "/api/host-auth/rows//check",
      "/api/host-auth/rows/a/b/check",
    ] {
      assert!(!matches(path), "{path} should not be claimed");
    }
  }
}
