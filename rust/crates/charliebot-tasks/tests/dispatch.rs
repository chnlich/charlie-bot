mod dispatch_support;

use std::sync::Arc;

use charliebot_core::event_types;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_store::StoreError;
use charliebot_store::StoreResult;
use charliebot_store::metadata::TaskProfile;
use charliebot_store::run_records::{self, RunOutcome, RunOutcomeKind};
use charliebot_tasks::dispatch::{
  DispatchExecutor, child_report_text, dispatcher, inputs_not_pending_conflict, note_run_liveness,
  unprocessed_input_blocker,
};
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

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
    _pending_inputs: &'a [Value],
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
    _pending_inputs: &'a [Value],
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
    child_report_text(report, Some("clone of source")),
    format!("[Report from task {} (clone of source) | outcome completed] synthetic close", child.id)
  );
  assert_eq!(
    unprocessed_input_blocker(std::slice::from_ref(report)),
    format!("has unprocessed input: {}", report["id"].as_str().unwrap())
  );
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
  assert_eq!(acknowledged["input_event_ids"], json!([first_report["id"]]));

  harness.dispatcher.set_executor(None);
  harness.close_task(&child_b.id, "close-b", Some(&parent.id)).await;
  harness.dispatcher.recover_pending_reports(&child_b.id).await.unwrap();

  let reloaded = charliebot_tasks::dispatch::Dispatcher::new(
    Arc::clone(&harness.store),
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
  let reports = vec![
    json!({"type":event_types::CHILD_REPORT,"child_session_id":child.id}),
    json!({"type":event_types::CHILD_REPORT,"child_session_id":child.id}),
  ];
  assert_eq!(harness.dispatcher.child_report_provenances(&reports).await.unwrap().len(), 1);

  let (report, created) =
    harness.dispatcher.deliver_child_report(&child.id, created, "completed", "", None, None).await.unwrap();
  assert_eq!(report, Value::Null);
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
  assert_eq!(finished["input_event_ids"], json!(["existing-wake"]));
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

#[tokio::test]
async fn user_admission_lifts_unarchived_ancestors_and_stops_before_archived_ones() {
  let harness = Harness::new();
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let middle = harness.create_task("middle", Some(&root.id), TaskProfile::Manager, "Middle").await;
  let child = harness.create_task("child", Some(&middle.id), TaskProfile::Manager, "Child").await;
  let first = charliebot_store::metadata::UtcDateTime::parse("2030-01-01T00:00:00Z").unwrap();
  let second = charliebot_store::metadata::UtcDateTime::parse("2030-01-02T00:00:00Z").unwrap();

  harness
    .dispatcher
    .admit_input(
      &child.id,
      event_types::USER,
      "first",
      "user",
      json!({"input_id":"lift-1","timestamp":first.to_python_isoformat()}),
    )
    .await
    .unwrap();
  assert_eq!(harness.store.get_session(&child.id).await.unwrap().unwrap().updated_at, first);
  assert_eq!(harness.store.get_session(&middle.id).await.unwrap().unwrap().updated_at, first);
  assert_eq!(harness.store.get_session(&root.id).await.unwrap().unwrap().updated_at, first);
  harness
    .dispatcher
    .admit_input(
      &child.id,
      event_types::USER,
      "replay",
      "user",
      json!({"input_id":"lift-1","timestamp":second.to_python_isoformat()}),
    )
    .await
    .unwrap();
  assert_eq!(harness.store.get_session(&child.id).await.unwrap().unwrap().updated_at, first);
  for (event_type, actor) in [
    (event_types::AGENT_MESSAGE, "agent"),
    (event_types::SCHEDULED_TRIGGER, "system"),
    (event_types::CHILD_REPORT, "system"),
  ] {
    harness
      .dispatcher
      .admit_input(&child.id, event_type, "machine traffic", actor, json!({"timestamp":"2030-01-03T00:00:00Z"}))
      .await
      .unwrap();
  }
  assert_eq!(harness.store.get_session(&child.id).await.unwrap().unwrap().updated_at, first);
  assert_eq!(harness.store.get_session(&middle.id).await.unwrap().unwrap().updated_at, first);
  assert_eq!(harness.store.get_session(&root.id).await.unwrap().unwrap().updated_at, first);

  let archived_root = harness.create_task("archived-root", None, TaskProfile::Manager, "Archived root").await;
  let archived_middle =
    harness.create_task("archived-middle", Some(&archived_root.id), TaskProfile::Manager, "Archived middle").await;
  let archived_child =
    harness.create_task("archived-child", Some(&archived_middle.id), TaskProfile::Manager, "Archived child").await;
  harness.archive_task(&archived_middle.id).await;
  let archived_middle_time = harness.store.get_session(&archived_middle.id).await.unwrap().unwrap().updated_at;
  let archived_root_time = harness.store.get_session(&archived_root.id).await.unwrap().unwrap().updated_at;
  let lifted = charliebot_store::metadata::UtcDateTime::parse("2031-01-01T00:00:00Z").unwrap();
  harness
    .dispatcher
    .admit_input(
      &archived_child.id,
      event_types::USER,
      "branch message",
      "user",
      json!({"timestamp":lifted.to_python_isoformat()}),
    )
    .await
    .unwrap();
  assert_eq!(harness.store.get_session(&archived_child.id).await.unwrap().unwrap().updated_at, lifted);
  assert_eq!(harness.store.get_session(&archived_middle.id).await.unwrap().unwrap().updated_at, archived_middle_time);
  assert_eq!(harness.store.get_session(&archived_root.id).await.unwrap().unwrap().updated_at, archived_root_time);
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

#[test]
fn input_blocker_truncates_after_eight_ids() {
  let values = (0..10).map(|index| json!({"id":format!("in-{index}")})).collect::<Vec<Value>>();
  assert_eq!(
    unprocessed_input_blocker(&values),
    "has unprocessed input: in-0, in-1, in-2, in-3, in-4, in-5, in-6, in-7 (+2 more)"
  );
}
