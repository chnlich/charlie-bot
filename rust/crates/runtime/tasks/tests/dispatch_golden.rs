mod dispatch_support;

use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::event_types;
use charliebot_store::metadata::TaskProfile;
use charliebot_store::run_records::{RunOutcome, RunOutcomeKind};
use charliebot_tasks::master_trigger::{TriggerMasterRequest, trigger_master};
use serde_json::{Value, json};

use dispatch_support::{Harness, ScriptedExecutor, register_run};

const PYTHON_FIXTURE: &str = include_str!("fixtures/python_dispatch_golden.json");

#[tokio::test]
async fn python_and_rust_write_the_same_dispatch_facts() {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/tasks/tests/python_dispatch_golden.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let mut python_value = fixture["expected"].clone();
  replace_fixture_values(&mut python_value);
  parse_event_lines(&mut python_value);

  let harness = Harness::new();
  let executor = Arc::new(ScriptedExecutor::new(
    Arc::clone(&harness.store),
    &harness.dispatcher,
    vec![
      RunOutcomeKind::Success,
      RunOutcomeKind::Success,
      RunOutcomeKind::Failed,
      RunOutcomeKind::Success,
      RunOutcomeKind::Success,
      RunOutcomeKind::Success,
      RunOutcomeKind::Success,
    ],
  ));
  harness.dispatcher.set_executor(Some(executor.clone()));
  let root = harness.create_task("root", None, TaskProfile::Manager, "Root").await;
  let worker = harness.create_task("worker", Some(&root.id), TaskProfile::Worker, "Worker").await;

  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "root message", "user", json!({"input_id":"root-input"}))
    .await
    .unwrap();
  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "changed replay", "user", json!({"input_id":"root-input"}))
    .await
    .unwrap();
  harness
    .dispatcher
    .admit_input(&root.id, event_types::SCHEDULED_TRIGGER, "cron fire", "system", json!({}))
    .await
    .unwrap();
  harness
    .dispatcher
    .admit_input(
      &worker.id,
      event_types::AGENT_MESSAGE,
      "parent instruction",
      "agent",
      json!({"from_session":root.id,"from_session_name":"Root"}),
    )
    .await
    .unwrap();
  assert!(matches!(
    harness
      .dispatcher
      .admit_input(
        &worker.id,
        event_types::AGENT_MESSAGE,
        "outsider instruction",
        "agent",
        json!({"from_session":"outsider"})
      )
      .await,
    Err(charliebot_store::StoreError::Forbidden(_))
  ));

  harness.close_task(&worker.id, "worker-close", Some(&root.id)).await;
  harness
    .dispatcher
    .admit_input(&worker.id, event_types::USER, "restore worker", "user", json!({"input_id":"worker-input"}))
    .await
    .unwrap();
  harness.dispatcher.dispatch_pending(&root.id).await.unwrap();
  harness.dispatcher.dispatch_pending(&worker.id).await.unwrap();

  let close_event = harness
    .store
    .load_all_facts(&worker.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"] == event_types::TASK_CLOSED)
    .unwrap();
  harness.dispatcher.set_executor(None);
  harness
    .dispatcher
    .deliver_child_report(&worker.id, close_event, "completed", "synthetic close", Some(&[]), Some(&root.id))
    .await
    .unwrap();
  harness.dispatcher.set_executor(Some(executor));
  let lost = harness.create_task("lost", Some(&root.id), TaskProfile::Worker, "Lost").await;
  harness.close_task(&lost.id, "lost-close", Some(&root.id)).await;

  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "failed round", "user", json!({"input_id":"failed-input"}))
    .await
    .unwrap();
  harness.dispatcher.dispatch_pending(&root.id).await.unwrap();

  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "foreign batch", "user", json!({"input_id":"foreign-input"}))
    .await
    .unwrap();
  register_run(&harness, &root.id, "run-owner").await;
  {
    let guard = harness.store.control_lock().await;
    harness.dispatcher.claim_input_batch_locked(&guard, &root.id, "run-owner", None).unwrap();
  }
  register_run(&harness, &root.id, "run-foreign").await;
  let mut foreign_outcome = RunOutcome::new(RunOutcomeKind::Success);
  foreign_outcome.input_event_ids.push("foreign-input".to_string());
  assert!(matches!(
    harness.dispatcher.finish_run(&root.id, "run-foreign", foreign_outcome).await,
    Err(charliebot_store::StoreError::Conflict(_))
  ));
  harness.dispatcher.finish_run(&root.id, "run-owner", RunOutcome::new(RunOutcomeKind::Success)).await.unwrap();

  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "stopped queue", "user", json!({"input_id":"stopped-input"}))
    .await
    .unwrap();
  register_run(&harness, &root.id, "run-stopped").await;
  {
    let guard = harness.store.control_lock().await;
    harness.dispatcher.claim_input_batch_locked(&guard, &root.id, "run-stopped", None).unwrap();
  }
  harness.stop_run(&root.id, "run-stopped", "stop-stopped").await;
  harness.dispatcher.dispatch_pending(&root.id).await.unwrap();

  harness
    .dispatcher
    .admit_input(&root.id, event_types::USER, "report followup", "user", json!({"input_id":"report-followup"}))
    .await
    .unwrap();
  assert!(!harness.dispatcher.undelivered_report_blockers(&lost.id).unwrap().is_empty());
  assert_eq!(harness.dispatcher.recover_pending_reports(&lost.id).await.unwrap().len(), 1);
  assert!(harness.dispatcher.recover_pending_reports(&lost.id).await.unwrap().is_empty());

  let successor = harness.create_task("successor", None, TaskProfile::Manager, "Successor").await;
  harness.set_successor(&root.id, &successor.id).await;
  let mut successor_request = TriggerMasterRequest::new(&root.id, "wake successor");
  successor_request.input_id = Some("successor-input");
  trigger_master(&harness.features, successor_request).await.unwrap();
  let archived = harness.create_task("archived", None, TaskProfile::Manager, "Archived").await;
  harness.archive_task(&archived.id).await;
  let mut archived_request = TriggerMasterRequest::new(&archived.id, "pull back");
  archived_request.input_id = Some("archived-input");
  trigger_master(&harness.features, archived_request).await.unwrap();

  let mut rust_value = json!({
    "nodes": harness.output_for(&[
      ("root", &root.id),
      ("worker", &worker.id),
      ("lost", &lost.id),
      ("successor", &successor.id),
      ("archived", &archived.id),
    ])
  });
  parse_event_lines(&mut rust_value);
  normalize_output(&mut python_value);
  normalize_output(&mut rust_value);
  assert_eq!(python_value, rust_value);
  assert_eq!(
    python_value["nodes"]
      .as_array()
      .unwrap()
      .iter()
      .map(|node| node["events"].as_array().unwrap().len())
      .sum::<usize>(),
    29
  );
}

fn replace_fixture_values(value: &mut Value) {
  match value {
    Value::String(text) => *text = text.replace("$TIME", "2026-01-01T00:00:00Z"),
    Value::Array(values) => values.iter_mut().for_each(replace_fixture_values),
    Value::Object(fields) => fields.values_mut().for_each(replace_fixture_values),
    _ => {}
  }
}

fn normalize_output(output: &mut Value) {
  let mut ids = HashMap::new();
  let mut next_id = 0usize;
  normalize_value(output, &mut ids, &mut next_id);
}

fn parse_event_lines(output: &mut Value) {
  for node in output["nodes"].as_array_mut().expect("golden output has nodes") {
    let lines = node["events"].as_array_mut().expect("node output has event lines");
    for line in lines {
      *line = serde_json::from_str(line.as_str().expect("event output line is a string"))
        .expect("chat event line parses as JSON");
    }
  }
}

fn normalize_value(value: &mut Value, ids: &mut HashMap<String, String>, next_id: &mut usize) {
  match value {
    Value::Array(values) => {
      for value in values {
        normalize_value(value, ids, next_id);
      }
    }
    Value::Object(fields) => {
      for (key, value) in fields.iter_mut() {
        if is_time_field(key) {
          if value.is_string() {
            *value = Value::String("<time>".to_string());
          }
        } else if is_id_field(key) {
          normalize_id_value(value, ids, next_id);
        } else {
          normalize_value(value, ids, next_id);
        }
      }
    }
    _ => {}
  }
}

fn normalize_id_value(value: &mut Value, ids: &mut HashMap<String, String>, next_id: &mut usize) {
  match value {
    Value::String(value) => {
      let normalized = ids.entry(value.clone()).or_insert_with(|| {
        let normalized = format!("<id-{}>", *next_id);
        *next_id += 1;
        normalized
      });
      *value = normalized.clone();
    }
    Value::Array(values) => {
      for value in values {
        normalize_id_value(value, ids, next_id);
      }
    }
    _ => {}
  }
}

fn is_time_field(field: &str) -> bool {
  field == "timestamp" || field.ends_with("_at")
}

fn is_id_field(field: &str) -> bool {
  matches!(
    field,
    "id"
      | "input_id"
      | "request_id"
      | "session_id"
      | "source_session_id"
      | "child_session_id"
      | "child_event_id"
      | "from_session"
      | "report_to"
      | "run_id"
      | "input_event_ids"
      | "closed_event_id"
      | "task_parent_id"
      | "successor_session_id"
      | "parent_session_id"
  )
}
