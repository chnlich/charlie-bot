//! The plan routes (`src/features/artifacts/api.py`): `GET /api/sessions/{session_id}/plans`, the session's plan registry
//! with derived states and read errors, and the internal `plan/present`, `plan/amend`, `plan/approve` and `plan/close`
//! verbs that `charliebot plan` calls.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::{get_only, post_only};
use charliebot_registry::FeatureContext;
use serde_json::{Map, Value, json};

use crate::plan_verbs::{self, Amendment, SessionLocks, VerbError};
use crate::plans::{self, PLAN_CLOSE_ABANDONED, PLAN_CLOSE_COMPLETED, PLAN_CLOSE_SUPERSEDED, PLANS_FILE};

/// The state of the plan routes: the feature context, and the session locks that the verbs share.
#[derive(Clone)]
pub(crate) struct PlanRoutes {
  features: Arc<FeatureContext>,
  locks: Arc<SessionLocks>,
}

/// The route table, built with the shared feature context.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  let state = PlanRoutes { features: Arc::clone(features), locks: Arc::default() };
  Router::new()
    .route("/api/sessions/{session_id}/plans", get_only(list_plans))
    .route("/api/internal/plan/present", post_only(plan_present))
    .route("/api/internal/plan/amend", post_only(plan_amend))
    .route("/api/internal/plan/approve", post_only(plan_approve))
    .route("/api/internal/plan/close", post_only(plan_close))
    .with_state(state)
}

/// Whether a path is one of the plan routes, for the server's trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  if matches!(
    path,
    "/api/internal/plan/present"
      | "/api/internal/plan/amend"
      | "/api/internal/plan/approve"
      | "/api/internal/plan/close"
  ) {
    return true;
  }
  let Some(rest) = path.strip_prefix("/api/sessions/") else {
    return false;
  };
  matches!(rest.split_once('/'), Some((session_id, "plans")) if !session_id.is_empty())
}

/// The plan registry of a session with derived states and read errors. A missing session answers 404; a corrupt
/// registry answers 200 with its error entries.
async fn list_plans(State(state): State<PlanRoutes>, AxumPath(session_id): AxumPath<String>) -> Response {
  if let Some(response) = require_session(&state.features, &session_id).await {
    return response;
  }
  let plans_path = state.features.store.session_dir(&session_id).join(PLANS_FILE);
  let read_session = session_id.clone();
  let read = match tokio::task::spawn_blocking(move || plans::read_plans_tolerant(&plans_path, &read_session)).await {
    Ok(read) => read,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  match read {
    Ok(read) => responses::fast_json(StatusCode::OK, &read.into_json()),
    Err(error) => responses::internal_error(error.kind, error.detail),
  }
}

/// `POST /api/internal/plan/present`: the body, then the session check, then the registration.
async fn plan_present(State(state): State<PlanRoutes>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match PresentRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(response) = require_session(&state.features, &model.session_id).await {
    return response;
  }
  answer(
    plan_verbs::present(&state.features, &state.locks, &model.session_id, &model.file, model.title, model.base).await,
  )
}

/// `POST /api/internal/plan/amend`: the body, then the session check, then the amendment.
async fn plan_amend(State(state): State<PlanRoutes>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match AmendRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(response) = require_session(&state.features, &model.session_id).await {
    return response;
  }
  answer(plan_verbs::amend(&state.features, &state.locks, &model.session_id, &model.file, model.amendment).await)
}

/// `POST /api/internal/plan/approve`: the body, then the session check, then the approval.
async fn plan_approve(State(state): State<PlanRoutes>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match ApproveRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(response) = require_session(&state.features, &model.session_id).await {
    return response;
  }
  answer(plan_verbs::approve(&state.features, &state.locks, &model.session_id, model.plan_id).await)
}

/// `POST /api/internal/plan/close`: the body, then the session check, then the close.
async fn plan_close(State(state): State<PlanRoutes>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match CloseRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  if let Some(response) = require_session(&state.features, &model.session_id).await {
    return response;
  }
  answer(plan_verbs::close(&state.features, &state.locks, &model.session_id, model.plan_id, model.close_as).await)
}

/// The fields of `PlanPresentRequest`, in the model's order. The base fields default to `None`.
const PRESENT_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("file", FieldType::String),
  Field::required("title", FieldType::String),
  Field::optional("base_repo", FieldType::NullableString),
  Field::optional("base_branch", FieldType::NullableString),
  Field::optional("base_sha", FieldType::NullableString),
];
const PRESENT_BODY: BodyModel = BodyModel::required(PRESENT_FIELDS).forbid_extra();

/// The triggers of `PlanAmendTrigger`, and the fields of `PlanAmendRequest` in the model's order. `trigger` defaults to
/// `feedback`, `note` is required, and the base fields default to `None`.
const AMEND_TRIGGERS: &[&str] = &["auto_amend", "feedback"];
const AMEND_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("file", FieldType::String),
  Field::optional("plan_id", FieldType::NullableInteger),
  Field::optional("trigger", FieldType::Literal(AMEND_TRIGGERS)),
  Field::required("note", FieldType::String),
  Field::optional("base_repo", FieldType::NullableString),
  Field::optional("base_branch", FieldType::NullableString),
  Field::optional("base_sha", FieldType::NullableString),
];
const AMEND_BODY: BodyModel = BodyModel::required(AMEND_FIELDS).forbid_extra();

/// The validated `PlanPresentRequest`. `base` is `null` when the request names no base field.
struct PresentRequest {
  session_id: String,
  file: String,
  title: String,
  base: Value,
}

impl PresentRequest {
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let Some(Value::Object(mut fields)) = body.validate(&PRESENT_BODY)? else {
      unreachable!("a required model validates a body to its fields")
    };
    Ok(Self {
      session_id: take_string(&mut fields, "session_id"),
      file: take_string(&mut fields, "file"),
      title: take_string(&mut fields, "title"),
      base: build_base(&mut fields),
    })
  }
}

/// The validated `PlanAmendRequest`. `trigger` is `feedback` when the body omits it.
struct AmendRequest {
  session_id: String,
  file: String,
  amendment: Amendment,
}

impl AmendRequest {
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let Some(Value::Object(mut fields)) = body.validate(&AMEND_BODY)? else {
      unreachable!("a required model validates a body to its fields")
    };
    let session_id = take_string(&mut fields, "session_id");
    let file = take_string(&mut fields, "file");
    let plan_id = match fields.remove("plan_id") {
      None | Some(Value::Null) => None,
      Some(plan_id) => Some(plan_id),
    };
    let trigger = match fields.remove("trigger") {
      None => "feedback".to_string(),
      Some(Value::String(trigger)) => trigger,
      Some(_) => unreachable!("a validated trigger is one of the triggers"),
    };
    let amendment =
      Amendment { plan_id, trigger, note: take_string(&mut fields, "note"), base: build_base(&mut fields) };
    Ok(Self { session_id, file, amendment })
  }
}

/// `_build_base(req)`: the base a version records, `null` when the request names none of the three base fields.
fn build_base(fields: &mut Map<String, Value>) -> Value {
  let repo = fields.remove("base_repo").unwrap_or(Value::Null);
  let branch = fields.remove("base_branch").unwrap_or(Value::Null);
  let sha = fields.remove("base_sha").unwrap_or(Value::Null);
  if repo.is_null() && branch.is_null() && sha.is_null() {
    Value::Null
  } else {
    json!({"repo": repo, "branch": branch, "sha": sha})
  }
}

/// The fields of `PlanApproveRequest`, in the model's order. `plan_id` defaults to `None`.
const APPROVE_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::optional("plan_id", FieldType::NullableInteger)];
const APPROVE_BODY: BodyModel = BodyModel::required(APPROVE_FIELDS).forbid_extra();

/// The close modes of `PlanCloseMode`, and the fields of `PlanCloseRequest` in the model's order.
const CLOSE_MODES: &[&str] = &[PLAN_CLOSE_SUPERSEDED, PLAN_CLOSE_ABANDONED, PLAN_CLOSE_COMPLETED];
const CLOSE_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("plan_id", FieldType::Integer),
  Field::required("close_as", FieldType::Literal(CLOSE_MODES)),
];
const CLOSE_BODY: BodyModel = BodyModel::required(CLOSE_FIELDS).forbid_extra();

/// The validated `PlanApproveRequest`. `plan_id` is `None` when the body omits it or sends null.
struct ApproveRequest {
  session_id: String,
  plan_id: Option<Value>,
}

impl ApproveRequest {
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let Some(Value::Object(mut fields)) = body.validate(&APPROVE_BODY)? else {
      unreachable!("a required model validates a body to its fields")
    };
    let plan_id = match fields.remove("plan_id") {
      None | Some(Value::Null) => None,
      Some(plan_id) => Some(plan_id),
    };
    Ok(Self { session_id: take_string(&mut fields, "session_id"), plan_id })
  }
}

/// The validated `PlanCloseRequest`. `plan_id` is the request's integer, kept as sent.
struct CloseRequest {
  session_id: String,
  plan_id: Value,
  close_as: String,
}

impl CloseRequest {
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let Some(Value::Object(mut fields)) = body.validate(&CLOSE_BODY)? else {
      unreachable!("a required model validates a body to its fields")
    };
    let session_id = take_string(&mut fields, "session_id");
    let plan_id = fields.remove("plan_id").unwrap_or_else(|| unreachable!("a validated plan_id is present"));
    Ok(Self { session_id, plan_id, close_as: take_string(&mut fields, "close_as") })
  }
}

/// A string field of a validated model.
fn take_string(fields: &mut Map<String, Value>, name: &str) -> String {
  match fields.remove(name) {
    Some(Value::String(text)) => text,
    _ => unreachable!("a validated {name} is a string"),
  }
}

/// A verb's answer: its body with 200, a refusal as 400 with the message as `detail`, or the unhandled-exception 500.
fn answer(result: Result<Value, VerbError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(VerbError::Refused(detail)) => responses::json(StatusCode::BAD_REQUEST, &json!({"detail": detail})),
    Err(VerbError::Failed { kind, detail }) => responses::internal_error(kind, detail),
  }
}

/// The `require_session` dependency: the response that refuses the request when the session is missing, else `None`.
async fn require_session(features: &FeatureContext, session_id: &str) -> Option<Response> {
  match features.store.require_session(session_id).await {
    Ok(_) => None,
    Err(error) if error.status_code() == Some(404) => {
      Some(responses::json(StatusCode::NOT_FOUND, &json!({"detail": error.to_string()})))
    }
    Err(error) => Some(responses::internal_error("RuntimeError", error.to_string())),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, header};
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::{
    AWAITING_REGISTRY, features, plan_page, plan_session, session_with_registry, write_plan_artifact, write_session,
  };

  /// The status and the raw body of one GET to the plans route.
  async fn get(features: &Arc<FeatureContext>, uri: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder().uri(uri).body(Body::empty()).expect("the request builds");
    let response = routes(features).oneshot(request).await.expect("the router is infallible");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, body.to_vec())
  }

  /// The status and the JSON body of one POST with a JSON body.
  async fn post(features: &Arc<FeatureContext>, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
      .method(Method::POST)
      .uri(uri)
      .header(header::CONTENT_TYPE, "application/json")
      .body(Body::from(body.to_string()))
      .expect("the request builds");
    let response = routes(features).oneshot(request).await.expect("the router is infallible");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, serde_json::from_slice(&bytes).expect("the body is JSON"))
  }

  #[tokio::test]
  async fn an_unknown_session_answers_404_with_the_session_detail() {
    let (_home, features) = features();
    let (status, body) = get(&features, "/api/sessions/no-such/plans").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
      serde_json::from_slice::<Value>(&body).expect("the body is JSON"),
      json!({"detail": "Session not found"})
    );
  }

  #[tokio::test]
  async fn a_corrupt_registry_answers_200_with_one_error_entry() {
    let (_home, features) = features();
    write_session(&features, "sess");
    fs::write(features.store.session_dir("sess").join(PLANS_FILE), "{not valid json").expect("the registry is written");
    let (status, body) = get(&features, "/api/sessions/sess/plans").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
      serde_json::from_slice::<Value>(&body).expect("the body is JSON"),
      json!({
        "plans": [],
        "errors": [{
          "session_id": "sess",
          "plan_id": null,
          "error": "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        }]
      })
    );
  }

  #[tokio::test]
  async fn a_session_without_a_registry_answers_no_plans() {
    let (_home, features) = features();
    write_session(&features, "sess");
    assert_eq!(
      get(&features, "/api/sessions/sess/plans").await,
      (StatusCode::OK, br#"{"plans":[],"errors":[]}"#.to_vec())
    );
  }

  #[tokio::test]
  async fn the_listing_renders_the_derived_states_in_the_registry_order() {
    let (_home, features) = features();
    write_session(&features, "sess");
    let registry = r#"{"plans": [
      {"id": 1, "title": "Draft", "takeoff": null, "closed": null,
       "versions": [{"v": 1, "file": "artifacts/plan_01.html", "created_at": "2026-07-20T00:00:00+00:00",
                     "trigger": "initial", "base": null, "note": null}]},
      {"id": 2, "title": "Approved", "closed": null,
       "takeoff": {"v": 1, "at": "2026-07-20T01:00:00+00:00"},
       "versions": [{"v": 1, "file": "artifacts/plan_02.html", "created_at": "2026-07-20T00:00:00+00:00",
                     "trigger": "initial", "base": null, "note": null}]}
    ]}"#;
    fs::write(features.store.session_dir("sess").join(PLANS_FILE), registry).expect("the registry is written");
    let (status, body) = get(&features, "/api/sessions/sess/plans").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
      String::from_utf8(body).expect("the body is UTF-8"),
      r#"{"plans":[{"id":1,"title":"Draft","versions":[{"v":1,"file":"artifacts/plan_01.html","created_at":"2026-07-20T00:00:00+00:00","trigger":"initial","base":null,"note":null}],"takeoff":null,"closed":null,"state":"awaiting approval"},{"id":2,"title":"Approved","versions":[{"v":1,"file":"artifacts/plan_02.html","created_at":"2026-07-20T00:00:00+00:00","trigger":"initial","base":null,"note":null}],"takeoff":{"v":1,"at":"2026-07-20T01:00:00+00:00"},"closed":null,"state":"approved"}],"errors":[]}"#
    );
  }

  #[test]
  fn the_route_claims_the_plans_path_of_one_session() {
    assert!(matches("/api/sessions/sess/plans"));
    assert!(!matches("/api/sessions//plans"));
    assert!(!matches("/api/sessions/sess/plans/extra"));
    assert!(!matches("/api/sessions/sess"));
  }

  #[test]
  fn the_route_claims_the_present_amend_approve_and_close_paths() {
    assert!(matches("/api/internal/plan/present"));
    assert!(matches("/api/internal/plan/amend"));
    assert!(matches("/api/internal/plan/approve"));
    assert!(matches("/api/internal/plan/close"));
    assert!(!matches("/api/internal/plan/extra"));
  }

  #[tokio::test]
  async fn plan_present_endpoint_happy_path() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    let (status, body) = post(
      &features,
      "/api/internal/plan/present",
      json!({"session_id": "sess", "file": "artifacts/plan_01.html", "title": "P1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"plan": 1, "v": 1, "state": "awaiting approval"}));
  }

  #[tokio::test]
  async fn plan_amend_endpoint_happy_path() {
    let (_home, features) = plan_session("sess", None);
    write_plan_artifact(&features, "sess", "plan_01.html", &plan_page("Ship the fix."));
    plan_verbs::present(
      &features,
      &SessionLocks::default(),
      "sess",
      "artifacts/plan_01.html",
      "P1".to_string(),
      Value::Null,
    )
    .await
    .expect("the presentation succeeds");
    write_plan_artifact(&features, "sess", "plan_02.html", &plan_page("Ship the fix."));
    let (status, body) = post(
      &features,
      "/api/internal/plan/amend",
      json!({
        "session_id": "sess",
        "file": "artifacts/plan_02.html",
        "plan_id": 1,
        "note": "split the executor into its own deployment",
      }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"plan": 1, "v": 2, "state": "awaiting approval"}));
    let (status, listing) = get(&features, "/api/sessions/sess/plans").await;
    assert_eq!(status, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&listing).expect("the listing is JSON");
    let versions = &listing["plans"][0]["versions"];
    assert_eq!(versions[0]["note"], Value::Null);
    assert_eq!(versions[1]["note"], "split the executor into its own deployment");
  }

  #[tokio::test]
  async fn plan_amend_rejects_422_missing_note() {
    let (_home, features) = plan_session("sess", Some(AWAITING_REGISTRY));
    let (status, body) = post(
      &features,
      "/api/internal/plan/amend",
      json!({"session_id": "sess", "file": "artifacts/plan_02.html", "plan_id": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "missing");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "note"]));
  }

  #[tokio::test]
  async fn plan_amend_rejects_422_initial_trigger() {
    let (_home, features) = plan_session("sess", Some(AWAITING_REGISTRY));
    let (status, body) = post(
      &features,
      "/api/internal/plan/amend",
      json!({
        "session_id": "sess",
        "file": "artifacts/plan_02.html",
        "plan_id": 1,
        "note": "why changed",
        "trigger": "initial",
      }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "literal_error");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "trigger"]));
  }

  #[tokio::test]
  async fn plan_present_rejects_unknown_session_404() {
    let (_home, features) = features();
    let (status, body) = post(
      &features,
      "/api/internal/plan/present",
      json!({"session_id": "nonexistent", "file": "f.html", "title": "P1"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "Session not found"}));
  }

  #[tokio::test]
  async fn plan_approve_endpoint_happy_path() {
    let (_home, features) = session_with_registry("sess", AWAITING_REGISTRY);
    let (status, body) = post(&features, "/api/internal/plan/approve", json!({"session_id": "sess"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"plan": 1, "v": 1, "state": "approved"}));
  }

  #[tokio::test]
  async fn plan_close_endpoint_happy_path() {
    let (_home, features) = session_with_registry("sess", AWAITING_REGISTRY);
    let (status, body) = post(
      &features,
      "/api/internal/plan/close",
      json!({"session_id": "sess", "plan_id": 1, "close_as": "superseded"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"plan": 1, "state": "superseded"}));
  }

  #[tokio::test]
  async fn plan_close_rejects_already_closed_400() {
    let (_home, features) = session_with_registry("sess", AWAITING_REGISTRY);
    let close_superseded = json!({"session_id": "sess", "plan_id": 1, "close_as": "superseded"});
    post(&features, "/api/internal/plan/close", close_superseded).await;
    let (status, body) =
      post(&features, "/api/internal/plan/close", json!({"session_id": "sess", "plan_id": 1, "close_as": "abandoned"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"detail": "plan 1 is already closed ('superseded')"}));
  }

  #[tokio::test]
  async fn the_verbs_answer_404_for_an_unknown_session() {
    let (_home, features) = features();
    let (status, body) = post(&features, "/api/internal/plan/approve", json!({"session_id": "no-such"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "Session not found"}));
  }

  #[tokio::test]
  async fn a_body_the_model_refuses_answers_422_before_the_session_check() {
    let (_home, features) = features();
    let (status, body) =
      post(&features, "/api/internal/plan/approve", json!({"session_id": "no-such", "extra": 1})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "extra_forbidden");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "extra"]));
  }
}
