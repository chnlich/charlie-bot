//! The session routes that remain available to a signed token after its run ends.

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use charliebot_core::run_token::{RunTokenClaims, sign_run_token};
use charliebot_store::metadata::TaskProfile;
use charliebot_store::{CreateTask, RunOutcome, RunOutcomeKind, TaskCaller};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use super::composed_app;

#[tokio::test]
async fn ended_run_token_reads_session_detail_but_cannot_change_structure() {
  let home = tempfile::tempdir().expect("a temporary home");
  let key = "synthetic-access-key";
  std::fs::write(home.path().join("credentials.yaml"), format!("charliebot:\n  access_key: {key}\n"))
    .expect("the credentials file");
  let (app, features) = composed_app(home.path());
  let session = features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic session".to_string()),
        backend: Some("synthetic".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .expect("a synthetic session");
  let run_id = "synthetic-run";
  let run_path = features.store.run_metadata_path(&session.id, run_id);
  std::fs::create_dir_all(run_path.parent().expect("run directory")).expect("create the run directory");
  std::fs::write(
    &run_path,
    json!({"id": run_id, "session_id": session.id, "kind": "manager_turn", "pid": 424242, "pid_start": "1"})
      .to_string(),
  )
  .expect("the launched run record");
  let claims = RunTokenClaims { session_id: session.id.clone(), run_id: run_id.to_string(), agent: "synthetic".into() };
  let token = sign_run_token(&claims, key).expect("sign the run token");
  assert_eq!(features.store.run_identity_refusal(&session.id, run_id).unwrap(), None);
  {
    let guard = features.store.control_lock().await;
    features
      .store
      .record_finish_locked(&guard, &session.id, run_id, RunOutcome::new(RunOutcomeKind::Success))
      .await
      .expect("finish the launched run");
  }
  assert!(features.store.read_run_sync(&session.id, run_id).unwrap().unwrap().ended_at.is_some());

  // DLF nepscript/reviewer_session_check.py reads task_state after the invoking run has ended to check cancellation.
  let path = format!("/api/sessions/{}", session.id);
  let authorization = format!("Bearer {token}");
  let get = Request::builder()
    .method(Method::GET)
    .uri(&path)
    .header(header::AUTHORIZATION, &authorization)
    .body(Body::empty())
    .expect("build the session read");
  let response = app.clone().oneshot(get).await.expect("the session read answers");
  assert_eq!(response.status(), StatusCode::OK);
  let body = to_bytes(response.into_body(), usize::MAX).await.expect("read the session detail");
  let body: Value = serde_json::from_slice(&body).expect("the session detail is JSON");
  assert_eq!(body.get("task_state").and_then(Value::as_str), Some("open"));

  let patch = Request::builder()
    .method(Method::PATCH)
    .uri(&path)
    .header(header::AUTHORIZATION, &authorization)
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(json!({"task_parent_id": null}).to_string()))
    .expect("build the structural request");
  let response = app.oneshot(patch).await.expect("the structural request answers");
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
