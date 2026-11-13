//! The host-auth routes: the page, the status JSON, the manual probe, and the browser-login page with its start
//! route.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::{get_only, post_only};
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::{STATIC_ASSET_VERSION_KEY, Templating};
use futures_util::future::join_all;
use http::{StatusCode, header};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::browser_login;
use crate::config::{BrowserLoginConfig, HostAuthConfig};
use crate::host_auth::{self, RoundSlot};
use crate::login_page;
use crate::page;
use crate::timestamps;

/// The browser-login page's template.
const LOGIN_TEMPLATE: &str = "browser_login.html";

/// Register host-auth routes with the application feature context as shared state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/host-auth", get_only(host_auth_page))
    .route("/host-auth/logins", get_only(browser_logins_page))
    .route("/api/host-auth/status", get_only(host_auth_status))
    .route("/api/host-auth/probe", post_only(host_auth_probe))
    .route("/api/host-auth/logins/{name}/start", post_only(browser_login_start))
    .with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect: a slashed form of one of them redirects to it.
pub fn matches(path: &str) -> bool {
  if matches!(path, "/host-auth" | "/host-auth/logins" | "/api/host-auth/status" | "/api/host-auth/probe") {
    return true;
  }
  // `/api/host-auth/logins/{name}/start`: the name is one non-empty segment, as matchit's parameter captures.
  match path.strip_prefix("/api/host-auth/logins/").and_then(|rest| rest.strip_suffix("/start")) {
    Some(name) => !name.is_empty() && !name.contains('/'),
    None => false,
  }
}

/// `GET /host-auth`: the standing page, rendered from the state file.
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
pub fn status_body(state_path: &Path, now: OffsetDateTime) -> anyhow::Result<Value> {
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

/// `GET /host-auth/logins`: the browser-login page. Only idle logins run their readback here; no page view starts a
/// login.
async fn browser_logins_page(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let logins = &cfg.section::<HostAuthConfig>("host_auth").browser_logins;
  let idle: Vec<&BrowserLoginConfig> =
    logins.iter().filter(|login| browser_login::live_attempt(&login.name).is_none()).collect();
  let results = join_all(idle.iter().map(|login| browser_login::page_readback(login))).await;
  let mut readbacks = HashMap::new();
  for (login, result) in idle.iter().zip(results) {
    match result {
      Ok(result) => {
        readbacks.insert(login.name.clone(), result);
      }
      Err(error) => return responses::internal_error("OSError", error.to_string()),
    }
  }
  let mut context = match login_page::context(logins, &readbacks) {
    Ok(context) => context,
    Err(error) => return responses::internal_error("Exception", error.to_string()),
  };
  let templating = match features.startup.require::<Templating>() {
    Ok(templating) => templating,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let asset_version = match templating.static_asset_version() {
    Ok(version) => version,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  if let Some(object) = context.as_object_mut() {
    object.insert(STATIC_ASSET_VERSION_KEY.to_string(), Value::String(asset_version));
  }
  match templating.render_page(&features, LOGIN_TEMPLATE, &context, &request) {
    Ok(response) => response,
    Err(error) => responses::internal_error(error.kind(), error.to_string()),
  }
}

/// `POST /api/host-auth/logins/{name}/start`: start the named login and redirect to its approval link; 502 with the
/// failure body when the login gives none.
async fn browser_login_start(
  State(features): State<Arc<FeatureContext>>,
  RoutePath(name): RoutePath<String>,
) -> Response {
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let Some(login) = cfg.section::<HostAuthConfig>("host_auth").browser_logins.iter().find(|login| login.name == name)
  else {
    return detail(StatusCode::NOT_FOUND, format!("no browser login named '{name}'"));
  };
  match browser_login::start_login(login).await {
    Ok(link) => see_other_to(&link),
    Err(failed) => responses::plain_text(StatusCode::BAD_GATEWAY, &failed.0),
  }
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

  /// A home whose `host_auth:` section names three logins: one whose command prints the approval link, one whose
  /// command fails after a line of output, and one renewing `host1` under a pseudo-terminal. The names carry the
  /// test's prefix, so parallel tests share no record.
  struct Home {
    _dir: TempDir,
    router: Router,
    features: Arc<FeatureContext>,
    ok_name: String,
    failing_name: String,
    router_name: String,
  }

  fn home(prefix: &str, templating: bool) -> Home {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let ok_name = format!("{prefix}-aws-sso");
    let failing_name = format!("{prefix}-failing-sso");
    let router_name = format!("{prefix}-router-host1");
    let login = write_script(dir.path(), &format!("{prefix}-login"), &format!("printf '%s\\n' '{LINK}'\n"));
    let failing = write_script(dir.path(), &format!("{prefix}-failing"), "echo starting; exit 3\n");
    let readback = write_script(dir.path(), &format!("{prefix}-readback"), "echo valid\n");
    let failing_readback = write_script(dir.path(), &format!("{prefix}-failing-readback"), "echo failed\nexit 1\n");
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
    Home { _dir: dir, router, features, ok_name, failing_name, router_name }
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
  #[ignore = "integration"]
  async fn the_login_page_renders_the_configured_logins_and_their_page_load_readback() {
    let home = home("page", true);
    let (status, headers, body) = call(home.router, Method::GET, "/host-auth/logins").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).expect("a page names its type"), "text/html; charset=utf-8");
    assert!(body.contains("Browser logins"), "the page renders its title");
    assert!(body.contains("AWS SSO"), "the configured login's label shows");
    assert!(body.contains("Failing SSO"), "every configured login gets a row");
    assert!(body.contains("exit 0"), "the page-load readback's exit shows");
    assert!(body.contains("valid"), "the page-load readback's output shows");
    assert!(body.contains("exit 1"), "the failing login's readback exit shows");
    assert!(body.contains("failed"), "the failing login's readback output shows");
    assert!(
      body.contains("<h2>AWS SSO<span class=\"badge\">exit 0</span></h2>"),
      "the exit mark sits in the card title row"
    );
    assert!(
      body.contains("<h2>Failing SSO<span class=\"badge\">exit 1</span></h2>"),
      "the failing exit mark sits in the card title row"
    );
    let pos_failing = body.find("Failing SSO").expect("failing card is present");
    let pos_ok = body.find("AWS SSO").expect("healthy card is present");
    assert!(pos_failing < pos_ok, "failed login card comes first");
    let start_path = format!("/api/host-auth/logins/{}/start", home.ok_name);
    assert!(body.contains(&start_path), "each row's form posts to its login's start route");
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
  async fn a_pty_login_answers_the_enter_prompt_and_the_start_route_redirects_to_the_code_link() {
    let home = home("pty", false);
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

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_page_offers_the_renewal_login_to_a_router_rejected_row_and_names_a_missing_one() {
    let home = home("page-rows", true);
    let form = format!("action=\"/api/host-auth/logins/{}/start\"", home.router_name);

    let mapped = page_body(&home, &[("host1", "router-rejected")]).await;
    assert!(mapped.contains(&form), "a router-rejected row with a mapped login offers its start route");
    assert!(mapped.contains("Needs renewal"), "the state is labelled as needing renewal");
    assert!(!mapped.contains("No renewal login is configured"));

    let unmapped = page_body(&home, &[("host2", "router-rejected")]).await;
    assert!(unmapped.contains("No renewal login is configured for this host"));
    assert!(!unmapped.contains(&form), "a row with no mapped login shows no start form");

    let unreachable = page_body(&home, &[("host2", "unreachable")]).await;
    assert!(!unreachable.contains(&form), "an unreachable row takes no renewal login");
    assert!(!unreachable.contains("No renewal login is configured"));
    assert!(unreachable.contains("Nothing to renew"), "an unreachable row keeps its current next step");
  }

  #[tokio::test]
  async fn the_page_and_the_start_route_claim_their_slashed_forms_for_the_redirect() {
    assert!(matches("/host-auth/logins"));
    assert!(matches("/api/host-auth/logins/aws-sso/start"));
    assert!(matches("/api/host-auth/logins/a-b-c/start"));
    for path in ["/host-auth/logins/", "/api/host-auth/logins//start", "/api/host-auth/logins/a/b/start"] {
      assert!(!matches(path), "{path} should not be claimed");
    }
  }
}
