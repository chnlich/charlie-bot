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
use charliebot_registry::templating::Templating;
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
    object.insert("static_asset_version".to_string(), Value::String(asset_version));
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

  /// A home whose `host_auth:` section names two logins: one whose command prints the approval link and one whose
  /// command fails after a line of output. The names carry the test's prefix, so parallel tests share no record.
  struct Home {
    _dir: TempDir,
    router: Router,
    ok_name: String,
    failing_name: String,
  }

  fn home(prefix: &str, templating: bool) -> Home {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let ok_name = format!("{prefix}-aws-sso");
    let failing_name = format!("{prefix}-failing-sso");
    let login = write_script(dir.path(), &format!("{prefix}-login"), &format!("printf '%s\\n' '{LINK}'\n"));
    let failing = write_script(dir.path(), &format!("{prefix}-failing"), "echo starting; exit 3\n");
    let readback = write_script(dir.path(), &format!("{prefix}-readback"), "echo valid\n");
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
      readback_command: [{readback}]
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
    Home { _dir: dir, router, ok_name, failing_name }
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
    let start_path = format!("/api/host-auth/logins/{}/start", home.ok_name);
    assert!(body.contains(&start_path), "each row's form posts to its login's start route");
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
