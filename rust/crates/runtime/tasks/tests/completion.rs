mod dispatch_support;
#[allow(dead_code)]
mod process_guard;

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::event_types;
use charliebot_core::proc_identity;
use charliebot_core::run_token::{self, RunTokenClaims};
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::sequence::SequenceCompletion;
use charliebot_store::control_events::{self, ACTOR_AGENT, ACTOR_SYSTEM, ACTOR_USER};
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunOutcome, RunOutcomeKind, RunRecord, terminal_outcome};
use charliebot_store::{CreateTask, StoreError, StoreResult, TaskCaller};
use charliebot_tasks::completion::ArchiveAuthority;
use charliebot_tasks::completion::{CompletionEvidence, LandingEvidence, SequenceCloseOutcome, TaskCompletion};
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher, RunFinishedCompletion};
use charliebot_tasks::routes;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use dispatch_support::{Harness, ScriptedExecutor, new_run, register_run};
use process_guard::ProcessGroupGuard;

const ACCESS_KEY: &str = "synthetic-access-key";

struct WakeCountingExecutor(Arc<AtomicUsize>);

impl DispatchExecutor for WakeCountingExecutor {
  fn launch<'a>(
    &'a self,
    _session_id: &'a str,
    _pending_inputs: &'a [charliebot_store::ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move {
      self.0.fetch_add(1, Ordering::SeqCst);
      Ok(None)
    })
  }
}

struct CompletionHarness {
  base: Harness,
  features: Arc<FeatureContext>,
  completion: Arc<TaskCompletion>,
  dispatcher: Arc<Dispatcher>,
}

impl CompletionHarness {
  fn new() -> Self {
    let base = Harness::new();
    let startup = Arc::new(StartupState::default());
    let features = Arc::new(FeatureContext {
      config: Arc::clone(&base.features.config),
      credentials: Arc::clone(&base.features.credentials),
      store: Arc::clone(&base.store),
      registry: Arc::clone(&base.registry),
      startup: Arc::clone(&startup),
    });
    startup.insert(TaskCompletion::new(Arc::clone(&features))).expect("completion enters startup state once");
    let completion = startup.require::<TaskCompletion>().expect("completion is registered");
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = completion.clone();
    startup
      .insert(Dispatcher::new(Arc::clone(&features.store), Arc::clone(&features), sequence_completion, run_completion))
      .expect("dispatcher enters startup state once");
    let dispatcher = startup.require::<Dispatcher>().expect("dispatcher is registered");
    Self { base, features, completion, dispatcher }
  }

  async fn create_with_task(
    &self,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
    name: &str,
    task: TaskSpec,
  ) -> charliebot_store::metadata::SessionMetadata {
    self
      .base
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: Some(task),
          name: Some(name.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .expect("task node with spec is created")
  }

  async fn register_record(&self, run: RunRecord) {
    let guard = self.base.store.control_lock().await;
    let session_id = run.session_id.clone();
    self.base.store.register_run_locked(&guard, &session_id, run, None).await.expect("run record is registered");
  }

  async fn finish(&self, session_id: &str, run_id: &str, outcome: RunOutcomeKind) {
    self.dispatcher.finish_run(session_id, run_id, RunOutcome::new(outcome)).await.expect("run finish fact is written");
  }

  async fn post(&self, path: &str, payload: Value) -> (StatusCode, Value) {
    let response = routes::routes(&Arc::clone(&self.features))
      .oneshot(
        Request::builder()
          .method(Method::POST)
          .uri(path)
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(payload.to_string()))
          .expect("request is valid"),
      )
      .await
      .expect("router answers request");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("response body reads");
    let value = serde_json::from_slice(&body).expect("route returns JSON");
    (status, value)
  }
}

fn agent(session_id: &str, run_id: &str) -> CallerIdentity {
  CallerIdentity::Agent {
    claims: RunTokenClaims {
      session_id: session_id.to_string(),
      run_id: run_id.to_string(),
      agent: "synthetic".into(),
    },
  }
}

fn evidence(summary: &str, result_refs: &[&str], run_ids: &[&str]) -> CompletionEvidence {
  CompletionEvidence {
    summary: summary.to_string(),
    result_refs: result_refs.iter().map(|value| (*value).to_string()).collect(),
    run_ids: run_ids.iter().map(|value| (*value).to_string()).collect(),
    ..CompletionEvidence::default()
  }
}

fn live_run(session_id: &str, run_id: &str, kind: RunKind) -> RunRecord {
  let mut run = new_run(session_id, run_id);
  run.kind = kind;
  run.pid = Some(std::process::id());
  run.pid_start = Some(proc_identity::read_pid_stat(std::process::id()).expect("test process exists").pid_start);
  run.started_at = Some(UtcDateTime::now());
  run
}

fn iteration_run(session_id: &str, run_id: &str) -> RunRecord {
  let mut run = live_run(session_id, run_id, RunKind::Iteration);
  run.sequence_ref = Some(charliebot_store::run_records::SequenceRef {
    kind: "improve".to_string(),
    owner_ref: "improve:synthetic".to_string(),
    position: 0,
  });
  run
}

fn create_request(
  request_id: &str,
  parent_id: Option<&str>,
  profile: TaskProfile,
  task: Option<TaskSpec>,
) -> CreateTask {
  CreateTask {
    request_id: request_id.to_string(),
    task_parent_id: parent_id.map(str::to_string),
    profile,
    task,
    name: Some(format!("Synthetic {request_id}")),
    backend: Some("synthetic".to_string()),
    group: None,
    session_id: None,
  }
}

fn git(repo: &Path, args: &[&str]) -> std::process::Output {
  Command::new("git").arg("-C").arg(repo).args(args).output().expect("git is installed")
}

fn synthetic_git_repo(path: &Path) -> String {
  fs::create_dir(path).expect("synthetic repository directory is created");
  assert!(git(path, &["init", "-q", "-b", "main"]).status.success());
  assert!(git(path, &["config", "user.email", "synthetic@example.test"]).status.success());
  assert!(git(path, &["config", "user.name", "Synthetic"]).status.success());
  fs::write(path.join("result.txt"), "synthetic result\n").expect("synthetic file is written");
  assert!(git(path, &["add", "."]).status.success());
  assert!(git(path, &["commit", "-q", "-m", "synthetic result"]).status.success());
  String::from_utf8(git(path, &["rev-parse", "HEAD"]).stdout).expect("synthetic git hash is UTF-8").trim().to_string()
}

#[tokio::test]
async fn three_level_delivery_closes_workers_and_keeps_project_open() {
  // Python: tests/test_task_completion.py::test_three_level_delivery_closes_workers_and_keeps_project_open.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let manager = h.base.create_task("manager", Some(&root.id), TaskProfile::Manager, "Manager").await;
  let worker = h.base.create_task("worker", Some(&manager.id), TaskProfile::Worker, "Worker").await;
  register_run(&h.base, &worker.id, "worker-run").await;
  h.finish(&worker.id, "worker-run", RunOutcomeKind::Success).await;
  h.completion.after_run_finished(&worker.id, "worker-run").await.unwrap();
  assert_eq!(h.base.store.task_state(&worker.id).unwrap(), "completed");

  let input = h.dispatcher.pending_inputs(&manager.id).unwrap();
  assert_eq!(input.len(), 1);
  let mut manager_run = new_run(&manager.id, "manager-run");
  manager_run.kind = RunKind::ManagerTurn;
  h.register_record(manager_run).await;
  let guard = h.base.store.control_lock().await;
  let claimed = h.dispatcher.claim_input_batch_locked(&guard, &manager.id, "manager-run", None).unwrap();
  drop(guard);
  assert_eq!(claimed.len(), 1);
  h.finish(&manager.id, "manager-run", RunOutcomeKind::Success).await;

  // Archiving the feature leaves the project open; the feature's report lands on the project with summary,
  // evidence refs, outcome archived, and its node id.
  let (status, payload) = h
    .completion
    .archive_task(
      &manager.id,
      "manager-close",
      &evidence("delegation delivered", &["run:manager-run"], &["manager-run"]),
      &CallerIdentity::Operator { session_id: None },
      false,
    )
    .await
    .unwrap();
  assert_eq!(status, 200);
  assert_eq!(payload, json!({"archived": [manager.id.clone()]}));
  assert_eq!(h.base.store.task_state(&manager.id).unwrap(), "archived");
  assert_eq!(h.base.store.task_state(&root.id).unwrap(), "open");
  let root_events = h.base.store.load_all_facts(&root.id).unwrap();
  let reports = root_events
    .iter()
    .filter(|event| {
      event["type"] == event_types::CHILD_REPORT && event["child_session_id"].as_str() == Some(manager.id.as_str())
    })
    .collect::<Vec<_>>();
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "archived");
  assert_eq!(reports[0]["summary"], "delegation delivered");
  assert_eq!(reports[0]["result_refs"].to_value(), json!(["run:manager-run"]));

  // A repeated archive writes no new facts; the archived target replays its close.
  let (status_again, payload_again) = h
    .completion
    .archive_task(
      &manager.id,
      "manager-close",
      &evidence("delegation delivered", &["run:manager-run"], &["manager-run"]),
      &CallerIdentity::Operator { session_id: None },
      false,
    )
    .await
    .unwrap();
  assert_eq!(status_again, 200);
  let closes = h
    .base
    .store
    .load_all_facts(&manager.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::TASK_CLOSED)
    .collect::<Vec<_>>();
  assert_eq!(payload_again["closed_event_id"], closes[0]["id"].to_value());
  assert_eq!(closes.len(), 1);
}

#[tokio::test]
async fn operator_archive_of_child_delivers_one_report_and_wakes_parent() {
  // Python: tests/test_archive_unify.py::test_operator_archive_of_child_delivers_one_report_and_wakes_parent.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = h.base.create_task("child", Some(&root.id), TaskProfile::Manager, "Child").await;
  let descendant = h.base.create_task("descendant", Some(&child.id), TaskProfile::Worker, "Descendant").await;
  let wakes = Arc::new(AtomicUsize::new(0));
  h.dispatcher.set_executor(Some(Arc::new(WakeCountingExecutor(Arc::clone(&wakes)))));

  let archived = h
    .completion
    .archive_subtree(&child.id, Some("archive-child"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();

  assert_eq!(archived, [child.id.clone(), descendant.id.clone()]);
  let child_close = h
    .base
    .store
    .load_all_facts(&child.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(child_close["report_to"], root.id);
  let descendant_close = h
    .base
    .store
    .load_all_facts(&descendant.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(descendant_close["report_to"].to_value(), Value::Null);
  assert_eq!(descendant_close["archived_with"], child.id);
  let reports = h
    .base
    .store
    .load_all_facts(&root.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::CHILD_REPORT)
    .collect::<Vec<_>>();
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["child_session_id"], child.id);
  assert_eq!(reports[0]["child_event_id"], child_close["id"]);
  assert_eq!(reports[0]["outcome"], "archived");
  assert_eq!(reports[0]["summary"], "archived by the user");
  assert_eq!(wakes.load(Ordering::SeqCst), 1, "the parent is woken once for the one report");
}

#[tokio::test]
async fn archived_parent_keeps_child_archive_report_as_history_without_waking() {
  // Python: tests/test_archive_unify.py::test_archived_parent_keeps_child_archive_report_as_history_without_waking.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = h.base.create_task("child", Some(&root.id), TaskProfile::Manager, "Child").await;
  let root_close = h.completion.archived_close_event(
    &root.id,
    "archive-root",
    "archived by the user",
    None,
    None,
    None,
    ACTOR_USER,
    &[],
    &[],
  );
  let guard = h.base.store.control_lock().await;
  h.base.store.append_control_fact(&guard, &root.id, root_close).await.unwrap();
  drop(guard);
  h.base.store.invalidate_tree_index();
  let wakes = Arc::new(AtomicUsize::new(0));
  h.dispatcher.set_executor(Some(Arc::new(WakeCountingExecutor(Arc::clone(&wakes)))));

  h.completion
    .archive_subtree(&child.id, Some("archive-child"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();

  let reports = h
    .base
    .store
    .load_all_facts(&root.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::CHILD_REPORT)
    .collect::<Vec<_>>();
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "archived");
  let child_close_id = h
    .base
    .store
    .load_all_facts(&child.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap()["id"]
    .as_str()
    .expect("the close fact id is a string")
    .to_string();
  assert!(h.base.store.facts_of(&root.id).unwrap().delivered_reports.contains(&(child.id.clone(), child_close_id)));
  assert_eq!(h.base.store.task_state(&root.id).unwrap(), "archived");
  assert!(h.dispatcher.pending_inputs(&root.id).unwrap().is_empty());
  assert_eq!(wakes.load(Ordering::SeqCst), 0, "an archived parent wakes nobody");
  assert!(
    h.base.store.load_all_facts(&root.id).unwrap().iter().all(|event| event["type"] != event_types::TASK_REOPENED)
  );
}

#[tokio::test]
async fn preexisting_close_request_closes_as_archived_and_reports_to_parent() {
  // Python: tests/test_task_completion.py::test_preexisting_close_request_closes_as_archived_and_reports_to_parent.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let manager = h.base.create_task("manager", Some(&root.id), TaskProfile::Manager, "Manager").await;
  h.register_record(live_run(&manager.id, "run-manager", RunKind::ManagerTurn)).await;
  let request_id = "saved-before-archive-route";
  let request = control_events::build_control_event(
    event_types::TASK_CLOSE_REQUESTED,
    ACTOR_SYSTEM,
    &manager.id,
    Some(&control_events::stable_close_request_event_id(&manager.id, request_id)),
    None,
    [
      ("request_id".to_string(), json!(request_id)),
      ("owner_run_id".to_string(), json!("run-manager")),
      ("summary".to_string(), json!("finished the task")),
      ("result_refs".to_string(), json!(["report:finished"])),
      ("run_ids".to_string(), json!([])),
    ],
  );
  let guard = h.base.store.control_lock().await;
  h.base.store.append_control_fact(&guard, &manager.id, request).await.unwrap();
  drop(guard);
  h.base.store.invalidate_tree_index();

  h.finish(&manager.id, "run-manager", RunOutcomeKind::Success).await;

  let closes = h
    .base
    .store
    .load_all_facts(&manager.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::TASK_CLOSED)
    .collect::<Vec<_>>();
  assert_eq!(closes.len(), 1);
  assert_eq!(closes[0]["outcome"], "archived");
  assert_eq!(closes[0]["report_to"], root.id);
  let reports = h
    .base
    .store
    .load_all_facts(&root.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::CHILD_REPORT)
    .collect::<Vec<_>>();
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "archived");
}

#[tokio::test]
async fn implement_completion_requires_review_and_landing_evidence() {
  // Python: tests/test_task_completion.py::test_implement_completion_requires_review_and_landing_evidence.
  let h = CompletionHarness::new();
  let temp = tempfile::tempdir().unwrap();
  let repo = temp.path().join("repo");
  let commit = synthetic_git_repo(&repo);
  let task = h
    .create_with_task(
      "implement",
      None,
      TaskProfile::Worker,
      "Implement",
      TaskSpec {
        goal: "synthetic implementation".into(),
        repo_path: Some(repo.to_string_lossy().into_owned()),
        base_branch: Some("main".into()),
        task_type: Some(TaskType::Implement),
        ..TaskSpec::default()
      },
    )
    .await;
  let mut work = new_run(&task.id, "work-run");
  work.repo_path = Some(repo.to_string_lossy().into_owned());
  work.base_branch = Some("main".into());
  h.register_record(work).await;
  h.finish(&task.id, "work-run", RunOutcomeKind::Success).await;

  let meta = h.base.store.load_task_meta(&task.id).await.unwrap();
  let blockers = h
    .completion
    .verified_evidence_blockers(&meta, &evidence("implementation", &["run:work-run"], &["work-run"]), None)
    .await
    .unwrap();
  assert!(blockers.contains(&"implement completion requires a successful review run of this task".to_string()));
  assert!(blockers.contains(&"implement completion requires target-branch landing evidence".to_string()));

  let mut review = new_run(&task.id, "review-run");
  review.kind = RunKind::Review;
  review.review_of_run_id = Some("work-run".into());
  review.base_branch = Some("main".into());
  h.register_record(review).await;
  h.finish(&task.id, "review-run", RunOutcomeKind::Success).await;
  let mut complete = evidence("implementation", &["run:work-run"], &["work-run"]);
  complete.review_run_ids = vec!["review-run".into()];
  complete.landing =
    Some(LandingEvidence { branch: "main".into(), commit, repo_path: Some(repo.to_string_lossy().into_owned()) });
  assert_eq!(
    h.completion
      .evaluate_automatic_completion(&task.id, "work-run", None, None, Some("valid-implement"), Some(&complete))
      .await
      .unwrap()
      .0,
    200
  );
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "completed");
}

#[tokio::test]
async fn worker_automatic_close_with_forged_landing_ref_stays_open() {
  // Python: tests/test_task_execution.py::test_worker_automatic_close_with_forged_landing_ref_stays_open.
  let h = CompletionHarness::new();
  let temp = tempfile::tempdir().unwrap();
  let repo = temp.path().join("repo");
  let _landed = synthetic_git_repo(&repo);
  assert!(git(&repo, &["switch", "-q", "-c", "side-work"]).status.success());
  fs::write(repo.join("unlanded.txt"), "synthetic unlanded result\n").unwrap();
  assert!(git(&repo, &["add", "-A"]).status.success());
  assert!(git(&repo, &["commit", "-q", "-m", "unlanded result"]).status.success());
  let unlanded = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();
  assert!(git(&repo, &["switch", "-q", "main"]).status.success());

  let task = h
    .create_with_task(
      "forged-landing",
      None,
      TaskProfile::Worker,
      "Implement",
      TaskSpec {
        goal: "synthetic implementation".into(),
        repo_path: Some(repo.to_string_lossy().into_owned()),
        base_branch: Some("main".into()),
        task_type: Some(TaskType::Implement),
        ..TaskSpec::default()
      },
    )
    .await;
  let mut work = new_run(&task.id, "work-run");
  work.repo_path = Some(repo.to_string_lossy().into_owned());
  work.base_branch = Some("main".into());
  h.register_record(work).await;
  h.finish(&task.id, "work-run", RunOutcomeKind::Success).await;
  let mut review = new_run(&task.id, "review-run");
  review.kind = RunKind::Review;
  review.review_of_run_id = Some("work-run".into());
  review.repo_path = Some(repo.to_string_lossy().into_owned());
  review.base_branch = Some("main".into());
  h.register_record(review).await;
  h.finish(&task.id, "review-run", RunOutcomeKind::Success).await;

  let mut completion = evidence("forged landing", &["run:work-run", &format!("landed:main@{unlanded}")], &["work-run"]);
  completion.review_run_ids = vec!["review-run".into()];
  let result = h
    .completion
    .evaluate_automatic_completion(&task.id, "work-run", None, None, Some("forged-auto"), Some(&completion))
    .await;
  assert!(
    matches!(result, Err(StoreError::Conflict(blockers)) if blockers.iter().any(|b| b.starts_with("landing evidence unverified in ")))
  );
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "open");
}

#[tokio::test]
async fn own_run_request_naming_a_foreign_run_is_refused_and_not_saved() {
  // Python: tests/test_task_completion.py::test_own_run_request_naming_a_foreign_run_is_refused_and_not_saved.
  let h = CompletionHarness::new();
  let task = h.base.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  let foreign = h.base.create_task("foreign", None, TaskProfile::Worker, "Foreign").await;
  h.register_record(live_run(&task.id, "owner-run", RunKind::ManagerTurn)).await;
  register_run(&h.base, &foreign.id, "foreign-run").await;
  h.finish(&foreign.id, "foreign-run", RunOutcomeKind::Success).await;

  let result = h
    .completion
    .archive_task(
      &task.id,
      "foreign-evidence",
      &evidence("manager done", &["run:foreign-run"], &["foreign-run"]),
      &agent(&task.id, "owner-run"),
      false,
    )
    .await;
  assert!(
    matches!(result, Err(StoreError::Conflict(blockers)) if blockers.iter().any(|b| b == &format!("run foreign-run is not a Run of task {} or its children", task.id)))
  );
  assert!(h.base.store.facts_of(&task.id).unwrap().close_requests.is_empty());
}

#[tokio::test]
async fn own_run_request_with_an_open_child_is_refused_and_not_saved() {
  // Python: tests/test_task_completion.py::test_own_run_request_with_an_open_child_is_refused_and_not_saved.
  let h = CompletionHarness::new();
  let task = h.base.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  let child = h.base.create_task("child", Some(&task.id), TaskProfile::Worker, "Child").await;
  h.register_record(live_run(&task.id, "owner-run", RunKind::ManagerTurn)).await;
  let result = h
    .completion
    .archive_task(
      &task.id,
      "open-child",
      &evidence("manager done", &["report:child"], &["owner-run"]),
      &agent(&task.id, "owner-run"),
      false,
    )
    .await;
  assert!(
    matches!(result, Err(StoreError::Conflict(blockers)) if blockers.contains(&format!("has open descendant task {}", child.id)))
  );
  assert!(h.base.store.facts_of(&task.id).unwrap().close_requests.is_empty());
}

#[tokio::test]
async fn saved_request_blocked_after_its_run_wakes_the_requester_once() {
  // Python: tests/test_task_completion.py::test_saved_request_blocked_after_its_run_wakes_the_requester_once.
  let h = CompletionHarness::new();
  let task = h.base.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  h.register_record(live_run(&task.id, "owner-run", RunKind::ManagerTurn)).await;
  let own = agent(&task.id, "owner-run");
  assert_eq!(
    h.completion
      .archive_task(
        &task.id,
        "close-after-finish",
        &evidence("manager done", &["run:owner-run"], &["owner-run"]),
        &own,
        false,
      )
      .await
      .unwrap()
      .0,
    202
  );
  let child = h.base.create_task("late-child", Some(&task.id), TaskProfile::Worker, "Late child").await;
  h.finish(&task.id, "owner-run", RunOutcomeKind::Success).await;

  let notice_id = control_events::stable_close_blocked_notice_id(&task.id, "close-after-finish");
  let notices = h
    .base
    .store
    .load_all_facts(&task.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::SCHEDULED_TRIGGER && event["id"] == notice_id)
    .count();
  assert_eq!(notices, 1);
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&child.id).unwrap(), "open");
}

#[tokio::test]
async fn agent_cancels_only_its_own_direct_child() {
  // Python: tests/test_task_completion.py::test_agent_cancels_only_its_own_direct_child.
  let h = CompletionHarness::new();
  let parent = h.base.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  let child = h.base.create_task("child", Some(&parent.id), TaskProfile::Worker, "Child").await;
  let unrelated = h.base.create_task("unrelated", None, TaskProfile::Worker, "Unrelated").await;

  let closed = h.completion.cancel_task(&child.id, "agent-cancel", "obsolete", &agent(&parent.id, "parent-run")).await;
  assert_eq!(closed.unwrap()["session_id"], child.id);
  let denied =
    h.completion.cancel_task(&unrelated.id, "agent-cancel-2", "outside scope", &agent(&parent.id, "parent-run")).await;
  assert!(
    matches!(denied, Err(StoreError::Forbidden(message)) if message == "an agent may only cancel a direct child of its own task")
  );
  assert_eq!(h.base.store.task_state(&unrelated.id).unwrap(), "open");
}

#[tokio::test]
async fn worker_cancel_stops_queued_runs_and_keeps_manager_and_sequence_blockers() {
  // Python: tests/test_task_completion.py::test_cancel_stops_a_queued_worker_run_without_launching_it.
  let h = CompletionHarness::new();
  let caller = CallerIdentity::Operator { session_id: None };
  let manager = h.base.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  h.register_record(live_run(&manager.id, "manager-run", RunKind::ManagerTurn)).await;
  let manager_result = h.completion.cancel_task(&manager.id, "cancel-manager", "stop", &caller).await;
  assert!(matches!(manager_result, Err(StoreError::Conflict(_))));
  assert_eq!(h.base.store.task_state(&manager.id).unwrap(), "open");

  let sequence = h.base.create_task("sequence", Some(&manager.id), TaskProfile::Worker, "Sequence").await;
  let mut sequence_run = new_run(&sequence.id, "sequence-run");
  sequence_run.sequence_ref = Some(charliebot_store::run_records::SequenceRef {
    kind: "improve".to_string(),
    owner_ref: "improve:synthetic".to_string(),
    position: 0,
  });
  h.register_record(sequence_run).await;
  let sequence_events = h.base.store.load_all_facts(&sequence.id).unwrap();
  let sequence_result = h.completion.cancel_task(&sequence.id, "cancel-sequence", "stop", &caller).await;
  assert!(matches!(sequence_result, Err(StoreError::Conflict(_))));
  assert_eq!(h.base.store.load_all_facts(&sequence.id).unwrap(), sequence_events);

  let worker = h.base.create_task("worker", Some(&manager.id), TaskProfile::Worker, "Worker").await;
  register_run(&h.base, &worker.id, "queued-run").await;
  let result = h.completion.cancel_task(&worker.id, "cancel-worker", "stop", &caller).await;
  assert!(result.is_ok(), "cancelling a worker should stop its queued run");
  let result = result.unwrap();
  assert_eq!(result["stopped_runs"], json!([{"run_id":"queued-run","outcome":"never_launched"}]));
  assert_eq!(h.base.store.task_state(&worker.id).unwrap(), "cancelled");
  let replay = h.completion.cancel_task(&worker.id, "cancel-worker", "changed", &caller).await.unwrap();
  assert_eq!(replay, result);
}

#[tokio::test]
async fn cancel_by_the_manager_parent_records_close_and_child_report() {
  // Python: tests/test_task_completion.py::test_cancel_by_the_manager_parent_records_close_and_child_report.
  let h = CompletionHarness::new();
  let parent = h.base.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  let child = h.base.create_task("child", Some(&parent.id), TaskProfile::Worker, "Child").await;
  h.completion
    .cancel_task(&child.id, "agent-cancel", "obsolete delegation", &agent(&parent.id, "parent-run"))
    .await
    .unwrap();
  let close = h
    .base
    .store
    .load_all_facts(&child.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(close["actor"], ACTOR_AGENT);
  let report = h
    .base
    .store
    .load_all_facts(&parent.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::CHILD_REPORT && event["child_event_id"] == close["id"])
    .unwrap();
  assert_eq!(report["outcome"], "cancelled");
}

#[tokio::test]
async fn manager_archive_of_a_subtree_task_archives_it_and_reports_to_its_parent() {
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let manager = h.base.create_task("manager", Some(&root.id), TaskProfile::Manager, "Manager").await;
  let target = h.base.create_task("target", Some(&manager.id), TaskProfile::Manager, "Target").await;
  let grandchild = h.base.create_task("grandchild", Some(&target.id), TaskProfile::Worker, "Grandchild").await;

  let (status, payload) = h
    .completion
    .archive_task(
      &target.id,
      "agent-subtree",
      &evidence("line cleanup", &[], &[]),
      &agent(&manager.id, "manager-run"),
      false,
    )
    .await
    .unwrap();
  assert_eq!(status, 200);
  assert_eq!(payload, json!({"archived": [target.id.clone(), grandchild.id.clone()]}));
  assert_eq!(h.base.store.task_state(&target.id).unwrap(), "archived");
  assert_eq!(h.base.store.task_state(&grandchild.id).unwrap(), "archived");
  assert_eq!(h.base.store.task_state(&manager.id).unwrap(), "open");

  // The target's close fact names the caller in archived_by and reports to the target's parent.
  let close = h
    .base
    .store
    .load_all_facts(&target.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(close["actor"], ACTOR_AGENT);
  assert_eq!(close["archived_by"], manager.id);
  assert_eq!(close["summary"], "line cleanup");
  assert_eq!(close["report_to"], manager.id);
  let report = h
    .base
    .store
    .load_all_facts(&manager.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::CHILD_REPORT && event["child_event_id"] == close["id"])
    .unwrap();
  assert_eq!(report["outcome"], "archived");
}

#[tokio::test]
async fn manager_archive_of_another_task_needs_the_subtree_or_the_non_child_confirmation() {
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let manager = h.base.create_task("manager", Some(&root.id), TaskProfile::Manager, "Manager").await;
  let worker = h.base.create_task("worker", Some(&manager.id), TaskProfile::Worker, "Worker").await;
  let outside = h.base.create_task("outside", None, TaskProfile::Manager, "Outside").await;
  let caller = agent(&manager.id, "manager-run");

  // A worker caller may not archive any other task, with or without the confirmation.
  let worker_refusal = h
    .completion
    .archive_task(&root.id, "worker-archive", &evidence("", &[], &[]), &agent(&worker.id, "worker-run"), true)
    .await;
  assert!(
    matches!(worker_refusal, Err(StoreError::Forbidden(message)) if message == "only a manager can archive another task")
  );

  // A manager refuses a task outside its subtree without the confirmation and changes nothing.
  let outside_refusal =
    h.completion.archive_task(&outside.id, "outside-archive", &evidence("", &[], &[]), &caller, false).await;
  assert!(
    matches!(outside_refusal, Err(StoreError::Forbidden(ref message)) if message.contains("--archive-non-child")),
    "{outside_refusal:?}"
  );
  assert_eq!(h.base.store.task_state(&outside.id).unwrap(), "open");
  assert!(
    !h.base.store.load_all_facts(&outside.id).unwrap().iter().any(|event| event["type"] == event_types::TASK_CLOSED)
  );

  // With the confirmation the archive lands; the close fact names the caller and defaults the summary to it.
  let (status, payload) =
    h.completion.archive_task(&outside.id, "outside-archive", &evidence("", &[], &[]), &caller, true).await.unwrap();
  assert_eq!(status, 200);
  assert_eq!(payload, json!({"archived": [outside.id.clone()]}));
  let close = h
    .base
    .store
    .load_all_facts(&outside.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(close["actor"], ACTOR_AGENT);
  assert_eq!(close["archived_by"], manager.id);
  assert_eq!(close["summary"], format!("archived by {}", manager.id));
}

#[tokio::test]
async fn agent_archive_of_a_subtree_with_an_unfinished_run_refuses_like_the_operator() {
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let manager = h.base.create_task("manager", Some(&root.id), TaskProfile::Manager, "Manager").await;
  let target = h.base.create_task("target", Some(&manager.id), TaskProfile::Worker, "Target").await;
  register_run(&h.base, &target.id, "queued-run").await;

  let agent_refusal = h
    .completion
    .archive_task(&target.id, "agent-blocked", &evidence("", &[], &[]), &agent(&manager.id, "manager-run"), false)
    .await;
  let operator_refusal = h
    .completion
    .archive_subtree(
      &target.id,
      Some("operator-blocked"),
      "archived by the user",
      &[],
      &[],
      &ArchiveAuthority::Operator,
    )
    .await;
  let (agent_refusal, operator_refusal) = (agent_refusal.unwrap_err(), operator_refusal.unwrap_err());
  assert_eq!(agent_refusal.status_code(), Some(409));
  assert!(!agent_refusal.blockers().unwrap().is_empty());
  assert_eq!(agent_refusal.blockers(), operator_refusal.blockers());
  assert_eq!(h.base.store.task_state(&target.id).unwrap(), "open");
}

#[tokio::test]
async fn own_run_archive_closes_without_takeoff_and_reports_to_ancestor() {
  // Python: tests/test_task_manager_authorization.py::test_own_run_archive_closes_without_takeoff_and_reports_to_ancestor.
  // Its three cases: a task with no earlier run, one whose earlier run succeeded, and one whose earlier run failed.
  for previous_outcome in [None, Some(RunOutcomeKind::Success), Some(RunOutcomeKind::Failed)] {
    let h = CompletionHarness::new();
    let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
    let child = h.base.create_task("child", Some(&root.id), TaskProfile::Manager, "Child").await;
    let blocked_manager = h.base.create_task("blocked-manager", Some(&root.id), TaskProfile::Manager, "Blocked").await;
    h.register_record(live_run(&blocked_manager.id, "blocked-owner-run", RunKind::ManagerTurn)).await;
    h.base.create_task("open-child", Some(&blocked_manager.id), TaskProfile::Worker, "Open child").await;
    if let Some(outcome) = previous_outcome {
      h.register_record(live_run(&child.id, "previous-turn", RunKind::ManagerTurn)).await;
      h.finish(&child.id, "previous-turn", outcome).await;
    }
    h.register_record(live_run(&child.id, "child-owner-run", RunKind::ManagerTurn)).await;

    // An agent may not archive a task outside its subtree without the non-child confirmation.
    let outside_subtree = h
      .completion
      .archive_task(
        &root.id,
        "wrong-task",
        &evidence("coordination complete", &["report:feature-coordinated"], &[]),
        &agent(&child.id, "child-owner-run"),
        false,
      )
      .await;
    assert!(matches!(outside_subtree, Err(StoreError::Forbidden(message)) if message.contains("--archive-non-child")));

    // An open descendant refuses the request before it is saved.
    let blocked = h
      .completion
      .archive_task(
        &blocked_manager.id,
        "blocked-by-child",
        &evidence("manager report", &["report:manager-report"], &[]),
        &agent(&blocked_manager.id, "blocked-owner-run"),
        false,
      )
      .await;
    assert!(
      matches!(blocked, Err(StoreError::Conflict(blockers)) if blockers.iter().any(|b| b.contains("open descendant")))
    );
    assert_eq!(
      h.base
        .store
        .load_all_facts(&blocked_manager.id)
        .unwrap()
        .iter()
        .filter(|event| event["type"] == event_types::TASK_CLOSE_REQUESTED)
        .count(),
      0
    );

    // The owner's own active Run may archive its task with no run_ids evidence at all.
    let pending = h
      .completion
      .archive_task(
        &child.id,
        "close-1",
        &evidence("coordination complete", &["report:feature-coordinated"], &[]),
        &agent(&child.id, "child-owner-run"),
        false,
      )
      .await
      .unwrap();
    assert_eq!(pending.0, 202);
    assert_eq!(pending.1["status"], "pending_run_finish");
    assert_eq!(h.base.store.task_state(&child.id).unwrap(), "open");

    // The owner Run finishes successfully: the archive lands and reports to the parent.
    h.finish(&child.id, "child-owner-run", RunOutcomeKind::Success).await;
    assert_eq!(h.base.store.task_state(&child.id).unwrap(), "archived");
    let closes = h
      .base
      .store
      .load_all_facts(&child.id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"] == event_types::TASK_CLOSED)
      .collect::<Vec<_>>();
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0]["outcome"], "archived");
    assert_eq!(closes[0]["report_to"], root.id);
    let reports = h
      .base
      .store
      .load_all_facts(&root.id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"] == event_types::CHILD_REPORT)
      .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["outcome"], "archived");

    // The ancestor holds its own conditions: it stays open after the child archived.
    assert_eq!(h.base.store.task_state(&root.id).unwrap(), "open");
  }
}

#[tokio::test]
async fn agent_manager_child_creation_replays_without_takeoff() {
  // Python: tests/test_task_manager_authorization.py::test_manager_child_creation_needs_no_takeoff_and_replays_stably.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let caller = TaskCaller::Agent { session_id: root.id.clone() };
  let request = create_request("child-manager", Some(&root.id), TaskProfile::Manager, None);
  let child = h.base.store.create_task(request.clone(), caller.clone(), Map::new()).await.unwrap();
  let replay = h.base.store.create_task(request, caller, Map::new()).await.unwrap();
  assert_eq!(child.id, replay.id);
  assert_eq!(child.profile, TaskProfile::Manager);
  let created = h
    .base
    .store
    .load_all_facts(&child.id)
    .unwrap()
    .into_iter()
    .filter(|event| event["type"] == event_types::TASK_CREATED)
    .collect::<Vec<_>>();
  assert_eq!(created.len(), 1);
  assert_eq!(created[0]["actor"], ACTOR_AGENT);
  assert!(h.base.store.load_all_facts(&root.id).unwrap().iter().all(|event| event["type"] != event_types::USER));
}

#[tokio::test]
async fn implementation_gate_uses_only_real_user_input_across_manager_depth() {
  // Python: tests/test_task_manager_authorization.py::test_implementation_blocked_until_real_user_authorizes_then_delegates_from_depth.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = h
    .base
    .store
    .create_task(
      create_request("child-manager", Some(&root.id), TaskProfile::Manager, None),
      TaskCaller::Agent { session_id: root.id.clone() },
      Map::new(),
    )
    .await
    .unwrap();
  let grandchild = h
    .base
    .store
    .create_task(
      create_request("grandchild-manager", Some(&child.id), TaskProfile::Manager, None),
      TaskCaller::Agent { session_id: child.id.clone() },
      Map::new(),
    )
    .await
    .unwrap();
  let worker_request = create_request(
    "implementation",
    Some(&grandchild.id),
    TaskProfile::Worker,
    Some(TaskSpec {
      goal: "synthetic implementation".into(),
      task_type: Some(TaskType::Implement),
      ..TaskSpec::default()
    }),
  );
  let caller = TaskCaller::Agent { session_id: grandchild.id.clone() };
  let blocked = h.base.store.create_task(worker_request.clone(), caller.clone(), Map::new()).await.unwrap_err();
  assert!(blocked.to_string().starts_with("Delegation blocked: no user request on the task chain"));

  h.dispatcher
    .admit_input(
      &grandchild.id,
      event_types::AGENT_MESSAGE,
      "take off and implement",
      ACTOR_AGENT,
      json!({"from_session": child.id}),
    )
    .await
    .unwrap();
  h.dispatcher
    .admit_input(
      &grandchild.id,
      event_types::CHILD_REPORT,
      "take off, the child finished",
      ACTOR_AGENT,
      json!({"from_session": child.id}),
    )
    .await
    .unwrap();
  let still_blocked = h.base.store.create_task(worker_request.clone(), caller.clone(), Map::new()).await.unwrap_err();
  assert!(still_blocked.to_string().starts_with("Delegation blocked: no user request on the task chain"));

  h.dispatcher
    .admit_input(&root.id, event_types::USER, "Can you ship the feature?", ACTOR_USER, json!({}))
    .await
    .unwrap();
  let worker = h.base.store.create_task(worker_request, caller, Map::new()).await.unwrap();
  assert_eq!(worker.profile, TaskProfile::Worker);
  for session_id in [&child.id, &grandchild.id] {
    assert!(h.base.store.load_all_facts(session_id).unwrap().iter().all(|event| event["type"] != event_types::USER));
  }
}

#[tokio::test]
async fn queued_retry_launch_skips_reauthorization_and_verify_is_exempt() {
  // Python: tests/test_task_manager_authorization.py::test_queued_retry_launches_without_reauthorizing_and_verify_exemption.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  h.dispatcher
    .admit_input(&root.id, event_types::USER, "Take off. Ship the feature.", ACTOR_USER, json!({}))
    .await
    .unwrap();
  let worker = h
    .base
    .store
    .create_task(
      create_request(
        "implementation",
        Some(&root.id),
        TaskProfile::Worker,
        Some(TaskSpec {
          goal: "synthetic implementation".into(),
          task_type: Some(TaskType::Implement),
          ..TaskSpec::default()
        }),
      ),
      TaskCaller::Agent { session_id: root.id.clone() },
      Map::new(),
    )
    .await
    .unwrap();
  h.dispatcher
    .admit_input(
      &worker.id,
      event_types::AGENT_MESSAGE,
      "implement the feature",
      ACTOR_AGENT,
      json!({"from_session": root.id}),
    )
    .await
    .unwrap();
  let mut original = new_run(&worker.id, "work-run");
  original.kind = RunKind::Work;
  original.backend = Some("synthetic".into());
  original.model = Some("synthetic-model".into());
  h.register_record(original).await;
  h.finish(&worker.id, "work-run", RunOutcomeKind::Failed).await;
  let cfg = h.features.config.get().unwrap();
  let retry = charliebot_tasks::task_sessions::create_retry(
    &h.features.store,
    &cfg,
    &worker.id,
    "retry-1",
    "work-run",
    None,
    None,
  )
  .await
  .unwrap();

  let guard = h.base.store.control_lock().await;
  let clone_start =
    control_events::build_control_event("clone_start", "system", &root.id, None, None, std::iter::empty());
  h.base.store.append_control_fact(&guard, &root.id, clone_start).await.unwrap();
  drop(guard);
  assert!(h.base.store.check_task_authorization(&root.id).is_err());

  h.dispatcher.set_executor(Some(Arc::new(ScriptedExecutor::new(
    Arc::clone(&h.features.store),
    &h.dispatcher,
    vec![RunOutcomeKind::Success, RunOutcomeKind::Success],
  ))));
  let decision = h.dispatcher.dispatch_pending(&worker.id).await.unwrap();
  assert_eq!(decision["run_id"], retry.id);
  let worker_finish = h
    .base
    .store
    .load_all_facts(&worker.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::RUN_FINISHED && event["run_id"] == retry.id)
    .unwrap();
  assert_eq!(worker_finish["outcome"], "success");

  let verify_root = h.base.create_task("verify-root", None, TaskProfile::Manager, "Verify root").await;
  assert!(h.base.store.check_task_authorization(&verify_root.id).is_err());
  let verify = h
    .base
    .store
    .create_task(
      create_request(
        "verify-task",
        Some(&verify_root.id),
        TaskProfile::Worker,
        Some(TaskSpec {
          goal: "synthetic verification".into(),
          task_type: Some(TaskType::Verify),
          ..TaskSpec::default()
        }),
      ),
      TaskCaller::Agent { session_id: verify_root.id.clone() },
      Map::new(),
    )
    .await
    .unwrap();
  h.dispatcher
    .admit_input(
      &verify.id,
      event_types::AGENT_MESSAGE,
      "verify the result",
      ACTOR_AGENT,
      json!({"from_session": verify_root.id}),
    )
    .await
    .unwrap();
  let verify_decision = h.dispatcher.dispatch_pending(&verify.id).await.unwrap();
  assert_eq!(verify_decision["launch"], true);
  let verify_finish = h
    .base
    .store
    .load_all_facts(&verify.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::RUN_FINISHED)
    .unwrap();
  assert_eq!(verify_finish["outcome"], "success");
}

#[tokio::test]
async fn profile_changing_agent_create_replay_keeps_the_original_product() {
  // Python: tests/test_task_manager_authorization.py::test_profile_changing_replay_never_bypasses_authorization.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let caller = TaskCaller::Agent { session_id: root.id.clone() };
  let worker = h
    .base
    .store
    .create_task(
      create_request("worker-original", Some(&root.id), TaskProfile::Worker, None),
      TaskCaller::Operator,
      Map::new(),
    )
    .await
    .unwrap();

  for profile in [TaskProfile::Manager, TaskProfile::Worker] {
    let request = create_request("worker-original", Some(&root.id), profile, None);
    let replay = h.base.store.create_task(request, caller.clone(), Map::new()).await.unwrap_err();
    assert!(replay.to_string().starts_with("Delegation blocked: no user request on the task chain"));
  }
  let manager = h
    .base
    .store
    .create_task(
      create_request("manager-original", Some(&root.id), TaskProfile::Manager, None),
      caller.clone(),
      Map::new(),
    )
    .await
    .unwrap();
  let manager_as_worker = h
    .base
    .store
    .create_task(
      create_request("manager-original", Some(&root.id), TaskProfile::Worker, None),
      caller.clone(),
      Map::new(),
    )
    .await
    .unwrap();
  assert_eq!(manager_as_worker.id, manager.id);
  assert_eq!(manager_as_worker.profile, TaskProfile::Manager);

  h.dispatcher
    .admit_input(&root.id, event_types::USER, "Can you ship the feature?", ACTOR_USER, json!({}))
    .await
    .unwrap();
  let worker_as_manager = h
    .base
    .store
    .create_task(create_request("worker-original", Some(&root.id), TaskProfile::Manager, None), caller, Map::new())
    .await
    .unwrap();
  assert_eq!(worker_as_manager.id, worker.id);
  assert_eq!(worker_as_manager.profile, TaskProfile::Worker);
}

#[tokio::test]
async fn acknowledgement_replays_and_refuses_unknown_inputs() {
  // Python: tests/test_session_dispatch.py::test_complete_cancel_reopen_routes_and_scope (acknowledgement contract).
  let h = CompletionHarness::new();
  let task = h.base.create_task("ack", None, TaskProfile::Worker, "Ack").await;
  let input = h
    .dispatcher
    .admit_input(&task.id, event_types::USER, "handled out of band", ACTOR_USER, json!({"input_id":"input-one"}))
    .await
    .unwrap();
  let caller = CallerIdentity::Operator { session_id: None };
  let first = h
    .completion
    .acknowledge_inputs(&task.id, "ack-1", &[input["id"].as_str().unwrap().to_string()], "handled", &caller)
    .await
    .unwrap();
  let replay = h
    .completion
    .acknowledge_inputs(&task.id, "ack-1", &[input["id"].as_str().unwrap().to_string()], "changed", &caller)
    .await
    .unwrap();
  assert_eq!(first, replay);
  let already = h
    .completion
    .acknowledge_inputs(&task.id, "ack-2", &[input["id"].as_str().unwrap().to_string()], "handled", &caller)
    .await
    .unwrap();
  assert_eq!(already["input_ids"], json!([]));
  assert_eq!(already["already_acknowledged"], json!([input["id"].to_value()]));
  let unknown = h.completion.acknowledge_inputs(&task.id, "ack-3", &["missing-input".into()], "handled", &caller).await;
  assert!(
    matches!(unknown, Err(StoreError::Conflict(blockers)) if blockers == vec![format!("input(s) not pending for {}: missing-input", task.id)])
  );
}

#[tokio::test]
async fn sequence_completion_requires_proven_run_and_empty_delivery_is_cancelled() {
  // Python: tests/test_task_completion.py::test_close_sequence_child_completed_on_proven_run_evidence.
  // Python: tests/test_task_completion.py::test_close_sequence_child_completed_without_a_successful_run_closes_cancelled.
  // Python: tests/test_task_sequences.py::test_merge_back_with_only_failed_iterations_closes_cancelled_with_completed_report.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let proven = h.base.create_task("proven", Some(&root.id), TaskProfile::Worker, "Proven").await;
  h.register_record(iteration_run(&proven.id, "run-iter-1")).await;
  h.finish(&proven.id, "run-iter-1", RunOutcomeKind::Success).await;
  let result = h
    .completion
    .close_sequence_child(
      &proven.id,
      "improve:1:close",
      SequenceCloseOutcome::Completed,
      &evidence("[Improve loop 1] landed", &["loop:1"], &["run-iter-1"]),
    )
    .await
    .unwrap();
  assert_eq!(result.0, 200);
  assert_eq!(h.base.store.task_state(&proven.id).unwrap(), "completed");
  let close = h
    .base
    .store
    .load_all_facts(&proven.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(close["actor"], ACTOR_SYSTEM);
  assert_eq!(close["outcome"], "completed");
  assert_eq!(close["sequence_outcome"], "completed");
  assert_eq!(close["run_ids"].to_value(), json!(["run-iter-1"]));

  let empty = h.base.create_task("empty", Some(&root.id), TaskProfile::Worker, "Empty").await;
  h.completion
    .close_sequence_child(
      &empty.id,
      "improve:2:close",
      SequenceCloseOutcome::Completed,
      &evidence("[Improve loop 2] landed", &["loop:2"], &[]),
    )
    .await
    .unwrap();
  assert_eq!(h.base.store.task_state(&empty.id).unwrap(), "cancelled");
  let empty_close = h
    .base
    .store
    .load_all_facts(&empty.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(empty_close["outcome"], "cancelled");
  assert_eq!(empty_close["sequence_outcome"], "completed");
  let empty_report = h
    .base
    .store
    .load_all_facts(&root.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::CHILD_REPORT && event["child_session_id"] == empty.id)
    .unwrap();
  assert_eq!(empty_report["outcome"], "completed");
}

#[tokio::test]
async fn sequence_noncompleted_outcomes_record_evidence_as_content() {
  // Python: tests/test_task_completion.py::test_close_sequence_child_non_completed_outcomes_record_evidence_as_content (blocked, failed, cancelled cases).
  // Python: tests/test_task_sequences.py::test_controller_exception_closes_the_child_failed.
  // Python: tests/test_task_sequences.py::test_worktree_failure_closes_the_child_failed.
  // Python: tests/test_task_sequences.py::test_improve_stop_mid_iteration_closes_after_the_controller_ends.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  for (index, outcome) in ["blocked", "failed", "cancelled"].into_iter().enumerate() {
    let child = h
      .base
      .create_task(&format!("child-{index}"), Some(&root.id), TaskProfile::Worker, &format!("Child {index}"))
      .await;
    let run_id = format!("run-{index}");
    register_run(&h.base, &child.id, &run_id).await;
    h.finish(&child.id, &run_id, RunOutcomeKind::Failed).await;
    h.completion
      .close_sequence_child(
        &child.id,
        &format!("sequence-close-{index}"),
        match outcome {
          "blocked" => SequenceCloseOutcome::Blocked,
          "failed" => SequenceCloseOutcome::Failed,
          "cancelled" => SequenceCloseOutcome::Cancelled,
          _ => unreachable!(),
        },
        &evidence(&format!("[Improve loop 1] {outcome}"), &["loop:1"], &[&run_id]),
      )
      .await
      .unwrap();
    assert_eq!(h.base.store.task_state(&child.id).unwrap(), "cancelled");
    let close = h
      .base
      .store
      .load_all_facts(&child.id)
      .unwrap()
      .into_iter()
      .find(|event| event["type"] == event_types::TASK_CLOSED)
      .unwrap();
    assert_eq!(close["outcome"], "cancelled");
    assert_eq!(close["sequence_outcome"], outcome);
    assert_eq!(close["run_ids"].to_value(), json!([run_id]));
  }
}

#[tokio::test]
async fn sequence_close_blocks_only_on_execution_not_pending_input() {
  // Python: tests/test_task_completion.py::test_close_sequence_child_blocks_on_execution_only.
  // Python: tests/test_task_sequences.py::test_withheld_iteration_launch_closes_the_child_blocked.
  let h = CompletionHarness::new();
  let task = h.base.create_task("sequence", None, TaskProfile::Worker, "Sequence").await;
  register_run(&h.base, &task.id, "run-queued").await;
  h.dispatcher
    .admit_input(&task.id, event_types::USER, "unconsumed note", ACTOR_USER, json!({"input_id":"pending-note"}))
    .await
    .unwrap();
  let evidence = evidence("[Improve loop 1] blocked", &["loop:1"], &[]);
  let blocked =
    h.completion.close_sequence_child(&task.id, "sequence-close", SequenceCloseOutcome::Blocked, &evidence).await;
  assert!(
    matches!(blocked, Err(StoreError::Conflict(blockers)) if blockers == vec!["run run-queued is queued (pending dispatch)".to_string()])
  );
  h.base.store.request_stop(&task.id, "run-queued", "sequence-close").await.unwrap();
  assert_eq!(h.dispatcher.pending_inputs(&task.id).unwrap().len(), 1);
  h.completion
    .close_sequence_child(&task.id, "sequence-close", SequenceCloseOutcome::Blocked, &evidence)
    .await
    .unwrap();
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "cancelled");
}

#[tokio::test]
async fn synthetic_sequence_controller_closes_after_its_live_iteration_finishes() {
  // Python: tests/test_task_sequences.py::test_controller_exception_while_an_iteration_runs_closes_after_its_finish.
  // Python: tests/test_task_sequences.py::test_improve_stop_mid_iteration_closes_after_the_controller_ends.
  let h = CompletionHarness::new();
  let task = h.base.create_task("sequence-live", None, TaskProfile::Worker, "Sequence").await;
  h.register_record(iteration_run(&task.id, "iteration-run")).await;
  let sequence_completion: Arc<dyn SequenceCompletion> = h.completion.clone();
  let completion_evidence = evidence("[Improve loop 1] controller failed", &["loop:1"], &["iteration-run"]);
  let evidence_json = serde_json::to_value(&completion_evidence).unwrap();

  let refused = sequence_completion.close_sequence_child(&task.id, "loop-close", "failed", &evidence_json).await;
  assert!(
    matches!(refused, Err(StoreError::Conflict(blockers)) if blockers.iter().any(|blocker| blocker.contains("iteration-run")))
  );
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "open");

  h.finish(&task.id, "iteration-run", RunOutcomeKind::Success).await;
  let (status, closed) =
    sequence_completion.close_sequence_child(&task.id, "loop-close", "failed", &evidence_json).await.unwrap();
  assert_eq!(status, 200);
  assert_eq!(h.base.store.task_state(&task.id).unwrap(), "cancelled");
  let close = h
    .base
    .store
    .load_all_facts(&task.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(closed["closed_event_id"], close["id"].to_value());
  assert_eq!(close["sequence_outcome"], "failed");
  assert_eq!(close["run_ids"].to_value(), json!(["iteration-run"]));
}

#[tokio::test]
async fn sequence_close_replays_races_repeats_and_post_reopen_calls() {
  // Python: tests/test_task_completion.py::test_close_sequence_child_replays_raced_repeated_and_post_reopen_calls.
  // Python: tests/test_task_sequences.py::test_raced_close_step_calls_land_one_close_and_one_report.
  // Python: tests/test_task_sequences.py::test_loop_end_wakes_its_parent_exactly_once_and_a_replay_never_wakes.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = h.base.create_task("child", Some(&root.id), TaskProfile::Worker, "Child").await;
  let wakes = Arc::new(AtomicUsize::new(0));
  h.dispatcher.set_executor(Some(Arc::new(WakeCountingExecutor(Arc::clone(&wakes)))));
  let completion_evidence = evidence("[Improve loop 1] blocked", &["loop:1"], &[]);
  let (first, second) = tokio::join!(
    h.completion.close_sequence_child(
      &child.id,
      "improve:1:close",
      SequenceCloseOutcome::Blocked,
      &completion_evidence,
    ),
    h.completion.close_sequence_child(
      &child.id,
      "improve:1:close",
      SequenceCloseOutcome::Blocked,
      &completion_evidence,
    ),
  );
  let first = first.unwrap();
  let second = second.unwrap();
  assert_eq!(first.1["closed_event_id"], second.1["closed_event_id"]);
  let close_count = h
    .base
    .store
    .load_all_facts(&child.id)
    .unwrap()
    .iter()
    .filter(|event| event["type"] == event_types::TASK_CLOSED)
    .count();
  let report_count = h
    .base
    .store
    .load_all_facts(&root.id)
    .unwrap()
    .iter()
    .filter(|event| event["type"] == event_types::CHILD_REPORT)
    .count();
  assert_eq!(close_count, 1);
  assert_eq!(report_count, 1);
  assert_eq!(wakes.load(Ordering::SeqCst), 1);

  let replay = h
    .completion
    .close_sequence_child(
      &child.id,
      "improve:1:close",
      SequenceCloseOutcome::Failed,
      &evidence("other", &["other"], &[]),
    )
    .await
    .unwrap();
  assert_eq!(replay.1["closed_event_id"], first.1["closed_event_id"]);
  h.completion
    .restore_task(&child.id, "reopen-1", "operator retry", &CallerIdentity::Operator { session_id: None })
    .await
    .unwrap();
  let after_reopen = h
    .completion
    .close_sequence_child(&child.id, "improve:1:close", SequenceCloseOutcome::Blocked, &completion_evidence)
    .await
    .unwrap();
  assert_eq!(after_reopen.1["closed_event_id"], first.1["closed_event_id"]);
  assert_eq!(h.base.store.task_state(&child.id).unwrap(), "open");
  assert_eq!(wakes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovered_sequence_close_report_takes_the_sequence_outcome() {
  // Python: tests/test_task_completion.py::test_recovered_sequence_close_report_takes_the_sequence_outcome.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = h.base.create_task("child", Some(&root.id), TaskProfile::Worker, "Child").await;
  let close = control_events::build_control_event(
    event_types::TASK_CLOSED,
    ACTOR_SYSTEM,
    &child.id,
    Some(&control_events::stable_close_event_id(&child.id, "loop-close")),
    None,
    [
      ("request_id".into(), json!("loop-close")),
      ("outcome".into(), json!("cancelled")),
      ("summary".into(), json!("[Improve loop 1] blocked")),
      ("result_refs".into(), json!(["loop:1"])),
      ("run_ids".into(), json!([])),
      ("report_to".into(), json!(root.id)),
      ("sequence_outcome".into(), json!("blocked")),
    ],
  );
  let guard = h.base.store.control_lock().await;
  h.base.store.append_control_fact(&guard, &child.id, close).await.unwrap();
  drop(guard);
  assert!(
    h.base.store.load_all_facts(&root.id).unwrap().iter().all(|event| event["type"] != event_types::CHILD_REPORT)
  );
  let delivered = h.dispatcher.recover_pending_reports(&child.id).await.unwrap();
  assert_eq!(delivered.len(), 1);
  assert_eq!(delivered[0]["outcome"], "blocked");
  assert_eq!(delivered[0]["summary"], "[Improve loop 1] blocked");
}

#[tokio::test]
async fn close_clears_candidates_and_reopen_opens_a_fresh_boundary() {
  // Python: tests/test_archive_unify.py::test_close_clears_candidates_and_reopen_opens_a_fresh_boundary.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  h.dispatcher
    .admit_input(&root.id, event_types::USER, "old input", ACTOR_USER, json!({"input_id":"old-input"}))
    .await
    .unwrap();
  h.completion
    .archive_subtree(&root.id, Some("archive-test"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();
  assert!(h.dispatcher.pending_inputs(&root.id).unwrap().is_empty());
  let restored = h
    .completion
    .restore_task(&root.id, "restore-root", "sidebar unarchive", &CallerIdentity::Operator { session_id: None })
    .await
    .unwrap();
  assert_eq!(restored["restored"], json!([root.id]));
  h.dispatcher
    .admit_input(&root.id, event_types::USER, "fresh input", ACTOR_USER, json!({"input_id":"fresh-input"}))
    .await
    .unwrap();
  let pending = h.dispatcher.pending_inputs(&root.id).unwrap();
  assert_eq!(pending.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(), ["fresh-input"]);
}

#[tokio::test]
async fn restore_brings_archived_ancestor_chain_and_leaves_siblings() {
  // Python: tests/test_archive_unify.py::test_restore_brings_the_archived_ancestor_chain_and_leaves_the_rest.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let middle = h.base.create_task("middle", Some(&root.id), TaskProfile::Manager, "Middle").await;
  let leaf = h.base.create_task("leaf", Some(&middle.id), TaskProfile::Worker, "Leaf").await;
  let sibling = h.base.create_task("sibling", Some(&middle.id), TaskProfile::Worker, "Sibling").await;
  h.completion
    .archive_subtree(&root.id, Some("archive-test"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();
  let restored = h
    .completion
    .restore_task(&leaf.id, "restore-chain", "sidebar unarchive", &CallerIdentity::Operator { session_id: None })
    .await
    .unwrap();
  assert_eq!(restored["restored"], json!([root.id, middle.id, leaf.id]));
  assert_eq!(h.base.store.task_state(&root.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&middle.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&leaf.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&sibling.id).unwrap(), "archived");
  for node in [&root.id, &middle.id, &leaf.id] {
    let events = h.base.store.load_all_facts(node).unwrap();
    let close_id = events.iter().find(|event| event["type"] == event_types::TASK_CLOSED).unwrap()["id"].clone();
    let reopen = events.iter().find(|event| event["type"] == event_types::TASK_REOPENED).unwrap();
    assert_eq!(reopen["reason"], "sidebar unarchive");
    assert_eq!(reopen["closed_event_id"], close_id);
  }
}

#[tokio::test]
async fn restore_of_open_node_is_noop_and_operator_only() {
  // Python: tests/test_archive_unify.py::test_restore_of_an_open_node_is_a_no_op_and_operator_only.
  let h = CompletionHarness::new();
  let task = h.base.create_task("open", None, TaskProfile::Worker, "Open").await;
  assert_eq!(
    h.completion
      .restore_task(&task.id, "no-op", "sidebar unarchive", &CallerIdentity::Operator { session_id: None })
      .await
      .unwrap()["restored"],
    json!([])
  );
  let denied = h.completion.restore_task(&task.id, "agent", "no", &agent(&task.id, "run")).await;
  assert!(
    matches!(denied, Err(StoreError::Forbidden(message)) if message == "task restore requires operator credentials")
  );
}

#[tokio::test]
async fn user_message_restores_archived_chain_and_becomes_its_only_pending_input() {
  // Python: tests/test_archive_unify.py::test_user_message_restores_and_is_the_rounds_only_input.
  // Python: tests/test_archive_unify.py::test_user_message_announces_every_restored_node.
  // Python: tests/test_archive_unify.py::test_agent_message_to_an_archived_node_is_refused_with_the_archived_sentence.
  let h = CompletionHarness::new();
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let middle = h.base.create_task("middle", Some(&root.id), TaskProfile::Manager, "Middle").await;
  let leaf = h.base.create_task("leaf", Some(&middle.id), TaskProfile::Worker, "Leaf").await;
  let old_input = h
    .dispatcher
    .admit_input(&leaf.id, event_types::USER, "old input", ACTOR_USER, json!({"input_id":"old-input"}))
    .await
    .unwrap();
  h.completion
    .archive_subtree(&root.id, Some("archive-test"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();
  let nodes = [&root.id, &middle.id, &leaf.id];
  let mut subscriptions = nodes.map(|node| h.base.store.subscribe(&charliebot_store::session_channel(node)));

  let refused = h
    .dispatcher
    .admit_input(
      &leaf.id,
      event_types::AGENT_MESSAGE,
      "agent continuation",
      ACTOR_AGENT,
      json!({"from_session": middle.id}),
    )
    .await;
  assert!(matches!(refused, Err(StoreError::Archived(_))));

  let input = h
    .dispatcher
    .admit_input(&leaf.id, event_types::USER, "continue from here", ACTOR_USER, json!({"input_id":"new-input"}))
    .await
    .unwrap();
  assert_eq!(input["id"], "new-input");
  for (node, subscription) in nodes.into_iter().zip(subscriptions.iter_mut()) {
    assert_eq!(h.base.store.task_state(node).unwrap(), "open");
    let events = h.base.store.load_all_facts(node).unwrap();
    let reopens = events.iter().filter(|event| event["type"] == event_types::TASK_REOPENED).collect::<Vec<_>>();
    assert_eq!(reopens.len(), 1);
    assert_eq!(reopens[0]["reason"], "user message");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), subscription.recv())
      .await
      .expect("reopen announcement arrives")
      .expect("session stream stays open");
    let frame: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["type"], "message");
    assert_eq!(frame["message"]["id"], reopens[0]["id"].to_value());
  }
  let pending = h.dispatcher.pending_inputs(&leaf.id).unwrap();
  assert_eq!(pending.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(), ["new-input"]);
  assert_ne!(old_input["id"], input["id"]);
}

#[tokio::test]
async fn user_message_route_restores_the_chain_and_refuses_archived_agent_input() {
  // Python: tests/test_archive_api.py::test_user_message_restores_through_the_chat_route_and_agent_is_409.
  let h = CompletionHarness::new();
  fs::write(h.base._home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
    .expect("synthetic credentials are written to test home");
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let middle = h.base.create_task("middle", Some(&root.id), TaskProfile::Manager, "Middle").await;
  let leaf = h.base.create_task("leaf", Some(&middle.id), TaskProfile::Worker, "Leaf").await;
  h.completion
    .archive_subtree(&root.id, Some("archive-test"), "archived by the user", &[], &[], &ArchiveAuthority::Operator)
    .await
    .unwrap();
  h.register_record(live_run(&middle.id, "manager-run", RunKind::ManagerTurn)).await;

  let token = run_token::sign_run_token(
    &RunTokenClaims { session_id: middle.id.clone(), run_id: "manager-run".into(), agent: "synthetic".into() },
    ACCESS_KEY,
  )
  .unwrap();
  let app = routes::routes(&Arc::clone(&h.features));
  let agent_request = Request::builder()
    .method(Method::POST)
    .uri(format!("/api/chat/{}/message", leaf.id))
    .header(header::AUTHORIZATION, format!("Bearer {token}"))
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(json!({"content":"keep going","request_id":"archived-agent-send"}).to_string()))
    .unwrap();
  let response = app.clone().oneshot(agent_request).await.unwrap();
  let status = response.status();
  let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["detail"], format!("task {} is archived", leaf.id));
  assert!(h.dispatcher.pending_inputs(&leaf.id).unwrap().is_empty());

  let user_request = Request::builder()
    .method(Method::POST)
    .uri(format!("/api/chat/{}/message", leaf.id))
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(json!({"content":"continue from here","request_id":"archived-user-send"}).to_string()))
    .unwrap();
  let response = app.oneshot(user_request).await.unwrap();
  let status = response.status();
  let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
  let body: Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(status, StatusCode::ACCEPTED);
  assert_eq!(body["status"], "accepted");
  for node in [&root.id, &middle.id, &leaf.id] {
    assert_eq!(h.base.store.task_state(node).unwrap(), "open");
    let reopens = h
      .base
      .store
      .load_all_facts(node)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"] == event_types::TASK_REOPENED)
      .collect::<Vec<_>>();
    assert_eq!(reopens.len(), 1);
    assert_eq!(reopens[0]["reason"], "user message");
  }
  let pending = h.dispatcher.pending_inputs(&leaf.id).unwrap();
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["id"].to_value(), body["input_event_id"]);
}

#[tokio::test]
async fn archive_cancel_reopen_routes_and_scope() {
  // Python: tests/test_session_dispatch.py::test_archive_cancel_reopen_routes_and_scope.
  let h = CompletionHarness::new();
  fs::write(h.base._home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
    .expect("synthetic credentials are written to test home");
  let root = h.base.create_task("root", None, TaskProfile::Manager, "Root").await;
  let worker = h.base.create_task("worker", Some(&root.id), TaskProfile::Worker, "Worker").await;
  let other = h.base.create_task("other", None, TaskProfile::Manager, "Other").await;
  h.register_record(live_run(&other.id, "agent-run", RunKind::ManagerTurn)).await;
  let token = run_token::sign_run_token(
    &RunTokenClaims { session_id: other.id.clone(), run_id: "agent-run".into(), agent: "synthetic".into() },
    ACCESS_KEY,
  )
  .unwrap();
  let app = routes::routes(&Arc::clone(&h.features));
  let call = |path: String, auth: Option<String>, body: Option<Value>| {
    let app = app.clone();
    async move {
      let mut builder = Request::builder().method(Method::POST).uri(path);
      if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {auth}"));
      }
      let request = match body {
        Some(body) => {
          builder.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap()
        }
        None => builder.body(Body::empty()).unwrap(),
      };
      let response = app.oneshot(request).await.unwrap();
      let status = response.status();
      let raw = to_bytes(response.into_body(), usize::MAX).await.unwrap();
      let value = if raw.is_empty() { Value::Null } else { serde_json::from_slice(&raw).unwrap() };
      (status, value)
    }
  };

  // An agent token may not archive a task outside its subtree without the non-child confirmation.
  let (status, body) =
    call(format!("/api/sessions/{}/archive", root.id), Some(token.clone()), Some(json!({"request_id": "wrong-task"})))
      .await;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
  assert!(body["detail"].as_str().is_some_and(|detail| detail.contains("--archive-non-child")), "{}", body["detail"]);

  // The operator archives the open subtree, descendants included.
  let (status, body) =
    call(format!("/api/sessions/{}/archive", root.id), None, Some(json!({"request_id": "archive-root"}))).await;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body, json!({"archived": [root.id.clone(), worker.id.clone()]}));
  assert_eq!(h.base.store.task_state(&root.id).unwrap(), "archived");
  assert_eq!(h.base.store.task_state(&worker.id).unwrap(), "archived");

  // Unarchive is operator action too; an agent token cannot mutate.
  let (status, _) = call(format!("/api/sessions/{}/unarchive", root.id), Some(token.clone()), None).await;
  assert_eq!(status, StatusCode::FORBIDDEN);
  let (status, body) = call(format!("/api/sessions/{}/unarchive", root.id), None, Some(json!({}))).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["restored"], json!([root.id.clone()]));
  assert_eq!(h.base.store.task_state(&root.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&worker.id).unwrap(), "archived");

  // Cancel preserves history and stays visible (an end state like archive).
  let (status, body) = call(
    format!("/api/sessions/{}/cancel", root.id),
    None,
    Some(json!({"request_id": "route-5", "reason": "not needed"})),
  )
  .await;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body["task_state"], "cancelled");
  let (status, _) =
    call(format!("/api/sessions/{}/cancel", root.id), None, Some(json!({"request_id": "route-6", "reason": "again"})))
      .await;
  assert_eq!(status, StatusCode::CONFLICT);

  // With `non_child` in the body the same token archives a task outside its subtree, and the close fact
  // names the caller.
  let extra = h.base.create_task("extra", None, TaskProfile::Manager, "Extra").await;
  let (status, body) = call(
    format!("/api/sessions/{}/archive", extra.id),
    Some(token),
    Some(json!({"request_id": "agent-non-child", "non_child": true})),
  )
  .await;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body, json!({"archived": [extra.id.clone()]}));
  let close = h
    .base
    .store
    .load_all_facts(&extra.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  assert_eq!(close["archived_by"], other.id);
}

#[tokio::test]
async fn unarchive_route_restores_the_chain_and_reports_the_ids() {
  // Python: tests/test_archive_api.py::test_unarchive_route_restores_the_chain_and_reports_the_ids.
  let h = CompletionHarness::new();
  let archived_root = h.base.create_task("archived-root", None, TaskProfile::Manager, "Archived root").await;
  let archived_branch =
    h.base.create_task("archived-branch", Some(&archived_root.id), TaskProfile::Worker, "Archived branch").await;
  let archived_sibling =
    h.base.create_task("archived-sibling", Some(&archived_root.id), TaskProfile::Worker, "Archived sibling").await;
  h.completion
    .archive_subtree(
      &archived_root.id,
      Some("archive-test"),
      "archived by the user",
      &[],
      &[],
      &ArchiveAuthority::Operator,
    )
    .await
    .unwrap();
  let restored = h.post(&format!("/api/sessions/{}/unarchive", archived_branch.id), json!({})).await;
  assert_eq!(restored, (StatusCode::OK, json!({"restored":[archived_root.id,archived_branch.id]})));
  assert_eq!(h.base.store.task_state(&archived_root.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&archived_branch.id).unwrap(), "open");
  assert_eq!(h.base.store.task_state(&archived_sibling.id).unwrap(), "archived");
}

#[tokio::test]
async fn agent_cancel_route_checks_direct_child_scope_and_unarchive_is_operator_only() {
  // Python: tests/test_task_manager_authorization.py::test_agent_cancels_only_its_own_direct_child_over_http.
  let h = CompletionHarness::new();
  fs::write(h.base._home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
    .expect("synthetic credentials are written to test home");
  let parent = h.base.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  let child = h.base.create_task("child", Some(&parent.id), TaskProfile::Worker, "Child").await;
  register_run(&h.base, &child.id, "queued-run").await;
  let other = h.base.create_task("other", None, TaskProfile::Manager, "Other").await;
  let foreign_child = h.base.create_task("foreign-child", Some(&other.id), TaskProfile::Worker, "Foreign child").await;
  h.register_record(live_run(&parent.id, "parent-run", RunKind::ManagerTurn)).await;
  h.register_record(live_run(&other.id, "other-run", RunKind::ManagerTurn)).await;
  let archived_child =
    h.base.create_task("archived-child", Some(&parent.id), TaskProfile::Worker, "Archived child").await;
  h.completion
    .archive_subtree(
      &archived_child.id,
      Some("archive-test"),
      "archived by the user",
      &[],
      &[],
      &ArchiveAuthority::Operator,
    )
    .await
    .unwrap();

  let token = |session_id: &str, run_id: &str| {
    run_token::sign_run_token(
      &RunTokenClaims { session_id: session_id.into(), run_id: run_id.into(), agent: "synthetic".into() },
      ACCESS_KEY,
    )
    .unwrap()
  };
  let app = routes::routes(&Arc::clone(&h.features));
  let call_agent = |path: String, auth: String, body: Value| {
    let app = app.clone();
    async move {
      let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {auth}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
      let response = app.oneshot(request).await.unwrap();
      let status = response.status();
      let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
      (status, serde_json::from_slice::<Value>(&body).unwrap())
    }
  };

  let accepted = call_agent(
    format!("/api/sessions/{}/cancel", child.id),
    token(&parent.id, "parent-run"),
    json!({"request_id":"agent-cancel","reason":"obsolete delegation"}),
  )
  .await;
  assert_eq!(accepted.0, StatusCode::OK);
  assert_eq!(accepted.1["task_state"], "cancelled");
  assert_eq!(accepted.1["stopped_runs"], json!([{"run_id":"queued-run","outcome":"never_launched"}]));
  let denied = call_agent(
    format!("/api/sessions/{}/cancel", foreign_child.id),
    token(&parent.id, "parent-run"),
    json!({"request_id":"agent-cancel-2","reason":"not my child"}),
  )
  .await;
  assert_eq!(denied.0, StatusCode::FORBIDDEN);
  assert_eq!(denied.1["detail"], "an agent may only cancel a direct child of its own task");
  let restore_denied =
    call_agent(format!("/api/sessions/{}/unarchive", archived_child.id), token(&parent.id, "parent-run"), json!({}))
      .await;
  assert_eq!(restore_denied.0, StatusCode::FORBIDDEN);
  assert_eq!(restore_denied.1["detail"], "unarchiving a task requires operator credentials");
}

#[tokio::test]
async fn agent_cancel_route_stops_a_live_worker_and_returns_its_outcome() {
  // Python: tests/test_task_manager_authorization.py::test_agent_run_token_cancels_live_worker_over_http.
  let h = CompletionHarness::new();
  fs::write(h.base._home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
    .expect("synthetic credentials are written to test home");
  let parent = h.base.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  h.register_record(live_run(&parent.id, "parent-run", RunKind::ManagerTurn)).await;
  let worker = h.base.create_task("worker", Some(&parent.id), TaskProfile::Worker, "Worker").await;
  let process_guard = ProcessGroupGuard::new();
  let mut command = Command::new("/bin/sleep");
  command.arg("60").process_group(0);
  let mut process = command.spawn().unwrap();
  let pid = process.id();
  process_guard.track(pid);
  let pid_start = proc_identity::read_pid_stat(pid).unwrap().pid_start;
  let mut run = new_run(&worker.id, "worker-run");
  run.pid = Some(pid);
  run.pid_start = Some(pid_start);
  run.started_at = Some(UtcDateTime::now());
  h.register_record(run).await;

  let token = run_token::sign_run_token(
    &RunTokenClaims { session_id: parent.id.clone(), run_id: "parent-run".into(), agent: "synthetic".into() },
    ACCESS_KEY,
  )
  .unwrap();
  let response = routes::routes(&Arc::clone(&h.features))
    .oneshot(
      Request::builder()
        .method(Method::POST)
        .uri(format!("/api/sessions/{}/cancel", worker.id))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"request_id":"cancel-live-worker","reason":"stop"}).to_string()))
        .unwrap(),
    )
    .await
    .unwrap();
  let status = response.status();
  let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
  let body: Value = serde_json::from_slice(&body).unwrap();

  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body["task_state"], "cancelled");
  assert_eq!(body["stopped_runs"], json!([{"run_id":"worker-run","outcome":"interrupted"}]));
  assert!(process.try_wait().unwrap().is_some());
  assert_eq!(
    terminal_outcome(&h.base.store.load_all_facts(&worker.id).unwrap(), "worker-run").unwrap(),
    Some(RunOutcomeKind::Interrupted)
  );
}

/// A running copy of the system sleep binary named `name`, whose `/proc/<pid>/exe` file name is `name`. The fake
/// session cgroup lists its pid, and the close's browser sweep reads that list.
fn sleep_stub(directory: &Path, name: &str) -> (std::process::Child, std::path::PathBuf) {
  let source = ["/bin/sleep", "/usr/bin/sleep"]
    .iter()
    .find(|path| Path::new(path).is_file())
    .expect("the sleep binary exists")
    .to_owned();
  let binary = directory.join(name);
  fs::copy(source, &binary).expect("the sleep binary copies");
  let child = Command::new(&binary).arg("300").spawn().expect("the stub starts");
  (child, binary)
}

#[tokio::test]
async fn closing_a_task_starts_the_session_browser_sweep() {
  let h = CompletionHarness::new();
  let caller = CallerIdentity::Operator { session_id: None };
  let worker = h.base.create_task("worker", None, TaskProfile::Worker, "Worker").await;
  let directory = tempfile::tempdir().unwrap();
  let base = directory.path().join("cgroup-base");
  let slice = charliebot_core::cgroup::agents_slice_dir(&base);
  let cgroup = charliebot_core::cgroup::session_cgroup_path(&slice, &worker.id);
  fs::create_dir_all(&cgroup).unwrap();
  let (mut browser, _) = sleep_stub(directory.path(), "chrome");
  let (mut bystander, _) = sleep_stub(directory.path(), "sleeper");
  fs::write(cgroup.join("cgroup.procs"), format!("{}\n{}\n", browser.id(), bystander.id())).unwrap();

  {
    let _base_guard = charliebot_core::cgroup::use_test_cgroup_base_dir(base);
    h.completion.cancel_task(&worker.id, "cancel-worker", "stop", &caller).await.unwrap();
    // The close does not wait for the spawned sweep: poll until its SIGKILL lands.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while browser.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  }
  assert!(browser.try_wait().unwrap().is_some(), "the close ended the session's chrome stub");
  assert!(bystander.try_wait().unwrap().is_none(), "the plain sleep stays");
  bystander.kill().unwrap();
  bystander.wait().unwrap();
}
