//! Review context, backend rotation, review delivery and recovery parity tests.

#[allow(dead_code)]
mod process_guard;
mod recovery_support;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;
use charliebot_agents::backend_pool::{BackendPool, BackendRoute};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunOutcomeKind, RunRecord};
use charliebot_store::{PatchTask, TaskCaller};
use charliebot_tasks::completion::pause_reason;
use charliebot_tasks::review;
use process_guard::ProcessGroupGuard;
use recovery_support::{Stack, SyntheticRepo, script_option, synthetic_run};
use serde_json::{Value, json};

const ACCESS_KEY: &str = "synthetic-access-key";

fn result_script(summary: &str) -> String {
  let event = json!({"type": "result", "subtype": "success", "result": summary});
  format!("printf '%s\\n' '{}'", serde_json::to_string(&event).expect("JSON event serializes"))
}

fn gate_script(control_dir: &Path, summary: &str) -> String {
  let control = control_dir.display();
  let result = serde_json::to_string(&json!({"type": "result", "subtype": "success", "result": summary}))
    .expect("JSON event serializes");
  format!(
    "counter=0; if [ -f '{control}/count' ]; then counter=$(/bin/cat '{control}/count'); fi; counter=$((counter + 1)); printf '%s' \"$counter\" > '{control}/count'; touch '{control}/started-'\"$counter\"; while [ ! -e '{control}/release-'\"$counter\" ]; do /bin/sleep 0.01; done; printf '%s\\n' '{result}'"
  )
}

fn gate_script_with_native_id(control_dir: &Path, summary: &str, native_id: &str) -> String {
  let init = serde_json::to_string(&json!({
    "type": "system",
    "subtype": "init",
    "session_id": native_id,
  }))
  .expect("native init event serializes");
  format!("printf '%s\\n' '{init}'; {}", gate_script(control_dir, summary))
}

fn first_n_gates_script(control_dir: &Path, summary: &str, native_id: Option<&str>, gates: usize) -> String {
  let control = control_dir.display();
  let result = serde_json::to_string(&json!({"type": "result", "subtype": "success", "result": summary}))
    .expect("JSON event serializes");
  let init = native_id
    .map(|native_id| {
      serde_json::to_string(&json!({"type": "system", "subtype": "init", "session_id": native_id}))
        .expect("native init event serializes")
    })
    .map(|event| format!("printf '%s\\n' '{event}'; "))
    .unwrap_or_default();
  format!(
    "counter=0; if [ -f '{control}/count' ]; then counter=$(/bin/cat '{control}/count'); fi; counter=$((counter + 1)); printf '%s' \"$counter\" > '{control}/count'; {init}if [ \"$counter\" -le {gates} ]; then touch '{control}/started-'\"$counter\"; while [ ! -e '{control}/release-'\"$counter\" ]; do /bin/sleep 0.01; done; fi; printf '%s\\n' '{result}'"
  )
}

fn gated_review_then_failure_script(control_dir: &Path) -> String {
  let control = control_dir.display();
  let init = serde_json::to_string(&json!({"type": "system", "subtype": "init", "session_id": "review-native"}))
    .expect("native init event serializes");
  let result = serde_json::to_string(&json!({"type": "result", "subtype": "success", "result": "review approved"}))
    .expect("JSON event serializes");
  format!(
    "counter=0; if [ -f '{control}/count' ]; then counter=$(/bin/cat '{control}/count'); fi; counter=$((counter + 1)); printf '%s' \"$counter\" > '{control}/count'; if [ \"$counter\" -eq 1 ]; then printf '%s\\n' '{init}'; touch '{control}/started-1'; while [ ! -e '{control}/release-1' ]; do /bin/sleep 0.01; done; printf '%s\\n' '{result}'; else exit 1; fi"
  )
}

fn pending_trigger_path(stack: &Stack, session_id: &str) -> PathBuf {
  stack.store().session_dir(session_id).join("triggers").join("pending.json")
}

async fn wait_for_new_run(
  stack: &Stack,
  session_id: &str,
  existing_ids: &HashSet<String>,
  kind: RunKind,
) -> anyhow::Result<RunRecord> {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    if let Some(run) =
      stack.run_records(session_id)?.into_iter().find(|run| run.kind == kind && !existing_ids.contains(&run.id))
    {
      return Ok(run);
    }
    anyhow::ensure!(Instant::now() < deadline, "session {session_id} did not register a new {kind:?} run");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn edit_task(goal: &str, repo: &Path) -> TaskSpec {
  TaskSpec {
    goal: goal.to_string(),
    repo_path: Some(repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::QuickEdit),
    ..TaskSpec::default()
  }
}

fn set_access_key(stack: &Stack) -> anyhow::Result<()> {
  fs::write(stack.cfg.credentials_file(), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))?;
  Ok(())
}

fn implement_task(goal: &str, repo: Option<&Path>) -> TaskSpec {
  TaskSpec {
    goal: goal.to_string(),
    repo_path: repo.map(|path| path.to_string_lossy().into_owned()),
    base_branch: repo.map(|_| "main".to_string()),
    task_type: Some(TaskType::Implement),
    ..TaskSpec::default()
  }
}

fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
  let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
  anyhow::ensure!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    cwd.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

async fn wait_for_run(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  session_id: &str,
  run_id: &str,
) -> anyhow::Result<RunOutcomeKind> {
  let deadline = Instant::now() + Duration::from_secs(30);
  let mut tracked = false;
  loop {
    let run = stack.store().read_run_sync(session_id, run_id)?.context("the run record exists")?;
    if !tracked && let Some(pid) = run.pid {
      guard.track(pid);
      tracked = true;
    }
    let events = stack.run_events(session_id)?;
    if let Some(outcome) = run_records::terminal_outcome(&events, run_id)? {
      return Ok(outcome);
    }
    if Instant::now() >= deadline {
      let proc_state = run.pid.map(|pid| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
          .map(|stat| stat.rsplit(") ").next().unwrap_or("").chars().next().unwrap_or('?').to_string())
          .unwrap_or_else(|error| format!("gone({error})"))
      });
      let proc_details = run.pid.map(|pid| {
        let read = |file: &str| match std::fs::read_to_string(format!("/proc/{pid}/{file}")) {
          Ok(value) => value.trim().replace('\0', " "),
          Err(error) => format!("read failed: {error}"),
        };
        format!(
          "cmdline={:?} wchan={:?} status={:?} children={:?}",
          read("cmdline"),
          read("wchan"),
          read("status"),
          read(&format!("task/{pid}/children"))
        )
      });
      let mine: Vec<String> = events
        .iter()
        .filter(|event| event.to_value().to_string().contains(run_id))
        .map(|event| event.to_value().get("type").and_then(Value::as_str).unwrap_or("?").to_string())
        .collect();
      let dir = stack.store().run_dir(session_id, run_id);
      eprintln!("DIAG run={run:?} proc_state={proc_state:?} proc_details={proc_details:?} run_events={mine:?}");
      for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        eprintln!("DIAG file {:?} len={} modified={:?}", entry.file_name(), meta.len(), meta.modified()?);
      }
    }
    anyhow::ensure!(Instant::now() < deadline, "run {run_id} did not finish before the test deadline");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn wait_for_review(stack: &Stack, session_id: &str, work_run_id: &str) -> anyhow::Result<RunRecord> {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    if let Some(run) = stack
      .run_records(session_id)?
      .into_iter()
      .find(|run| run.kind == RunKind::Review && run.review_of_run_id.as_deref() == Some(work_run_id))
    {
      return Ok(run);
    }
    anyhow::ensure!(Instant::now() < deadline, "work run {work_run_id} did not register its review");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn wait_for_task_state(stack: &Stack, session_id: &str, expected: &str) -> anyhow::Result<()> {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    if stack.store().task_state(session_id)? == expected {
      return Ok(());
    }
    anyhow::ensure!(Instant::now() < deadline, "task {session_id} did not become {expected}");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

async fn wait_for_worktree_removed(path: &Path) -> anyhow::Result<()> {
  let deadline = Instant::now() + Duration::from_secs(10);
  while path.exists() {
    anyhow::ensure!(Instant::now() < deadline, "delivered task worktree {} was not removed", path.display());
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  Ok(())
}

async fn wait_for_run_kind(
  stack: &Stack,
  guard: &ProcessGroupGuard,
  session_id: &str,
  kind: RunKind,
) -> anyhow::Result<RunRecord> {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    if let Some(run) = stack.run_records(session_id)?.into_iter().find(|run| run.kind == kind) {
      let _ = wait_for_run(stack, guard, session_id, &run.id).await?;
      return Ok(run);
    }
    anyhow::ensure!(Instant::now() < deadline, "session {session_id} did not launch a {kind:?} run");
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn events_file(stack: &Stack, session_id: &str, run_id: &str) -> PathBuf {
  stack.store().run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME)
}

fn write_events(path: &Path, events: &[Value]) -> anyhow::Result<()> {
  let mut bytes = Vec::new();
  for event in events {
    bytes.extend(serde_json::to_vec(event)?);
    bytes.push(b'\n');
  }
  fs::write(path, bytes)?;
  Ok(())
}

fn child_reports(stack: &Stack, parent_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(
    stack
      .run_events(parent_id)?
      .into_iter()
      .filter(|event| event["type"].as_str() == Some(event_types::CHILD_REPORT))
      .map(|event| event.to_value())
      .collect(),
  )
}

/// The parent's child reports, polled until *count* arrive: the report dispatches after the run's
/// terminal fact, so reading it right after `wait_for_run` races under load.
async fn wait_for_child_reports(stack: &Stack, parent_id: &str, count: usize) -> anyhow::Result<Vec<Value>> {
  tokio::time::timeout(Duration::from_secs(10), async {
    loop {
      let reports = child_reports(stack, parent_id)?;
      if reports.len() >= count {
        return Ok(reports);
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .map_err(|_| anyhow::anyhow!("timed out waiting for {count} child reports to reach {parent_id}"))?
}

async fn queue_and_launch_work(stack: &Stack, session_id: &str, run_id: &str, task_spec: &str) -> anyhow::Result<()> {
  let record = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    ..synthetic_run(session_id, run_id, RunKind::Work)
  };
  stack.register_with_task_spec(record, task_spec).await?;
  stack.adapter.launch(session_id, run_id)?;
  Ok(())
}

async fn claimed_agent_message(
  stack: &Stack,
  manager_id: &str,
  worker_id: &str,
  content: &str,
) -> anyhow::Result<String> {
  stack.dispatch.admit_input(manager_id, event_types::USER, "Start the delegated task.", "user", json!({})).await?;
  let event = stack
    .dispatch
    .admit_input(
      worker_id,
      event_types::AGENT_MESSAGE,
      content,
      "agent",
      json!({"from_session": manager_id, "from_session_name": "Synthetic manager"}),
    )
    .await?;
  Ok(event["id"].as_str().context("agent message has an id")?.to_string())
}

async fn claim_context_prompt(claims: &[Option<&str>]) -> anyhow::Result<String> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("adjusted")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("rename config", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  let branch = "task/review-claims";
  let worktree = Path::new(&stack.cfg.paths.worktree_dir).join(&worker.id);
  repo.add_worktree(&worktree, branch)?;
  let guard = ProcessGroupGuard::new();
  let mut claimed = Vec::new();
  for (index, content) in claims.iter().enumerate() {
    let input_event_ids = match content {
      Some(content) => vec![claimed_agent_message(&stack, &manager.id, &worker.id, content).await?],
      None => Vec::new(),
    };
    let run_id = format!("run-work-{}", index + 1);
    let mut record = RunRecord {
      backend: Some("fake".to_string()),
      model: Some("fake-model".to_string()),
      input_event_ids,
      repo_path: Some(repo.repo.to_string_lossy().into_owned()),
      base_branch: Some("main".to_string()),
      branch_name: Some(branch.to_string()),
      worktree_path: Some(worktree.to_string_lossy().into_owned()),
      ..synthetic_run(&worker.id, &run_id, RunKind::Work)
    };
    if index + 1 < claims.len() {
      record.started_at = Some(UtcDateTime::parse(&format!("2026-01-0{}T00:00:00Z", index + 1))?);
      stack.register_with_task_spec(record, "rename config").await?;
      claimed.push(run_id);
      continue;
    }
    stack.register_with_task_spec(record, "rename config").await?;
    stack.adapter.launch(&worker.id, &run_id)?;
    assert_eq!(wait_for_run(&stack, &guard, &worker.id, &run_id).await?, RunOutcomeKind::Success);
    claimed.push(run_id);
  }
  let work_run_id = claimed.last().context("at least one claimed message is supplied")?;
  let review_run = wait_for_review(&stack, &worker.id, work_run_id).await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  Ok(fs::read_to_string(stack.store().run_dir(&worker.id, &review_run.id).join("launch_prompt.md"))?)
}

// Python: tests/test_review_context_extraction.py::test_codex_style_falls_back_to_assistant_text
#[tokio::test]
async fn review_context_falls_back_to_assistant_text() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let path = dir.path().join("events.jsonl");
  write_events(
    &path,
    &[
      json!({"type":"assistant","message":{"content":[{"type":"text","text":"Done. Commit abcdef."}]}}),
      json!({"type":"result","result":""}),
    ],
  )?;
  assert_eq!(review::extract_review_context("session", "run", &path).await.as_deref(), Some("Done. Commit abcdef."));
  Ok(())
}

// Python: tests/test_review_context_extraction.py::test_result_text_is_the_worker_summary
#[tokio::test]
async fn review_context_uses_result_text_as_worker_summary() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let path = dir.path().join("events.jsonl");
  write_events(&path, &[json!({"type":"result","result":"All done."})])?;
  assert_eq!(review::extract_review_context("session", "run", &path).await.as_deref(), Some("All done."));
  Ok(())
}

// Python: tests/test_review_context_extraction.py::test_worker_summary_prefers_newest_assistant_over_older_result
#[tokio::test]
async fn review_context_prefers_the_newest_assistant_summary() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let path = dir.path().join("events.jsonl");
  write_events(
    &path,
    &[
      json!({"type":"result","result":"older result text"}),
      json!({"type":"assistant","message":{"content":[{"type":"text","text":"newest words"}]}}),
    ],
  )?;
  assert_eq!(review::extract_review_context("session", "run", &path).await.as_deref(), Some("newest words"));
  Ok(())
}

// Python: tests/test_review_context_extraction.py::test_worker_summary_skips_malformed_tail_lines
#[tokio::test]
async fn review_context_skips_malformed_tail_lines() -> anyhow::Result<()> {
  let dir = tempfile::tempdir()?;
  let path = dir.path().join("events.jsonl");
  fs::write(&path, "{\"type\":\"result\",\"result\":\"Worker did X successfully.\"}\n\n{broken json\n")?;
  assert_eq!(
    review::extract_review_context("session", "run", &path).await.as_deref(),
    Some("Worker did X successfully.")
  );
  Ok(())
}

// Python: tests/test_backend_pool.py::test_reviewer_avoids_every_member_the_worker_node_used
#[tokio::test]
async fn reviewer_avoids_every_member_the_worker_node_used() -> anyhow::Result<()> {
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0"), script_option("fake3", "exit 0")],
    vec!["fake2".to_string(), "fake3".to_string()],
  )
  .await?;
  let pool = BackendPool::default();
  let used = HashSet::from(["fake2".to_string()]);
  let selection =
    review::select_reviewer_backend(&stack.cfg, &pool, "reviewer", "fake", Some("fake-model"), &[], &used)?;
  assert_eq!(selection, Some(("fake3".to_string(), Some("fake3-model".to_string()), vec!["fake3".to_string()])));
  Ok(())
}

// Python: tests/test_backend_pool.py::test_reviewer_never_falls_back_to_a_member_the_worker_node_used
#[tokio::test]
async fn reviewer_never_falls_back_to_a_member_the_worker_node_used() -> anyhow::Result<()> {
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake2".to_string()],
  )
  .await?;
  let pool = BackendPool::default();
  pool.cool("fake2", None);
  let used = HashSet::from(["fake".to_string()]);
  assert!(
    review::select_reviewer_backend(&stack.cfg, &pool, "reviewer", "fake", Some("fake-model"), &[], &used)?.is_none()
  );
  assert!(
    review::select_reviewer_backend(
      &stack.cfg,
      &pool,
      "reviewer",
      "fake",
      Some("fake-model"),
      &[],
      &HashSet::from(["fake".to_string(), "fake2".to_string()]),
    )?
    .is_none()
  );
  Ok(())
}

// Python: tests/test_backend_pool.py::test_a_check_without_used_members_keeps_the_session_backend_fallback
#[tokio::test]
async fn check_without_used_members_keeps_the_session_backend_fallback() -> anyhow::Result<()> {
  let stack = Stack::start_with(
    vec![script_option("fake", "exit 0"), script_option("fake2", "exit 0")],
    vec!["fake2".to_string()],
  )
  .await?;
  let pool = BackendPool::default();
  let tried = vec!["fake2".to_string()];
  let selection =
    review::select_reviewer_backend(&stack.cfg, &pool, "verify", "fake", Some("fake-model"), &tried, &HashSet::new())?;
  assert_eq!(
    selection,
    Some(("fake".to_string(), Some("fake-model".to_string()), vec!["fake2".to_string(), "fake".to_string()]))
  );
  Ok(())
}

// Python: tests/test_backend_pool.py::test_an_empty_pool_runs_the_check_on_the_workers_backend
#[tokio::test]
async fn empty_pool_reuses_the_worker_backend_on_every_attempt() -> anyhow::Result<()> {
  let stack = Stack::start_with(vec![script_option("fake", "exit 0")], Vec::new()).await?;
  let pool = BackendPool::default();
  let first =
    review::select_reviewer_backend(&stack.cfg, &pool, "reviewer", "fake", Some("fake-model"), &[], &HashSet::new())?
      .expect("an empty pool falls back to the worker backend");
  let second = review::select_reviewer_backend(
    &stack.cfg,
    &pool,
    "reviewer",
    "fake",
    Some("fake-model"),
    &first.2,
    &HashSet::from(["fake".to_string()]),
  )?
  .expect("an empty pool keeps the legacy worker-backend fallback on retries");
  assert_eq!(first.0, "fake");
  assert_eq!(second.0, "fake");
  assert_eq!(second.2, ["fake", "fake"]);
  Ok(())
}

// Python: tests/test_task_execution.py::test_repo_review_context_carries_the_delegated_spec
#[tokio::test]
async fn repo_review_context_carries_the_delegated_spec() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("implemented")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let spec = TaskSpec {
    goal: "rename the config key".to_string(),
    acceptance: vec!["the legacy key still loads".to_string()],
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    task_type: Some(TaskType::Implement),
    ..TaskSpec::default()
  };
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(spec)).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "rename the config key").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &review_run.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("the legacy key still loads"), "{prompt}");
  assert!(prompt.contains("**Task spec:**"), "{prompt}");
  Ok(())
}

// Python: tests/test_task_execution.py::test_repo_review_context_refuses_a_task_with_an_empty_goal
#[tokio::test]
async fn repo_review_context_refuses_a_task_with_an_empty_goal() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("implemented")), script_option("fake2", "exit 0")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_task("", Some(&repo.repo)))).await?;
  let work = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some("charliebot/task-empty-goal".to_string()),
    worktree_path: Some(repo.repo.join("synthetic-worktree").to_string_lossy().into_owned()),
    ..synthetic_run(&worker.id, "run-work", RunKind::Work)
  };
  stack.register_with_task_spec(work, "").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  stack.node_pass(&worker.id).await?;
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  let guard = ProcessGroupGuard::new();
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Failed);
  let events = fs::read_to_string(events_file(&stack, &worker.id, &review_run.id))?;
  assert!(events.contains("review has no task spec: the task record's goal is empty"), "{events}");
  Ok(())
}

// Python: tests/test_task_execution.py::test_review_context_without_claimed_inputs_goes_from_spec_to_summary
#[tokio::test]
async fn review_context_without_claimed_inputs_goes_from_spec_to_summary() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("renamed")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("worker", None, TaskProfile::Worker, Some(implement_task("rename config", Some(&repo.repo)))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "rename config").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &review_run.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("**Task spec:**\nrename config\n**Worker summary:** renamed"), "{prompt}");
  assert!(!prompt.contains("Later Messages"), "{prompt}");
  Ok(())
}

// Python: tests/test_task_execution.py::test_review_contexts_list_the_agent_message_the_second_run_claimed
#[tokio::test]
async fn review_context_lists_the_message_claimed_by_the_second_work_run() -> anyhow::Result<()> {
  let prompt = claim_context_prompt(&[None, Some("Second run's delegated instruction.")]).await?;
  assert!(
    prompt
      .contains("**Later Messages:**\n[Message from session Synthetic manager] Second run's delegated instruction."),
    "{prompt}"
  );
  Ok(())
}

// Python: tests/test_task_execution.py::test_review_context_lists_each_runs_claimed_messages_in_claim_order
#[tokio::test]
async fn review_context_lists_claimed_messages_in_work_run_order() -> anyhow::Result<()> {
  let prompt = claim_context_prompt(&[Some("First claimed instruction."), Some("Second claimed instruction.")]).await?;
  let first =
    prompt.find("[Message from session Synthetic manager] First claimed instruction.").context("first claim")?;
  let second =
    prompt.find("[Message from session Synthetic manager] Second claimed instruction.").context("second claim")?;
  assert!(first < second, "{prompt}");
  Ok(())
}

// Python: tests/test_task_execution.py::test_worker_run_kinds_deliver_snapshot_bytes_and_task_context (review launch)
#[tokio::test]
async fn review_launch_builds_its_task_context_and_managed_snapshot() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("worker done")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("worker", None, TaskProfile::Worker, Some(implement_task("ship it", Some(&repo.repo)))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "ship it").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(review_run.kind, RunKind::Review);
  assert_eq!(review_run.review_of_run_id.as_deref(), Some("run-work"));
  assert_eq!(review_run.backend.as_deref(), Some("fake2"));
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &review_run.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("**Task spec:**\nship it"), "{prompt}");
  assert!(prompt.contains("worker done"), "{prompt}");
  assert!(prompt.contains("Review the changes: `git diff origin/main..."), "{prompt}");
  let snapshot: Value =
    serde_json::from_slice(&fs::read(stack.store().run_dir(&worker.id, &review_run.id).join("prompt_snapshot.json"))?)?;
  let instruction_text = snapshot["blocks"]
    .as_array()
    .context("review snapshot has blocks")?
    .iter()
    .filter_map(|block| block["text"].as_str())
    .collect::<Vec<_>>()
    .join("\n\n");
  assert!(instruction_text.contains("## Code Review"), "{instruction_text}");
  assert!(instruction_text.contains("IMPORTANT: Make minimal changes"), "{instruction_text}");
  Ok(())
}

/// The reviewer follows the worker's route and skips the backend that ran the work.
#[tokio::test]
async fn reviewer_reuses_the_worker_route_and_avoids_its_backend() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:422-447.
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("worker done")),
      script_option("fake2", &result_script("pool reviewer")),
      script_option("fake3", &result_script("route reviewer")),
    ],
    vec!["fake2".to_string(), "fake3".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("routed-worker", None, TaskProfile::Worker, Some(implement_task("ship it", Some(&repo.repo)))).await?;
  let guard = stack.store().control_lock().await;
  stack
    .store()
    .update_task_slot_fields_locked(
      &guard,
      &worker.id,
      charliebot_agents::backend_pool::METADATA_OWNER,
      serde_json::Map::from_iter([(
        "route".to_string(),
        serde_json::to_value(BackendRoute {
          backends: vec!["fake3".to_string(), "fake2".to_string()],
          reason: "review route".to_string(),
        })?,
      )]),
    )
    .await?;
  drop(guard);

  let process_guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-routed-work", "ship it").await?;
  assert_eq!(wait_for_run(&stack, &process_guard, &worker.id, "run-routed-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-routed-work").await?;
  assert_eq!(review_run.backend.as_deref(), Some("fake3"));
  assert_eq!(wait_for_run(&stack, &process_guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  Ok(())
}

/// A route whose only member is the worker's backend leaves the reviewer to today's pool selection.
#[tokio::test]
async fn a_fully_avoided_route_falls_back_to_todays_reviewer_selection() -> anyhow::Result<()> {
  // Python: tests/test_backend_router.py:450.
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("worker done")), script_option("fake2", &result_script("pool reviewer"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("avoided-worker", None, TaskProfile::Worker, Some(implement_task("ship it", Some(&repo.repo)))).await?;
  let guard = stack.store().control_lock().await;
  stack
    .store()
    .update_task_slot_fields_locked(
      &guard,
      &worker.id,
      charliebot_agents::backend_pool::METADATA_OWNER,
      serde_json::Map::from_iter([(
        "route".to_string(),
        serde_json::to_value(BackendRoute { backends: vec!["fake".to_string()], reason: "design p=0.9".to_string() })?,
      )]),
    )
    .await?;
  drop(guard);

  queue_and_launch_work(&stack, &worker.id, "run-avoided-work", "ship it").await?;
  let review_run = wait_for_review(&stack, &worker.id, "run-avoided-work").await?;
  assert_eq!(review_run.backend.as_deref(), Some("fake2"), "today's pool selection picks the other member");
  Ok(())
}

// Python: tests/test_task_execution.py::test_implement_delivery_requires_review_and_real_landing
// Python: tests/test_worker_pause_continuation.py::test_delivery_without_pending_wake_behaves_as_today
#[tokio::test]
async fn implement_delivery_requires_review_and_real_landing() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("implemented")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  let close = stack
    .run_events(&worker.id)?
    .into_iter()
    .find(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
    .context("automatic completion writes task_closed")?;
  let refs = close["result_refs"].as_array().context("completion carries result refs")?;
  assert!(refs.iter().any(|value| value.as_str() == Some("run:run-work")));
  assert!(refs.iter().any(|value| value.as_str().is_some_and(|value| value.starts_with("spec:"))));
  assert!(refs.iter().any(|value| value.as_str() == Some(format!("review:{}", review_run.id).as_str())));
  assert!(refs.iter().any(|value| value.as_str().is_some_and(|value| value.starts_with("landed:origin/main@"))));
  let work = stack.store().read_run_sync(&worker.id, "run-work")?.expect("work run remains recorded");
  wait_for_worktree_removed(Path::new(work.worktree_path.as_deref().context("work run has a worktree")?)).await?;
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "completed");
  Ok(())
}

// Python: tests/test_task_execution.py::test_repo_less_implement_delivers_after_review_passes
#[tokio::test]
async fn repo_less_implement_delivers_after_review_passes() -> anyhow::Result<()> {
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("implemented")), script_option("fake2", &result_script("review ok"))],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("write a report", None))).await?;
  stack.dispatch.set_executor(None);
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "write a report").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  let close = stack
    .run_events(&worker.id)?
    .into_iter()
    .find(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
    .unwrap();
  assert!(
    close["result_refs"]
      .as_array()
      .unwrap()
      .iter()
      .any(|value| value.as_str() == Some(format!("review:{}", review_run.id).as_str()))
  );
  assert!(
    !close["result_refs"]
      .as_array()
      .unwrap()
      .iter()
      .any(|value| value.as_str().is_some_and(|value| value.starts_with("landed:")))
  );
  assert_eq!(child_reports(&stack, &manager.id)?[0]["outcome"], "completed");
  Ok(())
}

// Python: tests/test_task_execution.py::test_repo_less_implement_review_failure_takes_the_failure_report
#[tokio::test]
async fn repo_less_implement_review_failure_takes_the_failure_report() -> anyhow::Result<()> {
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("implemented")), script_option("fake2", "exit 1")],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("write a report", None))).await?;
  stack.dispatch.set_executor(None);
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "write a report").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Failed);
  let reports = wait_for_child_reports(&stack, &manager.id, 1).await?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("failed on every configured reviewer backend"));
  Ok(())
}

// Python: tests/test_task_execution.py::test_a_successful_worker_with_no_available_reviewer_reports_blocked
#[tokio::test]
async fn successful_worker_with_no_available_reviewer_reports_blocked() -> anyhow::Result<()> {
  let stack =
    Stack::start_with(vec![script_option("fake", &result_script("implemented"))], vec!["fake".to_string()]).await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("write a report", None))).await?;
  stack.dispatch.set_executor(None);
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "write a report").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let reports = wait_for_child_reports(&stack, &manager.id, 1).await?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("found no available reviewer backend"));
  assert!(stack.run_records(&worker.id)?.iter().all(|run| run.kind != RunKind::Review));
  Ok(())
}

// Python: tests/test_task_execution.py::test_a_failed_review_with_a_queued_retry_reports_nothing_and_stays_open
#[tokio::test]
async fn failed_review_with_a_queued_retry_reports_nothing_and_stays_open() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("implemented")),
      script_option("fake2", "exit 1"),
      script_option("fake3", &gate_script(control.path(), "review retry ok")),
    ],
    vec!["fake".to_string(), "fake2".to_string(), "fake3".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first_review = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &first_review.id).await?, RunOutcomeKind::Failed);
  let retry = loop {
    if let Some(review) =
      stack.run_records(&worker.id)?.into_iter().find(|run| run.kind == RunKind::Review && run.id != first_review.id)
    {
      break review;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  };
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &retry.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  assert!(child_reports(&stack, &manager.id)?.is_empty());
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  fs::write(control.path().join("release-1"), "release")?;
  let control_entries: Vec<String> = fs::read_dir(control.path())?
    .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
    .collect::<Result<_, _>>()?;
  eprintln!(
    "DIAG control={} count={:?} entries={control_entries:?}",
    control.path().display(),
    fs::read_to_string(control.path().join("count"))
  );
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &retry.id).await?, RunOutcomeKind::Success);
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  Ok(())
}

// Python: tests/test_review_path_dispatch.py::test_input_admitted_during_a_review_pauses_the_merge_and_continues_the_review
// Python: tests/test_worker_pause_continuation.py::test_review_continuation_runs_on_the_paused_reviews_backend_and_model
#[tokio::test]
async fn input_during_review_pauses_landing_and_continues_review() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("implemented")),
      script_option("fake2", &gate_script_with_native_id(control.path(), "review approved", "review-native")),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  stack.dispatch.admit_input(&manager.id, event_types::USER, "Start the delegated task.", "user", json!({})).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first_review = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(first_review.backend.as_deref(), Some("fake2"));
  assert_eq!(first_review.model.as_deref(), Some("fake2-model"));
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &first_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  let admitted = stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::AGENT_MESSAGE,
      "one more tweak",
      "agent",
      json!({"from_session": manager.id, "from_session_name": "Synthetic manager"}),
    )
    .await?;
  let worktree = stack
    .store()
    .read_run_sync(&worker.id, "run-work")?
    .and_then(|run| run.worktree_path)
    .context("work run has a worktree")?;
  git(Path::new(&worktree), &["commit", "--allow-empty", "-m", "unlanded synthetic change"])?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &first_review.id).await?, RunOutcomeKind::Success);
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert!(child_reports(&stack, &manager.id)?.is_empty());

  let second_review = loop {
    if let Some(run) =
      stack.run_records(&worker.id)?.into_iter().find(|run| run.kind == RunKind::Review && run.id != first_review.id)
    {
      break run;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  };
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &second_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  assert_eq!(second_review.review_of_run_id.as_deref(), Some("run-work"));
  assert_eq!(second_review.backend.as_deref(), Some("fake2"));
  assert_eq!(second_review.model.as_deref(), Some("fake2-model"));
  assert_eq!(second_review.input_event_ids, [admitted["id"].as_str().context("input has id")?.to_string()]);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &second_review.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("one more tweak"), "{prompt}");
  stack.dispatch.set_executor(None);
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &second_review.id).await?, RunOutcomeKind::Success);
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    let reports = child_reports(&stack, &manager.id)?;
    if reports.iter().any(|report| report["summary"].as_str().unwrap_or_default().contains("review"))
      && stack.store().task_state(&worker.id)? == "archived"
    {
      break;
    }
    anyhow::ensure!(Instant::now() < deadline, "review continuation did not report its failed landing");
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  assert!(reports[0]["summary"].as_str().unwrap_or_default().contains("ended without landing work run"));
  assert_eq!(stack.store().task_state(&worker.id)?, "archived");
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_input_admitted_during_a_work_run_pauses_it_the_same_way
// Python: tests/test_worker_pause_continuation.py::test_work_continuation_keeps_the_node_default_backend_and_model
// Python: tests/test_worker_pause_continuation.py::test_native_session_id_reads_the_claude_system_init_event
#[tokio::test]
async fn input_during_work_pauses_and_continues_the_node_backend_conversation() -> anyhow::Result<()> {
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &gate_script_with_native_id(control.path(), "work done", "work-native")),
      script_option("fake2", &result_script("review approved")),
    ],
    vec!["fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", None))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-work")?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  let admitted = claimed_agent_message(&stack, &manager.id, &worker.id, "one more thing").await?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first_run = stack.store().read_run_sync(&worker.id, "run-work")?.context("work run is recorded")?;
  assert_eq!(first_run.native_session_id.as_deref(), Some("work-native"));

  let continuation =
    wait_for_new_run(&stack, &worker.id, &HashSet::from(["run-work".to_string()]), RunKind::Work).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &continuation.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert_eq!(continuation.kind, RunKind::Work);
  assert_eq!((continuation.backend.as_deref(), continuation.model.as_deref()), (Some("fake"), Some("fake-model")));
  assert_eq!(continuation.input_event_ids, [admitted]);
  assert!(stack.run_records(&worker.id)?.iter().all(|run| run.kind != RunKind::Review));
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &continuation.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("one more thing"), "{prompt}");
  assert!(!prompt.contains("add marker"), "{prompt}");
  stack.dispatch.set_executor(None);
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, &continuation.id).await?;
  assert_eq!(review_run.backend.as_deref(), Some("fake2"));
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_changed_prompt_hash_at_continuation_starts_a_fresh_conversation
#[tokio::test]
async fn changed_prompt_hash_at_work_continuation_starts_a_fresh_context() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &first_n_gates_script(control.path(), "work done", Some("work-native"), 2)),
      script_option("fake2", &result_script("review approved")),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-work")?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  claimed_agent_message(&stack, &manager.id, &worker.id, "keep going").await?;
  stack
    .store()
    .patch_task(
      &worker.id,
      PatchTask { node_prompt: Some(Some("changed rule".to_string())), ..PatchTask::default() },
      TaskCaller::Operator,
      &*stack.dispatch,
    )
    .await?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);

  let continuation =
    wait_for_new_run(&stack, &worker.id, &HashSet::from(["run-work".to_string()]), RunKind::Work).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &continuation.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  let first = stack.store().read_run_sync(&worker.id, "run-work")?.context("first work run is recorded")?;
  assert_eq!(continuation.worktree_path, first.worktree_path);
  assert_eq!(continuation.branch_name, first.branch_name);
  assert_eq!(continuation.input_event_ids.len(), 1);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &continuation.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("add marker"), "{prompt}");
  assert!(prompt.contains("keep going"), "{prompt}");
  assert!(prompt.contains("managed instructions or sources changed"), "{prompt}");
  assert!(prompt.contains(&events_file(&stack, &worker.id, "run-work").display().to_string()), "{prompt}");
  let first_snapshot: Value =
    serde_json::from_slice(&fs::read(stack.store().run_dir(&worker.id, "run-work").join("prompt_snapshot.json"))?)?;
  let continuation_snapshot: Value = serde_json::from_slice(&fs::read(
    stack.store().run_dir(&worker.id, &continuation.id).join("prompt_snapshot.json"),
  )?)?;
  assert_ne!(first_snapshot["prompt_hash"], continuation_snapshot["prompt_hash"]);
  stack.dispatch.set_executor(None);
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Success);
  let review = wait_for_review(&stack, &worker.id, &continuation.id).await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_continuation_on_an_unchanged_backend_names_a_missing_native_id
#[tokio::test]
async fn work_continuation_without_native_id_starts_a_fresh_context() -> anyhow::Result<()> {
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &first_n_gates_script(control.path(), "work done", None, 2)),
      script_option("fake2", &result_script("review approved")),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", None))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-work")?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  claimed_agent_message(&stack, &manager.id, &worker.id, "keep going").await?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first = stack.store().read_run_sync(&worker.id, "run-work")?.context("first work run is recorded")?;
  assert!(first.native_session_id.is_none());

  let continuation =
    wait_for_new_run(&stack, &worker.id, &HashSet::from(["run-work".to_string()]), RunKind::Work).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &continuation.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &continuation.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("previous run recorded no native conversation id"), "{prompt}");
  assert!(prompt.contains("keep going"), "{prompt}");
  stack.dispatch.set_executor(None);
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Success);
  let review = wait_for_review(&stack, &worker.id, &continuation.id).await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review.id).await?, RunOutcomeKind::Success);
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_implement_work_run_with_pending_trigger_pauses_then_continues_and_delivers
#[tokio::test]
async fn implement_work_with_pending_trigger_pauses_then_continues_and_delivers() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &gate_script_with_native_id(control.path(), "work done", "work-native")),
      script_option("fake2", &result_script("review approved")),
    ],
    vec!["fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let worker =
    stack.node("worker", None, TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo)))).await?;
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-work")?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;
  assert!(run_records::terminal_outcome(&stack.run_events(&worker.id)?, "run-work")?.is_some());
  assert!(stack.run_records(&worker.id)?.iter().all(|run| run.kind != RunKind::Review));
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  let first = stack.store().read_run_sync(&worker.id, "run-work")?.context("first work run is recorded")?;
  let worktree = first.worktree_path.clone().context("paused work retains its worktree")?;
  assert!(Path::new(&worktree).is_dir());

  fs::remove_file(pending_trigger_path(&stack, &worker.id))?;
  let trigger = stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::SCHEDULED_TRIGGER,
      "build finished, check the log",
      "system",
      json!({"input_id": "synthetic-fired-work-trigger"}),
    )
    .await?;
  stack.dispatch.dispatch_pending(&worker.id).await?;
  let continuation =
    wait_for_new_run(&stack, &worker.id, &HashSet::from(["run-work".to_string()]), RunKind::Work).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &continuation.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  assert_eq!(continuation.worktree_path.as_deref(), Some(worktree.as_str()));
  assert_eq!(continuation.branch_name, first.branch_name);
  assert_eq!(continuation.input_event_ids, [trigger["id"].as_str().context("fired trigger has an id")?]);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &continuation.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("build finished, check the log"), "{prompt}");
  assert!(!prompt.contains("add marker"), "{prompt}");
  stack.dispatch.set_executor(None);
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, &continuation.id).await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  wait_for_worktree_removed(Path::new(&worktree)).await?;
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_failed_run_with_pending_trigger_reports_failure_as_today
#[tokio::test]
async fn failed_work_with_pending_trigger_still_reports_and_keeps_the_task_open() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 1").await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Failed);
  let reports = wait_for_child_reports(&stack, &manager.id, 1).await?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "failed");
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  let work = stack.store().read_run_sync(&worker.id, "run-work")?.context("failed work run remains recorded")?;
  assert!(Path::new(work.worktree_path.as_deref().context("failed run keeps its worktree")?).exists());
  assert!(pause_reason(&stack.features, &worker.id)?.is_some());
  assert!(stack.run_records(&worker.id)?.iter().all(|run| run.kind != RunKind::Review));
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_non_implement_work_run_with_pending_trigger_skips_auto_complete
#[tokio::test]
async fn quick_edit_with_pending_trigger_defers_completion_until_continuation() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start(&gate_script_with_native_id(control.path(), "edit done", "edit-native")).await?;
  set_access_key(&stack)?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(edit_task("edit the marker", &repo.repo))).await?;
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-edit", "edit the marker").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, "run-edit")?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  let first = stack.store().read_run_sync(&worker.id, "run-edit")?.context("quick-edit work run is recorded")?;
  let worktree = first.worktree_path.clone().context("quick edit prepares a worktree")?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-edit").await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert!(
    stack.run_events(&worker.id)?.iter().all(|event| event["type"] != event_types::TASK_CLOSED),
    "a pending trigger pauses quick-edit completion"
  );
  assert!(Path::new(&worktree).is_dir());

  fs::remove_file(pending_trigger_path(&stack, &worker.id))?;
  stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::SCHEDULED_TRIGGER,
      "quick edit wake",
      "system",
      json!({"input_id": "synthetic-fired-quick-edit-trigger"}),
    )
    .await?;
  stack.dispatch.dispatch_pending(&worker.id).await?;
  let continuation =
    wait_for_new_run(&stack, &worker.id, &HashSet::from(["run-edit".to_string()]), RunKind::Work).await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &continuation.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  assert_eq!(continuation.worktree_path.as_deref(), Some(worktree.as_str()));
  assert_eq!(continuation.backend.as_deref(), Some("fake"));
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &continuation.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("quick edit wake"), "{prompt}");
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Success);
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  wait_for_worktree_removed(Path::new(&worktree)).await?;
  assert!(stack.run_records(&worker.id)?.iter().all(|run| run.kind != RunKind::Review));
  Ok(())
}

// Python: tests/test_git_stash_guard.py::test_worker_and_review_runs_put_the_guard_first_on_path
#[tokio::test]
async fn worker_and_review_runs_put_the_guard_first_on_path() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let temp = tempfile::tempdir()?;
  let worker_path = temp.path().join("worker-path");
  let review_path = temp.path().join("review-path");
  let path_script = |record: &Path, summary: &str| {
    format!("printf '%s' \"$PATH\" > '{}'; {}", record.display(), result_script(summary))
  };
  let stack = Stack::start_with(
    vec![
      script_option("fake", &path_script(&worker_path, "implemented")),
      script_option("fake2", &path_script(&review_path, "review ok")),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("worker", None, TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo)))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let review_run = wait_for_review(&stack, &worker.id, "run-work").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &review_run.id).await?, RunOutcomeKind::Success);
  let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).context("checkout root exists")?;
  let guard_dir = checkout.join("src/runtime/git_stash_guard");
  assert!(fs::read_to_string(worker_path)?.starts_with(&format!("{}:", guard_dir.display())));
  assert!(fs::read_to_string(review_path)?.starts_with(&format!("{}:", guard_dir.display())));
  Ok(())
}

// Python: tests/test_review_landing_target.py::test_commit_published_only_on_origin_base_closes_the_reviewed_task
#[tokio::test]
async fn commit_published_only_on_origin_base_closes_the_reviewed_task() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  stack.dispatch.set_executor(None);
  let worker =
    stack.node("worker", None, TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo)))).await?;
  let branch = "task/review-published";
  let worktree = Path::new(&stack.cfg.paths.worktree_dir).join(charliebot_tasks::git::git_worktree_dir_name(branch));
  repo.add_worktree(&worktree, branch)?;
  git(&worktree, &["commit", "--allow-empty", "-m", "synthetic reviewed commit"])?;
  repo.publish_worktree(&worktree)?;
  let commit = git(&repo.repo, &["rev-parse", branch])?;
  assert_eq!(git(&repo.repo, &["rev-parse", "origin/main"])?, commit);
  assert_ne!(git(&repo.repo, &["rev-parse", "main"])?, commit);
  let work = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some(branch.to_string()),
    worktree_path: Some(worktree.to_string_lossy().into_owned()),
    ..synthetic_run(&worker.id, "run-work", RunKind::Work)
  };
  stack.register_with_task_spec(work, "add marker").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  let review = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake2-model".to_string()),
    review_of_run_id: Some("run-work".to_string()),
    ..synthetic_run(&worker.id, "review-run", RunKind::Review)
  };
  stack.register(review).await?;
  stack.record_terminal(&worker.id, "review-run", RunOutcomeKind::Success).await?;

  stack.node_pass(&worker.id).await?;
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  wait_for_worktree_removed(&worktree).await?;
  let close = stack
    .run_events(&worker.id)?
    .into_iter()
    .find(|event| event["type"].as_str() == Some(event_types::TASK_CLOSED))
    .context("review delivery closes the task")?;
  assert!(
    close["result_refs"]
      .as_array()
      .unwrap()
      .iter()
      .any(|value| value.as_str() == Some(format!("landed:origin/main@{commit}").as_str()))
  );
  Ok(())
}

// Python: tests/test_review_landing_target.py::test_commit_on_neither_base_ref_reports_blocked_with_the_git_reason
#[tokio::test]
async fn commit_on_neither_base_ref_reports_blocked_with_the_git_reason() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  let branch = "task/review-unpublished";
  let worktree = Path::new(&stack.cfg.paths.worktree_dir).join(&worker.id);
  repo.add_worktree(&worktree, branch)?;
  git(&worktree, &["commit", "--allow-empty", "-m", "synthetic unlanded commit"])?;
  let work = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some(branch.to_string()),
    worktree_path: Some(worktree.to_string_lossy().into_owned()),
    ..synthetic_run(&worker.id, "run-work", RunKind::Work)
  };
  stack.register_with_task_spec(work, "add marker").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  let review = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake2-model".to_string()),
    review_of_run_id: Some("run-work".to_string()),
    ..synthetic_run(&worker.id, "review-run", RunKind::Review)
  };
  stack.register(review).await?;
  stack.record_terminal(&worker.id, "review-run", RunOutcomeKind::Success).await?;
  write_events(&events_file(&stack, &worker.id, "review-run"), &[json!({"type":"result","result":"review ok"})])?;

  stack.node_pass(&worker.id).await?;
  let report = child_reports(&stack, &manager.id)?.into_iter().next().context("landing failure reports to parent")?;
  assert_eq!(report["outcome"], "blocked");
  let summary = report["summary"].as_str().context("blocked report has a summary")?;
  let (head, verdict) = summary.split_once("\n\nReview verdict:\n").context("blocked report quotes the verdict")?;
  assert!(head.starts_with("review run review-run ended without landing work run run-work on main: "), "{head}");
  assert!(head.contains("ancestry check failed") && head.contains("(origin/main)"), "{head}");
  // The reviewer's closing words, whole, are the verdict the manager reads.
  assert_eq!(verdict, "review ok");
  assert!(worktree.exists(), "an unproven landing keeps the worktree for recovery");
  Ok(())
}

// Python: tests/test_review_landing_target.py:143
#[tokio::test]
async fn review_without_closing_words_reports_its_verdict_unknown() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start("exit 0").await?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  stack.dispatch.set_executor(None);
  let branch = "task/review-unpublished";
  let worktree = Path::new(&stack.cfg.paths.worktree_dir).join(&worker.id);
  repo.add_worktree(&worktree, branch)?;
  git(&worktree, &["commit", "--allow-empty", "-m", "synthetic unlanded commit"])?;
  let work = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some(branch.to_string()),
    worktree_path: Some(worktree.to_string_lossy().into_owned()),
    ..synthetic_run(&worker.id, "run-work", RunKind::Work)
  };
  stack.register_with_task_spec(work, "add marker").await?;
  stack.record_terminal(&worker.id, "run-work", RunOutcomeKind::Success).await?;
  let review = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake2-model".to_string()),
    review_of_run_id: Some("run-work".to_string()),
    ..synthetic_run(&worker.id, "review-run", RunKind::Review)
  };
  stack.register(review).await?;
  stack.record_terminal(&worker.id, "review-run", RunOutcomeKind::Success).await?;
  write_events(&events_file(&stack, &worker.id, "review-run"), &[json!({"type":"result","result":""})])?;

  stack.node_pass(&worker.id).await?;
  let reports = child_reports(&stack, &manager.id)?;
  assert_eq!(reports.len(), 1);
  assert_eq!(reports[0]["outcome"], "blocked");
  let summary = reports[0]["summary"].as_str().context("blocked report has a summary")?;
  assert_eq!(
    summary.lines().last(),
    Some("The review run left no closing words, so its verdict is unknown."),
    "{summary}"
  );
  Ok(())
}

// Python: tests/test_review_landing_target.py::test_blocked_child_report_reaches_the_manager_turn_with_its_summary
#[tokio::test]
async fn blocked_child_report_carries_landing_summary_to_parent_history() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let stack = Stack::start_with(
    vec![script_option("fake", &result_script("parent received the report"))],
    vec!["fake".to_string()],
  )
  .await?;
  let parent = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let child = stack
    .node("worker", Some(&parent.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  let branch = "task/review-report";
  let worktree = Path::new(&stack.cfg.paths.worktree_dir).join(&child.id);
  repo.add_worktree(&worktree, branch)?;
  git(&worktree, &["commit", "--allow-empty", "-m", "synthetic unlanded commit"])?;
  let work = RunRecord {
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
    repo_path: Some(repo.repo.to_string_lossy().into_owned()),
    base_branch: Some("main".to_string()),
    branch_name: Some(branch.to_string()),
    worktree_path: Some(worktree.to_string_lossy().into_owned()),
    ..synthetic_run(&child.id, "run-work", RunKind::Work)
  };
  stack.register_with_task_spec(work, "add marker").await?;
  stack.record_terminal(&child.id, "run-work", RunOutcomeKind::Success).await?;
  let review = RunRecord {
    backend: Some("fake2".to_string()),
    model: Some("fake2-model".to_string()),
    review_of_run_id: Some("run-work".to_string()),
    ..synthetic_run(&child.id, "review-run", RunKind::Review)
  };
  stack.register(review).await?;
  stack.record_terminal(&child.id, "review-run", RunOutcomeKind::Success).await?;
  stack.node_pass(&child.id).await?;
  let guard = ProcessGroupGuard::new();
  let manager_run = wait_for_run_kind(&stack, &guard, &parent.id, RunKind::ManagerTurn).await?;
  let prompt = fs::read_to_string(stack.store().run_dir(&parent.id, &manager_run.id).join("launch_prompt.md"))?;
  let reports = child_reports(&stack, &parent.id)?;
  assert_eq!(reports.len(), 1);
  let summary = reports[0]["summary"].as_str().context("blocked report has a summary")?;
  assert!(summary.contains("ancestry check failed"), "{summary}");
  assert!(prompt.contains(&format!("[Report from task {} | outcome blocked] {summary}", child.id)), "{prompt}");
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_review_with_pending_trigger_pauses_merge_and_continues_the_review
#[tokio::test]
async fn review_with_pending_trigger_pauses_merge_and_continues_the_review() -> anyhow::Result<()> {
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("implemented")),
      script_option("fake2", &gate_script_with_native_id(control.path(), "review approved", "review-native")),
    ],
    vec!["fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let worker = stack.node("worker", None, TaskProfile::Worker, Some(implement_task("add marker", None))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first_review = wait_for_review(&stack, &worker.id, "run-work").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &first_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  assert!(pause_reason(&stack.features, &worker.id)?.is_some());
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &first_review.id).await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert_eq!(
    stack.run_records(&worker.id)?.iter().filter(|run| run.kind == RunKind::Review).count(),
    1,
    "the pending trigger pauses the successful review before landing"
  );

  fs::remove_file(pending_trigger_path(&stack, &worker.id))?;
  let trigger = stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::SCHEDULED_TRIGGER,
      "CI passed",
      "system",
      json!({"input_id": "synthetic-fired-review-trigger"}),
    )
    .await?;
  let dispatch = stack.dispatch.dispatch_pending(&worker.id).await?;
  assert_eq!(dispatch["launch"], true);
  let second_review = wait_for_new_run(
    &stack,
    &worker.id,
    &HashSet::from(["run-work".to_string(), first_review.id.clone()]),
    RunKind::Review,
  )
  .await?;
  assert_eq!(second_review.review_of_run_id.as_deref(), Some("run-work"));
  assert_eq!(second_review.backend.as_deref(), Some("fake2"));
  assert_eq!(second_review.model.as_deref(), Some("fake2-model"));
  assert_eq!(second_review.input_event_ids, [trigger["id"].as_str().context("fired trigger has an id")?]);
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-2").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &second_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  fs::write(control.path().join("release-2"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &second_review.id).await?, RunOutcomeKind::Success);
  let prompt = fs::read_to_string(stack.store().run_dir(&worker.id, &second_review.id).join("launch_prompt.md"))?;
  assert!(prompt.contains("[Scheduled trigger] CI passed"), "{prompt}");
  assert!(!prompt.contains("add marker"), "{prompt}");
  wait_for_task_state(&stack, &worker.id, "completed").await?;
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_failed_review_continuation_reports_failure_to_the_parent
#[tokio::test]
async fn failed_review_continuation_reports_failure_to_parent() -> anyhow::Result<()> {
  let repo = SyntheticRepo::create()?;
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("implemented")),
      script_option("fake2", &gated_review_then_failure_script(control.path())),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker = stack
    .node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("add marker", Some(&repo.repo))))
    .await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "add marker").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let work = stack.store().read_run_sync(&worker.id, "run-work")?.context("work run is recorded")?;
  let worktree = PathBuf::from(work.worktree_path.context("work run has a worktree")?);
  fs::write(worktree.join("marker.txt"), "marked\n")?;
  git(&worktree, &["add", "-A"])?;
  git(&worktree, &["commit", "-m", "the work"])?;
  let first_review = wait_for_review(&stack, &worker.id, "run-work").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &first_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &first_review.id).await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;
  assert_eq!(stack.store().task_state(&worker.id)?, "open");
  assert!(child_reports(&stack, &manager.id)?.is_empty());

  fs::remove_file(pending_trigger_path(&stack, &worker.id))?;
  stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::SCHEDULED_TRIGGER,
      "CI passed",
      "system",
      json!({"input_id": "synthetic-review-continuation-wake"}),
    )
    .await?;
  stack.dispatch.dispatch_pending(&worker.id).await?;
  let continuation = wait_for_new_run(
    &stack,
    &worker.id,
    &HashSet::from(["run-work".to_string(), first_review.id.clone()]),
    RunKind::Review,
  )
  .await?;
  assert_eq!(continuation.review_of_run_id.as_deref(), Some("run-work"));
  stack.dispatch.set_executor(None);
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Failed);
  let reports = wait_for_child_reports(&stack, &manager.id, 1).await?;
  assert_eq!(reports[0]["outcome"], "failed");
  Ok(())
}

// Python: tests/test_worker_pause_continuation.py::test_review_continuation_refuses_a_reviewer_backend_no_longer_configured
#[tokio::test]
async fn review_continuation_fails_when_its_backend_is_removed() -> anyhow::Result<()> {
  let control = tempfile::tempdir()?;
  let stack = Stack::start_with(
    vec![
      script_option("fake", &result_script("implemented")),
      script_option("fake2", &gate_script_with_native_id(control.path(), "review approved", "review-native")),
    ],
    vec!["fake".to_string(), "fake2".to_string()],
  )
  .await?;
  set_access_key(&stack)?;
  let manager = stack.node("manager", None, TaskProfile::Manager, None).await?;
  let worker =
    stack.node("worker", Some(&manager.id), TaskProfile::Worker, Some(implement_task("write a report", None))).await?;
  let guard = ProcessGroupGuard::new();
  queue_and_launch_work(&stack, &worker.id, "run-work", "write a report").await?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, "run-work").await?, RunOutcomeKind::Success);
  let first_review = wait_for_review(&stack, &worker.id, "run-work").await?;
  tokio::time::timeout(Duration::from_secs(10), async {
    while !control.path().join("started-1").exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await?;
  if let Some(pid) = stack.store().read_run_sync(&worker.id, &first_review.id)?.and_then(|run| run.pid) {
    guard.track(pid);
  }
  fs::create_dir_all(pending_trigger_path(&stack, &worker.id).parent().context("trigger directory exists")?)?;
  fs::write(pending_trigger_path(&stack, &worker.id), r#"{"status":"pending","fire_at":"2030-01-01T00:00:00Z"}"#)?;
  fs::write(control.path().join("release-1"), "release")?;
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &first_review.id).await?, RunOutcomeKind::Success);
  stack.node_pass(&worker.id).await?;

  let mut config: CharlieBotConfig = (*stack.cfg).clone();
  config.backends.options.retain(|option| option.id != "fake2");
  stack.replace_config(config)?;
  fs::remove_file(pending_trigger_path(&stack, &worker.id))?;
  stack
    .dispatch
    .admit_input(
      &worker.id,
      event_types::SCHEDULED_TRIGGER,
      "CI passed",
      "system",
      json!({"input_id": "synthetic-review-backend-removed-wake"}),
    )
    .await?;
  stack.dispatch.dispatch_pending(&worker.id).await?;
  let continuation = wait_for_new_run(
    &stack,
    &worker.id,
    &HashSet::from(["run-work".to_string(), first_review.id.clone()]),
    RunKind::Review,
  )
  .await?;
  assert_eq!(continuation.backend.as_deref(), Some("fake2"));
  stack.dispatch.set_executor(None);
  assert_eq!(wait_for_run(&stack, &guard, &worker.id, &continuation.id).await?, RunOutcomeKind::Failed);
  assert_eq!(fs::read_to_string(control.path().join("count"))?, "1");
  let reports = wait_for_child_reports(&stack, &manager.id, 1).await?;
  assert_eq!(reports[0]["outcome"], "failed");
  Ok(())
}
