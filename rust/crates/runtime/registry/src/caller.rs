//! The verified operator or agent identity that structural routes resolve.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::Response;
use charliebot_compat::headers::header_text;
use charliebot_compat::responses;
use charliebot_core::run_token::{self, CALLER_SESSION_HEADER, RunTokenClaims};
use charliebot_store::{Store, control_events};

use crate::FeatureContext;

/// The caller of a structural route: an operator or an agent bound to a verified run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerIdentity {
  Operator { session_id: Option<String> },
  Agent { claims: RunTokenClaims },
}

impl CallerIdentity {
  /// Whether this caller is the operator.
  pub fn is_operator(&self) -> bool {
    matches!(self, Self::Operator { .. })
  }

  /// The operator's caller-session header or the session bound by an agent's token.
  pub fn session_id(&self) -> Option<&str> {
    match self {
      Self::Operator { session_id } => session_id.as_deref(),
      Self::Agent { claims } => Some(&claims.session_id),
    }
  }

  /// The verified run claims for an agent caller.
  pub fn claims(&self) -> Option<&RunTokenClaims> {
    match self {
      Self::Operator { .. } => None,
      Self::Agent { claims } => Some(claims),
    }
  }

  /// The context of one reply this caller sends: the request id a verified agent's send derives from `verb`,
  /// `target` and `text`, and the start time of the run it answers.
  ///
  /// A replay of the same send carries the same id, so the reply it already posted is found again instead of
  /// posted twice. The operator caller carries neither value, and the reply path picks its own id and creation
  /// time.
  #[allow(clippy::result_large_err)]
  pub fn reply_context(
    &self,
    store: &Store,
    verb: &str,
    target: &str,
    text: &str,
  ) -> Result<(Option<String>, Option<String>), Response> {
    let Some(claims) = self.claims() else {
      return Ok((None, None));
    };
    let run = store
      .read_run_sync(&claims.session_id, &claims.run_id)
      .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?
      .ok_or_else(|| responses::internal_error("RuntimeError", "verified caller run disappeared".to_string()))?;
    let started_at = run.started_at.expect("a verified run has a start timestamp").to_isoformat_text();
    let request_id = control_events::derived_agent_send_request_id(&claims.run_id, verb, target, text);
    Ok((Some(request_id), Some(started_at)))
  }
}

/// The request caller resolved from the live credentials and run identity in the feature context.
#[allow(clippy::result_large_err)]
pub fn resolve_caller(headers: &HeaderMap, features: &FeatureContext) -> Result<CallerIdentity, Response> {
  let key = features
    .credentials
    .get()
    .map_err(|error| responses::internal_error("ValueError", error.to_string()))?
    .access_key();
  let authorization = header_text(headers, &header::AUTHORIZATION);
  let bearer = run_token::bearer_from_authorization(Some(&authorization));
  if bearer.is_empty() || key.as_deref() == Some(bearer) {
    return Ok(operator_identity(headers));
  }

  // Anything else is run-token use. Hand the verifier the configured key (empty when none is set) and fail closed
  // through its own refusal. The missing-key sentence then has one owner and one 401 detail.
  let key = key.unwrap_or_default();
  let claims =
    run_token::verify_run_token(bearer, &key).map_err(|error| unauthorized(&format!("invalid run token: {error}")))?;
  let refusal = features
    .store
    .run_identity_refusal(&claims.session_id, &claims.run_id)
    .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?;
  if let Some(detail) = refusal {
    return Err(unauthorized(detail));
  }
  Ok(CallerIdentity::Agent { claims })
}

impl FromRequestParts<Arc<FeatureContext>> for CallerIdentity {
  type Rejection = Response;

  async fn from_request_parts(parts: &mut Parts, state: &Arc<FeatureContext>) -> Result<Self, Self::Rejection> {
    resolve_caller(&parts.headers, state)
  }
}

fn operator_identity(headers: &HeaderMap) -> CallerIdentity {
  let name = HeaderName::from_static(CALLER_SESSION_HEADER);
  let session_id = header_text(headers, &name);
  CallerIdentity::Operator { session_id: (!session_id.is_empty()).then_some(session_id) }
}

fn unauthorized(detail: &str) -> Response {
  responses::json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "detail": detail }))
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::to_bytes;
  use axum::http::{HeaderValue, StatusCode, header};
  use axum::response::Response;
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::run_token::{self, RunTokenClaims};
  use charliebot_store::run_identity::{RUN_IDENTITY_NOT_LAUNCHED_DETAIL, RUN_IDENTITY_UNKNOWN_DETAIL};
  use charliebot_store::{RunKind, RunOutcome, RunOutcomeKind, RunRecord};
  use serde_json::{Value, json};

  use super::*;
  use crate::Registry;
  use crate::context::features_over;

  const KEY: &str = "synthetic-access-key";

  fn feature_context(key: Option<&str>) -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    if let Some(key) = key {
      std::fs::write(home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {key}\n")).unwrap();
    }
    let features = features_over(home.path(), Arc::new(Registry::new()));
    (home, features)
  }

  fn request_headers(authorization: Option<&str>, caller_session: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(authorization) = authorization {
      headers.insert(header::AUTHORIZATION, HeaderValue::from_str(authorization).unwrap());
    }
    if let Some(caller_session) = caller_session {
      headers.insert(CALLER_SESSION_HEADER, HeaderValue::from_str(caller_session).unwrap());
    }
    headers
  }

  fn bearer_headers(token: &str, caller_session: Option<&str>) -> HeaderMap {
    request_headers(Some(&format!("Bearer {token}")), caller_session)
  }

  fn claims() -> RunTokenClaims {
    RunTokenClaims { session_id: "session-a".into(), run_id: "run-a".into(), agent: "worker".into() }
  }

  fn run_record(claims: &RunTokenClaims) -> RunRecord {
    RunRecord {
      id: claims.run_id.clone(),
      session_id: claims.session_id.clone(),
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
    }
  }

  async fn register_run(features: &FeatureContext, claims: &RunTokenClaims, launched: bool) {
    let guard = features.store.control_lock().await;
    features
      .store
      .register_run_locked(&guard, &claims.session_id, run_record(claims), Some("synthetic task"))
      .await
      .unwrap();
    drop(guard);
    if launched {
      features.store.record_launch(&claims.session_id, &claims.run_id, 424242, "12345").await.unwrap();
    }
  }

  async fn finish_run(features: &FeatureContext, claims: &RunTokenClaims) {
    let guard = features.store.control_lock().await;
    features
      .store
      .record_finish_locked(&guard, &claims.session_id, &claims.run_id, RunOutcome::new(RunOutcomeKind::Success))
      .await
      .unwrap();
  }

  async fn response_json(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
  }

  async fn assert_unauthorized(response: Response, detail: &str) {
    assert_eq!(response_json(response).await, (StatusCode::UNAUTHORIZED, json!({ "detail": detail })));
  }

  #[test]
  fn no_bearer_uses_the_optional_operator_session_header() {
    let (_home, features) = feature_context(Some(KEY));
    let absent = resolve_caller(&HeaderMap::new(), &features).unwrap();
    assert!(absent.is_operator());
    assert_eq!(absent.session_id(), None);
    assert_eq!(absent.claims(), None);

    let empty = resolve_caller(&request_headers(Some(""), Some("")), &features).unwrap();
    assert!(empty.is_operator());
    assert_eq!(empty.session_id(), None);

    let with_session = resolve_caller(&request_headers(None, Some("operator-session")), &features).unwrap();
    assert_eq!(with_session, CallerIdentity::Operator { session_id: Some("operator-session".into()) });
  }

  #[test]
  fn the_access_key_uses_the_operator_session_header() {
    let (_home, features) = feature_context(Some(KEY));
    let caller = resolve_caller(&bearer_headers(KEY, Some("operator-session")), &features).unwrap();
    assert_eq!(caller, CallerIdentity::Operator { session_id: Some("operator-session".into()) });
  }

  #[test]
  fn the_access_key_is_read_from_the_live_credentials_on_each_call() {
    let (home, features) = feature_context(Some(KEY));
    assert!(resolve_caller(&bearer_headers(KEY, None), &features).unwrap().is_operator());

    let next_key = "updated-synthetic-access-key";
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {next_key}\n")).unwrap();
    assert!(resolve_caller(&bearer_headers(next_key, None), &features).unwrap().is_operator());
  }

  /// Every presented bearer reaches the verifier, whose own refusal becomes the 401 detail when no signing key is
  /// configured.
  #[tokio::test]
  async fn a_run_bearer_with_no_configured_key_is_refused_with_401() {
    let (_home, features) = feature_context(None);
    let token = run_token::sign_run_token(&claims(), KEY).unwrap();
    let response = resolve_caller(&bearer_headers(&token, None), &features).unwrap_err();
    assert_unauthorized(response, "invalid run token: run token presented but no signing key is configured").await;
  }

  #[tokio::test]
  async fn a_token_that_fails_verification_is_rejected() {
    let (_home, features) = feature_context(Some(KEY));
    let response = resolve_caller(&bearer_headers("malformed", None), &features).unwrap_err();
    assert_unauthorized(response, "invalid run token: malformed run token").await;
  }

  #[tokio::test]
  async fn a_token_for_an_unknown_run_is_rejected() {
    let (_home, features) = feature_context(Some(KEY));
    let token = run_token::sign_run_token(&claims(), KEY).unwrap();
    let response = resolve_caller(&bearer_headers(&token, None), &features).unwrap_err();
    assert_unauthorized(response, RUN_IDENTITY_UNKNOWN_DETAIL).await;
  }

  #[tokio::test]
  async fn a_token_for_a_run_that_has_not_launched_is_rejected() {
    let (_home, features) = feature_context(Some(KEY));
    let claims = claims();
    register_run(&features, &claims, false).await;
    let token = run_token::sign_run_token(&claims, KEY).unwrap();
    let response = resolve_caller(&bearer_headers(&token, None), &features).unwrap_err();
    assert_unauthorized(response, RUN_IDENTITY_NOT_LAUNCHED_DETAIL).await;
  }

  #[tokio::test]
  async fn a_token_for_a_finished_run_is_rejected() {
    let (_home, features) = feature_context(Some(KEY));
    let claims = claims();
    register_run(&features, &claims, true).await;
    finish_run(&features, &claims).await;
    let token = run_token::sign_run_token(&claims, KEY).unwrap();
    let response = resolve_caller(&bearer_headers(&token, None), &features).unwrap_err();
    assert_unauthorized(response, RUN_IDENTITY_UNKNOWN_DETAIL).await;
  }

  #[tokio::test]
  async fn a_valid_agent_uses_the_claim_session_and_ignores_the_header() {
    let (_home, features) = feature_context(Some(KEY));
    let claims = claims();
    register_run(&features, &claims, true).await;
    let token = run_token::sign_run_token(&claims, KEY).unwrap();
    let caller = resolve_caller(&bearer_headers(&token, Some("untrusted-session")), &features).unwrap();

    assert!(!caller.is_operator());
    assert_eq!(caller.session_id(), Some("session-a"));
    assert_eq!(caller.claims(), Some(&claims));
  }

  #[tokio::test]
  async fn an_agent_reply_context_repeats_for_a_replay_and_the_operator_has_none() {
    let (_home, features) = feature_context(Some(KEY));
    let claims = claims();
    register_run(&features, &claims, true).await;
    let agent = CallerIdentity::Agent { claims: claims.clone() };

    let first = agent.reply_context(&features.store, "slack reply", "session-b", "the answer").unwrap();
    let replay = agent.reply_context(&features.store, "slack reply", "session-b", "the answer").unwrap();
    assert_eq!(first, replay, "a replay of one send carries the same request id");
    let (request_id, started_at) = first;
    assert!(request_id.as_deref().is_some_and(|request_id| !request_id.is_empty()));
    let run = features.store.read_run_sync(&claims.session_id, &claims.run_id).unwrap().expect("the registered run");
    assert_eq!(
      started_at.as_deref(),
      Some(run.started_at.expect("the launched run has a start").to_isoformat_text().as_str()),
      "the reply is created after the run it answers"
    );

    let next_text = agent.reply_context(&features.store, "slack reply", "session-b", "another answer").unwrap();
    assert_ne!(next_text.0, request_id, "a different send derives a different request id");

    let operator = CallerIdentity::Operator { session_id: Some("operator-session".into()) };
    assert_eq!(
      operator.reply_context(&features.store, "slack reply", "session-b", "the answer").unwrap(),
      (None, None)
    );
  }
}
