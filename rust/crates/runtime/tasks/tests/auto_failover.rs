//! The Auto session's mid-turn failover: a member that runs out of quota during a manager turn continues the turn
//! on the next member, as a retry run that carries the switch note.

use std::sync::Arc;
use std::time::{Duration, Instant};

use charliebot_agents::backend_availability;
use charliebot_core::config::{BackendOption, PoolBilling, PoolConfig, PoolMember, PoolRule};
use charliebot_core::event_types;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{self, RunKind};
use charliebot_store::{CreateTask, RunRecord, TaskCaller};
use charliebot_tasks::handoff;
use charliebot_tasks::master_queue::PersistAndBroadcastFn;
use charliebot_tasks::master_run::{FINISH_ERROR_MESSAGE, FINISH_QUOTA_EXHAUSTED};
use charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME;
use charliebot_tasks::test_support::recovery_support::{BACKEND_TYPE, Stack, script_option, synthetic_run};
use serde_json::{Map, Value, json};

const MEMBER_A: &str = "member-a";
const MEMBER_B: &str = "member-b";
/// The error text aigw returns when the key's budget is spent.
const QUOTA_ERROR: &str = "API Error: 402 Insufficient Balance";

/// The id of the priority pool that the Auto sessions of these tests run on.
const AUTO_POOL_ID: &str = "auto";

/// The bound every wait in this file uses. The integration runner shares its CPUs with the build and the other
/// selected tests, so a state that takes milliseconds locally can take seconds there; a wait reaches this bound only
/// when the state never arrives, and a wait that reaches it names the state it awaited.
const TEST_DEADLINE: Duration = Duration::from_secs(120);

/// A scripted option billed as a subscription, the billing the Auto pool's members have.
fn subscription(id: &str, script: &str) -> BackendOption {
  BackendOption { billing: Some(PoolBilling::Subscription), ..script_option(id, script) }
}

fn auto_set() -> PoolConfig {
  PoolConfig {
    id: AUTO_POOL_ID.to_string(),
    label: "Auto".to_string(),
    rule: PoolRule::Priority,
    members: vec![PoolMember { backend: MEMBER_A.to_string() }, PoolMember { backend: MEMBER_B.to_string() }],
    visible: true,
  }
}

/// One scripted process that prints `events` and exits with `exit_code`.
fn scripted_process(events: &[Value], exit_code: i32) -> String {
  let lines = events.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
  format!("cat <<'CB_EVENTS'\n{lines}\nCB_EVENTS\nexit {exit_code}\n")
}

/// The tool calls a member makes before its quota runs out.
fn quota_events() -> Vec<Value> {
  vec![
    json!({"type":"assistant","message":{"content":[
      {"type":"text","text":"I read the entry point."},
      {"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"/synthetic/src/main.rs"}}
    ]}}),
    json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"fn main() {}"}]}}),
    json!({"type":"assistant","message":{"content":[
      {"type":"tool_use","id":"tool-2","name":"Bash","input":{"command":"cargo test -p cron"}}
    ]}}),
  ]
}

fn quota_error_result() -> Value {
  json!({"type":"result","subtype":"error","is_error":true,"result":QUOTA_ERROR})
}

/// The model-access refusal the fifth review run of the cron-dropdown task ended on.
const ACCESS_REFUSED: &str = "Failed to authenticate. API Error: 401 Not allowed to access model due to tags \
                               configuration. Passed model=deepseek-flash and tags=['deepseek']";

fn access_refused_result() -> Value {
  json!({"type":"result","subtype":"error","is_error":true,"api_error_status":401,"result":ACCESS_REFUSED})
}

fn success_script() -> String {
  scripted_process(
    &[
      json!({"type":"assistant","message":{"content":[{"type":"text","text":"Continued on the next member."}]}}),
      json!({"type":"result","subtype":"success","result":"continued"}),
    ],
    0,
  )
}

async fn auto_stack(script_a: &str, script_b: &str) -> anyhow::Result<Stack> {
  let stack = Stack::start_with_auto(
    vec![subscription(MEMBER_A, script_a), subscription(MEMBER_B, script_b)],
    Vec::new(),
    auto_set(),
  )
  .await?;
  stack.write_access_key()?;
  Ok(stack)
}

async fn auto_session(stack: &Stack, request_id: &str) -> anyhow::Result<SessionMetadata> {
  Ok(
    stack
      .store()
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Auto session".to_string()),
          backend: Some(AUTO_POOL_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?,
  )
}

fn quota_failovers(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .run_events(session_id)?
      .iter()
      .map(|event| event.to_value())
      .filter(|event| event["type"] == event_types::QUOTA_FAILOVER)
      .collect(),
  )
}

/// Wait until every registered run has a terminal fact.
async fn settle(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<RunRecord>> {
  let deadline = Instant::now() + TEST_DEADLINE;
  loop {
    let events = stack.run_events(session_id)?;
    let runs = stack.run_records(session_id)?;
    let open = runs
      .iter()
      .map(|run| run_records::terminal_outcome(&events, &run.id))
      .collect::<charliebot_store::StoreResult<Vec<_>>>()?
      .into_iter()
      .any(|outcome| outcome.is_none());
    if !open && !runs.is_empty() {
      return Ok(runs);
    }
    anyhow::ensure!(Instant::now() < deadline, "the session's runs did not settle");
    // A run settles in tens of milliseconds, and the 0.1 s unit-test budget counts this sleep's full length.
    tokio::time::sleep(Duration::from_millis(2)).await;
  }
}

/// Wait until the turn's retry run has launched: both runs are registered and the retry's launch text is on disk.
///
/// The launch text is the last fact the caller asserts, so this wait ends when the retry starts rather than when
/// it finishes.
async fn wait_for_retry_launch(stack: &Stack, session_id: &str) -> anyhow::Result<Vec<RunRecord>> {
  let deadline = Instant::now() + TEST_DEADLINE;
  loop {
    let runs = stack.run_records(session_id)?;
    if let Some(retry) = runs.iter().find(|run| run.retry_of_run_id.is_some()) {
      let launch_text = stack.store().run_dir(session_id, &retry.id).join(LAUNCH_TEXT_FILENAME);
      if launch_text.is_file() {
        return Ok(runs);
      }
    }
    anyhow::ensure!(Instant::now() < deadline, "the turn's retry run did not launch");
    tokio::time::sleep(Duration::from_millis(2)).await;
  }
}

async fn dispatch_turn(stack: &Stack, session_id: &str) -> anyhow::Result<()> {
  stack.dispatch.admit_input(session_id, event_types::USER, "fix the failing test", "user", json!({})).await?;
  stack.dispatch.dispatch_pending(session_id).await?;
  Ok(())
}

fn retry_of<'a>(runs: &'a [RunRecord], original: &RunRecord) -> &'a RunRecord {
  runs.iter().find(|run| run.retry_of_run_id.as_deref() == Some(original.id.as_str())).expect("the retry run")
}

fn original_run(runs: &[RunRecord]) -> &RunRecord {
  runs.iter().find(|run| run.retry_of_run_id.is_none()).expect("the turn's first run")
}

/// Tests ①, ② and ⑥: a 402 mid-turn continues on the next member with one switch record, and the retry's input
/// carries the absolute context paths and the four-part summary even though the session has no native id.
#[tokio::test]
#[ignore = "integration"]
async fn a_quota_end_continues_the_turn_on_the_next_member_with_the_switch_note() -> anyhow::Result<()> {
  let mut failing = quota_events();
  failing.push(quota_error_result());
  let stack = auto_stack(&scripted_process(&failing, 1), &success_script()).await?;
  let session = auto_session(&stack, "auto-failover").await?;
  dispatch_turn(&stack, &session.id).await?;
  let runs = wait_for_retry_launch(&stack, &session.id).await?;

  assert_eq!(runs.len(), 2, "the turn continued as one retry run: {runs:?}");
  let original = original_run(&runs);
  let retry = retry_of(&runs, original);
  assert_eq!(original.backend.as_deref(), Some(MEMBER_A));
  assert_eq!(retry.backend.as_deref(), Some(MEMBER_B));
  assert_eq!(retry.kind, RunKind::ManagerTurn);
  assert_eq!(retry.input_event_ids, original.input_event_ids, "the retry reuses the turn's input");
  assert!(!retry.input_event_ids.is_empty());

  let switches = quota_failovers(&stack, &session.id)?;
  assert_eq!(switches.len(), 1, "the chat holds one switch record: {switches:?}");
  assert_eq!(switches[0]["from"], MEMBER_A);
  assert_eq!(switches[0]["to"], MEMBER_B);
  assert_eq!(switches[0]["reason"], event_types::QUOTA_REASON_QUOTA_ERROR);
  assert_eq!(switches[0]["set"], AUTO_POOL_ID);
  assert_eq!(switches[0]["summary"], handoff::SUMMARY_OK);

  let availability = backend_availability::backend_availability(&stack.features)?;
  let remaining =
    availability.out_until(MEMBER_A).expect("the spent member is recorded out") - UtcDateTime::now().into_inner();
  assert!(remaining > time::Duration::minutes(29) && remaining <= backend_availability::DEFAULT_COOLDOWN);

  let stored = stack.store().read_metadata_fresh(&session.id)?.expect("the session is recorded");
  assert_eq!(stored.extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD), Some(&json!(MEMBER_B)));

  // The retry's input names the session chat log, the archives and the failed run's directory as absolute paths, and
  // the four summary parts. The session has no native id, so the switch note is the only context it carries.
  let prompt = std::fs::read_to_string(stack.store().run_dir(&session.id, &retry.id).join(LAUNCH_TEXT_FILENAME))?;
  let session_dir = stack.store().session_dir(&session.id);
  let chat_log = charliebot_store::layout::chat_events_path(&session_dir);
  let archives = charliebot_store::layout::archives_dir(&session_dir);
  let failed_run_dir = stack.store().run_dir(&session.id, &original.id);
  assert!(chat_log.is_absolute() && archives.is_absolute() && failed_run_dir.is_absolute());
  assert!(prompt.contains(&chat_log.display().to_string()), "{prompt}");
  assert!(prompt.contains(&archives.display().to_string()), "{prompt}");
  assert!(prompt.contains(&failed_run_dir.display().to_string()), "{prompt}");
  assert!(prompt.contains("pool 'auto' switched from member-a to member-b"), "{prompt}");
  // A manager's retry carries the manager note: the orientation order and the sort. Its Goals sit in its prompt.
  assert!(
    prompt.contains("get oriented from your session status, then your child tasks, then the last two turns"),
    "{prompt}"
  );
  assert!(prompt.contains(&format!("Run `charliebot session tree --root {}`", session.id)), "{prompt}");
  assert!(prompt.contains("Then sort the parts of your task"), "{prompt}");
  assert!(prompt.contains("Handoff summary:"), "{prompt}");
  assert!(prompt.contains("done (1):"), "{prompt}");
  assert!(prompt.contains("- $ Read "), "{prompt}");
  assert!(prompt.contains("in progress: $ Bash cargo test -p cron (no result yet)"), "{prompt}");
  assert!(prompt.contains("files: /synthetic/src/main.rs"), "{prompt}");
  Ok(())
}

/// Test ②'s failed assembly: the note still carries the paths, and the note and the switch record both say the
/// summary is unavailable with its reason.
#[tokio::test]
#[ignore = "integration"]
async fn a_failed_summary_keeps_the_paths_and_reports_the_reason() -> anyhow::Result<()> {
  let gate = tempfile::tempdir()?;
  let release_path = gate.path().join("release");
  let lines = quota_events().iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
  // The hold's poll step bounds how late the script sees the release file, and the 0.1 s unit-test budget counts it.
  let failing = format!(
    "cat <<'CB_EVENTS'\n{lines}\nCB_EVENTS\nwhile [ ! -e '{}' ]; do /bin/sleep 0.002; done\n\
     printf '%s\\n' '{}'\nexit 1\n",
    release_path.display(),
    quota_error_result()
  );
  let stack = auto_stack(&failing, &success_script()).await?;
  let session = auto_session(&stack, "auto-summary-unavailable").await?;
  dispatch_turn(&stack, &session.id).await?;
  // The first run holds before its error result, so the test knows its id and can break its summary assembly.
  let original = wait_for_run(&stack, &session.id).await?;
  handoff::force_summary_failure_for_test(&original.id, "synthetic assembly failure");
  std::fs::write(&release_path, "release")?;
  let runs = wait_for_retry_launch(&stack, &session.id).await?;
  handoff::clear_summary_failure_for_test(&original.id);

  let retry = retry_of(&runs, &original);
  let switches = quota_failovers(&stack, &session.id)?;
  assert_eq!(switches.len(), 1);
  assert_eq!(switches[0]["summary"], "unavailable: synthetic assembly failure");

  let prompt = std::fs::read_to_string(stack.store().run_dir(&session.id, &retry.id).join(LAUNCH_TEXT_FILENAME))?;
  let session_dir = stack.store().session_dir(&session.id);
  assert!(prompt.contains(&charliebot_store::layout::chat_events_path(&session_dir).display().to_string()), "{prompt}");
  assert!(prompt.contains(&stack.store().run_dir(&session.id, &original.id).display().to_string()), "{prompt}");
  assert!(
    prompt.contains("Handoff summary unavailable: synthetic assembly failure"),
    "the note states the same reason as the switch record: {prompt}"
  );
  Ok(())
}

/// A refusal mid-turn continues on the next member the way a quota end does, records the member with the refusal
/// reason, and hands the retry a note that names the refusal.
#[tokio::test]
#[ignore = "integration"]
async fn an_access_refused_turn_continues_on_the_next_member_with_the_refusal_reason() -> anyhow::Result<()> {
  let mut failing = quota_events();
  failing.push(access_refused_result());
  let stack = auto_stack(&scripted_process(&failing, 1), &success_script()).await?;
  let session = auto_session(&stack, "auto-access-refused").await?;
  dispatch_turn(&stack, &session.id).await?;
  let runs = wait_for_retry_launch(&stack, &session.id).await?;

  assert_eq!(runs.len(), 2, "the turn continued as one retry run: {runs:?}");
  let original = original_run(&runs);
  let retry = retry_of(&runs, original);
  assert_eq!(retry.backend.as_deref(), Some(MEMBER_B));

  let switches = quota_failovers(&stack, &session.id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["from"], MEMBER_A);
  assert_eq!(switches[0]["to"], MEMBER_B);
  assert_eq!(switches[0]["reason"], event_types::QUOTA_REASON_ACCESS_REFUSED);

  let availability = backend_availability::backend_availability(&stack.features)?;
  let row = availability.row(MEMBER_A).expect("the refused member is recorded out");
  assert_eq!(row.reason, backend_availability::REASON_ACCESS_REFUSED);
  let remaining = row.out_until.into_inner() - UtcDateTime::now().into_inner();
  assert!(remaining > time::Duration::minutes(29) && remaining <= backend_availability::DEFAULT_COOLDOWN);

  let prompt = std::fs::read_to_string(stack.store().run_dir(&session.id, &retry.id).join(LAUNCH_TEXT_FILENAME))?;
  assert!(prompt.contains("pool 'auto' switched from member-a to member-b"), "{prompt}");
  assert!(prompt.contains("the member refused access"), "{prompt}");
  assert!(!prompt.contains("the member's quota is spent"), "{prompt}");
  Ok(())
}

/// Test ③: with every member out, the turn ends with one error that lists each member.
#[tokio::test]
#[ignore = "integration"]
async fn an_exhausted_set_ends_the_turn_with_one_error_listing_every_member() -> anyhow::Result<()> {
  let stack = auto_stack(&success_script(), &success_script()).await?;
  let session = auto_session(&stack, "auto-exhausted").await?;
  let availability = backend_availability::backend_availability(&stack.features)?;
  let reset = UtcDateTime::parse("2999-01-02T03:04:05Z")?;
  for member in [MEMBER_A, MEMBER_B] {
    availability.record(member, Some(reset), backend_availability::REASON_QUOTA_ERROR, QUOTA_ERROR)?;
  }
  dispatch_turn(&stack, &session.id).await?;
  let runs = settle(&stack, &session.id).await?;

  let errors = stack
    .run_events(&session.id)?
    .iter()
    .map(|event| event.to_value())
    .filter(|event| event["type"] == event_types::ASSISTANT_ERROR)
    .collect::<Vec<_>>();
  assert_eq!(errors.len(), 1, "one error ends the turn: {errors:?}");
  assert_eq!(errors[0]["quota_exhausted"], json!(true));
  let content = errors[0]["content"].as_str().expect("the error carries text");
  for line in [
    "- member-a (subscription): quota_error, recovers at 2999-01-02T03:04+00:00",
    "- member-b (subscription): quota_error, recovers at 2999-01-02T03:04+00:00",
  ] {
    assert!(content.contains(line), "{content}");
  }
  assert_eq!(runs.len(), 1, "the exhausted turn launches no member: {runs:?}");
  assert_eq!(runs[0].pid, None);
  assert!(quota_failovers(&stack, &session.id)?.is_empty());
  Ok(())
}

/// Acceptance ③'s manager half: a transient error (5xx) ends the manager turn on its member with the error; the turn
/// switches no member, records none out, and starts no retry.
#[tokio::test]
#[ignore = "integration"]
async fn a_transient_error_ends_the_turn_without_switching_members() -> anyhow::Result<()> {
  let failure = scripted_process(
    &[
      json!({"type":"assistant","message":{"content":[{"type":"text","text":"I tried."}]}}),
      json!({"type":"result","subtype":"error","is_error":true,"result":"API Error: 500 Internal Server Error"}),
    ],
    1,
  );
  let stack = auto_stack(&failure, &success_script()).await?;
  let session = auto_session(&stack, "auto-transient").await?;
  dispatch_turn(&stack, &session.id).await?;
  let runs = settle(&stack, &session.id).await?;

  assert_eq!(runs.len(), 1, "a transient error starts no retry: {runs:?}");
  assert_eq!(runs[0].backend.as_deref(), Some(MEMBER_A), "the turn stays on its member");
  assert!(quota_failovers(&stack, &session.id)?.is_empty(), "a transient error switches no member");
  let errors = stack
    .run_events(&session.id)?
    .iter()
    .map(|event| event.to_value())
    .filter(|event| event["type"] == event_types::ASSISTANT_ERROR)
    .collect::<Vec<_>>();
  assert_eq!(errors.len(), 1, "the turn ends with one error: {errors:?}");
  let content = errors[0]["content"].as_str().expect("the error carries text");
  assert!(content.contains("500"), "the error is the run's own: {content}");
  let availability = backend_availability::backend_availability(&stack.features)?;
  assert!(availability.out_until(MEMBER_A).is_none(), "a transient error records no member out");
  let stored = stack.store().read_metadata_fresh(&session.id)?.expect("the session is recorded");
  assert_eq!(stored.extra(charliebot_agents::model_pool::POOL_MEMBER_FIELD), Some(&json!(MEMBER_A)));
  Ok(())
}

/// The loader reads the pool from `config.yaml`: the payg member is listed first, so the turn runs on the subscription
/// member only because the option's billing says so.
#[tokio::test]
#[ignore = "integration"]
async fn a_priority_pool_read_from_the_config_file_runs_on_its_subscription_member() -> anyhow::Result<()> {
  let backends = json!({
    "options": [
      {"id": MEMBER_A, "label": "Payg", "type": BACKEND_TYPE, "model": "payg-model", "billing": "payg",
       "script": success_script()},
      {"id": MEMBER_B, "label": "Subscription", "type": BACKEND_TYPE, "model": "subscription-model",
       "billing": "subscription", "script": success_script()},
    ],
    "pools": [{"id": AUTO_POOL_ID, "label": "Auto", "rule": "priority", "members": [MEMBER_A, MEMBER_B]}],
  });
  let stack = Stack::start_with_backends_file(&backends).await?;
  stack.write_access_key()?;
  let session = auto_session(&stack, "auto-from-file").await?;
  dispatch_turn(&stack, &session.id).await?;
  let runs = settle(&stack, &session.id).await?;
  assert_eq!(runs[0].backend.as_deref(), Some(MEMBER_B), "the subscription member runs the turn: {runs:?}");
  Ok(())
}

/// Test ④: a quota end the live process never reports — the turn drains after a restart — switches the same way.
#[tokio::test]
#[ignore = "integration"]
async fn a_drained_quota_end_switches_like_a_live_turn() -> anyhow::Result<()> {
  let first = auto_stack("exit 0", "exit 0").await?;
  let session = auto_session(&first, "auto-drained").await?;
  first
    .store()
    .update_slot_fields(
      &session.id,
      charliebot_agents::model_pool::OWNER,
      Map::from_iter([(charliebot_agents::model_pool::POOL_MEMBER_FIELD.to_string(), json!(MEMBER_A))]),
    )
    .await?;
  first.dispatch.admit_input(&session.id, event_types::USER, "fix the failing test", "user", json!({})).await?;
  let run = {
    let guard = first.store().control_lock().await;
    let record = RunRecord {
      backend: Some(MEMBER_A.to_string()),
      model: Some(format!("{MEMBER_A}-model")),
      ..synthetic_run(&session.id, "run-drained", RunKind::ManagerTurn)
    };
    let run = first.store().register_run_locked(&guard, &session.id, record, None).await?;
    first.dispatch.claim_input_batch_locked(&guard, &session.id, &run.id, None)?;
    run
  };
  let run_dir = first.store().run_dir(&session.id, &run.id);
  std::fs::create_dir_all(&run_dir)?;
  let mut raw = quota_events();
  raw.push(quota_error_result());
  let raw_text = raw.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
  std::fs::write(run_dir.join(run_records::RAW_LOG_NAME), format!("{raw_text}\n"))?;
  // The process ended unseen: the next boot drains the turn from its raw log.
  let raw_deadline = std::time::SystemTime::now() - Duration::from_secs(3600);
  std::fs::OpenOptions::new().write(true).open(run_dir.join(run_records::RAW_LOG_NAME))?.set_modified(raw_deadline)?;
  first.launch(&session.id, &run.id, 424_821, "1-424000").await?;
  let home = Arc::clone(&first.home);
  let run_id = run.id.clone();
  drop(first);

  let next = Stack::start_on_home_with_auto(
    home,
    vec![subscription(MEMBER_A, "exit 0"), subscription(MEMBER_B, &success_script())],
    Vec::new(),
    auto_set(),
  )
  .await?;
  next.write_access_key()?;
  next.boot().await?;
  let runs = settle(&next, &session.id).await?;

  let original = runs.iter().find(|candidate| candidate.id == run_id).expect("the drained run is recorded");
  let retry = retry_of(&runs, original);
  assert_eq!(retry.backend.as_deref(), Some(MEMBER_B));
  let switches = quota_failovers(&next, &session.id)?;
  assert_eq!(switches.len(), 1, "{switches:?}");
  assert_eq!(switches[0]["from"], MEMBER_A);
  assert_eq!(switches[0]["reason"], event_types::QUOTA_REASON_QUOTA_ERROR);
  assert_eq!(switches[0]["set"], AUTO_POOL_ID);
  Ok(())
}

/// Test ⑤: a refused launch that carries `quota_exhausted` is the finish recorder's signal to switch, exactly as a
/// quota error in the text is.
#[tokio::test]
#[ignore = "integration"]
async fn a_refused_launch_with_quota_exhausted_switches_the_turn() -> anyhow::Result<()> {
  let stack = auto_stack(&success_script(), &success_script()).await?;
  let session = auto_session(&stack, "auto-refused").await?;
  stack
    .store()
    .update_slot_fields(
      &session.id,
      charliebot_agents::model_pool::OWNER,
      Map::from_iter([(charliebot_agents::model_pool::POOL_MEMBER_FIELD.to_string(), json!(MEMBER_A))]),
    )
    .await?;
  stack.dispatch.admit_input(&session.id, event_types::USER, "fix the failing test", "user", json!({})).await?;
  let run = {
    let guard = stack.store().control_lock().await;
    let record = RunRecord {
      backend: Some(MEMBER_A.to_string()),
      model: Some(format!("{MEMBER_A}-model")),
      ..synthetic_run(&session.id, "run-refused", RunKind::ManagerTurn)
    };
    let run = stack.store().register_run_locked(&guard, &session.id, record, None).await?;
    stack.dispatch.claim_input_batch_locked(&guard, &session.id, &run.id, None)?;
    run
  };
  let meta = stack.store().read_metadata_fresh(&session.id)?.expect("the session is recorded");
  let option = stack.cfg.get_backend_option(MEMBER_A).expect("the member is a configured option").clone();
  let raw_log = stack.store().run_dir(&session.id, &run.id).join(run_records::RAW_LOG_NAME);
  std::fs::create_dir_all(raw_log.parent().expect("the raw log has a directory"))?;
  let persist_done: PersistAndBroadcastFn = {
    let store = Arc::clone(&stack.features.store);
    Arc::new(move |session_id: String, event: Value| {
      let store = Arc::clone(&store);
      Box::pin(async move {
        store.persist_and_broadcast(&session_id, event).await?;
        Ok(())
      }) as futures_util::future::BoxFuture<'static, anyhow::Result<()>>
    })
  };
  let recorder = stack.adapter.manager_finish_recorder(&meta, &run.id, &option, raw_log, None, persist_done);
  let extras = Map::from_iter([
    (FINISH_QUOTA_EXHAUSTED.to_string(), json!(true)),
    (FINISH_ERROR_MESSAGE.to_string(), json!("Claude account pool has no available account")),
  ]);
  recorder(Some("native".to_string()), 1, extras, json!({"type": "master_done"})).await?;
  let runs = settle(&stack, &session.id).await?;

  let retry = retry_of(&runs, &run);
  assert_eq!(retry.backend.as_deref(), Some(MEMBER_B));
  let switches = quota_failovers(&stack, &session.id)?;
  assert_eq!(switches.len(), 1);
  assert_eq!(switches[0]["from"], MEMBER_A);
  assert_eq!(switches[0]["summary"], handoff::SUMMARY_OK);
  Ok(())
}

/// Wait for the session's first run to be registered.
async fn wait_for_run(stack: &Stack, session_id: &str) -> anyhow::Result<RunRecord> {
  let deadline = Instant::now() + TEST_DEADLINE;
  loop {
    if let Some(run) = stack.run_records(session_id)?.into_iter().next() {
      return Ok(run);
    }
    anyhow::ensure!(Instant::now() < deadline, "the session's first run did not register");
    // A run registers in tens of milliseconds, and the 0.1 s unit-test budget counts this sleep's full length.
    tokio::time::sleep(Duration::from_millis(2)).await;
  }
}
