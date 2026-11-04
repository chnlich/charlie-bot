//! The two draft routes: the internal one the CLI registers a draft through, and the operator-only one that sends it.
//!
//! Registration validates the platform target and appends `draft_registered`. A send is admitted only to the operator,
//! derives sendability from the log under the draft's lock, appends `draft_sent` or `draft_failed`, and wakes the
//! session with the sent fact. Nothing is retried.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_cli_common::draft::REGISTER_PATH;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, text_field};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::event_types;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::draft::{DraftContext, DraftPlatform, DraftSendFailure, DraftSent};
use charliebot_store::control_events::sha256_hex;
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Value, json};

use crate::lock;
use crate::log::{self, Sendability};

/// The send route's path pattern.
pub const SEND_PATH: &str = "/api/sessions/{session_id}/drafts/{draft_id}/send";

/// The draft routes with the feature context as their state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route(REGISTER_PATH, post_only(register))
    .route(SEND_PATH, post_only(send))
    .with_state(Arc::clone(features))
}

/// Whether a path is one of the draft routes, whatever the method.
pub fn matches(path: &str) -> bool {
  path == REGISTER_PATH || send_path_claims(path)
}

fn send_path_claims(path: &str) -> bool {
  let Some(rest) = path.strip_prefix("/api/sessions/") else { return false };
  let Some((session_id, rest)) = rest.split_once("/drafts/") else { return false };
  !session_id.is_empty() && rest.strip_suffix("/send").is_some_and(|draft_id| !draft_id.is_empty())
}

const REGISTER_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("platform", FieldType::String),
  Field::required("target", FieldType::Dictionary),
  Field::required("body", FieldType::String),
  Field::optional("replaces", FieldType::NullableString),
];
const REGISTER_BODY: BodyModel = BodyModel::required(REGISTER_FIELDS).forbid_extra();

async fn register(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let caller = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  let fields = match body.declared_fields(&REGISTER_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, "session_id");
  let platform_name = text_field(&fields, "platform");
  let text = text_field(&fields, "body");
  let replaces = match fields.get("replaces") {
    Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
    _ => None,
  };
  let requested_target = fields.get("target").cloned().expect("a validated target is an object");
  match features.store.get_session(&session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Session not found"})),
    Err(error) => return internal(error.to_string()),
  }
  let platform = match platform(&features, &platform_name) {
    Ok(platform) => platform,
    Err(response) => return *response,
  };
  let target = match platform.validate_target(&requested_target) {
    Ok(target) => target,
    Err(detail) => return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": detail})),
  };
  if text.trim().is_empty() {
    return responses::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({"detail": "draft body is empty"}));
  }
  let target_label = platform.target_label(&target);
  let sha256 = sha256_hex(&text);
  let created_at = UtcDateTime::now().to_isoformat_text();
  let draft_id = uuid::Uuid::new_v4().to_string();
  let event = json!({
    "type": event_types::DRAFT_REGISTERED,
    "id": draft_id.clone(),
    "actor": actor(&caller),
    log::DRAFT_ID: draft_id,
    log::PLATFORM: platform_name,
    log::TARGET: target,
    log::TARGET_LABEL: target_label,
    log::BODY: text,
    log::REPLACES: replaces,
    log::SHA256: sha256,
    log::CREATED_AT: created_at,
  });
  if let Err(error) = features.store.persist_and_broadcast(&session_id, event).await {
    return internal(error.to_string());
  }
  responses::json(
    StatusCode::OK,
    &json!({"draft_id": draft_id, "card": format!("/?session={session_id}#draft-{draft_id}")}),
  )
}

async fn send(
  State(features): State<Arc<FeatureContext>>,
  Path((session_id, draft_id)): Path<(String, String)>,
  request: Request<Body>,
) -> Response {
  let (parts, _stream) = request.into_parts();
  match features.store.get_session(&session_id).await {
    Ok(Some(_)) => {}
    Ok(None) => return responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Session not found"})),
    Err(error) => return internal(error.to_string()),
  }
  // Only the operator's own click reaches this route. An agent run credential gets 403 before anything is read.
  let caller = match caller::resolve_caller(&parts.headers, &features) {
    Ok(caller) => caller,
    Err(response) => return response,
  };
  if !caller.is_operator() {
    return responses::json(StatusCode::FORBIDDEN, &json!({"detail": "sending a draft requires operator credentials"}));
  }
  let _guard = lock::acquire(&session_id, &draft_id).await;
  let events = match features.store.load_all_facts_view_sync(&session_id) {
    Ok(events) => events,
    Err(error) => return internal(error.to_string()),
  };
  let draft = match log::sendability(events.iter(), &draft_id) {
    Sendability::Sendable(draft) => draft,
    Sendability::Missing => {
      return responses::json(
        StatusCode::NOT_FOUND,
        &json!({"detail": format!("draft {draft_id} is not registered in session {session_id}")}),
      );
    }
    Sendability::AlreadySent => {
      return responses::json(StatusCode::CONFLICT, &json!({"detail": format!("draft {draft_id} was already sent")}));
    }
    Sendability::Replaced => {
      return responses::json(
        StatusCode::CONFLICT,
        &json!({"detail": format!("draft {draft_id} was replaced by a later draft")}),
      );
    }
  };
  let platform = match platform(&features, &draft.platform) {
    Ok(platform) => platform,
    Err(response) => return *response,
  };
  match platform.send(&draft.target, &draft.body).await {
    Ok(sent) => match record_sent(&features, &session_id, &draft, &sent).await {
      Ok(()) => responses::json(StatusCode::OK, &json!({"sender": sent.sender, "url": sent.url})),
      Err(response) => *response,
    },
    Err(failure) => {
      if let Err(error) = record_failed(&features, &session_id, &draft_id, &failure).await {
        return internal(error.to_string());
      }
      responses::json(StatusCode::BAD_GATEWAY, &json!({"detail": failure.detail, "outcome": failure.outcome.as_str()}))
    }
  }
}

/// Append `draft_sent`, then wake the session with it. The wake is best effort: the send is already a fact.
async fn record_sent(
  features: &FeatureContext,
  session_id: &str,
  draft: &log::RegisteredDraft,
  sent: &DraftSent,
) -> Result<(), Box<Response>> {
  let event = json!({
    "type": event_types::DRAFT_SENT,
    "content": format!("Draft {} sent by {}: {}", draft.draft_id, sent.sender, sent.url),
    "actor": "system",
    log::DRAFT_ID: draft.draft_id,
    log::SHA256: draft.sha256,
    log::SENDER: sent.sender,
    log::URL: sent.url,
    log::SENT_AT: UtcDateTime::now().to_isoformat_text(),
  });
  features
    .store
    .persist_and_broadcast(session_id, event)
    .await
    .map_err(|error| Box::new(internal(error.to_string())))?;
  match charliebot_tasks::dispatch::dispatcher(features) {
    Ok(dispatch) => {
      if let Err(error) = dispatch.dispatch_pending(session_id).await {
        tracing::error!(session_id, draft_id = %draft.draft_id, %error, "draft_sent_wake_failed");
      }
    }
    Err(error) => tracing::error!(session_id, draft_id = %draft.draft_id, %error, "draft_sent_wake_unavailable"),
  }
  Ok(())
}

async fn record_failed(
  features: &FeatureContext,
  session_id: &str,
  draft_id: &str,
  failure: &DraftSendFailure,
) -> charliebot_store::StoreResult<()> {
  let event = json!({
    "type": event_types::DRAFT_FAILED,
    "actor": "system",
    log::DRAFT_ID: draft_id,
    log::ERROR: failure.detail,
    log::OUTCOME: failure.outcome.as_str(),
  });
  features.store.persist_and_broadcast(session_id, event).await?;
  Ok(())
}

fn platform(features: &FeatureContext, name: &str) -> Result<Arc<dyn DraftPlatform>, Box<Response>> {
  let Some(factory) = features.registry.draft_platform_factory(name) else {
    return Err(Box::new(responses::json(
      StatusCode::UNPROCESSABLE_ENTITY,
      &json!({"detail": format!("no draft platform is registered as {name}")}),
    )));
  };
  let context = DraftContext::new(Arc::clone(&features.config), Arc::clone(&features.credentials));
  factory(&context).map_err(|error| Box::new(internal(format!("building the {name} draft platform: {error:#}"))))
}

fn actor(caller: &CallerIdentity) -> &'static str {
  if caller.is_operator() { "user" } else { "agent" }
}

fn internal(detail: String) -> Response {
  responses::internal_error("RuntimeError", detail)
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use axum::body::{Body, to_bytes};
  use axum::http::{Request as HttpRequest, StatusCode, header};
  use charliebot_core::run_token::{self, RunTokenClaims};
  use charliebot_store::run_records::{RunKind, RunRecord};
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::{self, ACCESS_KEY, Reply, SyntheticPlatform};

  fn service(features: &Arc<FeatureContext>) -> axum::Router {
    routes(features)
  }

  fn register_request(session_id: &str, target: Value, body: &str, replaces: Option<&str>) -> HttpRequest<Body> {
    let payload = json!({
      "session_id": session_id,
      "platform": "synthetic",
      "target": target,
      "body": body,
      "replaces": replaces,
    });
    HttpRequest::builder()
      .method("POST")
      .uri(REGISTER_PATH)
      .header(header::CONTENT_TYPE, "application/json")
      .body(Body::from(payload.to_string()))
      .unwrap()
  }

  fn send_request(session_id: &str, draft_id: &str, authorization: Option<&str>) -> HttpRequest<Body> {
    let mut builder =
      HttpRequest::builder().method("POST").uri(format!("/api/sessions/{session_id}/drafts/{draft_id}/send"));
    if let Some(authorization) = authorization {
      builder = builder.header(header::AUTHORIZATION, authorization);
    }
    builder.body(Body::empty()).unwrap()
  }

  async fn answer(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
  }

  fn events(features: &FeatureContext, session_id: &str) -> Vec<Value> {
    features.store.load_chat_events_sync(session_id).unwrap().iter().map(|event| event.to_value()).collect()
  }

  #[test]
  fn the_routes_claim_the_register_path_and_each_send_path() {
    assert!(matches(REGISTER_PATH));
    assert!(matches("/api/sessions/s/drafts/d/send"));
    assert!(!matches("/api/sessions/s/drafts/d"));
    assert!(!matches("/api/sessions//drafts/d/send"));
    assert!(!matches("/api/sessions/s/drafts//send"));
    assert!(!matches("/api/sessions/s/drafts/d/send/extra"));
  }

  #[tokio::test]
  async fn registration_stores_the_draft_and_returns_its_card_link() {
    let (_home, features) = test_support::home(SyntheticPlatform::scripted(Vec::new()));
    let session = test_support::create_session(&features, "draft-register").await;
    let body = "first line\nsecond line\n";

    let (status, value) = answer(
      service(&features).oneshot(register_request(&session, json!({"channel": "C1"}), body, None)).await.unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let draft_id = value["draft_id"].as_str().unwrap();
    assert_eq!(value["card"], format!("/?session={session}#draft-{draft_id}"));
    let stored = events(&features, &session);
    let registered = stored.iter().find(|event| event["type"] == event_types::DRAFT_REGISTERED).unwrap();
    assert_eq!(registered["draft_id"], draft_id);
    assert_eq!(registered["platform"], "synthetic");
    assert_eq!(registered["target"], json!({"channel": "C1"}));
    assert_eq!(registered["target_label"], "Channel C1");
    assert_eq!(registered["body"], body, "the registered body is stored byte for byte");
    assert_eq!(registered["sha256"], sha256_hex(body));
    assert_eq!(registered["replaces"], Value::Null);
    assert_eq!(registered["actor"], "user");
  }

  #[tokio::test]
  async fn an_invalid_target_registers_nothing() {
    let (_home, features) = test_support::home(SyntheticPlatform::scripted(Vec::new()));
    let session = test_support::create_session(&features, "draft-bad-target").await;

    let (status, value) = answer(
      service(&features).oneshot(register_request(&session, json!({"channel": ""}), "body", None)).await.unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["detail"], "target.channel must be a non-empty string");
    assert!(events(&features, &session).iter().all(|event| event["type"] != event_types::DRAFT_REGISTERED));
  }

  #[tokio::test]
  async fn an_empty_body_registers_nothing() {
    let (_home, features) = test_support::home(SyntheticPlatform::scripted(Vec::new()));
    let session = test_support::create_session(&features, "draft-empty-body").await;

    let (status, value) = answer(
      service(&features).oneshot(register_request(&session, json!({"channel": "C1"}), "  \n", None)).await.unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["detail"], "draft body is empty");
    assert!(events(&features, &session).iter().all(|event| event["type"] != event_types::DRAFT_REGISTERED));
  }

  async fn registered_draft(features: &Arc<FeatureContext>, session: &str, body: &str) -> String {
    let (status, value) =
      answer(service(features).oneshot(register_request(session, json!({"channel": "C1"}), body, None)).await.unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    value["draft_id"].as_str().unwrap().to_string()
  }

  #[tokio::test]
  async fn the_operator_send_posts_the_registered_body_and_records_the_platform_result() {
    let platform = SyntheticPlatform::scripted(vec![Reply::sent()]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-send").await;
    let body = "line one\nline two\n";
    let draft_id = registered_draft(&features, &session, body).await;

    let (status, value) =
      answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(value, json!({"sender": "U0SYNTHETIC", "url": "https://chat.example/archives/C1/p1"}));
    let calls = platform.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body, body, "the platform receives the registered body byte for byte");
    let sent = events(&features, &session)
      .into_iter()
      .find(|event| event["type"] == event_types::DRAFT_SENT)
      .expect("the send records draft_sent");
    assert_eq!(sent["draft_id"], draft_id);
    assert_eq!(sent["sha256"], sha256_hex(body));
    assert_eq!(sent["sender"], "U0SYNTHETIC");
    assert_eq!(sent["url"], "https://chat.example/archives/C1/p1");
  }

  #[tokio::test]
  async fn the_sent_event_wakes_the_registered_session() {
    let platform = SyntheticPlatform::scripted(vec![Reply::sent()]);
    let (_home, features) = test_support::home(platform);
    let session = test_support::create_session(&features, "draft-wake").await;
    let recorder = test_support::install_wake_recorder(&features);
    let draft_id = registered_draft(&features, &session, "the body").await;

    let (status, _) = answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await;

    assert_eq!(status, StatusCode::OK);
    let launches = recorder.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].session_id, session);
    assert_eq!(launches[0].input_types, [event_types::DRAFT_SENT.to_string()]);
  }

  async fn launch_run(features: &Arc<FeatureContext>, session: &str, run: &str) {
    let record = RunRecord {
      id: run.to_string(),
      session_id: session.to_string(),
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
    };
    let guard = features.store.control_lock().await;
    features.store.register_run_locked(&guard, session, record, Some("synthetic task")).await.unwrap();
    drop(guard);
    features.store.record_launch(session, run, 424242, "12345").await.unwrap();
  }

  #[tokio::test]
  async fn an_agent_run_credential_gets_403_and_the_platform_receives_nothing() {
    let platform = SyntheticPlatform::scripted(vec![Reply::sent()]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-agent-caller").await;
    let draft_id = registered_draft(&features, &session, "the body").await;
    launch_run(&features, &session, "run-agent").await;
    let claims = RunTokenClaims { session_id: session.clone(), run_id: "run-agent".into(), agent: "worker".into() };
    let token = run_token::sign_run_token(&claims, ACCESS_KEY).unwrap();

    let (status, value) = answer(
      service(&features).oneshot(send_request(&session, &draft_id, Some(&format!("Bearer {token}")))).await.unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(value["detail"], "sending a draft requires operator credentials");
    assert!(platform.calls().is_empty(), "an agent caller sends nothing");
    assert!(events(&features, &session).iter().all(|event| event["type"] != event_types::DRAFT_SENT));
  }

  #[tokio::test]
  async fn a_sent_draft_and_a_replaced_draft_each_get_409() {
    let platform = SyntheticPlatform::scripted(vec![Reply::sent()]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-conflicts").await;
    let sent_id = registered_draft(&features, &session, "the body").await;
    let (status, _) = answer(service(&features).oneshot(send_request(&session, &sent_id, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, value) =
      answer(service(&features).oneshot(send_request(&session, &sent_id, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["detail"], format!("draft {sent_id} was already sent"));
    assert_eq!(platform.calls().len(), 1);

    let replaced_id = registered_draft(&features, &session, "the replacement").await;
    let request = register_request(&session, json!({"channel": "C1"}), "a newer replacement", Some(&replaced_id));
    let (status, _) = answer(service(&features).oneshot(request).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events(&features, &session).iter().any(|event| event["replaces"] == replaced_id));

    let (status, value) =
      answer(service(&features).oneshot(send_request(&session, &replaced_id, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["detail"], format!("draft {replaced_id} was replaced by a later draft"));
    assert_eq!(platform.calls().len(), 1);
  }

  #[tokio::test]
  async fn two_concurrent_sends_make_one_platform_request_and_one_409() {
    let platform = SyntheticPlatform::scripted(vec![Reply::delayed(150)]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-concurrent").await;
    let draft_id = registered_draft(&features, &session, "the body").await;

    let first = {
      let features = Arc::clone(&features);
      let session = session.clone();
      let draft_id = draft_id.clone();
      tokio::spawn(async move {
        answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await
      })
    };
    let second = {
      let features = Arc::clone(&features);
      let session = session.clone();
      let draft_id = draft_id.clone();
      tokio::spawn(async move {
        answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await
      })
    };
    let statuses: Vec<StatusCode> = vec![first.await.unwrap().0, second.await.unwrap().0];

    assert_eq!(statuses.iter().filter(|status| **status == StatusCode::OK).count(), 1, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|status| **status == StatusCode::CONFLICT).count(), 1, "{statuses:?}");
    assert_eq!(platform.calls().len(), 1);
  }

  #[tokio::test]
  async fn a_platform_error_records_rejected_and_is_not_retried() {
    let platform = SyntheticPlatform::scripted(vec![Reply::rejected("missing_scope")]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-rejected").await;
    let draft_id = registered_draft(&features, &session, "the body").await;

    let (status, value) =
      answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(value, json!({"detail": "missing_scope", "outcome": "rejected"}));
    assert_eq!(platform.calls().len(), 1, "one request, no retry");
    let failed = events(&features, &session)
      .into_iter()
      .find(|event| event["type"] == event_types::DRAFT_FAILED)
      .expect("the failure is recorded");
    assert_eq!(failed["draft_id"], draft_id);
    assert_eq!(failed["error"], "missing_scope");
    assert_eq!(failed["outcome"], "rejected");
    assert!(events(&features, &session).iter().all(|event| event["type"] != event_types::DRAFT_SENT));
  }

  #[tokio::test]
  async fn a_timeout_records_unknown_and_is_not_retried() {
    let platform = SyntheticPlatform::scripted(vec![Reply::unknown("request timed out")]);
    let (_home, features) = test_support::home(Arc::clone(&platform));
    let session = test_support::create_session(&features, "draft-unknown").await;
    let draft_id = registered_draft(&features, &session, "the body").await;

    let (status, value) =
      answer(service(&features).oneshot(send_request(&session, &draft_id, None)).await.unwrap()).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(value["outcome"], "unknown");
    assert_eq!(platform.calls().len(), 1, "one request, no retry");
    let failed = events(&features, &session)
      .into_iter()
      .find(|event| event["type"] == event_types::DRAFT_FAILED)
      .expect("the failure is recorded");
    assert_eq!(failed["outcome"], "unknown");
  }

  #[tokio::test]
  async fn an_unknown_draft_and_an_unknown_session_are_404() {
    let platform = SyntheticPlatform::scripted(Vec::new());
    let (_home, features) = test_support::home(platform);
    let session = test_support::create_session(&features, "draft-404").await;

    let (status, _) = answer(service(&features).oneshot(send_request(&session, "no-such", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = answer(service(&features).oneshot(send_request("no-such", "no-such", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
  }
}
