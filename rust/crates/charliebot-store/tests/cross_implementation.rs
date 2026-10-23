use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;

use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_core::proc_identity;
use charliebot_core::run_token;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType, ThreadMetadata, UtcDateTime};
use charliebot_store::run_records::terminal_outcome;
use charliebot_store::session_fork::CloseReportRedelivery;
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, ControlGuard, CreateTask, EmptyRuntimeEventHooks, RetryRunFields,
  RunBlocker, RunKind, RunOrder, RunOutcome, RunOutcomeKind, RunPage, RunRecord, SequenceArchiveHook,
  SequenceBindingMover, SequenceListingFields, StopResult, Store, StoreError, StoreHookFuture, StoreHooks, TaskCaller,
};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgid};
use regex::Regex;
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
  "mark_read",
  "rename_session",
  "set_group",
  "rename_group",
  "delete_group",
  "switch_backend",
  "archive_session",
  "unarchive_session",
  "delete_session_permanently",
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
    if *operation != "delete_session_permanently" {
      let python_metadata = python_store.read_metadata_fresh(SESSION_ID).unwrap().unwrap();
      assert_eq!(python_metadata.extra("left_by_deleted_package").unwrap()["values"][1], 2.5);
    }

    if *operation == "event_append" {
      let python_events = python_store.load_chat_events_sync(SESSION_ID).unwrap();
      assert_eq!(python_events.len(), 1);
      assert_eq!(python_events[0]["id"].as_str(), Some("synthetic-event-id"));
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
        "mark_read" => {
          assert!(!rust_store.mark_read(SESSION_ID).await.unwrap().unwrap().has_unread);
        }
        "rename_session" => {
          rust_store.rename_session_at(SESSION_ID, "renamed session", fixed).await.unwrap();
        }
        "set_group" => {
          rust_store.set_group_at(SESSION_ID, Some("new-group"), fixed).await.unwrap();
        }
        "rename_group" => {
          assert_eq!(rust_store.rename_group_at("old-group", "new-group", fixed).await.unwrap(), 1);
        }
        "delete_group" => {
          assert_eq!(rust_store.delete_group_at("old-group", fixed).await.unwrap(), 1);
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
        "delete_session_permanently" => {
          assert!(rust_store.delete_session_permanently(SESSION_ID).await.unwrap());
        }
        "update_slot_fields" => {
          let mut values = Map::new();
          values.insert("slack_watermark_ts".to_string(), json!("new-slack-watermark"));
          rust_store.update_slot_fields(SESSION_ID, "slack", values).await.unwrap();
        }
        _ => unreachable!("the operation list names every operation"),
      }
    }

    if *operation == "delete_session_permanently" {
      assert!(!rust_store.session_dir(SESSION_ID).exists());
      assert!(!python_store.session_dir(SESSION_ID).exists());
    } else {
      let rust_metadata_path = rust_store.metadata_path(SESSION_ID);
      let python_metadata_path = python_store.metadata_path(SESSION_ID);
      assert_eq!(fs::read(&rust_metadata_path).unwrap(), fs::read(&python_metadata_path).unwrap(), "{operation}");
    }
    if *operation != "delete_session_permanently" {
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
    }
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
  let range: Vec<Value> =
    store.load_chat_events_range(SESSION_ID, 0, 1).unwrap().0.iter().map(|event| event.to_value()).collect();
  assert_eq!(
    range,
    [json!({
      "type": "user", "id": "old", "timestamp": "2025-02-01T00:00:00Z"
    })]
  );
  assert_eq!(store.load_chat_events_range(SESSION_ID, 1, 2).unwrap().0[0]["id"].as_str(), Some("new"));
  assert_eq!(store.find_event_by_id(SESSION_ID, "old").unwrap().unwrap()["id"].as_str(), Some("old"));

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

fn live_log_path(home: &Path) -> PathBuf {
  home.join("sessions").join(SESSION_ID).join("data").join("chat_events.jsonl")
}

fn archive_log_path(home: &Path) -> PathBuf {
  home.join("sessions").join(SESSION_ID).join("data").join("archives").join("chat_events.2025-W06.jsonl")
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
  match fs::read(path) {
    Ok(bytes) => Some(bytes),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
    Err(error) => panic!("{}: {error}", path.display()),
  }
}

/// `src/runtime/chat_events.py::archive_old_chat_events_sync` and `Store::archive_events_before` on the same live-log
/// bytes, one input per case. Each case must match in the failure outcome, the archived count, and the live and
/// archive bytes; a failed call must also leave the live log, the archive and `archive_offset` unchanged.
#[tokio::test]
async fn archive_events_before_matches_python_archive_old_chat_events_sync() {
  let cutoff = UtcDateTime::parse("2025-02-03T00:00:00Z").unwrap();
  let cases: [(&str, &[u8]); 9] = [
    (
      "invalid_utf8_in_kept_line",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\
        {\"id\": \"kept\xff\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
    ),
    (
      "crlf_lines",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\r\n\
        {\"id\": \"old-two\", \"timestamp\": \"2025-02-01T01:00:00Z\"}\r\n\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\r\n",
    ),
    (
      "lone_cr_between_events",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\r\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
    ),
    (
      "blank_line_of_x1c",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\x1c\n\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
    ),
    (
      "json_array_line",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n[1, 2]\n\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
    ),
    (
      "numeric_timestamp",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\
        {\"id\": \"numeric\", \"timestamp\": 1738368000}\n\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}\n",
    ),
    (
      "zero_timestamp",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\
        {\"id\": \"zero\", \"timestamp\": 0}\n\
        {\"id\": \"old-two\", \"timestamp\": \"2025-02-01T01:00:00Z\"}\n",
    ),
    (
      "empty_timestamp",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\
        {\"id\": \"empty\", \"timestamp\": \"\"}\n\
        {\"id\": \"old-two\", \"timestamp\": \"2025-02-01T01:00:00Z\"}\n",
    ),
    (
      "last_line_without_newline",
      b"{\"id\": \"old\", \"timestamp\": \"2025-02-01T00:00:00Z\"}\n\
        {\"id\": \"new\", \"timestamp\": \"2025-02-04T00:00:00Z\"}",
    ),
  ];
  let root = tempfile::tempdir().unwrap();
  for (name, live) in cases {
    let python_home = root.path().join(name).join("python-home");
    let rust_home = root.path().join(name).join("rust-home");
    for home in [&python_home, &rust_home] {
      let path = live_log_path(home);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, live).unwrap();
    }
    write_session_metadata(&rust_home, SESSION_ID, 0);
  }

  let names: Vec<&str> = cases.iter().map(|(name, _)| *name).collect();
  run_python_helper("archive_cases", root.path(), &names);
  let python: Value =
    serde_json::from_str(&fs::read_to_string(root.path().join("archive-cases.json")).unwrap()).unwrap();

  for (name, live) in cases {
    let python_home = root.path().join(name).join("python-home");
    let rust_home = root.path().join(name).join("rust-home");
    let store = make_store(&rust_home);
    let metadata_before = fs::read(store.metadata_path(SESSION_ID)).unwrap();

    let result = store.archive_events_before(SESSION_ID, cutoff).await;
    let rust_failed = result.is_err();
    let rust_live = fs::read(live_log_path(&rust_home)).unwrap();
    let rust_archive = read_optional(&archive_log_path(&rust_home));
    let rust_count = rust_archive.as_ref().map_or(0, |bytes| bytes.iter().filter(|byte| **byte == b'\n').count());

    let python_failed = python[name]["failed"].as_bool().unwrap();
    let python_count = python[name]["events_archived"].as_u64().unwrap() as usize;
    let python_live = fs::read(live_log_path(&python_home)).unwrap();
    let python_archive = read_optional(&archive_log_path(&python_home));

    println!(
      "{name}: python failed={python_failed} archived={python_count}; rust failed={rust_failed} archived={rust_count}"
    );
    assert_eq!(rust_failed, python_failed, "{name}: failure outcome");
    assert_eq!(rust_count, python_count, "{name}: archived count");
    assert_eq!(rust_live, python_live, "{name}: live log bytes");
    assert_eq!(rust_archive, python_archive, "{name}: archive bytes");
    if rust_failed {
      assert_eq!(rust_live, live, "{name}: a failed archive leaves the live log unchanged");
      assert!(rust_archive.is_none(), "{name}: a failed archive writes no archive file");
      assert_eq!(fs::read(store.metadata_path(SESSION_ID)).unwrap(), metadata_before, "{name}: archive_offset");
    } else {
      assert_eq!(store.read_metadata_fresh(SESSION_ID).unwrap().unwrap().archive_offset, rust_count as i64);
    }
  }
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
async fn python_and_rust_task_trees_share_nodes_gates_and_mutation_bytes() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_tasks", root.path(), &[]);

  let python_home = root.path().join("python-task-home");
  let rust_home = root.path().join("rust-task-home");
  copy_tree(&root.path().join("task-base"), &python_home);
  copy_tree(&root.path().join("task-base"), &rust_home);

  let python_seed = make_store(&python_home);
  let rust_seed = make_store(&rust_home);
  for store in [&python_seed, &rust_seed] {
    let task_root = store.read_metadata_fresh("synthetic-task-root").unwrap().unwrap();
    assert_eq!(task_root.profile, charliebot_store::metadata::TaskProfile::Manager);
    assert_eq!(store.child_ids(&task_root.id).unwrap().len(), 3);
    assert_eq!(store.task_state("synthetic-task-worker").unwrap(), "open");
    assert_eq!(store.last_ended_run_id("synthetic-task-worker").unwrap().as_deref(), Some("synthetic-failed-run"));
  }

  let reverse_home = root.path().join("rust-created-home");
  let reverse_store = make_store(&reverse_home);
  for (session_id, request_id, parent) in [
    ("rust-created-root", "reverse-root-request", None),
    ("rust-created-child", "reverse-child-request", Some("rust-created-root")),
  ] {
    reverse_store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile: charliebot_store::metadata::TaskProfile::Manager,
          task: None,
          name: Some(request_id.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: Some(session_id.to_string()),
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
  }
  run_python_helper("verify_rust_created", root.path(), &[]);

  run_python_helper("mutate_tasks", root.path(), &["python-task-home"]);
  let child_request = CreateTask {
    request_id: "replayed-child-request".to_string(),
    task_parent_id: Some("synthetic-task-root".to_string()),
    profile: charliebot_store::metadata::TaskProfile::Manager,
    task: None,
    name: Some("replayed child".to_string()),
    backend: Some("synthetic".to_string()),
    group: None,
    session_id: None,
  };
  let child = rust_seed.create_task(child_request.clone(), TaskCaller::System, Map::new()).await.unwrap();
  let child_event_count = rust_seed.load_chat_events_sync(&child.id).unwrap().len();
  let replay = rust_seed
    .create_task(
      CreateTask { name: Some("ignored replay name".to_string()), ..child_request },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .unwrap();
  assert_eq!(replay.id, child.id);
  assert_eq!(rust_seed.load_chat_events_sync(&child.id).unwrap().len(), child_event_count);
  let guard = rust_seed.control_lock().await;
  let mut slot_values = Map::new();
  slot_values.insert("slack_watermark_ts".to_string(), json!("updated-watermark"));
  rust_seed.update_task_slot_fields_locked(&guard, "synthetic-task-worker", "slack", slot_values).await.unwrap();
  assert!(
    rust_seed
      .archive_unsuccessful_worker(&guard, "synthetic-task-worker", "synthetic-failed-run", "failed",)
      .await
      .unwrap()
  );
  assert!(
    !rust_seed
      .archive_unsuccessful_worker(&guard, "synthetic-task-worker", "synthetic-failed-run", "failed",)
      .await
      .unwrap()
  );
  assert!(
    rust_seed.rebind_elone_bindings_locked(&guard, "synthetic-elone-source", "synthetic-elone-new").await.unwrap()
  );
  drop(guard);

  run_python_helper("verify_tasks", root.path(), &["python-task-home"]);
  run_python_helper("verify_tasks", root.path(), &["rust-task-home"]);
  assert_eq!(normalized_files(&python_home), normalized_files(&rust_home));
  rust_seed.invalidate_cache("synthetic-task-worker");
  rust_seed.invalidate_tree_index();
  assert_eq!(rust_seed.task_state("synthetic-task-worker").unwrap(), "archived");
  assert_eq!(
    rust_seed.read_metadata_fresh("synthetic-task-worker").unwrap().unwrap().extra("slack_watermark_ts"),
    Some(&json!("updated-watermark"))
  );
}

#[tokio::test]
async fn python_and_rust_name_an_unnamed_node_from_a_corrupt_counter_the_same_way() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_counter", root.path(), &[]);

  let python_home = root.path().join("python-counter-home");
  let rust_home = root.path().join("rust-counter-home");
  copy_tree(&root.path().join("counter-base"), &python_home);
  copy_tree(&root.path().join("counter-base"), &rust_home);

  run_python_helper("create_unnamed_node", root.path(), &["python-counter-home"]);
  let node = make_store(&rust_home)
    .create_task(
      CreateTask {
        request_id: "counter-request".to_string(),
        task_parent_id: Some("synthetic-counter-root".to_string()),
        profile: charliebot_store::metadata::TaskProfile::Manager,
        task: None,
        name: None,
        backend: Some("synthetic".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .unwrap();
  assert_eq!(node.name, "Session 2");
  assert_eq!(normalized_files(&python_home), normalized_files(&rust_home));
}

#[tokio::test]
async fn python_and_rust_archive_a_subtree_into_the_same_fact_bytes() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_archive", root.path(), &[]);

  let python_home = root.path().join("python-archive-home");
  let rust_home = root.path().join("rust-archive-home");
  copy_tree(&root.path().join("archive-base"), &python_home);
  copy_tree(&root.path().join("archive-base"), &rust_home);

  run_python_helper("archive_subtree", root.path(), &["python-archive-home"]);
  let archived = make_store(&rust_home).archive_subtree("synthetic-archive-root", TaskCaller::Operator).await.unwrap();
  assert_eq!(archived, ["synthetic-archive-root", "synthetic-archive-mid", "synthetic-archive-leaf"]);
  let leaf_facts =
    fs::read_to_string(rust_home.join("sessions/synthetic-archive-leaf/data/chat_events.jsonl")).unwrap();
  assert!(leaf_facts.contains("\"archived_with\": \"synthetic-archive-root\""));
  assert_eq!(normalized_files(&python_home), normalized_files(&rust_home));
}

const FORK_ROOT_ID: &str = "synthetic-fork-root";

/// The fork and elone cases of `python_store_ops.py::FORK_CASES`, in the same order.
const FORK_CASES: [(&str, &str, Option<i64>); 9] = [
  ("fork-null", "fork", None),
  ("fork-first", "fork", Some(0)),
  ("fork-middle", "fork", Some(2)),
  ("fork-last", "fork", Some(3)),
  ("fork-past-end", "fork", Some(4)),
  ("elone-first", "elone", Some(0)),
  ("elone-middle", "elone", Some(2)),
  ("elone-last", "elone", Some(3)),
  ("elone-past-end", "elone", Some(4)),
];

/// The elone redelivery step with no subtree close reports to redeliver: the fixture root has no children.
struct NoRedelivery;

impl CloseReportRedelivery for NoRedelivery {
  fn redeliver_close_reports_locked<'a>(
    &'a self,
    _guard: &'a ControlGuard,
    _session_id: &'a str,
  ) -> StoreHookFuture<'a, ()> {
    Box::pin(async { Ok(()) })
  }
}

/// The session directory names under a home.
fn session_names(home: &Path) -> BTreeSet<String> {
  fs::read_dir(home.join("sessions"))
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .collect()
}

/// The session directories created since the base home was copied.
fn fresh_sessions(home: &Path, base: &BTreeSet<String>) -> Vec<String> {
  session_names(home).difference(base).cloned().collect()
}

/// The home's normalized files, with each fresh session id replaced by one name: each implementation draws its own.
fn normalized_home(home: &Path, base: &BTreeSet<String>) -> BTreeMap<PathBuf, Vec<u8>> {
  let fresh = fresh_sessions(home, base);
  normalized_files(home)
    .into_iter()
    .map(|(path, bytes)| {
      let mut path = path.to_string_lossy().into_owned();
      for id in &fresh {
        path = path.replace(id.as_str(), "generated-session");
      }
      (PathBuf::from(path), bytes)
    })
    .collect()
}

#[tokio::test]
async fn python_and_rust_fork_and_elone_write_the_same_session_files_at_every_index() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_fork", root.path(), &[]);
  let base = root.path().join("fork-base");
  let base_sessions = session_names(&base);
  run_python_helper("fork_cases", root.path(), &[]);
  let python_outcomes: BTreeMap<String, Value> =
    serde_json::from_str(&fs::read_to_string(root.path().join("fork-outcomes.json")).unwrap()).unwrap();

  for (name, operation, event_index) in FORK_CASES {
    let python_home = root.path().join(format!("python-{name}"));
    let rust_home = root.path().join(format!("rust-{name}"));
    copy_tree(&base, &rust_home);
    let store = make_store(&rust_home);
    let rust = if operation == "fork" {
      store.fork_session(FORK_ROOT_ID, event_index, Some("synthetic")).await.map(|_| ())
    } else {
      let index = event_index.expect("an elone case names an index");
      store.elone_session(FORK_ROOT_ID, index, Some("synthetic"), &NoRedelivery).await.map(|_| ())
    };
    let rust_refused = match rust {
      Ok(()) => false,
      Err(StoreError::Invalid(_)) => true,
      Err(error) => panic!("{name}: unexpected store error {error:?}"),
    };
    assert_eq!(python_outcomes[name]["outcome"] == "refused", rust_refused, "{name}: refusal differs");
    let created = usize::from(!rust_refused);
    assert_eq!(fresh_sessions(&python_home, &base_sessions).len(), created, "{name}: python session dirs");
    assert_eq!(fresh_sessions(&rust_home, &base_sessions).len(), created, "{name}: rust session dirs");
    assert_eq!(normalized_home(&python_home, &base_sessions), normalized_home(&rust_home, &base_sessions), "{name}");
  }
}

#[tokio::test]
async fn python_and_rust_delegate_a_task_spec_into_the_same_node_run_and_spec_bytes() {
  let root = tempfile::tempdir().unwrap();
  run_python_helper("prepare_delegate", root.path(), &[]);
  let base = root.path().join("delegate-base");

  // A run's task_spec_ref is an absolute path, so both implementations write under one home path, in turn.
  let shared_home = root.path().join("delegate-shared-home");
  copy_tree(&base, &shared_home);
  run_python_helper("delegate_task", root.path(), &["delegate-shared-home"]);
  let python_home = root.path().join("delegate-python-result");
  copy_tree(&shared_home, &python_home);
  fs::remove_dir_all(&shared_home).unwrap();
  copy_tree(&base, &shared_home);

  let spec = TaskSpec {
    goal: "## Goal\n\nShip the synthetic change.\n\nKeep the public API.".to_string(),
    acceptance: vec!["the synthetic tests pass".to_string()],
    context_refs: vec!["docs/synthetic.md".to_string()],
    repo_path: Some("/home/synthetic/repo".to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    keep_worktree: true,
    cold_read: None,
  };
  let store = make_store(&shared_home);
  let child = store
    .create_task(
      CreateTask {
        request_id: "delegate-request".to_string(),
        task_parent_id: Some("synthetic-delegate-root".to_string()),
        profile: TaskProfile::Worker,
        task: Some(spec.clone()),
        name: None,
        backend: Some("synthetic".to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Agent { session_id: "synthetic-delegate-root".to_string() },
      Map::new(),
    )
    .await
    .unwrap();
  assert_eq!(child.name, "Ship the synthetic change.");
  let guard = store.control_lock().await;
  let record = RunRecord {
    backend: Some("synthetic".to_string()),
    repo_path: spec.repo_path.clone(),
    base_branch: spec.base_branch.clone(),
    session_id: child.id.clone(),
    ..empty_run("synthetic-delegate-run", RunKind::Work)
  };
  store.register_run_locked(&guard, &child.id, record, Some(&serde_json::to_string(&spec).unwrap())).await.unwrap();
  drop(guard);

  let run_dir = shared_home.join("sessions").join(&child.id).join("data/runs/synthetic-delegate-run");
  assert_eq!(fs::read_to_string(run_dir.join("task_spec.md")).unwrap(), serde_json::to_string(&spec).unwrap());
  let created = store
    .load_chat_events_sync(&child.id)
    .unwrap()
    .into_iter()
    .find(|event| event["type"].as_str() == Some("task_created"))
    .unwrap();
  assert!(created["task_spec_hash"].is_string());
  assert_eq!(normalized_files(&python_home), normalized_files(&shared_home));
}

#[tokio::test]
async fn task_gate_matches_python_cases_and_the_parity_fixture_tree() {
  let root = tempfile::tempdir().unwrap();
  let cases = materialize_gate_cases();
  fs::write(root.path().join("gate-cases.json"), serde_json::to_vec(&json!({"cases": cases})).unwrap()).unwrap();
  run_python_helper("gate_parity", root.path(), &[]);
  let expected: Value = serde_json::from_slice(&fs::read(root.path().join("gate-results.json")).unwrap()).unwrap();
  let store = make_store(&root.path().join("gate-cases-home"));
  for case in &cases {
    let name = case["name"].as_str().unwrap();
    let ids = &case["node_ids"];
    let (root_id, child_id, grandchild_id) = (
      ids["root"].as_str().unwrap().to_string(),
      ids["child"].as_str().unwrap().to_string(),
      ids["grandchild"].as_str().unwrap().to_string(),
    );
    let parents = &case["parents"];
    let profiles = &case["profiles"];
    let events = &case["events"];
    let states = &case["states"];
    let mut session_ids = HashMap::new();
    session_ids.insert("root", root_id);
    session_ids.insert("child", child_id);
    session_ids.insert("grandchild", grandchild_id);
    for node in ["root", "child", "grandchild"] {
      let session_id = session_ids[node].clone();
      let parent = parents[&session_id].as_str().map(str::to_string);
      let profile = match profiles[&session_id].as_str().unwrap() {
        "manager" => charliebot_store::metadata::TaskProfile::Manager,
        "worker" => charliebot_store::metadata::TaskProfile::Worker,
        profile => panic!("unknown gate profile {profile:?}"),
      };
      store
        .create_task(
          CreateTask {
            request_id: format!("{name}-{node}-request"),
            task_parent_id: parent,
            profile,
            task: None,
            name: Some(format!("{name} {node}")),
            backend: Some("synthetic".to_string()),
            group: None,
            session_id: Some(session_id.clone()),
          },
          TaskCaller::System,
          Map::new(),
        )
        .await
        .unwrap();
    }
    for node in ["root", "child", "grandchild"] {
      let session_id = session_ids[node].clone();
      for event in events[&session_id].as_array().unwrap() {
        store.append_chat_event(&session_id, event.clone()).await.unwrap();
      }
      if let Some(outcome) = states[&session_id].as_str() {
        let guard = store.control_lock().await;
        let close = charliebot_store::control_events::build_control_event(
          "task_closed",
          charliebot_store::control_events::ACTOR_SYSTEM,
          &session_id,
          None,
          None,
          [("outcome".to_string(), json!(outcome))],
        );
        store.append_control_fact(&guard, &session_id, close).await.unwrap();
      }
    }
    let start_id = case["start"].as_str().unwrap();
    let actual = match store.check_task_authorization(start_id) {
      Ok(user_node) => json!({"authorized": true, "user_node": user_node, "reason": null}),
      Err(error) => json!({"authorized": false, "user_node": null, "reason": error.to_string()}),
    };
    assert_eq!(actual, expected[name], "gate case {name}");
  }

  run_python_helper("prepare_parity_gate", root.path(), &[]);
  let fixture_home = root.path().join("parity-fixture").join("home");
  let rust_fixture_home = root.path().join("parity-fixture-rust-home");
  copy_tree(&fixture_home.join("sessions"), &rust_fixture_home.join("sessions"));
  let fixture_store = make_store(&rust_fixture_home);
  let fixture_sessions: Vec<String> =
    fs::read_to_string(root.path().join("parity-fixture-sessions.txt")).unwrap().lines().map(str::to_string).collect();
  let fixture_expected: Value =
    serde_json::from_slice(&fs::read(root.path().join("parity-gate-results.json")).unwrap()).unwrap();
  assert!(!fixture_sessions.is_empty());
  for session_id in fixture_sessions {
    let actual = match fixture_store.check_task_authorization(&session_id) {
      Ok(user_node) => json!({"authorized": true, "user_node": user_node, "reason": null}),
      Err(error) => json!({"authorized": false, "user_node": null, "reason": error.to_string()}),
    };
    assert_eq!(actual, fixture_expected[&session_id], "parity fixture task {session_id}");
  }
}

#[tokio::test]
async fn stop_checks_process_identity_and_reconciles_after_the_fact_is_durable() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());

  let mut mismatched = ordinary_sleep("30");
  let pid = mismatched.pid;
  let actual_start = mismatched.pid_start.clone();
  register_process_run(&store, "run-mismatch", pid, "forged-start").await;
  let stop_error = store.request_stop(RUN_SESSION_ID, "run-mismatch", "mismatch-stop").await.unwrap_err();
  let remained_alive = mismatched.try_wait().is_none();
  assert!(stop_error.to_string().contains("identity mismatch"));
  assert!(remained_alive);
  assert!(store.load_all_facts(RUN_SESSION_ID).unwrap().iter().any(|event| event["type"].as_str()
    == Some("run_stop_requested")
    && event["run_id"].as_str() == Some("run-mismatch")));
  assert_ne!(actual_start, "forged-start");

  let mut reconciled = isolated_sleep("30");
  let pid = reconciled.pid;
  let pid_start = reconciled.pid_start.clone();
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
  let process_exited = reconciled.try_wait().is_some();
  assert!(process_exited);
  assert_eq!(result.outcome, Some(RunOutcomeKind::Interrupted));
  assert_eq!(
    store.run_display_state(RUN_SESSION_ID, "run-reconcile").unwrap(),
    charliebot_store::RunDisplayState::Interrupted
  );
}

#[tokio::test]
async fn stop_request_overrides_a_later_successful_finish() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, empty_run("run-stop-finish", RunKind::Work), None).await.unwrap();
  drop(guard);

  store.request_stop(RUN_SESSION_ID, "run-stop-finish", "cancel:request").await.unwrap();
  let guard = store.control_lock().await;
  store
    .record_finish_locked(&guard, RUN_SESSION_ID, "run-stop-finish", RunOutcome::new(RunOutcomeKind::Success))
    .await
    .unwrap();
  drop(guard);

  let events = store.load_all_facts(RUN_SESSION_ID).unwrap();
  assert_eq!(terminal_outcome(&events, "run-stop-finish").unwrap(), Some(RunOutcomeKind::Interrupted));
}

// Python's record_finish_locked keeps the first write: a later finish fills only an empty ended_at, so the stopper's
// interrupted record without an exit code stays (runs.py:1036-1047, the stopper's write at runs.py:1161).
#[tokio::test]
async fn stopped_run_keeps_its_null_exit_code_after_a_late_process_finish() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let guard = store.control_lock().await;
  store.register_run_locked(&guard, RUN_SESSION_ID, empty_run("run-late-exit", RunKind::Work), None).await.unwrap();
  let stop_event = charliebot_store::control_events::build_control_event(
    "run_stop_requested",
    charliebot_store::control_events::ACTOR_SYSTEM,
    RUN_SESSION_ID,
    Some("run-late-exit-stop-event"),
    Some(UtcDateTime::now()),
    [("request_id".to_string(), json!("cancel:late-exit")), ("run_id".to_string(), json!("run-late-exit"))],
  );
  store.append_control_fact(&guard, RUN_SESSION_ID, stop_event).await.unwrap();
  store
    .record_finish_locked(&guard, RUN_SESSION_ID, "run-late-exit", RunOutcome::new(RunOutcomeKind::Interrupted))
    .await
    .unwrap();
  let mut process_finish = RunOutcome::new(RunOutcomeKind::Failed);
  process_finish.exit_code = Some(-15);
  store.record_finish_locked(&guard, RUN_SESSION_ID, "run-late-exit", process_finish).await.unwrap();
  drop(guard);

  let run = store.read_run_sync(RUN_SESSION_ID, "run-late-exit").unwrap().unwrap();
  let events = store.load_all_facts(RUN_SESSION_ID).unwrap();
  assert_eq!(run.exit_code, None);
  assert_eq!(terminal_outcome(&events, "run-late-exit").unwrap(), Some(RunOutcomeKind::Interrupted));
}

#[tokio::test]
async fn stop_escalates_to_sigkill_when_the_process_ignores_sigterm() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let mut process = isolated_shell("trap '' TERM; exec sleep 30");
  let pid = process.pid;
  let pid_start = process.pid_start.clone();
  let guard = store.control_lock().await;
  store
    .register_run_locked(&guard, RUN_SESSION_ID, launched_process_run("run-ignore-term", pid, &pid_start), None)
    .await
    .unwrap();
  drop(guard);

  let result = store.request_stop(RUN_SESSION_ID, "run-ignore-term", "stop-escalate").await.unwrap();
  assert_eq!(result.outcome, Some(RunOutcomeKind::Interrupted));
  assert!(process.try_wait().is_some());
}

#[tokio::test]
async fn stop_refuses_a_process_that_is_not_its_own_group_leader() {
  let root = tempfile::tempdir().unwrap();
  let store = make_store(root.path());
  let mut process = ordinary_sleep("30");
  let pid = process.pid;
  let pid_start = process.pid_start.clone();
  let process_group_id = process.process_group_id;
  assert_ne!(process_group_id, pid as i32);
  let guard = store.control_lock().await;
  store
    .register_run_locked(&guard, RUN_SESSION_ID, launched_process_run("run-wrong-group", pid, &pid_start), None)
    .await
    .unwrap();
  drop(guard);

  let result = store.request_stop(RUN_SESSION_ID, "run-wrong-group", "stop-wrong-group").await;
  assert!(result.is_err(), "a process outside its own process group must not be signalled");
  assert!(process.try_wait().is_none());
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
  assert!(
    !store
      .load_all_facts(RUN_SESSION_ID)
      .unwrap()
      .iter()
      .any(|event| event["type"].as_str() == Some("run_stop_requested"))
  );

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
async fn last_ended_run_ranks_a_run_by_its_last_finish_fact_like_python() {
  let root = tempfile::tempdir().unwrap();
  let seed_home = root.path().join("last_ended").join("seed-home");
  let store = make_store(&seed_home);
  register_runs(&store, vec![launched_run("run-early", 1), launched_run("run-late", 2)]).await;
  finish_run(&store, "run-late", Some("2026-01-01T00:05:00Z")).await;
  // run-early has no ended_at mirror, so its last finish fact times it.
  append_finish_fact(&store, "run-early", "2026-01-01T00:03:00Z", "success").await;
  append_finish_fact(&store, "run-early", "2026-01-01T00:09:00Z", "failed").await;

  let (python, rust) = read_on_copies(root.path(), &seed_home, "last_ended", read_last_ended);
  assert_eq!(python, rust);
  assert_eq!(rust["last_ended_run_id"], "run-early");
}

#[tokio::test]
async fn terminal_outcome_is_the_last_finish_fact_like_python() {
  let root = tempfile::tempdir().unwrap();
  let seed_home = root.path().join("terminal_outcome").join("seed-home");
  let store = make_store(&seed_home);
  register_runs(&store, vec![launched_run("run-two-finishes", 1)]).await;
  finish_run(&store, "run-two-finishes", None).await;
  append_finish_fact(&store, "run-two-finishes", "2026-01-01T00:05:00Z", "failed").await;

  let (python, rust) = read_on_copies(root.path(), &seed_home, "terminal_outcome", read_terminal_outcome);
  assert_eq!(python, rust);
  assert_eq!(rust["terminal_outcome"], "failed");
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

/// Append a run_finished fact directly, so one run can carry more than one.
async fn append_finish_fact(store: &Store, run_id: &str, timestamp: &str, outcome: &str) {
  let event = charliebot_store::control_events::build_control_event(
    "run_finished",
    charliebot_store::control_events::ACTOR_SYSTEM,
    RUN_SESSION_ID,
    None,
    Some(UtcDateTime::parse(timestamp).unwrap()),
    [
      ("run_id".to_string(), json!(run_id)),
      ("input_event_ids".to_string(), json!([])),
      ("outcome".to_string(), json!(outcome)),
    ],
  );
  let guard = store.control_lock().await;
  store.append_control_fact(&guard, RUN_SESSION_ID, event).await.unwrap();
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

struct GuardedProcess {
  child: Child,
  pid: u32,
  pid_start: String,
  process_group_id: i32,
}

impl GuardedProcess {
  fn isolated(command: &str, args: &[&str]) -> Self {
    let mut command = Command::new(command);
    command.args(args).process_group(0);
    Self::spawn(command)
  }

  fn ordinary(command: &str, args: &[&str]) -> Self {
    let mut command = Command::new(command);
    command.args(args);
    Self::spawn(command)
  }

  fn spawn(mut command: Command) -> Self {
    let child = command.spawn().unwrap();
    let pid = child.id();
    let pid_start = proc_identity::read_pid_stat(pid).unwrap().pid_start;
    let process_group_id = getpgid(Some(Pid::from_raw(pid as i32))).unwrap().as_raw();
    GuardedProcess { child, pid, pid_start, process_group_id }
  }

  fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
    self.child.try_wait().unwrap()
  }
}

impl Drop for GuardedProcess {
  fn drop(&mut self) {
    let identity_matches = proc_identity::read_pid_stat(self.pid).is_some_and(|stat| stat.pid_start == self.pid_start);
    if identity_matches && self.process_group_id == self.pid as i32 {
      let _ = killpg(Pid::from_raw(self.process_group_id), Signal::SIGKILL);
    } else if identity_matches {
      let _ = self.child.kill();
    }
    let _ = self.child.wait();
  }
}

fn isolated_sleep(seconds: &str) -> GuardedProcess {
  GuardedProcess::isolated("sleep", &[seconds])
}

fn isolated_shell(script: &str) -> GuardedProcess {
  GuardedProcess::isolated("sh", &["-c", script])
}

fn ordinary_sleep(seconds: &str) -> GuardedProcess {
  GuardedProcess::ordinary("sleep", &[seconds])
}

fn launched_process_run(run_id: &str, pid: u32, pid_start: &str) -> RunRecord {
  let mut run = empty_run(run_id, RunKind::Work);
  run.pid = Some(pid);
  run.pid_start = Some(pid_start.to_string());
  run.started_at = Some(UtcDateTime::now());
  run
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
      sequence_archive: Some(Arc::new(TestSequenceArchive)),
      sequence_binding_mover: Some(Arc::new(TestSequenceBindingMover)),
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
      fields: vec![
        SlotField { name: "backend_from_pool", default: json!(false) },
        SlotField { name: "route", default: Value::Null },
      ],
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

struct TestSequenceArchive;

impl SequenceArchiveHook for TestSequenceArchive {
  fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async { Ok(()) })
  }
}

struct TestSequenceBindingMover;

impl SequenceBindingMover for TestSequenceBindingMover {
  fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async { Ok(true) })
  }
}

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
}

fn run_python_helper(mode: &str, root: &Path, extra: &[&str]) {
  let root = root.to_string_lossy();
  let mut args = vec![mode, root.as_ref()];
  args.extend_from_slice(extra);
  let _ = run_python_helper_args(&args);
}

fn run_python_message_aggregator(events: &Value) -> Value {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf();
  let dir = tempfile::tempdir().unwrap();
  let events_path = dir.path().join("events.json");
  fs::write(&events_path, serde_json::to_vec(events).unwrap()).unwrap();
  let helper = "rust/crates/charliebot-store/tests/python_message_aggregator.py";
  let output =
    Command::new("uv").args(["run", "python", helper]).arg(&events_path).current_dir(repo_root).output().unwrap();
  assert!(
    output.status.success(),
    "python message aggregator helper failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  serde_json::from_slice(&output.stdout).unwrap()
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
  let empty_started_at_cursor = raw_run_cursor(&json!({ "s": "", "i": "queued-0" }));
  let empty_started_at =
    store.list_runs_page_sync(RUN_SESSION_ID, Some(&empty_started_at_cursor), 2, RunOrder::Ascending).unwrap();
  let truthy_order_cursor = raw_run_cursor(&json!({ "s": "2026-01-01T00:04:00+00:00", "i": "run-4", "d": "false" }));
  let truthy_order_flag =
    store.list_runs_page_sync(RUN_SESSION_ID, Some(&truthy_order_cursor), 2, RunOrder::Descending).unwrap();
  json!({
    "ascending": page_json(&ascending),
    "descending": page_json(&descending),
    "descending_next": page_json(&descending_next),
    "empty_started_at": page_json(&empty_started_at),
    "truthy_order_flag": page_json(&truthy_order_flag),
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

fn raw_run_cursor(payload: &Value) -> String {
  run_token::b64url_encode(serde_json::to_string(payload).unwrap().as_bytes())
}

fn page_json(page: &RunPage) -> Value {
  json!({ "ids": page.items.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(), "next_cursor": page.next_cursor })
}

fn refusal<T>(result: Result<T, StoreError>) -> String {
  result.err().expect("the cursor is refused under the other order").to_string()
}

fn read_last_ended(store: &Store) -> Value {
  json!({ "last_ended_run_id": store.last_ended_run_id(RUN_SESSION_ID).unwrap() })
}

fn read_terminal_outcome(store: &Store) -> Value {
  let events = store.load_all_facts(RUN_SESSION_ID).unwrap();
  json!({ "terminal_outcome": terminal_outcome(&events, "run-two-finishes").unwrap() })
}

fn read_blockers(store: &Store) -> Value {
  let reasons =
    store.run_blockers(RUN_SESSION_ID).unwrap().into_iter().map(|blocker| blocker.reason).collect::<Vec<_>>();
  json!({ "blockers": reasons })
}

fn normalized_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
  fn normalize(text: &str) -> Vec<u8> {
    use std::sync::OnceLock;

    static UUID: OnceLock<Regex> = OnceLock::new();
    static TIMESTAMP: OnceLock<Regex> = OnceLock::new();
    let uuid =
      UUID.get_or_init(|| Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap());
    let timestamp = TIMESTAMP
      .get_or_init(|| Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})").unwrap());
    let mut ids = HashMap::<String, String>::new();
    let with_ids = uuid.replace_all(text, |captures: &regex::Captures<'_>| {
      let value = captures.get(0).unwrap().as_str();
      let next = ids.len();
      ids.entry(value.to_string()).or_insert_with(|| format!("generated-id-{next}")).clone()
    });
    timestamp.replace_all(&with_ids, "<wall-clock>").as_bytes().to_vec()
  }

  fn collect(root: &Path, directory: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(directory).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      if entry.file_type().unwrap().is_dir() {
        collect(root, &path, output);
      } else {
        let bytes = fs::read(&path).unwrap();
        let normalized = match std::str::from_utf8(&bytes) {
          Ok(text) => normalize(text),
          Err(_) => bytes,
        };
        output.insert(path.strip_prefix(root).unwrap().to_path_buf(), normalized);
      }
    }
  }

  let mut output = BTreeMap::new();
  collect(root, root, &mut output);
  output
}

fn gate_node_ids(name: &str) -> (String, String, String) {
  (format!("gate-{name}-root"), format!("gate-{name}-child"), format!("gate-{name}-grandchild"))
}

fn materialize_gate_cases() -> Vec<Value> {
  gate_cases()
    .into_iter()
    .map(|mut case| {
      let name = case["name"].as_str().unwrap();
      let (root, child, grandchild) = gate_node_ids(name);
      let ids = HashMap::from([("root", root), ("child", child), ("grandchild", grandchild)]);
      for key in ["parents", "profiles", "states", "events"] {
        let source = case[key].as_object().unwrap().clone();
        let mut mapped = Map::new();
        for (node, value) in source {
          let node_id = &ids[node.as_str()];
          let value = if key == "parents" {
            value.as_str().map_or(Value::Null, |parent| Value::String(ids[parent].clone()))
          } else {
            value.clone()
          };
          mapped.insert(node_id.clone(), value);
        }
        case[key] = Value::Object(mapped);
      }
      let start = case["start"].as_str().unwrap();
      case["start"] = Value::String(ids[start].clone());
      case["node_ids"] = json!({"root": ids["root"], "child": ids["child"], "grandchild": ids["grandchild"]});
      case
    })
    .collect()
}

fn gate_cases() -> Vec<Value> {
  let manager_profiles = json!({"root": "manager", "child": "manager", "grandchild": "manager"});
  let empty_states = json!({});
  let mut cases = Vec::new();
  let mut add = |name: &str, start: &str, events: Value, states: Value, profiles: Value| {
    cases.push(json!({
      "name": name,
      "start": start,
      "parents": {"root": null, "child": "root", "grandchild": "child"},
      "profiles": profiles,
      "states": states,
      "events": events,
    }));
  };
  add(
    "user-at-root",
    "root",
    json!({"root": [{"id": "root-user", "type": "user", "content": "Inspect the flaky test."}], "child": [], "grandchild": []}),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "user-at-child",
    "child",
    json!({"root": [], "child": [{"id": "child-user", "type": "user", "content": "Which file holds it?"}], "grandchild": []}),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "grandchild-inherits-root",
    "grandchild",
    json!({
      "root": [{"id": "root-user", "type": "user", "content": "Check the test."}],
      "child": [{"type": "agent_message", "content": "progress"}],
      "grandchild": [{"type": "agent_message", "content": "progress"}]
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "nearest-user-on-chain",
    "grandchild",
    json!({
      "root": [{"id": "root-user", "type": "user", "content": "Check the test."}],
      "child": [{"id": "child-user", "type": "user", "content": "Which file holds it?"}],
      "grandchild": [{"type": "agent_message", "content": "progress"}]
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  for state in ["completed", "cancelled", "archived"] {
    add(
      &format!("closed-root-{state}"),
      "grandchild",
      json!({
        "root": [{"id": "root-user", "type": "user", "content": "Ship it?"}],
        "child": [],
        "grandchild": [{"id": "child-user", "type": "user", "content": "Go ahead."}]
      }),
      json!({"root": state}),
      manager_profiles.clone(),
    );
    add(
      &format!("closed-start-{state}"),
      "grandchild",
      json!({
        "root": [{"id": "root-user", "type": "user", "content": "Ship it?"}],
        "child": [],
        "grandchild": [{"id": "child-user", "type": "user", "content": "Go ahead."}]
      }),
      json!({"grandchild": state}),
      manager_profiles.clone(),
    );
  }
  let machine = json!([
    {"type": "agent_message", "content": "take off"},
    {"type": "scheduled_trigger", "content": "take off"},
    {"type": "child_report", "content": "take off"},
    {"type": "user", "content": [{"type": "tool_result", "content": "take off"}]}
  ]);
  // Python: tests/test_run_token.py:188. A recorded thread summon on the root is a user request for the chain below it.
  let slack_summon = json!({
    "type": "agent_message",
    "content": "Slack 线程召唤",
    "slack": {"channel_id": "C123", "thread_ts": "1700000000.000100", "mention_ts": "1700000000.000200"}
  });
  let legacy_slack_summon = json!({
    "type": "agent_message",
    "content": "Slack 线程召唤：帮我看下这个 flaky 的测试",
    "slack": {"channel_id": "C123", "thread_ts": "1700000000.000100", "mention_ts": "1700000000.000200"}
  });
  let discord_summon = json!({
    "type": "agent_message",
    "content": "Discord thread summon",
    "discord": {"guild_id": "G123", "channel_id": "C123", "thread_id": "T123", "mention_id": "M123"}
  });
  for (name, summon) in [
    ("summon-slack-at-root", slack_summon.clone()),
    ("summon-legacy-slack-at-root", legacy_slack_summon),
    ("summon-discord-at-root", discord_summon),
  ] {
    add(
      name,
      "grandchild",
      json!({"root": [summon], "child": [], "grandchild": []}),
      empty_states.clone(),
      manager_profiles.clone(),
    );
  }
  // Python: tests/test_run_token.py:226. A thread nudge, a plain agent message, a web-reply nudge, a scheduled trigger
  // and a child report each leave the chain without a user request.
  let refusals = [
    (
      "slack-nudge-only",
      json!({
        "type": "agent_message",
        "content": "Slack 线程召唤 nudge",
        "slack": {"channel_id": "C123", "thread_ts": "1700000000.000100", "nudge_of": "1700000000.000200"}
      }),
    ),
    ("plain-agent-message-only", json!({"type": "agent_message", "content": "take off"})),
    (
      "web-reply-nudge-only",
      json!({"type": "agent_message", "content": "web reply nudge", "web_reply": {"nudge_of": "1700000000.000200"}}),
    ),
    ("scheduled-trigger-only", json!({"type": "scheduled_trigger", "content": "take off"})),
    ("child-report-only", json!({"type": "child_report", "content": "take off"})),
  ];
  for (name, event) in refusals {
    add(
      name,
      "grandchild",
      json!({"root": [event], "child": [], "grandchild": []}),
      empty_states.clone(),
      manager_profiles.clone(),
    );
  }
  // Python: tests/test_run_token.py:233. A summon before the node's newest clone_start belongs to the source; one after
  // it counts.
  add(
    "summon-before-clone-marker",
    "child",
    json!({
      "root": [{"type": "agent_message", "content": "take off"}],
      "child": [slack_summon.clone(), {"type": "clone_start", "content": ""}],
      "grandchild": []
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "summon-after-clone-marker",
    "child",
    json!({
      "root": [{"type": "agent_message", "content": "take off"}],
      "child": [{"type": "clone_start", "content": ""}, slack_summon],
      "grandchild": []
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "machine-input-only",
    "grandchild",
    json!({"root": machine.clone(), "child": machine.clone(), "grandchild": machine}),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "clone-marker-clears-copied-user",
    "child",
    json!({
      "root": [],
      "child": [
        {"id": "source-user", "type": "user", "content": "the source request"},
        {"type": "clone_start"}
      ],
      "grandchild": []
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "clone-marker-keeps-own-user",
    "child",
    json!({
      "root": [],
      "child": [
        {"type": "clone_start"},
        {"id": "own-user", "type": "user", "content": "the child's own request"}
      ],
      "grandchild": []
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "later-clone-marker-clears-own-user",
    "child",
    json!({
      "root": [],
      "child": [
        {"type": "clone_start"},
        {"id": "own-user", "type": "user", "content": "the child's own request"},
        {"type": "clone_start"}
      ],
      "grandchild": []
    }),
    empty_states.clone(),
    manager_profiles.clone(),
  );
  add(
    "worker-start-is-refused",
    "grandchild",
    json!({"root": [{"type": "user", "content": "An unrelated root request."}], "child": [], "grandchild": []}),
    empty_states,
    json!({"root": "manager", "child": "manager", "grandchild": "worker"}),
  );
  drop(add);
  cases
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
    let event_values = Value::Array(events.iter().map(|event| event.to_value()).collect());
    assert_eq!(event_values, window["events"], "window {window}");
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
  assert_eq!(Value::Array(facts.iter().map(|event| event.to_value()).collect()), Value::Array(expected));
}

// The legacy layouts Python's readers still walk: a master run's raw log under
// `data/master_runs/<started_at>/` and a pre-task-tree thread's event log under
// `threads/<tid>/data/` (src/runtime/runs.py's DATA_DIR_NAME / MASTER_RUNS_DIR_NAME /
// THREADS_DIR_NAME). The store's readers (Python runs.py, Rust run_records.rs) must keep
// agreeing on these names.
#[test]
fn legacy_master_run_and_thread_log_readers_walk_the_python_layouts() {
  let home = tempfile::tempdir().unwrap();
  let sessions = home.path().join("sessions");
  let session = sessions.join("legacy-session");
  let master_log = session.join("data").join("master_runs").join("2025-02-03T04:05:06Z").join("agent.raw.ndjson");
  fs::create_dir_all(master_log.parent().unwrap()).unwrap();
  fs::write(&master_log, b"{}\n").unwrap();
  let thread_log = session.join("threads").join("legacy-thread").join("data").join("events.jsonl");
  fs::create_dir_all(thread_log.parent().unwrap()).unwrap();
  fs::write(&thread_log, b"{}\n").unwrap();
  // A non-legacy sibling the readers must not pick up: a task-tree run dir and a session file.
  let task_tree_run = session.join("data").join("runs").join("2025-02-03T04:05:07Z").join("agent.raw.ndjson");
  fs::create_dir_all(task_tree_run.parent().unwrap()).unwrap();
  fs::write(&task_tree_run, b"{}\n").unwrap();

  let store = make_store(home.path());
  assert_eq!(store.master_run_log_paths_sync().unwrap(), vec![master_log]);
  assert_eq!(store.thread_event_log_paths_sync().unwrap(), vec![thread_log]);
}

/// Every content and metadata shape that Python's aggregator iterates must feed the Rust port too.
///
/// A native transcript event carries `message.content` as a plain string rather than a list of
/// blocks; Python iterates the characters and ignores them, while the port used to reject the
/// whole event. The other fields here follow the same `or []` and truthiness rules.
#[test]
fn message_aggregator_accepts_every_shape_python_accepts() {
  let events = json!([
    {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "draft"}]}},
    {
      "type": "user",
      "message": {"role": "user", "content": "synthetic native prompt"},
      "parentUuid": null,
      "sessionId": "synthetic-session",
      "uuid": "synthetic-native-event",
      "timestamp": "2026-01-01T00:00:00Z",
    },
    {"type": "user", "message": {"role": "user", "content": {"nested": true}}},
    {"type": "assistant", "message": {"role": "assistant", "content": "plain string content"}},
    {"type": "assistant", "message": {"role": "assistant", "content": {"nested": "object"}}},
    {"type": "assistant", "message": {"role": "assistant", "content": [
      {"type": "tool_use", "name": "ExitPlanMode", "input": {"plan": {"steps": ["one"]}}}
    ]}},
    {"type": "model_fallback_notice", "served_models": "abc", "configured_model": "synthetic-model"},
    {"type": "task_input_acknowledged", "input_ids": "xy", "note": "synthetic note"},
    {
      "type": "run_header",
      "kind": "work",
      "launched": 1,
      "launch_failed": 0,
      "tool_calls": "2",
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {
      "type": "run_header",
      "kind": "work",
      "launched": true,
      "launch_failed": false,
      "tool_calls": "1_000",
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {
      "type": "run_header",
      "kind": "work",
      "launched": true,
      "launch_failed": false,
      "tool_calls": -0.75,
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {
      "type": "run_header",
      "kind": "work",
      "launched": true,
      "launch_failed": false,
      "tool_calls": "9223372036854775808",
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {
      "type": "run_header",
      "kind": "work",
      "launched": true,
      "launch_failed": false,
      "tool_calls": 2.75,
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {
      "type": "run_header",
      "kind": "work",
      "launched": true,
      "launch_failed": false,
      "tool_calls": true,
      "task_spec_ref": "synthetic-spec",
      "launch_prompt_ref": "synthetic-prompt",
      "state": "done",
    },
    {"type": "context_compacted", "trigger": "auto", "compact_metadata": {"pre_tokens": true, "post_tokens": 1000}},
  ]);
  let mut aggregator = charliebot_store::aggregator::MessageAggregator::new(0, true);
  let mut rust_frames = Vec::new();
  for event in events.as_array().unwrap() {
    rust_frames.extend(aggregator.feed(event).unwrap());
  }
  assert_eq!(Value::Array(rust_frames), run_python_message_aggregator(&events));
}
