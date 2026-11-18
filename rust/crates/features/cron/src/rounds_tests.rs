//! Fire routing of prompt tasks, the skips of a due fire, and the bound delegation's refusals, through the real
//! scheduler, the real registry readers, and the tasks router.

use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::registry::HostLogin;
use charliebot_store::metadata::{
  LAST_RUN_STATUS, LAST_SCHEDULED_RUN, SessionMetadata, TASK_SCHEDULE_SLOTS, TaskProfile,
};
use charliebot_store::run_records::{RunKind, RunRecord};
use charliebot_store::{CompactValue, CreateTask, TaskCaller};
use charliebot_tasks::test_support::process_guard::ProcessGroupGuard;
use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::config::ScheduledTaskConfig;
use crate::firing::round_request_id;
use crate::rounds::round_task_spec;
use crate::scheduler::CronScheduler;
use crate::test_support::{
  HELD_ROUND_GOAL, archive_task, bound_delegate, bound_round_leaf, feature_context, feature_context_with_host_backends,
  feature_context_with_repo_root, install_execution, manager, repo_root, set_host_login, write_prompted_task,
  write_task,
};

const GATED_BACKEND: &str = "synthetic-host";
const GATED_ALIAS: &str = "synthetic-login-host";
const FIRING: &str = "2026-11-18T03:00:00+00:00";

/// Tick the scheduler two minutes past the start, so a task that runs every minute is due once.
async fn tick(features: &Arc<FeatureContext>) {
  tick_minutes_after_start(features, 2).await;
}

/// Tick the scheduler `minutes` past the start. A task that runs every minute is due at a tick that is at least a
/// minute after its last fire, so two ticks that run in sequence need minutes that differ by one.
async fn tick_minutes_after_start(features: &Arc<FeatureContext>, minutes: i64) {
  CronScheduler::new(Arc::clone(features)).tick_at(Utc::now() + chrono::Duration::minutes(minutes)).await.unwrap();
}

/// The trigger inputs that are pending in `session_id`.
fn pending_inputs(features: &FeatureContext, session_id: &str) -> Vec<CompactValue> {
  charliebot_tasks::dispatch::dispatcher(features).unwrap().pending_inputs(session_id).unwrap()
}

/// The events of `session_id` of one type.
fn events_of(features: &FeatureContext, session_id: &str, kind: &str) -> Vec<CompactValue> {
  features
    .store
    .load_all_facts(session_id)
    .unwrap()
    .into_iter()
    .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(kind))
    .collect()
}

fn text_of(event: &CompactValue, key: &str) -> String {
  event.get(key).and_then(CompactValue::as_str).unwrap_or_default().to_string()
}

/// A prompt task of the bound session `node_id` that runs every minute, from the task file.
fn prompt_task(features: &FeatureContext, name: &str, node_id: &str, repo: Option<&str>) -> ScheduledTaskConfig {
  let home = features.config.get().unwrap().charliebot_home.clone();
  let mut body =
    json!({"cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node_id, "backend": "synthetic"});
  if let Some(repo) = repo {
    body["repo"] = Value::String(repo.to_string());
  }
  write_prompted_task(&home, name, &body);
  crate::loader::read_snapshot(features, &features.config.get().unwrap())
    .unwrap()
    .0
    .tasks
    .into_iter()
    .find(|task| task.name == name)
    .expect("the written task loads")
}

/// A round of `task` that is queued in `node`: a worker leaf named by the round's request id, with a registered run
/// that has not launched. The run is live, so the round is in flight.
async fn queued_round(features: &Arc<FeatureContext>, task: &ScheduledTaskConfig, node: &SessionMetadata) -> String {
  let leaf = features
    .store
    .create_task(
      CreateTask {
        request_id: round_request_id(task, FIRING),
        task_parent_id: Some(node.id.clone()),
        profile: TaskProfile::Worker,
        task: Some(round_task_spec(task, None)),
        name: None,
        backend: None,
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .unwrap();
  let run = RunRecord {
    id: charliebot_store::control_events::stable_run_id(&leaf.id, "synthetic-queued:work"),
    session_id: leaf.id.clone(),
    kind: RunKind::Work,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some("synthetic".to_string()),
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
  features.store.register_run_locked(&guard, &leaf.id, run, None).await.unwrap();
  leaf.id
}

#[tokio::test]
async fn a_prompt_task_with_a_repo_fires_its_trigger_into_the_bound_session_with_the_round_template() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-routing-node").await.unwrap();
  let _task = prompt_task(&features, "synthetic-routing-repo", &node.id, Some("/synthetic/repo"));

  tick(&features).await;

  let inputs = pending_inputs(&features, &node.id);
  assert_eq!(inputs.len(), 1, "one trigger input");
  assert!(text_of(&inputs[0], "id").starts_with("cron:synthetic-routing-repo:"));
  let content = text_of(&inputs[0], "content");
  let template = std::fs::read_to_string(repo_root().join("prompts/cron/session/round.md")).unwrap();
  assert_eq!(content, format!("task: synthetic-routing-repo\n{template}"));
  assert!(features.store.child_ids(&node.id).unwrap().is_empty(), "the fire starts no worker");
}

#[tokio::test]
async fn a_prompt_task_without_a_repo_gets_the_repo_less_template() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-routing-repo-less-node").await.unwrap();
  let _task = prompt_task(&features, "synthetic-routing-repo-less", &node.id, None);

  tick(&features).await;

  let inputs = pending_inputs(&features, &node.id);
  assert_eq!(inputs.len(), 1);
  let template = std::fs::read_to_string(repo_root().join("prompts/cron/session/repo_less.md")).unwrap();
  assert_eq!(text_of(&inputs[0], "content"), format!("task: synthetic-routing-repo-less\n{template}"));
}

#[tokio::test]
async fn a_master_task_fire_sends_its_own_prompt_without_a_template() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-routing-master-node").await.unwrap();
  let home = features.config.get().unwrap().charliebot_home.clone();
  write_prompted_task(
    &home,
    "synthetic-routing-master",
    &json!({"cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node.id, "mode": "master"}),
  );

  tick(&features).await;

  let inputs = pending_inputs(&features, &node.id);
  assert_eq!(inputs.len(), 1);
  assert_eq!(text_of(&inputs[0], "content"), format!("{HELD_ROUND_GOAL}\n"));
}

#[tokio::test]
async fn an_unreadable_template_posts_an_error_records_a_skip_and_starts_no_worker() {
  let checkout = tempfile::tempdir().unwrap();
  let (_home, features) = feature_context_with_repo_root(checkout.path());
  install_execution(&features).await;
  let node = manager(&features, "synthetic-unreadable-node").await.unwrap();
  let task = prompt_task(&features, "synthetic-unreadable", &node.id, Some("/synthetic/repo"));

  tick(&features).await;

  let template = checkout.path().join("prompts/cron/session/round.md");
  let errors = events_of(&features, &node.id, event_types::ASSISTANT_ERROR);
  assert_eq!(errors.len(), 1, "one error event in the bound session");
  assert!(text_of(&errors[0], "content").contains(&format!("prompt template unreadable: {}", template.display())));
  let skips = events_of(&features, &node.id, event_types::SCHEDULED_RUN_SKIPPED);
  assert_eq!(skips.len(), 1);
  assert_eq!(text_of(&skips[0], "reason"), format!("prompt template unreadable: {}", template.display()));
  assert!(pending_inputs(&features, &node.id).is_empty(), "no trigger input is queued");
  assert!(features.store.child_ids(&node.id).unwrap().is_empty(), "no worker starts");
  let node = features.store.get_session(&node.id).await.unwrap().unwrap();
  let slot = &node.extra[TASK_SCHEDULE_SLOTS][&task.name];
  assert_eq!(slot[LAST_RUN_STATUS], "skipped");
  assert!(slot[LAST_SCHEDULED_RUN].is_string(), "the skipped fire advances the scheduled slot: {slot}");
}

#[tokio::test]
async fn a_queued_trigger_input_of_the_task_skips_the_due_fire_and_another_tasks_input_does_not() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-t1-input-node").await.unwrap();
  let _first = prompt_task(&features, "synthetic-t1-first", &node.id, Some("/synthetic/repo"));
  tick_minutes_after_start(&features, 2).await;
  assert_eq!(pending_inputs(&features, &node.id).len(), 1);

  // The same task is due again while its trigger input is still queued: the fire is skipped with the input's reason.
  tick_minutes_after_start(&features, 3).await;
  let skips = events_of(&features, &node.id, event_types::SCHEDULED_RUN_SKIPPED);
  assert_eq!(skips.len(), 1);
  assert!(text_of(&skips[0], "reason").starts_with("previous round still running (input "), "{skips:?}");
  assert_eq!(pending_inputs(&features, &node.id).len(), 1, "the skipped fire queues no input");

  // Another task of the same session is not blocked by the first task's input.
  let _second = prompt_task(&features, "synthetic-t1-second", &node.id, Some("/synthetic/repo"));
  tick(&features).await;
  let names: Vec<String> = pending_inputs(&features, &node.id).iter().map(|event| text_of(event, "id")).collect();
  assert!(names.iter().any(|name| name.starts_with("cron:synthetic-t1-second:")), "{names:?}");
}

#[tokio::test]
async fn an_open_round_of_the_task_skips_its_due_fire_and_another_tasks_open_round_does_not() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-t1-round-node").await.unwrap();
  let busy = prompt_task(&features, "synthetic-t1-busy", &node.id, Some("/synthetic/repo"));
  let idle = prompt_task(&features, "synthetic-t1-idle", &node.id, Some("/synthetic/repo"));
  let busy_leaf = queued_round(&features, &busy, &node).await;

  tick(&features).await;

  let skips = events_of(&features, &node.id, event_types::SCHEDULED_RUN_SKIPPED);
  assert_eq!(skips.len(), 1, "only the task with an open round skips: {skips:?}");
  assert_eq!(text_of(&skips[0], "task"), "synthetic-t1-busy");
  assert_eq!(text_of(&skips[0], "reason"), format!("previous round still running ({busy_leaf})"));
  let inputs: Vec<String> = pending_inputs(&features, &node.id).iter().map(|event| text_of(event, "id")).collect();
  assert_eq!(inputs.len(), 1, "{inputs:?}");
  assert!(inputs[0].starts_with(&format!("cron:{}:", idle.name)), "{inputs:?}");
}

#[tokio::test]
async fn an_archived_binding_skips_each_due_fire_and_advances_the_slot() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-archived-node").await.unwrap();
  let _task = prompt_task(&features, "synthetic-archived", &node.id, Some("/synthetic/repo"));
  archive_task(&features, &node.id).await;

  tick(&features).await;

  let skips = events_of(&features, &node.id, event_types::SCHEDULED_RUN_SKIPPED);
  assert_eq!(skips.len(), 1, "one skip for the due fire");
  assert_eq!(text_of(&skips[0], "reason"), "binding unavailable: archived");
  assert!(pending_inputs(&features, &node.id).is_empty(), "no trigger input is queued into the archived session");
  assert!(events_of(&features, &node.id, event_types::ASSISTANT_ERROR).is_empty(), "no error per tick");
}

#[tokio::test]
async fn a_round_queued_before_a_restart_skips_the_due_fire_on_a_fresh_scheduler() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-restart-node").await.unwrap();
  let task = prompt_task(&features, "synthetic-restart", &node.id, Some("/synthetic/repo"));
  let previous_version_request_id = format!("cron:{}:{FIRING}:leaf", task.name);
  assert_eq!(round_request_id(&task, FIRING), previous_version_request_id);
  let leaf = queued_round(&features, &task, &node).await;

  // A fresh scheduler holds no state of the first one: the skip reads the persisted round only.
  CronScheduler::new(Arc::clone(&features)).tick_at(Utc::now() + chrono::Duration::minutes(2)).await.unwrap();

  let skips = events_of(&features, &node.id, event_types::SCHEDULED_RUN_SKIPPED);
  assert_eq!(skips.len(), 1);
  assert_eq!(text_of(&skips[0], "reason"), format!("previous round still running ({leaf})"));
}

#[tokio::test]
async fn a_bound_delegation_refuses_a_task_that_names_another_session_or_no_known_task() {
  let (_home, features) = feature_context();
  let node = manager(&features, "synthetic-refuse-node").await.unwrap();
  let other = manager(&features, "synthetic-refuse-other").await.unwrap();
  let _task = prompt_task(&features, "synthetic-refuse-owner", &other.id, Some("/synthetic/repo"));

  let (status, _) = bound_delegate(&features, &node.id, "synthetic-refuse-missing", "").await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  let (status, _) = bound_delegate(&features, &node.id, "synthetic-refuse-owner", "").await;
  assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_bound_delegation_refuses_a_master_steps_or_handler_task_and_a_note_on_a_repo_less_task() {
  let (_home, features) = feature_context();
  let node = manager(&features, "synthetic-kind-node").await.unwrap();
  let home = features.config.get().unwrap().charliebot_home.clone();
  write_prompted_task(
    &home,
    "synthetic-kind-master",
    &json!({"cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node.id, "mode": "master"}),
  );
  write_task(
    &home,
    "synthetic-kind-handler",
    &json!({"cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node.id, "handler": "synthetic_handler"}),
  );
  let step_prompt = home.join("synthetic-kind-step.md");
  std::fs::write(&step_prompt, "Run the first step.").unwrap();
  write_task(
    &home,
    "synthetic-kind-steps",
    &json!({
      "cron": "* * * * *",
      "enabled": true,
      "timezone": "UTC",
      "session_id": node.id,
      "steps": [{"name": "first", "prompt_file": step_prompt.display().to_string()}]
    }),
  );
  let _repo_less = prompt_task(&features, "synthetic-kind-repo-less", &node.id, None);

  for name in ["synthetic-kind-master", "synthetic-kind-steps", "synthetic-kind-handler"] {
    let (status, _) = bound_delegate(&features, &node.id, name, "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
  }
  let (status, body) = bound_delegate(&features, &node.id, "synthetic-kind-repo-less", "a note").await;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
  assert!(features.store.child_ids(&node.id).unwrap().is_empty(), "a refused delegation writes no child session");
}

#[tokio::test]
async fn a_bound_delegation_is_refused_while_a_round_is_in_flight_and_while_the_host_awaits_approval() {
  let (_home, features) = feature_context();
  let node = manager(&features, "synthetic-inflight-node").await.unwrap();
  let task = prompt_task(&features, "synthetic-inflight", &node.id, Some("/synthetic/repo"));
  let leaf = queued_round(&features, &task, &node).await;

  let (status, body) = bound_delegate(&features, &node.id, "synthetic-inflight", "").await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert!(body["detail"].as_str().unwrap().contains(&leaf), "{body}");
  assert_eq!(features.store.child_ids(&node.id).unwrap(), vec![leaf], "the refusal starts no second worker");

  let (_gated_home, gated_features) = feature_context_with_host_backends(&[(GATED_BACKEND, GATED_ALIAS)]);
  let gated_node = manager(&gated_features, "synthetic-gated-node").await.unwrap();
  let gated_home = gated_features.config.get().unwrap().charliebot_home.clone();
  set_host_login(
    &gated_home,
    HostLogin {
      alias: GATED_ALIAS.to_string(),
      status: "needs-interactive-auth".to_string(),
      detail: "Synthetic login check".to_string(),
      last_change_at: "2026-11-18T00:00:00+00:00".to_string(),
      awaits_approval: true,
    },
  );
  let _gated_task = gated_prompt_task(&gated_features, &gated_node.id);
  let (status, body) = bound_delegate(&gated_features, &gated_node.id, "synthetic-gated", "").await;
  assert_eq!(status, StatusCode::CONFLICT, "{body}");
  assert!(body["detail"].as_str().unwrap().contains("needs-interactive-auth"), "{body}");
  assert!(gated_features.store.child_ids(&gated_node.id).unwrap().is_empty());
}

fn gated_prompt_task(features: &FeatureContext, node_id: &str) -> ScheduledTaskConfig {
  let home = features.config.get().unwrap().charliebot_home.clone();
  write_prompted_task(
    &home,
    "synthetic-gated",
    &json!({"cron": "* * * * *", "enabled": true, "timezone": "UTC", "session_id": node_id, "backend": GATED_BACKEND}),
  );
  crate::loader::read_snapshot(features, &features.config.get().unwrap())
    .unwrap()
    .0
    .tasks
    .into_iter()
    .find(|task| task.name == "synthetic-gated")
    .expect("the gated task loads")
}

#[tokio::test]
#[ignore = "integration"]
async fn a_bound_delegation_starts_a_round_whose_worker_is_named_by_its_round_request_id() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-bound-round-node").await.unwrap();
  let task = prompt_task(&features, "synthetic-bound-round", &node.id, Some("/synthetic/repo"));

  let (leaf, run) = bound_round_leaf(&features, &task, "Synthetic session note").await;

  assert_eq!(leaf.task_parent_id.as_deref(), Some(node.id.as_str()));
  let spec = leaf.task.as_ref().unwrap();
  assert!(spec.goal.contains("## Session note\n\nSynthetic session note"), "{}", spec.goal);
  assert_eq!(spec.base_branch, None);
  assert_eq!(spec.task_type, None);
  assert_eq!(run.kind, RunKind::Work);
  let created = features.store.load_all_facts(&leaf.id).unwrap();
  let request_id = created
    .iter()
    .find(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_CREATED))
    .and_then(|event| event.get("request_id"))
    .and_then(CompactValue::as_str)
    .unwrap()
    .to_string();
  assert!(request_id.starts_with("cron:synthetic-bound-round:") && request_id.ends_with(":leaf"), "{request_id}");
}

#[tokio::test]
#[ignore = "integration"]
async fn two_concurrent_bound_delegations_of_one_task_start_one_worker_and_refuse_the_other() {
  let (_home, features) = feature_context();
  install_execution(&features).await;
  let node = manager(&features, "synthetic-concurrent-node").await.unwrap();
  // A repo-less task: its round needs no worktree, and its prompt holds the worker's process open long enough for the
  // second delegation to meet it in flight.
  let _task = prompt_task(&features, "synthetic-concurrent", &node.id, None);
  let processes = ProcessGroupGuard::new();

  let (first, second) = tokio::join!(
    bound_delegate(&features, &node.id, "synthetic-concurrent", ""),
    bound_delegate(&features, &node.id, "synthetic-concurrent", ""),
  );
  let mut statuses = [first.0, second.0];
  statuses.sort();
  assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT], "one starts the round, the other is refused");
  let accepted = if first.0 == StatusCode::OK { &first.1 } else { &second.1 };
  let child = accepted["session_id"].as_str().expect("the accepted delegation names its worker");

  // The held prompt keeps the run live; track its process so the test ends even on a later failure.
  let pid = tokio::time::timeout(std::time::Duration::from_secs(15), async {
    loop {
      if let Some(pid) = features.store.list_run_records_sync(child).unwrap().first().and_then(|run| run.pid) {
        return pid;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the accepted round launches its process");
  processes.track(pid);
  assert_eq!(features.store.child_ids(&node.id).unwrap(), vec![child.to_string()], "exactly one worker exists");
}
