use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::proc_identity;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, EmptyRuntimeEventHooks, RetryRunFields, RunKind, RunOutcome, RunOutcomeKind,
  RunRecord,
  SequenceListingFields, StopResult, Store, StoreError, StoreHooks,
};
use serde_json::{Map, Value, json};

const SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";
const THREAD_ID: &str = "00000000-0000-4000-8000-000000000002";
const RUN_SESSION_ID: &str = "00000000-0000-4000-8000-000000000101";
const FIXED_TIME: &str = "2025-02-03T04:05:06.123456Z";
const RUN_OPS: &[&str] = &[
  "register",
  "retry",
  "retry_spec",
  "retry_manager_turn",
  "launch",
  "observation",
  "finish",
  "repeat_finish",
  "stop",
];
const RUN_RETRY_OPS: &[&str] = &["retry", "retry_spec", "retry_manager_turn"];
const RETRY_TASK_SPEC: &str = "synthetic retry task spec";
const OPS: &[&str] = &[
  "persist_cc_session_id",
  "persist_native_backend",
  "persist_native_anchor_provenance",
  "clear_cc_session_anchor",
  "persist_account_label",
  "update_thinking_state",
  "mark_unread",
  "rename_session",
  "set_group",
  "switch_backend",
  "archive_session",
  "unarchive_session",
  "update_slot_fields",
  "event_append",
];

#[tokio::test]
async fn python_and_rust_store_operations_write_identical_files_and_read_each_others_output() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare", root.path());
  let fixed = UtcDateTime::parse(FIXED_TIME).unwrap();

  for operation in OPS {
    let python_home = root.path().join(operation).join("python-home");
    let rust_home = root.path().join(operation).join("rust-home");
    let rust_store = make_store(&rust_home);
    let python_store = make_store(&python_home);
    let initial = rust_store.get_session(SESSION_ID).await.unwrap().unwrap();
    assert_eq!(initial.extra("left_by_deleted_package").unwrap()["text"], "你好");
    let python_metadata = python_store.read_metadata_fresh(SESSION_ID).unwrap().unwrap();
    assert_eq!(python_metadata.extra("left_by_deleted_package").unwrap()["values"][1], 2.5);

    if *operation == "event_append" {
      let python_events = python_store.load_chat_events_sync(SESSION_ID).unwrap();
      assert_eq!(python_events.len(), 1);
      assert_eq!(python_events[0]["id"], "synthetic-event-id");
      rust_store.load_chat_events_sync(SESSION_ID).unwrap();
      rust_store
        .append_chat_event(
          SESSION_ID,
          json!({
            "type": "user",
            "id": "synthetic-event-id",
            "timestamp": "2025-02-03T04:05:06.123456+00:00",
            "content": "你好",
          }),
        )
        .await
        .unwrap();
    } else {
      match *operation {
        "persist_cc_session_id" => {
          assert_eq!(
            rust_store
              .persist_cc_session_id_at(SESSION_ID, "new-native-id", Some("new-backend"), fixed)
              .await
              .unwrap()
              .as_deref(),
            Some("new-native-id")
          );
        }
        "persist_native_backend" => {
          assert_eq!(
            rust_store.persist_native_backend(SESSION_ID, "new-backend").await.unwrap().as_deref(),
            Some("new-backend")
          );
        }
        "persist_native_anchor_provenance" => {
          rust_store
            .persist_native_anchor_provenance(SESSION_ID, "new-prompt-hash", "new-backend", Some("new-model"))
            .await
            .unwrap();
        }
        "clear_cc_session_anchor" => rust_store.clear_cc_session_anchor(SESSION_ID).await.unwrap(),
        "persist_account_label" => {
          assert_eq!(
            rust_store.persist_account_label(SESSION_ID, "new-account").await.unwrap().as_deref(),
            Some("new-account")
          );
        }
        "update_thinking_state" => rust_store.update_thinking_state(SESSION_ID, fixed).await.unwrap(),
        "mark_unread" => {
          assert!(rust_store.mark_unread(SESSION_ID).await.unwrap().unwrap().has_unread);
        }
        "rename_session" => {
          rust_store.rename_session_at(SESSION_ID, "renamed session", fixed).await.unwrap();
        }
        "set_group" => {
          rust_store.set_group_at(SESSION_ID, Some("new-group"), fixed).await.unwrap();
        }
        "switch_backend" => {
          rust_store.switch_backend_at(SESSION_ID, "new-backend", fixed).await.unwrap();
        }
        "archive_session" => {
          rust_store.archive_session_at(SESSION_ID, fixed).await.unwrap();
        }
        "unarchive_session" => {
          rust_store.unarchive_session_at(SESSION_ID, fixed).await.unwrap();
        }
        "update_slot_fields" => {
          let mut values = Map::new();
          values.insert("slack_watermark_ts".to_string(), json!("new-slack-watermark"));
          rust_store.update_slot_fields(SESSION_ID, "slack", values).await.unwrap();
        }
        _ => unreachable!("the operation list names every operation"),
      }
    }

    let rust_metadata_path = rust_store.metadata_path(SESSION_ID);
    let python_metadata_path = python_store.metadata_path(SESSION_ID);
    assert_eq!(fs::read(&rust_metadata_path).unwrap(), fs::read(&python_metadata_path).unwrap(), "{operation}");
    assert!(rust_store.read_metadata_fresh(SESSION_ID).unwrap().unwrap().extra("left_by_deleted_package").is_some());
    if *operation == "event_append" {
      assert_eq!(
        fs::read(rust_store.chat_events_path(SESSION_ID)).unwrap(),
        fs::read(python_store.chat_events_path(SESSION_ID)).unwrap()
      );
    }
    let thread_path = rust_store.thread_metadata_path(SESSION_ID, THREAD_ID);
    let thread = rust_store.read_thread_metadata(&thread_path).unwrap().unwrap();
    assert_eq!(thread.extra.get("left_by_deleted_package").unwrap()["kept"], true);
    if *operation == OPS[0] {
      let mut rust_thread: ThreadMetadata = serde_json::from_value(json!({
        "id": THREAD_ID,
        "session_id": SESSION_ID,
        "description": "synthetic thread",
        "created_at": FIXED_TIME,
        "claude_session_id": "native-thread-id",
        "left_by_deleted_package": {"kept": true},
      }))
      .unwrap();
      rust_thread.extra.insert("rust_written_unknown".to_string(), json!("preserved"));
      rust_store
        .write_thread_metadata(
          &rust_home.join("sessions").join(SESSION_ID).join("threads").join(THREAD_ID).join("rust-written.json"),
          &rust_thread,
        )
        .unwrap();
    }
  }
  run_python_helper("verify", root.path());
}

#[tokio::test]
async fn archive_updates_global_indexes_and_control_appends_require_the_store_guard() {
  let root = tempfile::tempdir().unwrap();
  let home = root.path().join("archive-home");
  let store = make_store(&home);
  let metadata =
    SessionMetadata::new(SESSION_ID, "synthetic session", charliebot_store::metadata::TaskProfile::Manager);
  let metadata_path = store.metadata_path(SESSION_ID);
  fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
  fs::write(&metadata_path, metadata.to_json_string(&slots(), true)).unwrap();
  let event_path = store.chat_events_path(SESSION_ID);
  fs::create_dir_all(event_path.parent().unwrap()).unwrap();
  fs::write(
    &event_path,
    "{\"type\": \"user\", \"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n{\"type\": \"user\", \"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
  ).unwrap();

  let archive = store
    .archive_events_before(SESSION_ID, UtcDateTime::parse("2025-02-03T00:00:00Z").unwrap())
    .await
    .unwrap()
    .unwrap();
  assert!(archive.ends_with("chat_events.2025-W06.jsonl"));
  assert_eq!(store.read_metadata_fresh(SESSION_ID).unwrap().unwrap().archive_offset, 1);
  let all = store.load_all_facts(SESSION_ID).unwrap();
  assert_eq!(all.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(), ["old", "new"]);
  assert_eq!(
    store.load_chat_events_range(SESSION_ID, 0, 1).unwrap().0,
    [json!({
      "type": "user", "id": "old", "timestamp": "2025-02-01T00:00:00Z"
    })]
  );
  assert_eq!(store.load_chat_events_range(SESSION_ID, 1, 2).unwrap().0[0]["id"], "new");
  assert_eq!(store.find_event_by_id(SESSION_ID, "old").unwrap().unwrap()["id"], "old");

  let guard = store.control_lock().await;
  let event = store.append_control_fact(&guard, SESSION_ID, json!({"type": "task_closed"})).await.unwrap();
  assert!(event["id"].is_string());
  let mut values = Map::new();
  values.insert("slack_watermark_ts".to_string(), json!("locked-update"));
  store.update_slot_fields_locked(&guard, SESSION_ID, "slack", values).await.unwrap();
  assert!(tokio::time::timeout(std::time::Duration::from_millis(10), store.control_lock()).await.is_err());
  let other_store = make_store(&root.path().join("other-home"));
  let foreign_guard = other_store.control_lock().await;
  assert!(store.append_control_fact(&foreign_guard, SESSION_ID, json!({"type": "task_closed"})).await.is_err());
  let mut foreign_values = Map::new();
  foreign_values.insert("slack_watermark_ts".to_string(), json!("foreign-update"));
  assert!(store.update_slot_fields_locked(&foreign_guard, SESSION_ID, "slack", foreign_values).await.is_err());
}

#[test]
fn listing_filters_status_and_builds_stable_archived_pages() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  for (id, status, group, updated_at) in [
    ("session-a", charliebot_store::metadata::SessionStatus::Archived, Some("Alpha"), "2025-02-01T00:00:00Z"),
    ("session-b", charliebot_store::metadata::SessionStatus::Archived, None, "2025-02-02T00:00:00Z"),
    ("session-c", charliebot_store::metadata::SessionStatus::Archived, Some("Beta"), "2025-02-03T00:00:00Z"),
    ("session-d", charliebot_store::metadata::SessionStatus::Active, Some("Alpha"), "2025-02-04T00:00:00Z"),
  ] {
    let mut meta = SessionMetadata::new(id, id, charliebot_store::metadata::TaskProfile::Manager);
    meta.status = status;
    meta.group = group.map(str::to_string);
    meta.updated_at = UtcDateTime::parse(updated_at).unwrap();
    let path = store.metadata_path(id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, meta.to_json_string(&slots(), true)).unwrap();
  }

  let listed = store.list_sessions(Some(charliebot_store::metadata::SessionStatus::Archived), None, None).unwrap();
  assert_eq!(listed.rows.len(), 3);
  assert!(listed.rows.iter().all(|row| row.status == charliebot_store::metadata::SessionStatus::Archived));
  assert_eq!(store.list_sessions(None, None, Some(true)).unwrap().rows.len(), 0);
  assert_eq!(store.list_sessions(None, None, Some(false)).unwrap().rows.len(), 4);
  assert_eq!(store.list_group_names().unwrap(), ["Alpha", "Beta"]);

  let first = store.list_archived_page(None, 2, None, None).unwrap();
  assert!(first.has_more);
  assert_eq!(first.sessions.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["session-c", "session-b"]);
  assert_eq!(first.groups, [(Some("Alpha".to_string()), 1), (Some("Beta".to_string()), 1), (None, 1)]);
  let next = store.list_archived_page(None, 2, first.next_before.as_deref(), first.next_before_id.as_deref()).unwrap();
  assert_eq!(next.sessions.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["session-a"]);
  assert!(!next.has_more);
  assert!(store.list_archived_page(None, 2, Some("2025-02-03T00:00:00Z"), None).is_err());
}

#[tokio::test]
async fn python_and_rust_run_operations_share_record_and_fact_bytes() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_runs", root.path());
  run_python_helper("apply_runs", root.path());
  let fixed = UtcDateTime::parse(FIXED_TIME).unwrap();

  for operation in RUN_OPS.iter().copied() {
    let is_retry = RUN_RETRY_OPS.contains(&operation);
    let operation_root = root.path().join(operation);
    let python_home = operation_root.join("python-result");
    let python_store = make_store(&python_home);
    let run_id = match operation {
      "register" => "run-register".to_string(),
      _ if is_retry => charliebot_store::control_events::stable_run_id(RUN_SESSION_ID, "retry-request"),
      _ => "run-a".to_string(),
    };
    let python_run = python_store.read_run_sync(RUN_SESSION_ID, &run_id).unwrap().unwrap();
    assert_eq!(python_store.list_run_records_sync(RUN_SESSION_ID).unwrap().len(), if is_retry { 2 } else { 1 });

    let shared_home = operation_root.join("shared-home");
    if shared_home.exists() {
      fs::remove_dir_all(&shared_home).unwrap();
    }
    copy_tree(&operation_root.join("seed-home"), &shared_home);
    let store = make_store(&shared_home);
    if matches!(operation, "finish" | "stop") {
      store.set_control_event_ids_for_replay(&["00000000-0000-4000-8000-000000000199"]);
    }

    match operation {
      "register" => {
        let guard = store.control_lock().await;
        let registered = store
          .register_run_locked(
            &guard,
            RUN_SESSION_ID,
            empty_run("run-register", RunKind::Work),
            Some("synthetic task spec"),
          )
          .await
          .unwrap();
        assert_eq!(registered.task_spec_hash, python_run.task_spec_hash);
        let duplicate = store
          .register_run_locked(
            &guard,
            RUN_SESSION_ID,
            empty_run("run-register", RunKind::Review),
            Some("ignored replacement"),
          )
          .await
          .unwrap();
        assert_eq!(duplicate.kind, RunKind::Work);
      }
      "retry" | "retry_spec" | "retry_manager_turn" => {
        let previous = store.read_run_sync(RUN_SESSION_ID, "run-previous").unwrap().unwrap();
        let task_spec = match operation {
          "retry" => None,
          _ => Some(RETRY_TASK_SPEC),
        };
        let input_event_ids = match operation {
          "retry_manager_turn" => previous.input_event_ids.clone(),
          _ => Vec::new(),
        };
        let fields = retry_fields(&previous, input_event_ids);
        let guard = store.control_lock().await;
        let retried = store
          .create_retry_run_locked(&guard, RUN_SESSION_ID, "retry-request", &previous.id, task_spec, fields.clone())
          .await
          .unwrap();
        assert_eq!(retried.id, run_id);
        assert_eq!(retried.retry_of_run_id.as_deref(), Some("run-previous"));
        assert_eq!(
          store
            .create_retry_run_locked(&guard, RUN_SESSION_ID, "retry-request", &previous.id, task_spec, fields)
            .await
            .unwrap(),
          retried
        );
      }
      "launch" => {
        store.record_launch(RUN_SESSION_ID, "run-a", 424242, "12345").await.unwrap();
      }
      "observation" => {
        let mut fields = Map::new();
        fields.insert("native_session_id".to_string(), json!("native-a"));
        fields.insert("model".to_string(), json!("model-b"));
        fields.insert("raw_log_ref".to_string(), json!("/tmp/run.raw"));
        store.record_observation(RUN_SESSION_ID, "run-a", fields).await.unwrap();
      }
      "finish" => {
        let guard = store.control_lock().await;
        let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
        outcome.input_event_ids = vec!["input-a".to_string()];
        outcome.exit_code = Some(0);
        outcome.ended_at = Some(fixed);
        store.record_finish_locked(&guard, RUN_SESSION_ID, "run-a", outcome).await.unwrap();
      }
      "repeat_finish" => {
        let guard = store.control_lock().await;
        let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
        outcome.input_event_ids = vec!["ignored-input".to_string()];
        outcome.exit_code = Some(0);
        outcome.ended_at = Some(fixed);
        store.record_finish_locked(&guard, RUN_SESSION_ID, "run-a", outcome).await.unwrap();
      }
      "stop" => {
        store.request_stop(RUN_SESSION_ID, "run-a", "stop-request").await.unwrap();
      }
      _ => unreachable!("the run operation list names every operation"),
    }

    assert_eq!(
      store.records_generation(),
      if is_retry || matches!(operation, "register" | "launch" | "observation" | "finish" | "repeat_finish") {
        1
      } else {
        0
      },
      "{operation}"
    );
    assert_run_files_equal(&python_home, &shared_home, &run_id, operation);
    let facts_path = store.chat_events_path(RUN_SESSION_ID);
    let python_facts_path = python_store.chat_events_path(RUN_SESSION_ID);
    assert_eq!(normalize_facts(&facts_path), normalize_facts(&python_facts_path), "{operation}");
    assert_eq!(store.read_run_sync(RUN_SESSION_ID, &run_id).unwrap().unwrap().id, run_id);
  }

  run_python_helper("verify_runs", root.path());
}

#[tokio::test]
async fn stop_checks_process_identity_and_reconciles_after_the_fact_is_durable() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());

  let mut mismatched = Command::new("sleep").arg("30").spawn().unwrap();
  let pid = mismatched.id();
  let actual_start = proc_identity::read_pid_stat(pid).unwrap().pid_start;
  register_process_run(&store, "run-mismatch", pid, "forged-start").await;
  let stop_error = store.request_stop(RUN_SESSION_ID, "run-mismatch", "mismatch-stop").await.unwrap_err();
  let remained_alive = mismatched.try_wait().unwrap().is_none();
  if remained_alive {
    mismatched.kill().unwrap();
  }
  mismatched.wait().unwrap();
  assert!(stop_error.to_string().contains("identity mismatch"));
  assert!(remained_alive);
  assert!(
    store
      .load_all_facts(RUN_SESSION_ID)
      .unwrap()
      .iter()
      .any(|event| event["type"] == "run_stop_requested" && event["run_id"] == "run-mismatch")
  );
  assert_ne!(actual_start, "forged-start");

  let mut reconciled = Command::new("sleep").arg("30").spawn().unwrap();
  let pid = reconciled.id();
  let pid_start = proc_identity::read_pid_stat(pid).unwrap().pid_start;
  register_process_run(&store, "run-reconcile", pid, &pid_start).await;
  let event = charliebot_store::control_events::build_control_event(
    "run_stop_requested",
    charliebot_store::control_events::ACTOR_SYSTEM,
    RUN_SESSION_ID,
    Some("reconcile-stop-event"),
    Some(UtcDateTime::now()),
    [("request_id".to_string(), json!("reconcile-stop")), ("run_id".to_string(), json!("run-reconcile"))],
  );
  let guard = store.control_lock().await;
  store.append_control_fact(&guard, RUN_SESSION_ID, event).await.unwrap();
  drop(guard);

  let result = store.reconcile_stop_request(RUN_SESSION_ID, "run-reconcile").await.unwrap();
  let process_exited = reconciled.try_wait().unwrap().is_some();
  if !process_exited {
    reconciled.kill().unwrap();
  }
  reconciled.wait().unwrap();
  assert!(process_exited);
  assert_eq!(result.outcome, Some(RunOutcomeKind::Interrupted));
  assert_eq!(
    store.run_display_state(RUN_SESSION_ID, "run-reconcile").unwrap(),
    charliebot_store::RunDisplayState::Interrupted
  );
}

#[tokio::test]
async fn launch_refuses_a_durable_stop_and_preserves_the_first_start_time() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, empty_run("run-launch", RunKind::Work), None).await.unwrap();
  drop(guard);

  let first = store.record_launch(RUN_SESSION_ID, "run-launch", 111111, "first-start").await.unwrap();
  let replacement = store.record_launch(RUN_SESSION_ID, "run-launch", 222222, "second-start").await.unwrap();
  assert_eq!(replacement.started_at, first.started_at);
  assert_eq!((replacement.pid, replacement.pid_start.as_deref()), (Some(222222), Some("second-start")));

  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, empty_run("run-queued-stop", RunKind::Work), None).await.unwrap();
  drop(guard);
  store.request_stop(RUN_SESSION_ID, "run-queued-stop", "queued-stop").await.unwrap();
  let error = store.record_launch(RUN_SESSION_ID, "run-queued-stop", 333333, "third-start").await.unwrap_err();
  assert!(error.to_string().contains("has a stop request"));
  let queued = store.read_run_sync(RUN_SESSION_ID, "run-queued-stop").unwrap().unwrap();
  assert_eq!((queued.pid, queued.pid_start), (None, None));
}

#[tokio::test]
async fn a_late_stop_keeps_a_finished_runs_outcome_and_a_repeat_finish_keeps_its_exit_code() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, empty_run("run-done", RunKind::Work), None).await.unwrap();
  store
    .record_finish_locked(&guard, RUN_SESSION_ID, "run-done", RunOutcome::new(RunOutcomeKind::Success))
    .await
    .unwrap();
  drop(guard);

  let stop = store.request_stop(RUN_SESSION_ID, "run-done", "late-stop").await.unwrap();
  assert_eq!(
    stop,
    StopResult { run_id: "run-done".to_string(), stop_requested: false, outcome: Some(RunOutcomeKind::Success) }
  );
  assert!(!store.load_all_facts(RUN_SESSION_ID).unwrap().iter().any(|event| event["type"] == "run_stop_requested"));

  let guard = store.control_lock().await;
  let mut repeat = RunOutcome::new(RunOutcomeKind::Failed);
  repeat.exit_code = Some(3);
  let repaired = store.record_finish_locked(&guard, RUN_SESSION_ID, "run-done", repeat).await.unwrap();
  assert_eq!((repaired.exit_code, repaired.ended_at.is_some()), (None, true));
}

fn retry_fields(previous: &RunRecord, input_event_ids: Vec<String>) -> RetryRunFields {
  RetryRunFields {
    kind: previous.kind,
    backend: previous.backend.clone(),
    model: previous.model.clone(),
    repo_path: previous.repo_path.clone(),
    base_branch: previous.base_branch.clone(),
    branch_name: previous.branch_name.clone(),
    worktree_path: previous.worktree_path.clone(),
    sequence_ref: previous.sequence_ref.clone(),
    review_of_run_id: previous.review_of_run_id.clone(),
    input_event_ids,
  }
}

async fn register_process_run(store: &Store, run_id: &str, pid: u32, pid_start: &str) {
  let mut run = empty_run(run_id, RunKind::Work);
  run.pid = Some(pid);
  run.pid_start = Some(pid_start.to_string());
  run.started_at = Some(UtcDateTime::now());
  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, run, None).await.unwrap();
}

fn make_store(home: &Path) -> Store {
  fs::create_dir_all(home).unwrap();
  let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
  let account_labels: Arc<dyn AccountLabelKeeper> = Arc::new(TestAccountLabelKeeper);
  let anchor_reconciler: Arc<dyn AnchorFieldReconciler> =
    Arc::new(TestAnchorReconciler { account_labels: Arc::clone(&account_labels) });
  let sequence_listing: Arc<dyn SequenceListingFields> = Arc::new(TestSequenceListing);
  Store::new(
    cfg,
    slots(),
    StoreHooks {
      account_labels: Some(account_labels),
      anchor_reconciler: Some(anchor_reconciler),
      sequence_listing: Some(sequence_listing),
      runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
    },
  )
  .unwrap()
}

fn slots() -> Vec<MetadataSlot> {
  vec![
    MetadataSlot {
      owner: "backend_pool",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "backend_from_pool", default: json!(false) }],
    },
    MetadataSlot {
      owner: "cron",
      on: MetadataFile::Session,
      after: Some("backend"),
      fields: vec![
        SlotField { name: "scheduled_task", default: Value::Null },
        SlotField { name: "last_scheduled_run", default: Value::Null },
        SlotField { name: "last_run_status", default: Value::Null },
        SlotField { name: "last_scheduled_cron", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "slack",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: vec![
        SlotField { name: "slack_origin", default: Value::Null },
        SlotField { name: "slack_watermark_ts", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "discord",
      on: MetadataFile::Session,
      after: Some("successor_session_id"),
      fields: vec![
        SlotField { name: "discord_origin", default: Value::Null },
        SlotField { name: "discord_watermark_id", default: Value::Null },
      ],
    },
    MetadataSlot {
      owner: "web_reply",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "web_status", default: Value::Null }],
    },
    MetadataSlot {
      owner: "claude_code",
      on: MetadataFile::Session,
      after: Some("cc_session_started_at"),
      fields: vec![SlotField { name: "claude_account", default: Value::Null }],
    },
    MetadataSlot {
      owner: "claude_code",
      on: MetadataFile::Thread,
      after: Some("exit_code"),
      fields: vec![SlotField { name: "claude_session_id", default: Value::Null }],
    },
  ]
}

struct TestAccountLabelKeeper;

impl AccountLabelKeeper for TestAccountLabelKeeper {
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> Result<bool, StoreError> {
    let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
    if meta.extra("claude_account") == Some(&value) {
      return Ok(false);
    }
    meta.set_extra("claude_account", value);
    Ok(true)
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
  }
}

struct TestAnchorReconciler {
  account_labels: Arc<dyn AccountLabelKeeper>,
}

impl AnchorFieldReconciler for TestAnchorReconciler {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> Result<(), StoreError> {
    attempted.cc_session_id.clone_from(&disk.cc_session_id);
    attempted.native_backend.clone_from(&disk.native_backend);
    let label = self.account_labels.account_label(disk);
    if self.account_labels.account_label(attempted) != label {
      self.account_labels.record_account_label(attempted, label.as_deref())?;
    }
    Ok(())
  }
}

struct TestSequenceListing;

impl SequenceListingFields for TestSequenceListing {
  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(
    &self,
    _session_ids: &[String],
    _now: UtcDateTime,
  ) -> Result<HashMap<String, Map<String, Value>>, StoreError> {
    Ok(HashMap::new())
  }

  fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> Result<HashMap<String, String>, StoreError> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(
    &self,
    _metas: &[SessionMetadata],
  ) -> Result<HashMap<String, HashMap<String, String>>, StoreError> {
    Ok(HashMap::new())
  }

  fn derived_archived_ids(&self) -> Result<Vec<String>, StoreError> {
    Ok(Vec::new())
  }
}

fn run_python_helper(mode: &str, root: &Path) {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let helper = "rust/crates/charliebot-store/tests/python_store_ops.py";
  let output = Command::new("uv")
    .args(["run", "python", helper, mode, &root.to_string_lossy()])
    .current_dir(repo_root)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "uv helper {mode} failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
}

fn empty_run(id: &str, kind: RunKind) -> RunRecord {
  RunRecord {
    id: id.to_string(),
    session_id: RUN_SESSION_ID.to_string(),
    kind,
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

fn copy_tree(source: &Path, destination: &Path) {
  fs::create_dir_all(destination).unwrap();
  for entry in fs::read_dir(source).unwrap() {
    let entry = entry.unwrap();
    let target = destination.join(entry.file_name());
    if entry.file_type().unwrap().is_dir() {
      copy_tree(&entry.path(), &target);
    } else {
      fs::copy(entry.path(), target).unwrap();
    }
  }
}

fn assert_run_files_equal(python_home: &Path, rust_home: &Path, run_id: &str, operation: &str) {
  let relative = Path::new("sessions").join(RUN_SESSION_ID).join("data").join("runs").join(run_id);
  let python_dir = python_home.join(&relative);
  let rust_dir = rust_home.join(relative);
  assert_eq!(
    normalize_model(&python_dir.join("metadata.json")),
    normalize_model(&rust_dir.join("metadata.json")),
    "{operation}"
  );
  let python_spec = python_dir.join("task_spec.md");
  let rust_spec = rust_dir.join("task_spec.md");
  assert_eq!(python_spec.exists(), rust_spec.exists(), "{operation}");
  if python_spec.exists() {
    assert_eq!(fs::read(python_spec).unwrap(), fs::read(rust_spec).unwrap(), "{operation}");
  }
}

fn normalize_model(path: &Path) -> String {
  let mut raw = fs::read_to_string(path).unwrap();
  let value: Value = serde_json::from_str(&raw).unwrap();
  for key in ["started_at", "ended_at"] {
    if let Some(Value::String(timestamp)) = value.get(key) {
      let field = format!("\"{key}\": {}", serde_json::to_string(timestamp).unwrap());
      assert_eq!(raw.matches(&field).count(), 1, "{key} in {}", path.display());
      raw = raw.replace(&field, &format!("\"{key}\": \"<wall-clock>\""));
    }
  }
  raw
}

fn normalize_facts(path: &Path) -> Vec<String> {
  let raw = match fs::read_to_string(path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
    Err(error) => panic!("{}: {error}", path.display()),
  };
  raw
    .lines()
    .map(|line| {
      let value: Value = serde_json::from_str(line).unwrap();
      let mut normalized = line.to_string();
      if let Some(Value::String(timestamp)) = value.get("timestamp") {
        let field = format!("\"timestamp\": {}", serde_json::to_string(timestamp).unwrap());
        assert_eq!(normalized.matches(&field).count(), 1, "{line}");
        normalized = normalized.replace(&field, "\"timestamp\": \"<wall-clock>\"");
      }
      normalized
    })
    .collect()
}
