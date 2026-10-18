use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::proc_identity;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, EmptyRuntimeEventHooks, RetryRunFields, RunBlocker, RunKind, RunOrder,
  RunOutcome, RunOutcomeKind, RunPage, RunRecord, SequenceListingFields, StopResult, Store, StoreError, StoreHooks,
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
  run_python_helper("prepare", root.path(), &[]);
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
  run_python_helper("verify", root.path(), &[]);
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
  run_python_helper("prepare_runs", root.path(), &[]);
  run_python_helper("apply_runs", root.path(), &[]);
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

  run_python_helper("verify_runs", root.path(), &[]);
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

#[tokio::test]
async fn run_pages_walk_the_launch_order_and_refuse_a_malformed_cursor() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let mut runs = vec![empty_run("queued-0", RunKind::Work), empty_run("queued-1", RunKind::Work)];
  runs.extend((0..5).map(|minute| launched_run(&format!("run-{minute}"), minute)));
  register_runs(&store, runs).await;

  let mut cursor: Option<String> = None;
  let mut pages = Vec::new();
  loop {
    let page = store.list_runs_page_sync(RUN_SESSION_ID, cursor.as_deref(), 2, RunOrder::Ascending).unwrap();
    pages.push(page.items.into_iter().map(|run| run.id).collect::<Vec<_>>());
    match page.next_cursor {
      Some(next) => cursor = Some(next),
      None => break,
    }
  }
  let expected: Vec<Vec<&str>> =
    vec![vec!["queued-0", "queued-1"], vec!["run-0", "run-1"], vec!["run-2", "run-3"], vec!["run-4"]];
  assert_eq!(pages, expected);
  assert!(store.list_runs_page_sync(RUN_SESSION_ID, Some("not-a-cursor"), 2, RunOrder::Ascending).is_err());
}

#[tokio::test]
async fn descending_run_pages_and_cross_order_cursor_refusals_match_python() {
  let root = tempfile::tempdir().unwrap();
  let seed_home = root.path().join("pages").join("seed-home");
  let store = make_store(&seed_home);
  let mut runs = vec![empty_run("queued-0", RunKind::Work), empty_run("queued-1", RunKind::Work)];
  runs.extend((0..5).map(|minute| launched_run(&format!("run-{minute}"), minute)));
  register_runs(&store, runs).await;

  let (python, rust) = read_on_copies(root.path(), &seed_home, "pages", read_pages);
  assert_eq!(python, rust);
  assert_eq!(rust["descending"]["ids"], json!(["run-4", "run-3"]));
  assert_eq!(rust["descending_next"]["ids"], json!(["run-2", "run-1"]));
  assert!(rust["refused_ascending_cursor"].as_str().unwrap().contains("minted for ascending order"));
}

#[tokio::test]
async fn newest_run_is_the_latest_launch_and_a_queued_reservation_never_leads() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  assert_eq!(store.newest_run_record_sync(RUN_SESSION_ID).unwrap(), None);

  register_runs(&store, vec![launched_run("run-a", 1), launched_run("run-b", 1)]).await;
  assert_eq!(newest_run_id(&store), Some("run-b".to_string()), "equal launch times order by id");

  register_runs(&store, vec![empty_run("queued-0", RunKind::Work)]).await;
  assert_eq!(newest_run_id(&store), Some("run-b".to_string()));

  register_runs(&store, vec![launched_run("run-c", 2)]).await;
  assert_eq!(newest_run_id(&store), Some("run-c".to_string()));
}

#[tokio::test]
async fn run_blockers_name_each_run_that_blocks_a_structural_change() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  assert_eq!(store.run_blockers(RUN_SESSION_ID).unwrap(), Vec::new());

  register_runs(&store, vec![empty_run("run-queued", RunKind::Work)]).await;
  assert_eq!(
    store.run_blockers(RUN_SESSION_ID).unwrap(),
    vec![RunBlocker {
      run_id: "run-queued".to_string(),
      reason: "run run-queued is queued (pending dispatch)".to_string(),
    }]
  );
  // A durable stop settles a run that never launched, so it blocks nothing.
  store.request_stop(RUN_SESSION_ID, "run-queued", "stop-queued").await.unwrap();
  assert_eq!(store.run_blockers(RUN_SESSION_ID).unwrap(), Vec::new());

  let mut live = Command::new("sleep").arg("30").spawn().unwrap();
  let pid_start = proc_identity::read_pid_stat(live.id()).unwrap().pid_start;
  register_process_run(&store, "run-active", live.id(), &pid_start).await;
  let active = store.run_blockers(RUN_SESSION_ID).unwrap();
  finish_run(&store, "run-active", None).await;
  register_process_run(&store, "run-unresolved", live.id(), "forged-start").await;
  let unresolved = store.run_blockers(RUN_SESSION_ID).unwrap();
  live.kill().unwrap();
  live.wait().unwrap();
  assert_eq!(
    active,
    vec![RunBlocker { run_id: "run-active".to_string(), reason: "run run-active is active".to_string() }]
  );
  assert_eq!(
    unresolved,
    vec![RunBlocker {
      run_id: "run-unresolved".to_string(),
      reason: "run run-unresolved has an unresolved process identity (needs recovery)".to_string(),
    }]
  );
}

#[tokio::test]
async fn a_session_with_two_blocked_runs_reports_both_like_python() {
  let root = tempfile::tempdir().unwrap();
  let seed_home = root.path().join("blockers").join("seed-home");
  let store = make_store(&seed_home);
  register_runs(
    &store,
    vec![
      launched_run("run-done", 1),
      empty_run("queued-a", RunKind::Work),
      empty_run("queued-stopped", RunKind::Work),
      unresolved_run("run-unresolved", 2),
    ],
  )
  .await;
  finish_run(&store, "run-done", None).await;
  store.request_stop(RUN_SESSION_ID, "queued-stopped", "stop-queued").await.unwrap();

  let (python, rust) = read_on_copies(root.path(), &seed_home, "blockers", read_blockers);
  assert_eq!(python, rust);
  assert_eq!(
    rust["blockers"],
    json!([
      "run queued-a is queued (pending dispatch)",
      "run run-unresolved has an unresolved process identity (needs recovery)",
    ])
  );
}

#[tokio::test]
async fn last_ended_run_follows_the_end_time_and_falls_back_to_the_finish_fact() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  register_runs(&store, vec![launched_run("run-early", 1), launched_run("run-late", 2)]).await;
  assert_eq!(store.last_ended_run_id(RUN_SESSION_ID).unwrap(), None);

  finish_run(&store, "run-late", Some("2026-01-01T00:03:00Z")).await;
  finish_run(&store, "run-early", Some("2026-01-01T00:05:00Z")).await;
  assert_eq!(store.last_ended_run_id(RUN_SESSION_ID).unwrap(), Some("run-early".to_string()));

  // A terminal run whose mirror lost its end time ranks by its finish fact, which is newer than both mirrors.
  clear_ended_at(&store, "run-late");
  assert_eq!(store.last_ended_run_id(RUN_SESSION_ID).unwrap(), Some("run-late".to_string()));
}

async fn register_runs(store: &Store, runs: Vec<RunRecord>) {
  let guard = store.control_lock().await;
  for run in runs {
    store.register_run_locked(&guard, RUN_SESSION_ID, run, None).await.unwrap();
  }
}

async fn finish_run(store: &Store, run_id: &str, ended_at: Option<&str>) {
  let mut outcome = RunOutcome::new(RunOutcomeKind::Success);
  outcome.ended_at = ended_at.map(|value| UtcDateTime::parse(value).unwrap());
  let guard = store.control_lock().await;
  store.record_finish_locked(&guard, RUN_SESSION_ID, run_id, outcome).await.unwrap();
}

fn launched_run(id: &str, minute: u32) -> RunRecord {
  let mut run = empty_run(id, RunKind::Work);
  run.started_at = Some(UtcDateTime::parse(&format!("2026-01-01T00:{minute:02}:00Z")).unwrap());
  run
}

/// A launched run whose pid is above the kernel's PID_MAX_LIMIT (2^22), so no live process can hold it.
fn unresolved_run(id: &str, minute: u32) -> RunRecord {
  let mut run = launched_run(id, minute);
  run.pid = Some(2_147_483_646);
  run.pid_start = Some("forged-start".to_string());
  run
}

fn newest_run_id(store: &Store) -> Option<String> {
  store.newest_run_record_sync(RUN_SESSION_ID).unwrap().map(|run| run.id)
}

fn clear_ended_at(store: &Store, run_id: &str) {
  let path = store.run_metadata_path(RUN_SESSION_ID, run_id);
  let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
  record["ended_at"] = Value::Null;
  fs::write(&path, record.to_string()).unwrap();
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

fn run_python_helper(mode: &str, root: &Path, extra: &[&str]) {
  let root = root.to_string_lossy();
  let mut args = vec![mode, root.as_ref()];
  args.extend_from_slice(extra);
  let _ = run_python_helper_args(&args);
}

fn run_python_helper_args(args: &[&str]) -> String {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let helper = "rust/crates/charliebot-store/tests/python_store_ops.py";
  let output = Command::new("uv").args(["run", "python", helper]).args(args).current_dir(repo_root).output().unwrap();
  assert!(
    output.status.success(),
    "uv helper {} failed:\nstdout:\n{}\nstderr:\n{}",
    args[0],
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  String::from_utf8(output.stdout).unwrap()
}

/// Copy one seeded home to a Python copy and a Rust copy, then read each copy through its own implementation.
fn read_on_copies(root: &Path, seed_home: &Path, case: &str, rust_read: fn(&Store) -> Value) -> (Value, Value) {
  let case_root = root.join(case);
  copy_tree(seed_home, &case_root.join("python-home"));
  copy_tree(seed_home, &case_root.join("rust-home"));
  let python = serde_json::from_str(&run_python_helper_args(&["read_runs", &root.to_string_lossy(), case])).unwrap();
  (python, rust_read(&make_store(&case_root.join("rust-home"))))
}

fn read_pages(store: &Store) -> Value {
  let ascending = store.list_runs_page_sync(RUN_SESSION_ID, None, 2, RunOrder::Ascending).unwrap();
  let descending = store.list_runs_page_sync(RUN_SESSION_ID, None, 2, RunOrder::Descending).unwrap();
  let descending_next =
    store.list_runs_page_sync(RUN_SESSION_ID, descending.next_cursor.as_deref(), 2, RunOrder::Descending).unwrap();
  json!({
    "ascending": page_json(&ascending),
    "descending": page_json(&descending),
    "descending_next": page_json(&descending_next),
    "refused_ascending_cursor": refusal(store.list_runs_page_sync(
      RUN_SESSION_ID,
      ascending.next_cursor.as_deref(),
      2,
      RunOrder::Descending,
    )),
    "refused_descending_cursor": refusal(store.list_runs_page_sync(
      RUN_SESSION_ID,
      descending.next_cursor.as_deref(),
      2,
      RunOrder::Ascending,
    )),
  })
}

fn page_json(page: &RunPage) -> Value {
  json!({ "ids": page.items.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(), "next_cursor": page.next_cursor })
}

fn refusal<T>(result: Result<T, StoreError>) -> String {
  result.err().expect("the cursor is refused under the other order").to_string()
}

fn read_blockers(store: &Store) -> Value {
  let reasons =
    store.run_blockers(RUN_SESSION_ID).unwrap().into_iter().map(|blocker| blocker.reason).collect::<Vec<_>>();
  json!({ "blockers": reasons })
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

fn write_session_metadata(home: &Path, session_id: &str, archive_offset: i64) {
  let mut meta = SessionMetadata::new(session_id, "seed", charliebot_store::metadata::TaskProfile::Manager);
  meta.archive_offset = archive_offset;
  let path = home.join("sessions").join(session_id).join("metadata.json");
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  fs::write(path, meta.to_json_string(&slots(), true)).unwrap();
}

/// Run one read-parity case: the Rust test builds one fixture home, copies it for Python and Rust,
/// asks the Python helper for its answer, and compares the store's results against it.
fn parity_case_dir(root: &Path, case: &str) -> PathBuf {
  let case_dir = root.join(case);
  fs::create_dir_all(&case_dir).unwrap();
  case_dir
}

#[tokio::test]
async fn range_read_matches_python_when_archive_offset_is_positive_and_lines_are_blank_or_malformed() {
  let root = tempfile::tempdir().unwrap();
  let case_dir = parity_case_dir(root.path(), "range_read");
  let base = case_dir.join("base-home");
  let session_dir = base.join("sessions").join(SESSION_ID);
  let archive_dir = session_dir.join("data").join("archives");
  fs::create_dir_all(&archive_dir).unwrap();
  let mut archive_body = String::new();
  for index in 0..6 {
    archive_body.push_str(&format!("{{\"id\": \"old-{index}\", \"timestamp\": \"2025-02-01T00:00:00+00:00\"}}\n"));
  }
  fs::write(archive_dir.join("chat_events.2025-W06.jsonl"), archive_body).unwrap();
  fs::write(
    session_dir.join("data").join("chat_events.jsonl"),
    b"{\"id\": \"a0\"}\n\n{bad json\n{\"id\": \"a1\"}\r\n{\"id\": \"a2\"}\n{\"id\": \"a3\"}",
  )
  .unwrap();
  write_session_metadata(&base, SESSION_ID, 6);

  copy_tree(&base, &case_dir.join("python-home"));
  copy_tree(&base, &case_dir.join("rust-home"));
  run_python_helper("parity:range_read", &case_dir, &[SESSION_ID]);
  let expected: Vec<Value> =
    serde_json::from_str(&fs::read_to_string(case_dir.join("expected.json")).unwrap()).unwrap();

  let store = make_store(&case_dir.join("rust-home"));
  for window in &expected {
    let (events, has_more) = store
      .load_chat_events_range(
        SESSION_ID,
        window["start"].as_u64().unwrap() as usize,
        window["end"].as_u64().unwrap() as usize,
      )
      .unwrap();
    assert_eq!(has_more, window["has_more"].as_bool().unwrap(), "window {window}");
    assert_eq!(serde_json::to_value(&events).unwrap(), window["events"], "window {window}");
  }
}

#[tokio::test]
async fn listing_and_raw_scan_skip_a_malformed_metadata_file_like_python() {
  let root = tempfile::tempdir().unwrap();
  let case_dir = parity_case_dir(root.path(), "listing_raw_scan");
  let base = case_dir.join("base-home");
  for (id, status, updated_at) in
    [("good-a", "active", "2025-02-01T00:00:00Z"), ("good-b", "archived", "2025-02-02T00:00:00Z")]
  {
    let mut meta = SessionMetadata::new(id, id, charliebot_store::metadata::TaskProfile::Manager);
    meta.status = if status == "archived" {
      charliebot_store::metadata::SessionStatus::Archived
    } else {
      charliebot_store::metadata::SessionStatus::Active
    };
    meta.updated_at = UtcDateTime::parse(updated_at).unwrap();
    let path = base.join("sessions").join(id).join("metadata.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, meta.to_json_string(&slots(), true)).unwrap();
  }
  let corrupt = base.join("sessions").join("corrupt").join("metadata.json");
  fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
  fs::write(&corrupt, "{corrupt").unwrap();
  let empty = base.join("sessions").join("empty").join("metadata.json");
  fs::create_dir_all(empty.parent().unwrap()).unwrap();
  fs::write(&empty, "   ").unwrap();

  copy_tree(&base, &case_dir.join("python-home"));
  copy_tree(&base, &case_dir.join("rust-home"));
  run_python_helper("parity:listing_raw_scan", &case_dir, &[SESSION_ID]);
  let expected: Value = serde_json::from_str(&fs::read_to_string(case_dir.join("expected.json")).unwrap()).unwrap();

  let store = make_store(&case_dir.join("rust-home"));
  let mut listed_ids: Vec<String> = store.load_session_metas(None).unwrap().into_iter().map(|meta| meta.id).collect();
  listed_ids.sort();
  let mut python_ids: Vec<&str> = expected["listed"].as_object().unwrap().keys().map(String::as_str).collect();
  python_ids.sort();
  assert_eq!(listed_ids.iter().map(String::as_str).collect::<Vec<_>>(), python_ids);
  for meta in store.load_session_metas(None).unwrap() {
    assert_eq!(expected["listed"][meta.id.as_str()]["name"], meta.name.as_str());
    assert_eq!(expected["listed"][meta.id.as_str()]["status"], serde_json::to_value(meta.status).unwrap());
  }

  let mut raw: Vec<String> = store.raw_metadata_scan().unwrap().into_iter().map(|row| row.session_id).collect();
  raw.sort();
  assert_eq!(json!(raw), expected["raw"]);
}

#[tokio::test]
async fn load_all_facts_matches_python_fact_history_over_a_stale_archive_file() {
  let root = tempfile::tempdir().unwrap();
  let case_dir = parity_case_dir(root.path(), "facts");
  let base = case_dir.join("base-home");
  let session_dir = base.join("sessions").join(SESSION_ID);
  let archive_dir = session_dir.join("data").join("archives");
  fs::create_dir_all(&archive_dir).unwrap();
  // One stale row beyond the archive offset: the offset says 2 archived rows, the archive file
  // holds 4, and the count composition reads only through the offset.
  let mut archive_body = String::new();
  for index in 0..4 {
    archive_body.push_str(&format!("{{\"id\": \"archived-{index}\"}}\n"));
  }
  fs::write(archive_dir.join("chat_events.2025-W06.jsonl"), archive_body).unwrap();
  fs::write(
    session_dir.join("data").join("chat_events.jsonl"),
    b"{\"id\": \"live-0\"}\n{\"id\": \"live-1\"}\n{\"id\": \"live-2\"}\n",
  )
  .unwrap();
  write_session_metadata(&base, SESSION_ID, 2);

  copy_tree(&base, &case_dir.join("python-home"));
  copy_tree(&base, &case_dir.join("rust-home"));
  run_python_helper("parity:facts", &case_dir, &[SESSION_ID]);
  let expected: Vec<Value> =
    serde_json::from_str(&fs::read_to_string(case_dir.join("expected.json")).unwrap()).unwrap();

  let store = make_store(&case_dir.join("rust-home"));
  let facts = store.load_all_facts(SESSION_ID).unwrap();
  assert_eq!(serde_json::to_value(&facts).unwrap(), Value::Array(expected));
}
