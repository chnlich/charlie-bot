//! The access-key gate (`src/runtime/api/auth.py`).
//!
//! Every HTTP request passes when no access key is configured, when its decoded path is public (`/`,
//! `/api/auth/status`, anything under `/static/`), or when it presents the key. The key arrives as an
//! `Authorization: Bearer` header or as the `charliebot_access_key` cookie. A bearer that is not the
//! key is run-token use: its signature decides, and the cookie is never consulted for it. A refused
//! `GET` whose `Accept` names `text/html` gets the login page; any other refusal gets a JSON 401.
//! Credentials compare in constant time.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use charliebot_compat::headers::header_text;
use charliebot_compat::responses;
use charliebot_core::run_token;
use charliebot_store::Store;
use http::request::Parts;
use http::{HeaderMap, Method, StatusCode, header};
use subtle::ConstantTimeEq;

use crate::state::AppState;
use crate::{cookies, request_path};

/// The cookie the login page sets; its served JavaScript spells the same name.
pub const ACCESS_KEY_COOKIE: &str = "charliebot_access_key";

pub const AUTH_STATUS_PATH: &str = "/api/auth/status";

const PUBLIC_PATHS: &[&str] = &["/", AUTH_STATUS_PATH];
const PUBLIC_PREFIXES: &[&str] = &["/static/"];

/// The self-contained page an unauthenticated browser navigation receives (`auth._LOGIN_PAGE`).
const LOGIN_PAGE: &str = include_str!("login_page.html");

/// The JSON 401 body: `json.dumps({"detail": "Unauthorized"})`, default separators.
const UNAUTHORIZED_JSON: &str = r#"{"detail": "Unauthorized"}"#;

/// What the gate does with one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
  Pass,
  LoginPage,
  JsonUnauthorized,
}

/// A credential that CPython's `hmac.compare_digest` refuses to compare: a `str` with non-ASCII
/// characters raises `TypeError`, which fails the request with a 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonAsciiCredential;

/// The verified caller of a route that takes it as an argument (`deps.require_caller`): the operator, or the run
/// that a verified run token names while that run is active. The structural routes that take it are ported with their
/// own parts, so until one exists the extractor below has no production caller.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerIdentity {
  Operator,
  Run(RunCallerIdentity),
}

/// The run a verified run token stands for.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCallerIdentity {
  pub session_id: String,
  pub run_id: String,
  pub agent: String,
}

/// Why a route refuses its caller: a 401 with this detail, or a failure reading the durable run records.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerRefusal {
  Unauthorized(String),
  Internal(String),
}

/// Resolve the caller of one request (`deps.require_caller`). No bearer, or the access key itself, is the operator.
/// Any other bearer is run-token use: it must verify under the key and name a run that is registered, unfinished and
/// launched. It never falls back to the cookie or to the access key.
#[allow(dead_code)]
pub fn resolve_caller(headers: &HeaderMap, key: Option<&str>, store: &Store) -> Result<CallerIdentity, CallerRefusal> {
  let authorization = header_text(headers, &header::AUTHORIZATION);
  let bearer = run_token::bearer_from_authorization(Some(&authorization));
  if bearer.is_empty() {
    return Ok(CallerIdentity::Operator);
  }
  let Some(key) = key else {
    return Err(CallerRefusal::Unauthorized("run token presented but no signing key is configured".to_string()));
  };
  if bearer == key {
    return Ok(CallerIdentity::Operator);
  }
  let claims = run_token::verify_run_token(bearer, key)
    .map_err(|error| CallerRefusal::Unauthorized(format!("invalid run token: {error}")))?;
  let refusal = store
    .run_identity_refusal(&claims.session_id, &claims.run_id)
    .map_err(|error| CallerRefusal::Internal(error.to_string()))?;
  if let Some(detail) = refusal {
    return Err(CallerRefusal::Unauthorized(detail.to_string()));
  }
  Ok(CallerIdentity::Run(RunCallerIdentity {
    session_id: claims.session_id,
    run_id: claims.run_id,
    agent: claims.agent,
  }))
}

impl FromRequestParts<Arc<AppState>> for CallerIdentity {
  type Rejection = Response;

  async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
    let key = state.access_key().map_err(|error| responses::internal_error("ValueError", error.to_string()))?;
    resolve_caller(&parts.headers, key.as_deref(), &state.features.store).map_err(|refusal| match refusal {
      CallerRefusal::Unauthorized(detail) => unauthorized_detail(&detail),
      CallerRefusal::Internal(message) => responses::internal_error("RuntimeError", message),
    })
  }
}

/// `hmac.compare_digest(candidate, key)` for two `str` values: constant-time equality, refusing non-ASCII text.
pub fn credential_matches(candidate: &str, key: &str) -> Result<bool, NonAsciiCredential> {
  if !candidate.is_ascii() || !key.is_ascii() {
    return Err(NonAsciiCredential);
  }
  Ok(bool::from(candidate.as_bytes().ct_eq(key.as_bytes())))
}

/// The gate's decision for an HTTP request with this method, decoded path and headers.
pub fn decide(
  method: &Method,
  path: &str,
  headers: &HeaderMap,
  key: Option<&str>,
) -> Result<Decision, NonAsciiCredential> {
  let Some(key) = key else {
    return Ok(Decision::Pass);
  };
  if PUBLIC_PATHS.contains(&path) || PUBLIC_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
    return Ok(Decision::Pass);
  }
  if has_access_key(headers, key)? {
    return Ok(Decision::Pass);
  }
  if method == Method::GET && header_text(headers, &header::ACCEPT).contains("text/html") {
    return Ok(Decision::LoginPage);
  }
  Ok(Decision::JsonUnauthorized)
}

fn has_access_key(headers: &HeaderMap, key: &str) -> Result<bool, NonAsciiCredential> {
  let authorization = header_text(headers, &header::AUTHORIZATION);
  let bearer = run_token::bearer_from_authorization(Some(&authorization));
  if !bearer.is_empty() {
    if credential_matches(bearer, key)? {
      return Ok(true);
    }
    // Run-token use: a valid signature passes (the route binds it to an active Run); an invalid one
    // fails closed without consulting the cookie.
    return Ok(run_token::verify_run_token(bearer, key).is_ok());
  }
  let cookie = cookie_key(headers);
  Ok(!cookie.is_empty() && credential_matches(&cookie, key)?)
}

/// The access-key cookie across every `Cookie` header (joined with `;`), or `""`.
fn cookie_key(headers: &HeaderMap) -> String {
  let joined: Vec<String> =
    headers.get_all(header::COOKIE).iter().map(|value| value.as_bytes().iter().map(|b| *b as char).collect()).collect();
  if joined.is_empty() {
    return String::new();
  }
  match cookies::cookie_value(&joined.join(";"), ACCESS_KEY_COOKIE) {
    Ok(Some(value)) => value,
    Ok(None) | Err(_) => String::new(),
  }
}

/// The WebSocket check (`auth.check_ws_auth`): the `token` query parameter must equal the key.
pub fn websocket_authorized(query: Option<&str>, key: Option<&str>) -> Result<bool, NonAsciiCredential> {
  let Some(key) = key else {
    return Ok(true);
  };
  // starlette's QueryParams.get returns the last value of a repeated parameter.
  let token = query
    .map(|query| {
      url_pairs(query).into_iter().rev().find(|(name, _)| name == "token").map(|(_, value)| value).unwrap_or_default()
    })
    .unwrap_or_default();
  credential_matches(&token, key)
}

/// `urllib.parse.parse_qsl(query, keep_blank_values=True)`: `&`-separated, `+` as space, percent-decoded.
fn url_pairs(query: &str) -> Vec<(String, String)> {
  query
    .split('&')
    .filter(|pair| !pair.is_empty())
    .map(|pair| {
      let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
      let decode = |text: &str| request_path::decode(&text.replace('+', " "));
      (decode(name), decode(value))
    })
    .collect()
}

/// The gate as an axum layer. It sits inside the request log, so its refusals are logged.
pub async fn layer(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
  let key = match state.access_key() {
    Ok(key) => key,
    Err(error) => return responses::internal_error("ValueError", error.to_string()),
  };
  match decide(request.method(), request_path::of(&request), request.headers(), key.as_deref()) {
    Ok(Decision::Pass) => next.run(request).await,
    Ok(Decision::LoginPage) => unauthorized("text/html; charset=utf-8", LOGIN_PAGE),
    Ok(Decision::JsonUnauthorized) => unauthorized("application/json", UNAUTHORIZED_JSON),
    Err(NonAsciiCredential) => {
      responses::internal_error("TypeError", "comparing strings with non-ASCII characters is not supported".to_string())
    }
  }
}

fn unauthorized(content_type: &'static str, body: &'static str) -> Response {
  Response::builder()
    .status(StatusCode::UNAUTHORIZED)
    .header(header::CONTENT_TYPE, content_type)
    .header(header::CONTENT_LENGTH, body.len())
    .body(Body::from(body))
    .expect("a fixed 401 response builds")
}

#[allow(dead_code)]
fn unauthorized_detail(detail: &str) -> Response {
  responses::json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "detail": detail }))
}

#[cfg(test)]
mod tests {
  use http::{HeaderName, HeaderValue};

  use super::*;
  use charliebot_core::run_token::RunTokenClaims;

  const KEY: &str = "k-1";

  /// A path, its request headers and the expected decision.
  type Case<'a> = (&'a str, Vec<(&'a HeaderName, String)>, Decision);

  fn headers(pairs: &[(&HeaderName, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
      map.append((*name).clone(), HeaderValue::from_str(value).unwrap());
    }
    map
  }

  fn get(path: &str, pairs: &[(&HeaderName, &str)]) -> Decision {
    decide(&Method::GET, path, &headers(pairs), Some(KEY)).unwrap()
  }

  #[test]
  fn the_decision_table_matches_auth_middleware() {
    use Decision::*;
    let run_token =
      run_token::sign_run_token(&RunTokenClaims { session_id: "s".into(), run_id: "r".into(), agent: "a".into() }, KEY)
        .unwrap();
    let bearer = |token: &str| format!("Bearer {token}");
    let cases: Vec<Case> = vec![
      ("/", vec![], Pass),
      ("/api/auth/status", vec![], Pass),
      ("/static/js/app.js", vec![], Pass),
      ("/static", vec![], JsonUnauthorized),
      ("/api/internal/version", vec![], JsonUnauthorized),
      ("/api/internal/version", vec![(&header::ACCEPT, "text/html,application/xhtml+xml".into())], LoginPage),
      ("/api/internal/version", vec![(&header::AUTHORIZATION, bearer(KEY))], Pass),
      ("/api/internal/version", vec![(&header::AUTHORIZATION, bearer(&run_token))], Pass),
      ("/api/internal/version", vec![(&header::AUTHORIZATION, bearer("wrong"))], JsonUnauthorized),
      // An invalid bearer fails closed even beside a valid cookie.
      (
        "/api/internal/version",
        vec![(&header::AUTHORIZATION, bearer("wrong")), (&header::COOKIE, format!("charliebot_access_key={KEY}"))],
        JsonUnauthorized,
      ),
      ("/api/internal/version", vec![(&header::COOKIE, format!("charliebot_access_key={KEY}"))], Pass),
      ("/api/internal/version", vec![(&header::COOKIE, "charliebot_access_key=nope".into())], JsonUnauthorized),
      ("/api/internal/version", vec![(&header::AUTHORIZATION, format!("bearer {KEY}"))], JsonUnauthorized),
    ];
    for (path, pairs, expected) in cases {
      let pairs: Vec<(&HeaderName, &str)> = pairs.iter().map(|(name, value)| (*name, value.as_str())).collect();
      assert_eq!(get(path, &pairs), expected, "{path} {pairs:?}");
    }
    // A non-GET HTML request gets the JSON 401; no key configured lets everything through.
    let html = headers(&[(&header::ACCEPT, "text/html")]);
    assert_eq!(decide(&Method::POST, "/api/x", &html, Some(KEY)).unwrap(), Decision::JsonUnauthorized);
    assert_eq!(decide(&Method::GET, "/api/x", &HeaderMap::new(), None).unwrap(), Decision::Pass);
  }

  #[test]
  fn websocket_tokens_compare_against_the_key() {
    assert_eq!(websocket_authorized(Some("token=k-1"), Some(KEY)), Ok(true));
    assert_eq!(websocket_authorized(Some("token=x&token=k-1"), Some(KEY)), Ok(true));
    assert_eq!(websocket_authorized(None, Some(KEY)), Ok(false));
    assert_eq!(websocket_authorized(Some("token=wrong"), Some(KEY)), Ok(false));
    assert_eq!(websocket_authorized(None, None), Ok(true));
  }

  #[test]
  fn non_ascii_credentials_fail_like_compare_digest() {
    assert_eq!(credential_matches("k\u{e9}", KEY), Err(NonAsciiCredential));
  }

  #[tokio::test]
  async fn the_caller_resolves_operators_and_refuses_runs_without_an_active_launch() {
    use charliebot_store::run_identity::{RUN_IDENTITY_NOT_LAUNCHED_DETAIL, RUN_IDENTITY_UNKNOWN_DETAIL};
    use charliebot_store::{RunKind, RunOutcome, RunOutcomeKind, RunRecord};

    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(crate::packages::registry().unwrap());
    let cfg = registry.load_config(home.path()).unwrap();
    let store = crate::store_setup::create(cfg, registry).unwrap();
    let claims = RunTokenClaims { session_id: "session-a".into(), run_id: "run-a".into(), agent: "worker".into() };
    let identity = CallerIdentity::Run(RunCallerIdentity {
      session_id: "session-a".into(),
      run_id: "run-a".into(),
      agent: "worker".into(),
    });
    let guard = store.control_lock().await;
    store
      .register_run_locked(
        &guard,
        "session-a",
        RunRecord {
          id: "run-a".into(),
          session_id: "session-a".into(),
          kind: RunKind::Work,
          input_event_ids: Vec::new(),
          task_spec_hash: None,
          task_spec_ref: None,
          prompt_snapshot_ref: None,
          retry_of_run_id: None,
          review_of_run_id: None,
          backend: None,
          model: None,
          native_session_id: None,
          pid: None,
          pid_start: None,
          started_at: None,
          ended_at: None,
          exit_code: None,
          repo_path: None,
          base_branch: None,
          branch_name: None,
          worktree_path: None,
          sequence_ref: None,
          raw_log_ref: None,
          events_ref: None,
          result_ref: None,
        },
        "synthetic task",
      )
      .await
      .unwrap();
    drop(guard);
    let token = run_token::sign_run_token(&claims, KEY).unwrap();
    let bearer = format!("Bearer {token}");
    let token_headers = headers(&[(&header::AUTHORIZATION, &bearer)]);
    let operator_headers = headers(&[(&header::AUTHORIZATION, &format!("Bearer {KEY}"))]);
    let unauthorized = |detail: &str| CallerRefusal::Unauthorized(detail.to_string());

    assert_eq!(resolve_caller(&HeaderMap::new(), Some(KEY), &store), Ok(CallerIdentity::Operator));
    assert_eq!(resolve_caller(&operator_headers, Some(KEY), &store), Ok(CallerIdentity::Operator));
    assert_eq!(
      resolve_caller(&token_headers, None, &store),
      Err(unauthorized("run token presented but no signing key is configured"))
    );
    assert_eq!(resolve_caller(&token_headers, Some(KEY), &store), Err(unauthorized(RUN_IDENTITY_NOT_LAUNCHED_DETAIL)));
    store.record_launch("session-a", "run-a", 424242, "12345").await.unwrap();
    assert_eq!(resolve_caller(&token_headers, Some(KEY), &store), Ok(identity));
    let guard = store.control_lock().await;
    store.record_finish_locked(&guard, "session-a", "run-a", RunOutcome::new(RunOutcomeKind::Success)).await.unwrap();
    drop(guard);
    assert_eq!(resolve_caller(&token_headers, Some(KEY), &store), Err(unauthorized(RUN_IDENTITY_UNKNOWN_DETAIL)));
  }
}
