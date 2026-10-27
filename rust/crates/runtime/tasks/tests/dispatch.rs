mod dispatch_support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::event_builders::make_master_done_event;
use charliebot_core::config::BackendOption;
use charliebot_core::event_types;
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_store::metadata::{
  ColdReadSpec, SessionMetadata, SessionStatus, TaskProfile, TaskSpec, TaskType, UtcDateTime,
};
use charliebot_store::run_records::{self, RunKind, RunOutcome, RunOutcomeKind, RunRecord, SequenceRef};
use charliebot_store::{ChatEvent, CreateTask, StoreError, StoreResult, TaskCaller};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{
  DispatchExecutor, child_report_text, dispatcher, inputs_not_pending_conflict, note_run_liveness,
  unprocessed_input_blocker,
};
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::master_queue::{MasterQueue, PersistAndBroadcastFn};
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use dispatch_support::{Harness, claim, input_fields, new_run, register_run};

#[test]
fn dispatcher_requires_its_typed_startup_state() {
  let harness = Harness::new();
  assert!(Arc::ptr_eq(&dispatcher(&harness.features).unwrap(), &harness.dispatcher));
  let empty_state = FeatureContext {
    config: Arc::clone(&harness.features.config),
    credentials: Arc::clone(&harness.features.credentials),
    store: Arc::clone(&harness.store),
    registry: Arc::clone(&harness.registry),
    startup: Arc::new(StartupState::default()),
  };
  assert!(dispatcher(&empty_state).err().unwrap().to_string().contains("Dispatcher"));
}

struct DecliningExecutor;

impl DispatchExecutor for DecliningExecutor {
  fn launch<'a>(
    &'a self,
    _session_id: &'a str,
    _pending_inputs: &'a [ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async { Ok(None) })
  }
}

struct CountingDecliningExecutor(std::sync::atomic::AtomicUsize);

impl DispatchExecutor for CountingDecliningExecutor {
  fn launch<'a>(
    &'a self,
    _session_id: &'a str,
    _pending_inputs: &'a [ChatEvent],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move {
      self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      Ok(None)
    })
  }
}

#[tokio::test]
async fn admission_replays_stable_ids_and_refuses_untrusted_provenance() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;

  let fields = input_fields(&[("input_id", json!("user-1")), ("timestamp", json!("2026-01-02T00:00:00Z"))]);
  let first =
    harness.dispatcher.admit_input(&root.id, event_types::USER, "hello", "user", fields.clone()).await.unwrap();
  let replay = harness.dispatcher.admit_input(&root.id, event_types::USER, "changed", "user", fields).await.unwrap();
  assert_eq!(first, replay);
  assert_eq!(
    harness.store.load_all_facts(&root.id).unwrap().iter().filter(|event| event["type"] == event_types::USER).count(),
    1
  );

  assert!(matches!(
    harness.dispatcher.admit_input(&root.id, event_types::USER, "bad", "agent", json!({})).await,
    Err(StoreError::Forbidden(message)) if message == "only verified operator input is a real user message"
  ));
  assert!(matches!(
    harness
      .dispatcher
      .admit_input(&root.id, event_types::SCHEDULED_TRIGGER, "bad", "agent", json!({}))
      .await,
    Err(StoreError::Forbidden(message)) if message == "only the server's own scheduler mints scheduled triggers"
  ));
  assert!(matches!(
    harness
      .dispatcher
      .admit_input(
        &root.id,
        event_types::AGENT_MESSAGE,
        "bad",
        "agent",
        json!({"input_mode":"voice"})
      )
      .await,
    Err(StoreError::Invalid(message)) if message == "input_mode applies to a user message, not to agent_message"
  ));
}

#[tokio::test]
async fn worker_agent_messages_require_parent_identity_and_authorization() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let worker = harness.create_task("worker", Some(&root.id), TaskProfile::Worker, "Worker").await;

  let parent_fields = json!({"from_session":root.id,"from_session_name":"Root"});
  assert!(matches!(
    harness
      .dispatcher
      .admit_input(&worker.id, event_types::AGENT_MESSAGE, "work", "agent", parent_fields.clone())
      .await,
    Err(StoreError::Forbidden(_))
  ));
  harness.dispatcher.admit_input(&root.id, event_types::USER, "authorize", "user", json!({})).await.unwrap();
  assert!(matches!(
    harness
      .dispatcher
      .admit_input(
        &worker.id,
        event_types::AGENT_MESSAGE,
        "wrong sender",
        "agent",
        json!({"from_session":"outsider"})
      )
      .await,
    Err(StoreError::Forbidden(message)) if message.contains("must come from its parent task")
  ));
  harness.dispatcher.admit_input(&worker.id, event_types::AGENT_MESSAGE, "work", "agent", parent_fields).await.unwrap();
  assert_eq!(harness.dispatcher.pending_inputs(&worker.id).unwrap().len(), 1);
}

/// `test_artifact_check.py::test_agent_message_to_a_read_only_child_passes_without_takeoff`: a parent's agent message
/// reaches a verify or cold-read child with no user message anywhere, and the same message to an implement child is
/// refused without that takeoff.
#[tokio::test]
async fn agent_message_to_a_read_only_child_passes_without_takeoff() {
  // Python: tests/core/test_artifact_check.py::test_agent_message_to_a_read_only_child_passes_without_takeoff.
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let parent_fields = json!({"from_session": root.id, "from_session_name": "Root"});
  for task_type in [TaskType::Verify, TaskType::ColdRead] {
    let child = typed_child(&harness, &root.id, task_type).await;
    let event = harness
      .dispatcher
      .admit_input(&child.id, event_types::AGENT_MESSAGE, "a note from the parent", "agent", parent_fields.clone())
      .await
      .unwrap();
    assert_eq!(event["content"], "a note from the parent");
  }

  // The control: an implement child refuses the same message without takeoff.
  let implement = typed_child(&harness, &root.id, TaskType::Implement).await;
  assert!(matches!(
    harness.dispatcher.admit_input(&implement.id, event_types::AGENT_MESSAGE, "x", "agent", parent_fields).await,
    Err(StoreError::Forbidden(_))
  ));
}

/// A worker child of `parent` whose task record names `task_type`; a cold-read child names a page too.
async fn typed_child(harness: &Harness, parent: &str, task_type: TaskType) -> SessionMetadata {
  let cold_read = (task_type == TaskType::ColdRead).then(|| ColdReadSpec {
    page_path: "/synthetic/plan.html".to_string(),
    genre: "plan".to_string(),
    trigger: "t".to_string(),
  });
  let task = TaskSpec { goal: "check it".to_string(), task_type: Some(task_type), cold_read, ..TaskSpec::default() };
  harness
    .store
    .create_task(
      CreateTask {
        request_id: format!("child-{task_type:?}"),
        task_parent_id: Some(parent.to_string()),
        profile: TaskProfile::Worker,
        task: Some(task),
        name: Some("Child".to_string()),
        backend: None,
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn claims_are_durable_and_only_successful_owner_finish_acknowledges() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  let admitted = harness
    .dispatcher
    .admit_input(&task.id, event_types::USER, "batch", "user", json!({"input_id":"batch-1"}))
    .await
    .unwrap();
  let input_id = admitted["id"].as_str().unwrap().to_string();
  register_run(&harness, &task.id, "run-success").await;
  assert_eq!(claim(&harness, &task.id, "run-success").await.as_slice(), std::slice::from_ref(&input_id));
  assert!(harness.dispatcher.pending_inputs(&task.id).unwrap().is_empty());
  harness.dispatcher.finish_run(&task.id, "run-success", RunOutcome::new(RunOutcomeKind::Success)).await.unwrap();
  assert_eq!(harness.completion.after_finish_count.load(std::sync::atomic::Ordering::Relaxed), 1);
  assert!(harness.store.facts_of(&task.id).unwrap().confirmed_input_ids.contains(&input_id));

  harness
    .dispatcher
    .admit_input(&task.id, event_types::USER, "failed batch", "user", json!({"input_id":"batch-2"}))
    .await
    .unwrap();
  register_run(&harness, &task.id, "run-failed").await;
  let failed_id = claim(&harness, &task.id, "run-failed").await[0].clone();
  harness.dispatcher.finish_run(&task.id, "run-failed", RunOutcome::new(RunOutcomeKind::Failed)).await.unwrap();
  assert_eq!(harness.completion.after_finish_count.load(std::sync::atomic::Ordering::Relaxed), 1);
  let facts = harness.store.facts_of(&task.id).unwrap();
  assert!(!facts.confirmed_input_ids.contains(&failed_id));
  assert!(!harness.dispatcher.pending_inputs(&task.id).unwrap().iter().any(|event| event["id"] == failed_id));
}

#[tokio::test]
async fn blocked_post_finish_completion_does_not_undo_the_durable_success() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  register_run(&harness, &task.id, "run-success").await;
  harness.completion.return_conflict.store(true, std::sync::atomic::Ordering::Relaxed);

  harness.dispatcher.finish_run(&task.id, "run-success", RunOutcome::new(RunOutcomeKind::Success)).await.unwrap();

  assert_eq!(
    run_records::terminal_outcome(&harness.store.load_all_facts(&task.id).unwrap(), "run-success").unwrap(),
    Some(RunOutcomeKind::Success)
  );
  assert_eq!(harness.completion.after_finish_count.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
async fn batchless_finish_cannot_acknowledge_another_runs_claim() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  harness
    .dispatcher
    .admit_input(&task.id, event_types::USER, "owned", "user", json!({"input_id":"owned-1"}))
    .await
    .unwrap();
  register_run(&harness, &task.id, "run-owner").await;
  assert_eq!(claim(&harness, &task.id, "run-owner").await, ["owned-1"]);
  register_run(&harness, &task.id, "run-foreign").await;
  let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
  outcome.input_event_ids.push("owned-1".to_string());
  let error = harness.dispatcher.finish_run(&task.id, "run-foreign", outcome).await.unwrap_err();
  assert!(
    matches!(error, StoreError::Conflict(ref blockers) if blockers == &[inputs_not_pending_conflict(&task.id, &["owned-1".to_string()])])
  );
  assert!(
    run_records::terminal_outcome(&harness.store.load_all_facts(&task.id).unwrap(), "run-foreign").unwrap().is_none()
  );
}

#[tokio::test]
async fn stopped_queued_run_releases_its_claim_and_is_not_launched() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  harness
    .dispatcher
    .admit_input(&task.id, event_types::USER, "retry me", "user", json!({"input_id":"retry-1"}))
    .await
    .unwrap();
  register_run(&harness, &task.id, "run-stopped").await;
  assert_eq!(claim(&harness, &task.id, "run-stopped").await, ["retry-1"]);
  harness.stop_run(&task.id, "run-stopped", "stop-request-1").await;
  let claim_error = {
    let guard = harness.store.control_lock().await;
    harness.dispatcher.claim_input_batch_locked(&guard, &task.id, "run-stopped", None).unwrap_err()
  };
  assert!(matches!(
    claim_error,
    StoreError::Conflict(ref blockers)
      if blockers == &["run run-stopped has a durable stop request; it never launches"]
  ));
  harness.install_executor(vec![RunOutcomeKind::Success]);

  let decision = harness.dispatcher.dispatch_pending(&task.id).await.unwrap();
  assert_eq!(decision["launch"], true);
  assert_eq!(decision["run_id"], "run-1");
  assert!(
    run_records::terminal_outcome(&harness.store.load_all_facts(&task.id).unwrap(), "run-stopped").unwrap().is_none()
  );
  assert!(harness.store.facts_of(&task.id).unwrap().confirmed_input_ids.contains("retry-1"));
}

#[tokio::test]
async fn dispatcher_never_launches_a_queued_sequence_run() {
  // Python: tests/test_sequence_controller_runs.py::test_dispatcher_never_launches_a_queued_sequence_run.
  let harness = Harness::new();
  let manager = harness.create_task("root", None, TaskProfile::Manager, "PM").await;
  let leaf = harness.create_task("leaf", Some(&manager.id), TaskProfile::Worker, "leaf").await;
  // The owning package is gone: no registered controller claims this kind.
  let run = RunRecord {
    kind: RunKind::Iteration,
    sequence_ref: Some(SequenceRef {
      kind: "deleted_feature".to_string(),
      owner_ref: "deleted:loop-1".to_string(),
      position: 1,
    }),
    ..new_run(&leaf.id, "seq-run")
  };
  {
    let guard = harness.store.control_lock().await;
    harness.store.register_run_locked(&guard, &leaf.id, run, None).await.expect("queued run registers");
  }
  // The executor has no scripted outcome, so a launch would fail the dispatch instead of passing silently.
  harness.install_executor(Vec::new());

  let decision = harness.dispatcher.dispatch_pending(&leaf.id).await.unwrap();

  assert_eq!(decision["launch"], false);
  let stored = harness.store.read_run_sync(&leaf.id, "seq-run").unwrap().expect("the queued run stays registered");
  assert_eq!(stored.pid, None, "nothing started");
  let events = harness.store.load_all_facts(&leaf.id).unwrap();
  assert!(run_records::terminal_outcome(&events, "seq-run").unwrap().is_none(), "nothing landed");
}

#[tokio::test]
async fn reports_are_stable_recovered_and_wake_the_parent_dispatcher() {
  let harness = Harness::new();
  let parent = harness.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  let child = harness.create_task("child", Some(&parent.id), TaskProfile::Worker, "Child").await;
  harness.close_task(&child.id, "close-child", Some(&parent.id)).await;
  assert_eq!(harness.dispatcher.undelivered_report_blockers(&child.id).unwrap().len(), 1);
  let wake_count = Arc::new(CountingDecliningExecutor(std::sync::atomic::AtomicUsize::new(0)));
  harness.dispatcher.set_executor(Some(wake_count.clone()));
  let reports = harness.dispatcher.recover_pending_reports(&child.id).await.unwrap();
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["child_session_id"], child.id);
  assert_eq!(wake_count.0.load(std::sync::atomic::Ordering::Relaxed), 1);
  assert!(harness.dispatcher.undelivered_report_blockers(&child.id).unwrap().is_empty());
  assert!(
    harness
      .dispatcher
      .pending_inputs(&parent.id)
      .unwrap()
      .iter()
      .any(|event| event["type"] == event_types::CHILD_REPORT)
  );
  assert!(harness.dispatcher.recover_pending_reports(&child.id).await.unwrap().is_empty());
  assert_eq!(wake_count.0.load(std::sync::atomic::Ordering::Relaxed), 1);
  assert_eq!(harness.output_for(&[("parent", &parent.id)]).as_array().unwrap().len(), 1);

  let report = &reports[0];
  assert_eq!(
    child_report_text(&report.to_value(), Some("clone of source")),
    format!("[Report from task {} (clone of source) | outcome completed] synthetic close", child.id)
  );
  assert_eq!(
    unprocessed_input_blocker(std::slice::from_ref(report)),
    format!("has unprocessed input: {}", report["id"].as_str().unwrap())
  );
}

#[tokio::test]
async fn elone_redelivers_a_subtree_close_report_before_the_source_closes() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let source = harness.create_task("source", Some(&root.id), TaskProfile::Manager, "Feature").await;
  let child = harness.create_task("child", Some(&source.id), TaskProfile::Worker, "Child").await;
  // The crash window: the child's close fact is durable and its report is not delivered yet.
  harness.close_task(&child.id, "close-child", Some(&source.id)).await;
  assert_eq!(harness.dispatcher.undelivered_report_blockers(&child.id).unwrap().len(), 1);

  let completion = TaskCompletion::new(Arc::clone(&harness.features));
  harness.store.elone_session(&source.id, 0, None, &*harness.dispatcher, &completion).await.unwrap();

  let facts = harness.store.load_all_facts(&source.id).unwrap();
  let reports: Vec<usize> =
    facts.iter().enumerate().filter(|(_, e)| e["type"] == event_types::CHILD_REPORT).map(|(i, _)| i).collect();
  let closes: Vec<usize> =
    facts.iter().enumerate().filter(|(_, e)| e["type"] == event_types::TASK_CLOSED).map(|(i, _)| i).collect();
  assert_eq!((reports.len(), closes.len()), (1, 1));
  assert!(reports[0] < closes[0]);
  assert_eq!(facts[reports[0]]["child_session_id"], child.id);
  assert_eq!(facts[closes[0]]["outcome"], "archived");
  assert!(harness.dispatcher.undelivered_report_blockers(&child.id).unwrap().is_empty());
}

#[tokio::test]
async fn acknowledging_one_child_report_leaves_the_later_report_pending_after_reload() {
  let harness = Harness::new();
  let parent = harness.create_task("parent", None, TaskProfile::Manager, "Parent").await;
  let child_a = harness.create_task("child-a", Some(&parent.id), TaskProfile::Worker, "Child A").await;
  let child_b = harness.create_task("child-b", Some(&parent.id), TaskProfile::Worker, "Child B").await;
  harness.close_task(&child_a.id, "close-a", Some(&parent.id)).await;
  harness.dispatcher.recover_pending_reports(&child_a.id).await.unwrap();
  let first_report = harness.dispatcher.pending_inputs(&parent.id).unwrap()[0].clone();

  harness.install_executor(vec![RunOutcomeKind::Success]);
  let decision = harness.dispatcher.dispatch_pending(&parent.id).await.unwrap();
  assert_eq!(decision["launch"], true);
  let acknowledged = harness
    .store
    .load_all_facts(&parent.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::RUN_FINISHED && event["outcome"] == "success")
    .unwrap();
  assert_eq!(acknowledged["input_event_ids"].to_value(), json!([first_report["id"].to_value()]));

  harness.dispatcher.set_executor(None);
  harness.close_task(&child_b.id, "close-b", Some(&parent.id)).await;
  harness.dispatcher.recover_pending_reports(&child_b.id).await.unwrap();

  let reloaded = charliebot_tasks::dispatch::Dispatcher::new(
    Arc::clone(&harness.store),
    Arc::clone(&harness.features),
    harness.completion.clone(),
    harness.completion.clone(),
  );
  let pending = reloaded.pending_inputs(&parent.id).unwrap();
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["child_session_id"], child_b.id);
}

#[tokio::test]
async fn child_report_provenance_and_source_fallback_follow_the_history_parent() {
  let harness = Harness::new();
  let source = harness.create_task("source", None, TaskProfile::Manager, "Source").await;
  let child = harness.create_task("history-child", None, TaskProfile::Manager, "History child").await;
  harness.set_history_parent(&child.id, &source.id).await;
  assert_eq!(
    harness.dispatcher.child_report_provenance(&child.id).await.unwrap(),
    Some(format!("clone of {}", source.id))
  );
  let created = harness.dispatcher.report_source_event(&child.id, "worker").unwrap();
  assert_eq!(created["type"], event_types::TASK_CREATED);
  let reports: Vec<ChatEvent> = vec![
    json!({"type":event_types::CHILD_REPORT,"child_session_id":child.id}).into(),
    json!({"type":event_types::CHILD_REPORT,"child_session_id":child.id}).into(),
  ];
  assert_eq!(harness.dispatcher.child_report_provenances(&reports).await.unwrap().len(), 1);

  let (report, created) =
    harness.dispatcher.deliver_child_report(&child.id, created, "completed", "", None, None).await.unwrap();
  assert!(report.is_null());
  assert!(!created);

  harness.set_successor(&source.id, &child.id).await;
  assert_eq!(
    harness.dispatcher.child_report_provenance(&child.id).await.unwrap(),
    Some(format!("elone of {}", source.id))
  );
}

#[tokio::test]
async fn trigger_master_redirects_to_successor_and_pulls_back_archived_node() {
  let harness = Harness::new();
  let source = harness.create_task("source", None, TaskProfile::Manager, "Source").await;
  let successor = harness.create_task("successor", None, TaskProfile::Manager, "Successor").await;
  harness.set_successor(&source.id, &successor.id).await;
  trigger_master(&harness.features, TriggerMasterRequest::new(&source.id, "wake successor")).await.unwrap();
  let successor_inputs = harness.dispatcher.pending_inputs(&successor.id).unwrap();
  assert_eq!(successor_inputs.len(), 1);
  assert_eq!(successor_inputs[0]["from_session"], source.id);
  assert_eq!(successor_inputs[0]["from_session_name"], "Source");

  let archived = harness.create_task("archived", None, TaskProfile::Manager, "Archived").await;
  harness.archive_task(&archived.id).await;
  trigger_master(&harness.features, TriggerMasterRequest::new(&archived.id, "pull back")).await.unwrap();
  assert_eq!(harness.store.task_state(&archived.id).unwrap(), "open");
  assert_eq!(harness.dispatcher.pending_inputs(&archived.id).unwrap().len(), 1);
}

#[tokio::test]
async fn trigger_master_dispatches_an_existing_stable_input_once() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  harness
    .dispatcher
    .admit_input(
      &task.id,
      event_types::AGENT_MESSAGE,
      "summon prompt",
      "agent",
      json!({"input_id":"existing-wake","from_session":task.id,"from_session_name":"Synthetic"}),
    )
    .await
    .unwrap();
  harness.install_executor(vec![RunOutcomeKind::Success]);
  let mut request = TriggerMasterRequest::new(&task.id, "summon prompt");
  request.input_id = Some("existing-wake");
  request.from_session = Some(&task.id);
  request.from_session_name = Some("Synthetic");

  trigger_master(&harness.features, request).await.unwrap();

  let events = harness.store.load_all_facts(&task.id).unwrap();
  assert_eq!(events.iter().filter(|event| event["id"] == "existing-wake").count(), 1);
  let finished = events.iter().rfind(|event| event["type"] == event_types::RUN_FINISHED).unwrap();
  assert_eq!(finished["input_event_ids"].to_value(), json!(["existing-wake"]));
}

#[tokio::test]
async fn dispatch_without_executor_returns_python_reason_and_liveness_clears_run() {
  let harness = Harness::new();
  let worker = harness.create_task("worker", None, TaskProfile::Worker, "Worker").await;
  harness.dispatcher.admit_input(&worker.id, event_types::USER, "work", "user", json!({})).await.unwrap();
  let decision = harness.dispatcher.dispatch_pending(&worker.id).await.unwrap();
  assert_eq!(decision["launch"], false);
  assert_eq!(decision["reason"], "execution adapters register in the next stage");
  let mut run = new_run(&worker.id, "run-live");
  run.started_at = Some(charliebot_store::metadata::UtcDateTime::parse("2026-01-02T00:00:00Z").unwrap());
  note_run_liveness(&harness.store, &worker.id, &run, true).await;
  assert_eq!(harness.store.busy_since(&worker.id), run.started_at);
  note_run_liveness(&harness.store, &worker.id, &run, false).await;
  assert_eq!(harness.store.busy_since(&worker.id), None);

  run.started_at = None;
  note_run_liveness(&harness.store, &worker.id, &run, true).await;
  assert!(harness.store.busy_since(&worker.id).is_some());
}

#[tokio::test]
async fn dispatch_decisions_report_empty_queued_and_lost_reservation_reasons() {
  let harness = Harness::new();
  let task = harness.create_task("task", None, TaskProfile::Manager, "Task").await;
  let empty = harness.dispatcher.dispatch_pending(&task.id).await.unwrap();
  assert_eq!(empty["launch"], false);
  assert_eq!(empty["reason"], "no pending inputs");

  harness
    .dispatcher
    .admit_input(&task.id, event_types::USER, "queued work", "user", json!({"input_id":"queued-input"}))
    .await
    .unwrap();
  register_run(&harness, &task.id, "run-queued").await;
  let pending_executor = harness.dispatcher.dispatch_pending(&task.id).await.unwrap();
  assert_eq!(pending_executor["reason"], "execution adapters register in the next stage");

  harness.dispatcher.set_executor(Some(Arc::new(DecliningExecutor)));
  let reservation_lost = harness.dispatcher.dispatch_pending(&task.id).await.unwrap();
  assert_eq!(reservation_lost["reason"], "queued run run-queued was not launched by this call");
}

#[tokio::test]
async fn machine_input_to_a_closed_node_is_archived_and_does_not_dispatch() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let worker = harness.create_task("worker", Some(&root.id), TaskProfile::Worker, "Worker").await;
  harness
    .dispatcher
    .admit_input(&worker.id, event_types::USER, "retain this", "user", json!({"input_id":"before-close"}))
    .await
    .unwrap();
  harness.close_task(&worker.id, "close-worker", None).await;

  assert!(matches!(
    harness
      .dispatcher
      .admit_input(
        &worker.id,
        event_types::AGENT_MESSAGE,
        "late machine input",
        "agent",
        json!({"from_session":root.id})
      )
      .await,
    Err(StoreError::Archived(id)) if id == worker.id
  ));
  let decision = harness.dispatcher.dispatch_pending(&worker.id).await.unwrap();
  assert_eq!(decision["launch"], false);
  assert_eq!(decision["reason"], "task is closed; input retained as history");
  assert!(harness.dispatcher.pending_inputs(&worker.id).unwrap().is_empty());
}

/// Parse one synthetic timestamp.
fn ts(value: &str) -> UtcDateTime {
  UtcDateTime::parse(value).expect("synthetic timestamp parses")
}

/// The one node's current sidebar sort key.
async fn updated_at_of(harness: &Harness, session_id: &str) -> UtcDateTime {
  harness.store.get_session(session_id).await.unwrap().expect("session exists").updated_at
}

/// The listing's session ids, newest first.
fn listing_ids(rows: &[SessionMetadata]) -> Vec<String> {
  rows.iter().map(|row| row.id.clone()).collect()
}

/// `tests/test_session_dispatch.py::test_user_admission_lifts_only_the_target_node`: a real user message lifts the
/// node it was sent to, forward only; its ancestors keep their places, and a message to an archived node reopens
/// the chain and lifts only the reopened target.
#[tokio::test]
async fn user_admission_lifts_only_the_target_node() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let middle = harness.create_task("mid", Some(&root.id), TaskProfile::Manager, "Mid").await;
  let child = harness.create_task("child", Some(&middle.id), TaskProfile::Manager, "Child").await;
  let root_at = updated_at_of(&harness, &root.id).await;
  let middle_at = updated_at_of(&harness, &middle.id).await;
  let first = ts("2030-01-01T01:00:00Z");
  let second = ts("2030-01-01T02:00:00Z");

  let admit = |event_type: &'static str, actor: &'static str, content: &'static str, fields: Value| {
    let dispatcher = Arc::clone(&harness.dispatcher);
    let session_id = child.id.clone();
    async move { dispatcher.admit_input(&session_id, event_type, content, actor, fields).await }
  };
  admit(event_types::USER, "user", "my turn", json!({"input_id":"m1","timestamp":first.to_python_isoformat()}))
    .await
    .unwrap();
  assert_eq!(updated_at_of(&harness, &child.id).await, first);
  assert_eq!(updated_at_of(&harness, &middle.id).await, middle_at);
  assert_eq!(updated_at_of(&harness, &root.id).await, root_at);

  // A replayed input_id returns the original event and lifts nothing; a new message lifts the target again.
  admit(event_types::USER, "user", "my turn", json!({"input_id":"m1","timestamp":second.to_python_isoformat()}))
    .await
    .unwrap();
  assert_eq!(updated_at_of(&harness, &child.id).await, first);
  admit(event_types::USER, "user", "again", json!({"input_id":"m2","timestamp":second.to_python_isoformat()}))
    .await
    .unwrap();
  assert_eq!(updated_at_of(&harness, &child.id).await, second);
  assert_eq!(updated_at_of(&harness, &middle.id).await, middle_at);
  assert_eq!(updated_at_of(&harness, &root.id).await, root_at);

  // Agent relays, scheduled triggers and child reports are machine traffic: none of them lifts, however fresh.
  for (event_type, actor) in [
    (event_types::AGENT_MESSAGE, "agent"),
    (event_types::SCHEDULED_TRIGGER, "system"),
    (event_types::CHILD_REPORT, "system"),
  ] {
    admit(event_type, actor, "machine traffic", json!({"timestamp":"2030-01-01T03:00:00Z"})).await.unwrap();
  }
  assert_eq!(updated_at_of(&harness, &child.id).await, second);
  assert_eq!(updated_at_of(&harness, &middle.id).await, middle_at);
  assert_eq!(updated_at_of(&harness, &root.id).await, root_at);

  // A message to the root lifts the root and nothing below it.
  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "to the root", "user", json!({"timestamp":"2030-01-01T05:00:00Z"}))
    .await
    .unwrap();
  assert_eq!(updated_at_of(&harness, &root.id).await, ts("2030-01-01T05:00:00Z"));
  assert_eq!(updated_at_of(&harness, &middle.id).await, middle_at);
  assert_eq!(updated_at_of(&harness, &child.id).await, second);

  // Archiving the middle node archives the leaf beneath it. A message to the leaf reopens the archived ancestor
  // and the leaf, and lifts only the leaf.
  let archived_root = harness.create_task("archived-root", None, TaskProfile::Manager, "Archived root").await;
  let archived_middle =
    harness.create_task("archived-middle", Some(&archived_root.id), TaskProfile::Manager, "Archived middle").await;
  let archived_child =
    harness.create_task("archived-child", Some(&archived_middle.id), TaskProfile::Manager, "Archived child").await;
  let archived = TaskCompletion::new(Arc::clone(&harness.features))
    .archive_subtree(
      &archived_middle.id,
      Some("archive-amid"),
      "archived",
      &[],
      &[],
      &CallerIdentity::Operator { session_id: None },
    )
    .await
    .unwrap();
  assert_eq!(archived, [archived_middle.id.clone(), archived_child.id.clone()]);
  assert_eq!(harness.store.task_state(&archived_child.id).unwrap(), "archived");
  let archived_middle_at = updated_at_of(&harness, &archived_middle.id).await;
  let archived_root_at = updated_at_of(&harness, &archived_root.id).await;
  let later = ts("2031-01-01T03:00:00Z");
  harness
    .dispatcher
    .admit_input(
      &archived_child.id,
      event_types::USER,
      "to the archived branch",
      "user",
      json!({"timestamp":later.to_python_isoformat()}),
    )
    .await
    .unwrap();
  assert_eq!(harness.store.task_state(&archived_child.id).unwrap(), "open");
  assert_eq!(updated_at_of(&harness, &archived_child.id).await, later);
  assert_eq!(updated_at_of(&harness, &archived_middle.id).await, archived_middle_at);
  assert_eq!(updated_at_of(&harness, &archived_root.id).await, archived_root_at);
}

/// `tests/test_session_dispatch.py::test_a_user_message_lift_is_stored_before_its_tree_changed_broadcast`: the
/// append broadcasts `task_tree_changed`, so the lift must be durable by the time that append runs. The harness's
/// anchor reconciler observes the metadata save itself: at that point the user event is still absent from the log.
#[tokio::test]
async fn a_user_message_lift_is_stored_before_its_tree_changed_broadcast() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let child = harness.create_task("child", Some(&root.id), TaskProfile::Manager, "Child").await;
  let when = ts("2030-02-02T02:00:00Z");
  let store = Arc::clone(&harness.store);
  let observed: Arc<Mutex<Vec<(UtcDateTime, bool)>>> = Arc::new(Mutex::new(Vec::new()));
  let sink = Arc::clone(&observed);
  harness.hooks.set_session_save_observer(Some(Arc::new(move |meta: &SessionMetadata| {
    let appended = store
      .load_all_facts(&meta.id)
      .expect("the session's facts read")
      .iter()
      .any(|event| event["type"] == event_types::USER);
    sink.lock().expect("the observation lock is not poisoned").push((meta.updated_at, appended));
  })));

  harness
    .dispatcher
    .admit_input(&child.id, event_types::USER, "my turn", "user", json!({"timestamp":when.to_python_isoformat()}))
    .await
    .unwrap();

  assert_eq!(
    *observed.lock().expect("the observation lock is not poisoned"),
    vec![(when, false)],
    "the lift must be stored before the message is appended, so the append's refetch reads it"
  );
}

/// `tests/test_session_dispatch.py::test_a_messaged_child_lists_ahead_of_its_siblings_and_roots_keep_their_order`:
/// a user message moves only the row it was sent to; the siblings and the root rows keep their relative order.
#[tokio::test]
async fn a_messaged_child_lists_ahead_of_its_siblings_and_roots_keep_their_order() {
  let harness = Harness::new();
  let r1 = harness.create_task("r1", None, TaskProfile::Manager, "R1").await;
  let r2 = harness.create_task("r2", None, TaskProfile::Manager, "R2").await;
  let c1 = harness.create_task("c1", Some(&r1.id), TaskProfile::Manager, "C1").await;
  let c2 = harness.create_task("c2", Some(&r1.id), TaskProfile::Manager, "C2").await;
  let c3 = harness.create_task("c3", Some(&r1.id), TaskProfile::Manager, "C3").await;
  // Pin each sort key, oldest first, so the order comes from the test's times.
  for (seconds, node) in [(1, &c1), (2, &c2), (3, &c3), (4, &r1), (5, &r2)] {
    harness.store.update_thinking_state(&node.id, ts(&format!("2032-05-01T00:00:0{seconds}Z"))).await.unwrap();
  }
  let listing = harness.store.list_sessions_readonly(SessionStatus::Active).unwrap();
  assert_eq!(listing_ids(&listing), [r2.id.clone(), r1.id.clone(), c3.id.clone(), c2.id.clone(), c1.id.clone()]);

  harness
    .dispatcher
    .admit_input(&c1.id, event_types::USER, "my turn", "user", json!({"timestamp":"2032-05-01T00:00:10Z"}))
    .await
    .unwrap();
  let listing = harness.store.list_sessions_readonly(SessionStatus::Active).unwrap();
  assert_eq!(listing_ids(&listing), [c1.id.clone(), r2.id.clone(), r1.id.clone(), c3.id.clone(), c2.id.clone()]);
}

#[tokio::test]
async fn user_message_lifts_a_node_ahead_of_one_that_only_received_a_scheduled_trigger() {
  let harness = Harness::new();
  let messaged = harness.create_task("messaged", None, TaskProfile::Manager, "Messaged").await;
  let scheduled = harness.create_task("scheduled", None, TaskProfile::Manager, "Scheduled").await;
  let base = "2032-04-05T00:00:00Z";
  harness
    .store
    .update_thinking_state(
      &messaged.id,
      charliebot_store::metadata::UtcDateTime::parse("2032-04-04T22:00:00Z").unwrap(),
    )
    .await
    .unwrap();
  harness
    .store
    .update_thinking_state(
      &scheduled.id,
      charliebot_store::metadata::UtcDateTime::parse("2032-04-04T23:00:00Z").unwrap(),
    )
    .await
    .unwrap();

  harness
    .dispatcher
    .admit_input(&scheduled.id, event_types::SCHEDULED_TRIGGER, "nightly fire", "system", json!({"timestamp":base}))
    .await
    .unwrap();
  harness
    .dispatcher
    .admit_input(&messaged.id, event_types::USER, "my turn", "user", json!({"timestamp":"2032-04-04T23:30:00Z"}))
    .await
    .unwrap();

  let listing = harness.store.list_sessions_readonly(charliebot_store::metadata::SessionStatus::Active).unwrap();
  assert_eq!(listing[0].id, messaged.id);
  assert_eq!(listing[1].id, scheduled.id);
}

#[tokio::test]
async fn a_finished_manager_turn_persists_its_observation_before_master_done_and_run_finished() {
  let harness = Harness::new();
  let task = harness.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  register_run(&harness, &task.id, "run-turn").await;
  let queue = Arc::new(MasterQueue::new(Arc::new(BackendPool::default())));
  let adapter = TaskExecutionAdapter::new(Arc::clone(&harness.features), queue, Arc::clone(&harness.dispatcher));
  let option = BackendOption {
    id: "synthetic-backend".to_string(),
    label: "Synthetic backend".to_string(),
    model: "synthetic-model".to_string(),
    prompt_overlay: None,
    backend_type: "synthetic".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  };
  // The run's raw log is set by the observation, so each master_done records whether it came after it.
  let raw_log_at_master_done = Arc::new(Mutex::new(Vec::new()));
  let persist_done: PersistAndBroadcastFn = {
    let store = Arc::clone(&harness.store);
    let raw_log_at_master_done = Arc::clone(&raw_log_at_master_done);
    Arc::new(move |session_id: String, event: Value| {
      let store = Arc::clone(&store);
      let raw_log_at_master_done = Arc::clone(&raw_log_at_master_done);
      Box::pin(async move {
        let run = store.read_run_sync(&session_id, "run-turn")?.expect("the run is registered");
        raw_log_at_master_done.lock().unwrap().push(run.raw_log_ref);
        store.persist_and_broadcast(&session_id, event).await?;
        Ok(())
      })
    })
  };
  let on_finish =
    adapter.manager_finish_recorder(&task, "run-turn", &option, PathBuf::from("raw-log.jsonl"), None, persist_done);

  let handled =
    on_finish(Some("native-session".to_string()), 0, Map::new(), make_master_done_event(0, false)).await.unwrap();

  assert!(handled);
  assert_eq!(*raw_log_at_master_done.lock().unwrap(), [Some("raw-log.jsonl".to_string())]);
  let order = harness
    .store
    .load_all_facts(&task.id)
    .unwrap()
    .iter()
    .filter_map(|event| event["type"].as_str())
    .filter(|kind| *kind == "master_done" || *kind == "run_finished")
    .map(str::to_string)
    .collect::<Vec<_>>();
  assert_eq!(order, ["master_done", "run_finished"]);
  let run = harness.store.read_run_sync(&task.id, "run-turn").unwrap().expect("the run is registered");
  assert_eq!(run.raw_log_ref.as_deref(), Some("raw-log.jsonl"));
}

#[test]
fn input_blocker_truncates_after_eight_ids() {
  let values = (0..10).map(|index| json!({"id":format!("in-{index}")}).into()).collect::<Vec<ChatEvent>>();
  assert_eq!(
    unprocessed_input_blocker(&values),
    "has unprocessed input: in-0, in-1, in-2, in-3, in-4, in-5, in-6, in-7 (+2 more)"
  );
}

// Python: tests/test_task_execution.py::test_manager_finish_claims_only_echoed_pending_user_ids
#[tokio::test]
async fn manager_finish_claims_only_echoed_pending_user_ids() {
  let harness = Harness::new();
  let manager = harness.create_task("manager", None, TaskProfile::Manager, "Manager").await;
  let initial = harness
    .dispatcher
    .admit_input(&manager.id, event_types::USER, "Initial", "user", json!({"input_id":"initial"}))
    .await
    .unwrap();
  let run_id = "42400111-2222-3333-4444-555555555555";
  let guard = harness.store.control_lock().await;
  harness
    .store
    .register_run_locked(
      &guard,
      &manager.id,
      RunRecord { kind: RunKind::ManagerTurn, ..dispatch_support::new_run(&manager.id, run_id) },
      None,
    )
    .await
    .unwrap();
  let batch = harness.dispatcher.claim_input_batch_locked(&guard, &manager.id, run_id, None).unwrap();
  drop(guard);
  let initial_id = initial["id"].as_str().unwrap().to_string();
  assert_eq!(batch, [initial_id.as_str()]);
  harness.store.record_launch(&manager.id, run_id, 424_001, "1-424000").await.unwrap();

  let echoed = harness
    .dispatcher
    .admit_input(&manager.id, event_types::USER, "Echoed", "user", json!({"input_id":"echoed"}))
    .await
    .unwrap();
  let queued = harness
    .dispatcher
    .admit_input(&manager.id, event_types::USER, "Still pending", "user", json!({"input_id":"queued"}))
    .await
    .unwrap();
  let agent = harness
    .dispatcher
    .admit_input(
      &manager.id,
      event_types::AGENT_MESSAGE,
      "Agent input",
      "agent",
      json!({"from_session":"agent-session"}),
    )
    .await
    .unwrap();
  let echoed_id = echoed["id"].as_str().unwrap().to_string();
  let queued_id = queued["id"].as_str().unwrap().to_string();
  let agent_id = agent["id"].as_str().unwrap().to_string();
  let transport_dir = harness.store.run_dir(&manager.id, run_id);
  std::fs::create_dir_all(&transport_dir).unwrap();
  let raw_path = transport_dir.join(run_records::RAW_LOG_NAME);
  let replay = |uuid: &str| json!({"type": event_types::USER, "uuid": uuid, "isReplay": true}).to_string();
  std::fs::write(&raw_path, format!("{}\n{}\n{}\n", replay(run_id), replay(&echoed_id), replay(&agent_id))).unwrap();

  let claimed = charliebot_tasks::midturn_delivery::claim_finish_inputs(
    harness.dispatcher.as_ref(),
    &manager,
    run_id,
    &transport_dir,
    true,
  )
  .await
  .unwrap();

  assert_eq!(claimed, [echoed_id.as_str()]);
  let updated = harness.store.read_run_sync(&manager.id, run_id).unwrap().expect("the run is registered");
  assert_eq!(updated.input_event_ids, [initial_id.clone(), echoed_id.clone()]);
  let pending_ids = harness
    .dispatcher
    .pending_inputs(&manager.id)
    .unwrap()
    .iter()
    .map(|event| event["id"].as_str().unwrap().to_string())
    .collect::<std::collections::HashSet<_>>();
  assert_eq!(pending_ids, [queued_id.clone(), agent_id.clone()].into_iter().collect());

  let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
  outcome.exit_code = Some(0);
  harness.dispatcher.finish_run(&manager.id, run_id, outcome).await.unwrap();
  let finished = harness
    .store
    .load_all_facts(&manager.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::RUN_FINISHED && event["run_id"] == run_id)
    .expect("the run finished");
  assert_eq!(finished.to_value()["input_event_ids"], json!([initial_id, echoed_id]));
  let pending_ids = harness
    .dispatcher
    .pending_inputs(&manager.id)
    .unwrap()
    .iter()
    .map(|event| event["id"].as_str().unwrap().to_string())
    .collect::<std::collections::HashSet<_>>();
  assert_eq!(pending_ids, [queued_id, agent_id].into_iter().collect());
}

// Python: tests/test_task_execution.py::test_input_admitted_during_active_run_dispatches_after_its_finish
// The Python case is parametrized over the session kind and the later input's type; a thread session keeps queueing,
// and a web session with no live stream-json process queues the same way.
#[tokio::test]
async fn input_admitted_during_an_active_run_dispatches_after_its_finish_for_every_session_kind() {
  for (session_kind, later_type, later_actor) in [
    ("web", event_types::USER, "user"),
    ("web", event_types::AGENT_MESSAGE, "agent"),
    ("slack", event_types::USER, "user"),
    ("slack", event_types::AGENT_MESSAGE, "agent"),
    ("discord", event_types::USER, "user"),
    ("discord", event_types::AGENT_MESSAGE, "agent"),
  ] {
    let harness = Harness::new();
    let manager = harness.create_task("manager", None, TaskProfile::Manager, "Manager").await;
    match session_kind {
      "slack" => {
        harness
          .store
          .update_slot_fields(
            &manager.id,
            "slack",
            Map::from_iter([("slack_origin".to_string(), json!({"team_id":"T","channel_id":"C","thread_ts":"1.0"}))]),
          )
          .await
          .unwrap();
      }
      "discord" => {
        harness
          .store
          .update_slot_fields(
            &manager.id,
            "discord",
            Map::from_iter([(
              "discord_origin".to_string(),
              json!({"guild_id":"G","parent_channel_id":"C","thread_id":"1"}),
            )]),
          )
          .await
          .unwrap();
      }
      _ => {}
    }
    harness
      .dispatcher
      .admit_input(&manager.id, event_types::USER, "Take off. First.", "user", json!({"input_id":"first"}))
      .await
      .unwrap();
    let run1 = "42400211-2222-3333-4444-555555555555";
    {
      let guard = harness.store.control_lock().await;
      harness
        .store
        .register_run_locked(
          &guard,
          &manager.id,
          RunRecord { kind: RunKind::ManagerTurn, ..dispatch_support::new_run(&manager.id, run1) },
          None,
        )
        .await
        .unwrap();
      harness.dispatcher.claim_input_batch_locked(&guard, &manager.id, run1, None).unwrap();
      drop(guard);
    }
    harness.store.record_launch(&manager.id, run1, 424_002, "1-424002").await.unwrap();

    let admitted_later = harness
      .dispatcher
      .admit_input(
        &manager.id,
        later_type,
        "Second message.",
        later_actor,
        if later_type == event_types::AGENT_MESSAGE { json!({"from_session":"agent-session"}) } else { json!({}) },
      )
      .await
      .unwrap();
    let later_id = admitted_later["id"].as_str().unwrap().to_string();
    let decision_later = harness.dispatcher.dispatch_pending(&manager.id).await.unwrap();
    assert_eq!(decision_later["launch"], json!(false), "{session_kind} {later_type}: the active run consumes");

    // The turn's own finish dispatches the waiting input: a scripted executor lands it as the next run.
    harness.install_executor(vec![RunOutcomeKind::Success]);
    let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
    outcome.exit_code = Some(0);
    harness.dispatcher.finish_run(&manager.id, run1, outcome).await.unwrap();
    let decision_next = harness.dispatcher.dispatch_pending(&manager.id).await.unwrap();
    assert_eq!(decision_next["launch"], json!(true), "{session_kind} {later_type}: the next turn launches");
    let run2 = decision_next["run_id"].as_str().unwrap().to_string();
    let second_run = harness.store.read_run_sync(&manager.id, &run2).unwrap().expect("the second run is registered");
    assert_eq!(second_run.input_event_ids, [later_id.as_str()], "{session_kind} {later_type}");
    assert!(harness.dispatcher.pending_inputs(&manager.id).unwrap().is_empty(), "{session_kind} {later_type}");
  }
}
